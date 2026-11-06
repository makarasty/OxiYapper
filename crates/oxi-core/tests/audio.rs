use oxi_core::audio::{
    AudioError, CHUNK_SAMPLES, MAX_GAIN_DB, PEAK_LIMIT, Pipeline, SAMPLE_RATE, TARGET_RMS_DBFS,
    downmix, level, process, samples_for,
};
use oxi_core::config::Audio;
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

fn opts(trim: bool, normalize: bool, denoise: bool) -> Audio {
    Audio {
        trim,
        normalize,
        denoise,
        ..Audio::default()
    }
}

/// Only resampling (and gain): no gate, no normalising.
fn plain() -> Audio {
    opts(false, false, false)
}

/// Trim on, normalise off, so the output keeps the input amplitude.
fn trim_only() -> Audio {
    opts(true, false, false)
}

#[test]
fn constants_are_consistent() {
    assert_eq!(CHUNK_SAMPLES, 320);
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
fn push_applies_gain_and_meters_it() {
    let mut p = Pipeline::new(
        SAMPLE_RATE,
        &Audio {
            gain_db: 6.0,
            ..plain()
        },
    )
    .unwrap();
    let l = p.push(&[0.1, -0.2]).unwrap();
    assert!((l.peak - 0.2 * 1.9953).abs() < 1e-4, "peak {}", l.peak);
    let out = process(
        &sine(300.0, SAMPLE_RATE, 16_000, 0.1),
        SAMPLE_RATE,
        &Audio {
            gain_db: -6.0,
            ..plain()
        },
    )
    .unwrap();
    assert!((db(level(&out).rms) - db(0.1 / 2.0_f32.sqrt()) + 6.0).abs() < 0.1);
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
fn resample_length_within_one_chunk() {
    for (rate, input_len) in [
        (48_000, 48_000),
        (44_100, 44_100),
        (44_100, 12_345),
        (16_000, 16_000),
        (8_000, 3_333),
    ] {
        let out = process(&sine(300.0, rate, input_len, 0.5), rate, &plain()).unwrap();
        let expected = input_len * SAMPLE_RATE as usize / rate as usize;
        assert!(
            out.len().abs_diff(expected) <= CHUNK_SAMPLES,
            "{rate}: {} vs {expected}",
            out.len()
        );
    }
}

#[test]
fn streaming_equals_one_shot_and_keeps_the_tone() {
    let input = sine(440.0, 48_000, 48_000, 0.5);
    let whole = process(&input, 48_000, &plain()).unwrap();

    // Callback-sized pieces that do not line up with the 20 ms chunk.
    let mut many = Pipeline::new(48_000, &plain()).unwrap();
    for part in input.chunks(441) {
        many.push(part).unwrap();
    }
    let pieces = many.finish().unwrap();

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
fn zero_rate_is_rejected() {
    for denoise in [false, true] {
        assert!(matches!(
            Pipeline::new(0, &opts(true, true, denoise)),
            Err(AudioError::Resample(_))
        ));
    }
}

#[test]
fn denoise_lowers_noise() {
    // 44.1 kHz input also exercises the resample to 48 kHz in front of RNNoise.
    for rate in [48_000, 44_100] {
        let input = noise(rate as usize, 0.05);
        let raw = process(&input, rate, &plain()).unwrap();
        let clean = process(&input, rate, &opts(false, false, true)).unwrap();
        assert!(clean.len().abs_diff(raw.len()) <= CHUNK_SAMPLES);
        // Skip the first 100 ms (RNNoise adapts). White noise is hard for it: 4 dB at 48 kHz, 2.3 at 44.1.
        let skip = 1600;
        let (before, after) = (level(&raw[skip..]).rms, level(&clean[skip..]).rms);
        assert!(
            db(after) < db(before) - 2.0,
            "{rate}: noise only dropped from {} to {} dB",
            db(before),
            db(after)
        );
    }
}

fn normalised(input: &[f32]) -> Vec<f32> {
    process(input, SAMPLE_RATE, &opts(false, true, false)).unwrap()
}

#[test]
fn normalise_hits_target_within_half_db() {
    for start_dbfs in [-40.0_f32, -30.0, -25.0, -20.0, -15.0] {
        let amp = 10.0_f32.powf(start_dbfs / 20.0) * 2.0_f32.sqrt();
        let got = db(level(&normalised(&sine(300.0, SAMPLE_RATE, 16_000, amp))).rms);
        assert!(
            (got - TARGET_RMS_DBFS).abs() < 0.5,
            "{start_dbfs} dBFS became {got}"
        );
    }
}

#[test]
fn normalise_caps_gain_at_20_db() {
    let s = sine(300.0, SAMPLE_RATE, 16_000, 0.001);
    let gain = db(level(&normalised(&s)).rms) - db(level(&s).rms);
    assert!((gain - MAX_GAIN_DB).abs() < 0.1, "gain {gain}");
}

#[test]
fn normalise_limits_the_peak() {
    // Quiet body with one spike: RMS wants +20 dB, the spike would clip.
    let mut s = sine(300.0, SAMPLE_RATE, 16_000, 0.02);
    s[8000] = 0.5;
    let peak = level(&normalised(&s)).peak;
    assert!(peak <= PEAK_LIMIT + 1e-6, "peak {peak}");
    assert!(
        peak > PEAK_LIMIT - 0.01,
        "peak limit should be the binding one, got {peak}"
    );
}

#[test]
fn all_zero_is_mic_silence_whatever_the_options() {
    for o in [
        Audio::default(),
        trim_only(),
        plain(),
        opts(true, true, true),
    ] {
        for len in [0, 32_000] {
            assert!(
                matches!(
                    process(&vec![0.0; len], SAMPLE_RATE, &o),
                    Err(AudioError::MicSilence)
                ),
                "{o:?} len {len}"
            );
        }
    }
}

#[test]
fn faint_noise_without_speech_is_nothing_heard() {
    assert!(matches!(
        process(&noise(32_000, 0.003), SAMPLE_RATE, &Audio::default()),
        Err(AudioError::NothingHeard)
    ));
}

/// `lead` silence, a tone, `tail` silence, all in samples at 16 kHz.
fn tone_in_silence(lead: usize, tone: usize, tail: usize) -> Vec<f32> {
    let mut s = vec![0.0; lead];
    s.extend(sine(300.0, SAMPLE_RATE, tone, 0.5));
    s.resize(lead + tone + tail, 0.0);
    s
}

/// Index of the first sample louder than -40 dBFS.
fn onset(s: &[f32]) -> usize {
    s.iter().position(|x| x.abs() > 0.01).unwrap()
}

#[test]
fn trim_keeps_tone_plus_200_ms_padding() {
    let pad = samples_for(Millis(200), SAMPLE_RATE);
    let s = tone_in_silence(16_000, 16_000, 16_000);
    let t = process(&s, SAMPLE_RATE, &trim_only()).unwrap();
    // The gate works in 256-sample frames and may miss the tone edges by a frame or two;
    // earshot also holds "voice" for about 100 ms after the tone ends.
    let expected = 16_000 + 2 * pad;
    assert!(
        (expected - 3 * 256..=expected + 8 * 256).contains(&t.len()),
        "{} vs {expected}",
        t.len()
    );
    // The padding before the tone is there.
    assert!(onset(&t).abs_diff(pad) <= 3 * 256, "onset {}", onset(&t));
}

#[test]
fn trim_padding_is_clamped_to_the_clip() {
    // Only 100 ms of silence before the tone and none after: nothing to cut.
    let s = tone_in_silence(1_600, 16_000, 0);
    let t = process(&s, SAMPLE_RATE, &trim_only()).unwrap();
    assert!(t.len() >= s.len() - 3 * 256, "{} of {}", t.len(), s.len());
}

#[test]
fn trim_keeps_silence_between_voiced_parts() {
    let mut s = tone_in_silence(8_000, 8_000, 16_000);
    s.extend(sine(300.0, SAMPLE_RATE, 8_000, 0.5));
    s.resize(s.len() + 8_000, 0.0);
    let t = process(&s, SAMPLE_RATE, &trim_only()).unwrap();
    let pad = samples_for(Millis(200), SAMPLE_RATE);
    assert!(t.len() >= 8_000 + 16_000 + 8_000 + 2 * pad - 4 * 256);
}

/// Uniform white noise amplitude for an RMS of `dbfs`.
fn noise_amp(dbfs: f32) -> f32 {
    10.0_f32.powf(dbfs / 20.0) * 3.0_f32.sqrt()
}

/// `tone` samples of 300 Hz at `amp` after `lead` and before 8000 samples of silence, all over
/// white noise of `noise_amp`, through the default chain (denoise as given).
fn gated(
    lead: usize,
    tone: usize,
    amp: f32,
    noise_amp: f32,
    denoise: bool,
) -> Result<Vec<f32>, AudioError> {
    let mut s = vec![0.0; lead];
    s.extend(sine(300.0, SAMPLE_RATE, tone, amp));
    s.resize(lead + tone + 8_000, 0.0);
    for (x, n) in s.iter_mut().zip(noise(lead + tone + 8_000, noise_amp)) {
        *x += n;
    }
    process(
        &s,
        SAMPLE_RATE,
        &Audio {
            denoise,
            ..Audio::default()
        },
    )
}

#[test]
fn speech_gate_needs_250_ms_without_counting_the_hold() {
    // earshot's hold after a tone must not lift 200 ms over the 250 ms bar.
    for denoise in [false, true] {
        for background in [0.0, noise_amp(-50.0)] {
            let case = |ms: usize| gated(8_000, ms * 16, 0.5, background, denoise);
            for ms in [100, 200] {
                assert!(
                    matches!(case(ms), Err(AudioError::NothingHeard)),
                    "{ms} ms, noise {background}, denoise {denoise}"
                );
            }
            for ms in [300, 600] {
                assert!(
                    case(ms).is_ok(),
                    "{ms} ms, noise {background}, denoise {denoise}"
                );
            }
        }
    }
}

#[test]
fn speech_gate_does_not_depend_on_the_noise_level() {
    // 2 s of -20 dBFS tone over -30 dBFS noise (10 dB SNR), and quieter at the same SNR.
    for (tone_dbfs, noise_dbfs) in [(-20.0_f32, -30.0), (-22.0, -30.0), (-45.0, -55.0)] {
        let amp = 10.0_f32.powf(tone_dbfs / 20.0) * 2.0_f32.sqrt();
        assert!(
            gated(8_000, 32_000, amp, noise_amp(noise_dbfs), false).is_ok(),
            "{tone_dbfs} over {noise_dbfs} dBFS"
        );
    }
}

#[test]
fn speech_from_the_first_sample_is_heard() {
    assert!(gated(0, 9_600, 0.5, 0.0, false).is_ok());
}
