use oxi_core::audio::{
    AudioError, CHUNK_SAMPLES, DENOISE_FRAME, DENOISE_RATE, Denoiser, MAX_GAIN_DB, PEAK_LIMIT,
    SAMPLE_RATE, StreamResampler, TARGET_RMS_DBFS, apply_gain_db, downmix, level, levels,
    normalise, samples_for, trim,
};
use oxi_core::types::Millis;
use std::f32::consts::TAU;

fn sine(freq: f32, rate: u32, len: usize, amp: f32) -> Vec<f32> {
    (0..len)
        .map(|i| amp * (TAU * freq * i as f32 / rate as f32).sin())
        .collect()
}

fn db(x: f32) -> f32 {
    20.0 * x.log10()
}

/// Deterministic white noise in `[-amp, amp]`.
fn noise(len: usize, amp: f32) -> Vec<f32> {
    let mut state = 12345_u32;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            amp * ((state >> 8) as f32 / (1 << 23) as f32 - 1.0)
        })
        .collect()
}

#[test]
fn constants_are_consistent() {
    assert_eq!(CHUNK_SAMPLES, 320);
    assert_eq!(DENOISE_FRAME, 480);
    assert_eq!(DENOISE_RATE, 48_000);
    assert_eq!(samples_for(Millis(200), SAMPLE_RATE), 3200);
}

#[test]
fn downmix_averages_channels() {
    assert_eq!(downmix(&[1.0, 3.0, -1.0, 1.0], 2), vec![2.0, 0.0]);
    assert_eq!(downmix(&[0.5, 0.25], 1), vec![0.5, 0.25]);
    // A trailing incomplete frame is dropped.
    assert_eq!(downmix(&[1.0, 1.0, 1.0], 2), vec![1.0]);
}

#[test]
fn gain_in_db() {
    let mut s = vec![0.1, -0.2];
    apply_gain_db(&mut s, 6.0);
    assert!((s[0] - 0.1 * 1.9953).abs() < 1e-4 && (s[1] + 0.2 * 1.9953).abs() < 1e-4);
    apply_gain_db(&mut s, -6.0);
    assert!((s[0] - 0.1).abs() < 1e-4);
}

#[test]
fn level_of_sine_and_silence() {
    let l = level(&sine(440.0, 16_000, 16_000, 0.5));
    assert!((l.peak - 0.5).abs() < 1e-3);
    assert!((l.rms - 0.5 / 2.0_f32.sqrt()).abs() < 1e-3);
    assert_eq!(level(&[]).peak, 0.0);
    assert_eq!(level(&[0.0; 10]).rms, 0.0);
}

#[test]
fn levels_per_20_ms_chunk() {
    // 50 ms at 16 kHz: two full chunks and a 10 ms rest; only the first is loud.
    let mut s = vec![0.0; 800];
    s[..320].fill(0.5);
    let ls = levels(&s, SAMPLE_RATE);
    assert_eq!(ls.len(), 3);
    assert_eq!(ls[0].peak, 0.5);
    assert_eq!(ls[1].peak, 0.0);
    assert_eq!(ls[2].peak, 0.0);
}

#[test]
fn resample_length_within_one_chunk() {
    for (rate, input_len) in [
        (48_000, 48_000),
        (44_100, 44_100),
        (44_100, 12_345),
        (16_000, 16_000),
        (8_000, 3_333),
    ] {
        let mut r = StreamResampler::new(rate, SAMPLE_RATE).unwrap();
        let mut out = r.push(&sine(300.0, rate, input_len, 0.5)).unwrap();
        out.extend(r.finish().unwrap());
        let expected = input_len * SAMPLE_RATE as usize / rate as usize;
        assert!(
            out.len().abs_diff(expected) <= CHUNK_SAMPLES,
            "{rate}: {} vs {expected}",
            out.len()
        );
    }
}

#[test]
fn resample_streaming_equals_one_shot_and_keeps_the_tone() {
    let input = sine(440.0, 48_000, 48_000, 0.5);
    let mut one = StreamResampler::new(48_000, SAMPLE_RATE).unwrap();
    let mut whole = one.push(&input).unwrap();
    whole.extend(one.finish().unwrap());

    // Callback-sized pieces that do not line up with the 20 ms chunk.
    let mut many = StreamResampler::new(48_000, SAMPLE_RATE).unwrap();
    let mut pieces = Vec::new();
    for part in input.chunks(441) {
        pieces.extend(many.push(part).unwrap());
    }
    pieces.extend(many.finish().unwrap());

    assert_eq!(whole.len(), 16_000);
    assert_eq!(pieces.len(), whole.len());
    // Same tone at the same amplitude, delay removed (check the middle, away from the edges).
    let mid = &whole[2000..14_000];
    assert!((level(mid).rms - 0.5 / 2.0_f32.sqrt()).abs() < 0.01);
    let ref_tone = sine(440.0, SAMPLE_RATE, 16_000, 0.5);
    let err = level(
        &mid.iter()
            .zip(&ref_tone[2000..14_000])
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>(),
    )
    .rms;
    // rubato reports an integer delay; the true one is a fraction of a sample off (rms 0.02 here).
    assert!(err < 0.03, "tone differs from reference by rms {err}");
    for (a, b) in whole.iter().zip(&pieces) {
        assert!((a - b).abs() < 1e-4);
    }
}

#[test]
fn resample_rejects_zero_rate() {
    assert!(matches!(
        StreamResampler::new(0, SAMPLE_RATE),
        Err(AudioError::Resample(_))
    ));
}

#[test]
fn denoise_keeps_length_and_lowers_noise() {
    let input = noise(DENOISE_RATE as usize, 0.05);
    let mut d = Denoiser::new();
    // Odd push sizes: frames must carry over between calls.
    let mut out = Vec::new();
    for part in input.chunks(1000) {
        out.extend(d.push(part));
    }
    out.extend(d.finish());
    assert_eq!(out.len(), input.len());
    // Skip the first 100 ms (RNNoise adapts).
    let skip = 4800;
    let before = level(&input[skip..]).rms;
    let after = level(&out[skip..]).rms;
    assert!(
        db(after) < db(before) - 3.0,
        "noise only dropped from {} to {} dB",
        db(before),
        db(after)
    );
}

#[test]
fn normalise_hits_target_within_half_db() {
    for start_dbfs in [-40.0_f32, -30.0, -25.0, -20.0, -15.0] {
        let amp = 10.0_f32.powf(start_dbfs / 20.0) * 2.0_f32.sqrt();
        let mut s = sine(300.0, SAMPLE_RATE, 16_000, amp);
        let gain = normalise(&mut s).unwrap();
        let got = db(level(&s).rms);
        if start_dbfs > TARGET_RMS_DBFS {
            // Already above target: attenuated, not boosted.
            assert!(gain < 0.0);
        }
        assert!(
            (got - TARGET_RMS_DBFS).abs() < 0.5,
            "{start_dbfs} dBFS became {got}"
        );
        assert!((gain - (TARGET_RMS_DBFS - start_dbfs)).abs() < 0.5);
    }
}

#[test]
fn normalise_caps_gain_at_20_db() {
    let mut s = sine(300.0, SAMPLE_RATE, 16_000, 0.001);
    let before = level(&s).rms;
    let gain = normalise(&mut s).unwrap();
    assert!((gain - MAX_GAIN_DB).abs() < 0.01);
    assert!((db(level(&s).rms) - db(before) - MAX_GAIN_DB).abs() < 0.01);
}

#[test]
fn normalise_limits_the_peak() {
    // Quiet body with one spike: RMS wants +20 dB, the spike would clip.
    let mut s = sine(300.0, SAMPLE_RATE, 16_000, 0.02);
    s[100] = 0.5;
    normalise(&mut s).unwrap();
    let peak = level(&s).peak;
    assert!(peak <= PEAK_LIMIT + 1e-6, "peak {peak}");
    assert!(
        peak > PEAK_LIMIT - 0.01,
        "peak limit should be the binding one, got {peak}"
    );
}

#[test]
fn normalise_all_zero_is_mic_silence() {
    let mut s = vec![0.0; 16_000];
    assert!(matches!(normalise(&mut s), Err(AudioError::MicSilence)));
    assert!(s.iter().all(|&x| x == 0.0));
    assert!(matches!(normalise(&mut []), Err(AudioError::MicSilence)));
}

#[test]
fn trim_silence_only_is_nothing_heard() {
    assert!(matches!(
        trim(&vec![0.0; 32_000]),
        Err(AudioError::NothingHeard)
    ));
    assert!(matches!(trim(&[]), Err(AudioError::NothingHeard)));
}

/// `lead` silence, a tone, `tail` silence, all in samples at 16 kHz.
fn tone_in_silence(lead: usize, tone: usize, tail: usize) -> Vec<f32> {
    let mut s = vec![0.0; lead];
    s.extend(sine(300.0, SAMPLE_RATE, tone, 0.5));
    s.resize(lead + tone + tail, 0.0);
    s
}

#[test]
fn trim_keeps_tone_plus_200_ms_padding() {
    let pad = samples_for(Millis(200), SAMPLE_RATE);
    let s = tone_in_silence(16_000, 16_000, 16_000);
    let t = trim(&s).unwrap();
    // The gate works in 256-sample frames and may miss the tone edges by a frame or two;
    // earshot also holds "voice" for about 100 ms after the tone ends.
    let expected = 16_000 + 2 * pad;
    assert!(
        (expected - 3 * 256..=expected + 8 * 256).contains(&t.len()),
        "{} vs {expected}",
        t.len()
    );
    // It is a slice of the input, positioned around the tone.
    let start = t.as_ptr() as usize - s.as_ptr() as usize;
    let start = start / size_of::<f32>();
    assert!(start.abs_diff(16_000 - pad) <= 3 * 256, "start {start}");
}

#[test]
fn trim_padding_is_clamped_to_the_clip() {
    // Only 100 ms of silence before the tone and none after: nothing to cut.
    let s = tone_in_silence(1_600, 16_000, 0);
    let t = trim(&s).unwrap();
    assert!(t.len() >= s.len() - 3 * 256, "{} of {}", t.len(), s.len());
}

#[test]
fn trim_keeps_silence_between_voiced_parts() {
    let mut s = tone_in_silence(8_000, 8_000, 16_000);
    s.extend(sine(300.0, SAMPLE_RATE, 8_000, 0.5));
    s.resize(s.len() + 8_000, 0.0);
    let t = trim(&s).unwrap();
    let pad = samples_for(Millis(200), SAMPLE_RATE);
    assert!(t.len() >= 8_000 + 16_000 + 8_000 + 2 * pad - 4 * 256);
}

#[test]
fn trim_short_burst_is_nothing_heard() {
    // 100 ms of tone is under the 250 ms of voice needed; 600 ms is enough.
    let short = tone_in_silence(8_000, 1_600, 8_000);
    assert!(matches!(trim(&short), Err(AudioError::NothingHeard)));
    let long = tone_in_silence(8_000, 9_600, 8_000);
    assert!(trim(&long).is_ok());
}
