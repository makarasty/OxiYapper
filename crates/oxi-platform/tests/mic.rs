//! Real default microphone. Runs only with `OXI_MIC_TEST=1`; audio is never written to disk.

use std::thread::sleep;
use std::time::{Duration, Instant};

use oxi_core::audio::{Level, SAMPLE_RATE, level};
use oxi_core::config::Audio;
use oxi_platform::capture::{Capture, CpalCapture, LevelMeter, list_input_devices};

const SILENT: Level = Level {
    peak: 0.0,
    rms: 0.0,
};

fn mic_test() -> bool {
    let on = std::env::var_os("OXI_MIC_TEST").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set OXI_MIC_TEST=1");
    }
    on
}

#[test]
fn devices_are_listed_and_an_unknown_name_falls_back_to_the_default() {
    if !mic_test() {
        return;
    }
    let names = list_input_devices().unwrap();
    eprintln!("input devices: {}", names.len());
    assert!(!names.is_empty() && names.iter().all(|n| !n.is_empty()));
    let default = CpalCapture::open(&Audio::default(), LevelMeter::default(), |_| {}).unwrap();
    assert!(!default.fell_back_to_default());
    drop(default);
    let gone = Audio {
        device: "no such microphone".into(),
        ..Audio::default()
    };
    let fallback = CpalCapture::open(&gone, LevelMeter::default(), |_| {}).unwrap();
    assert!(fallback.fell_back_to_default());
}

#[test]
fn default_mic_captures_one_second() {
    if !mic_test() {
        return;
    }
    // No trim or normalise: a quiet room must not turn into NothingHeard here.
    let opts = Audio {
        trim: false,
        normalize: false,
        ..Audio::default()
    };
    let meter = LevelMeter::default();
    let capture = CpalCapture::open(&opts, meter.clone(), |_| {}).unwrap();
    let rate = capture.input_rate();
    // Read the meter the way the overlay does: 30 Hz for one second, without the capture object.
    let mut loudest = SILENT;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        let l = meter.read();
        loudest.peak = loudest.peak.max(l.peak);
        loudest.rms = loudest.rms.max(l.rms);
        sleep(Duration::from_millis(33));
    }
    let dropped = capture.dropped_samples();
    let samples = capture.finish().unwrap();
    let l = level(&samples);
    eprintln!(
        "mic: device rate {rate}, {} samples, peak {:.4}, rms {:.4}, dropped {dropped}; \
         meter max peak {:.4}, max rms {:.4}",
        samples.len(),
        l.peak,
        l.rms,
        loudest.peak,
        loudest.rms
    );
    assert!(samples.len() > SAMPLE_RATE as usize * 8 / 10);
    assert!(samples.len() <= SAMPLE_RATE as usize * 12 / 10);
    assert!(loudest.peak > 0.0 && loudest.rms > 0.0);
    assert_eq!(meter.read(), SILENT, "finish zeroes the meter");

    // Cancel: dropping without finish must stop the drain thread (the meter is zeroed after the join).
    let capture = CpalCapture::open(&opts, meter.clone(), |_| {}).unwrap();
    let start = Instant::now();
    while meter.read() == SILENT && start.elapsed() < Duration::from_secs(1) {
        sleep(Duration::from_millis(10));
    }
    assert_ne!(meter.read(), SILENT, "meter live before the drop");
    drop(capture);
    assert_eq!(meter.read(), SILENT, "drop stopped the capture");
}

/// Probe every input: default format, raw per-channel level from cpal, then the level through
/// `CpalCapture` (the daemon's path) with and without trim/normalise. Numbers only, never audio.
#[test]
fn probe_every_input() {
    use cpal::traits::{DeviceTrait, HostTrait};
    if !mic_test() {
        return;
    }
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_owned());
    eprintln!("default input: {default:?}");
    for dev in host.input_devices().unwrap() {
        let name = dev
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_default();
        let Ok(cfg) = dev.default_input_config() else {
            eprintln!("{name}: no default config");
            continue;
        };
        let ch = usize::from(cfg.channels());
        eprintln!(
            "{name}: {ch} ch, {} Hz, {:?}",
            cfg.sample_rate(),
            cfg.sample_format()
        );
        raw_levels(&dev, &cfg);
        for (label, trim) in [("pipeline no-trim", false), ("pipeline default", true)] {
            let opts = Audio {
                device: name.clone(),
                trim,
                normalize: trim,
                ..Audio::default()
            };
            let meter = LevelMeter::default();
            let capture = match CpalCapture::open(&opts, meter.clone(), |_| {}) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  {label}: open failed: {e}");
                    continue;
                }
            };
            let mut loudest = SILENT;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                let l = meter.read();
                loudest.peak = loudest.peak.max(l.peak);
                loudest.rms = loudest.rms.max(l.rms);
                sleep(Duration::from_millis(33));
            }
            let fell = capture.fell_back_to_default();
            match capture.finish() {
                Ok(s) => {
                    let l = level(&s);
                    eprintln!(
                        "  {label}: fell_back {fell}, out {} samples peak {:.5} rms {:.5}; meter max peak {:.5} rms {:.5}",
                        s.len(),
                        l.peak,
                        l.rms,
                        loudest.peak,
                        loudest.rms
                    );
                }
                Err(e) => eprintln!(
                    "  {label}: fell_back {fell}, finish error: {e}; meter max peak {:.5} rms {:.5}",
                    loudest.peak, loudest.rms
                ),
            }
        }
    }
}

/// Per-channel peak and RMS of 3 s straight from cpal, f32 streams only (the default format here).
fn raw_levels(dev: &cpal::Device, cfg: &cpal::SupportedStreamConfig) {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use std::sync::{Arc, Mutex};
    let ch = usize::from(cfg.channels());
    // Raw: per channel sum of squares and peak, f32 stream only (the default format here).
    let acc = Arc::new(Mutex::new((vec![0.0_f64; ch], vec![0.0_f32; ch], 0_usize)));
    let a = Arc::clone(&acc);
    let stream = dev.build_input_stream(
        cfg.config(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mut g = a.lock().unwrap();
            for frame in data.chunks_exact(ch) {
                for (c, s) in frame.iter().enumerate() {
                    g.0[c] += f64::from(s * s);
                    g.1[c] = g.1[c].max(s.abs());
                }
                g.2 += 1;
            }
        },
        |_| {},
        None,
    );
    match stream {
        Ok(s) => {
            s.play().unwrap();
            sleep(Duration::from_secs(3));
            drop(s);
            let g = acc.lock().unwrap();
            for c in 0..ch {
                eprintln!(
                    "  raw ch{c}: peak {:.5} rms {:.5} ({} frames)",
                    g.1[c],
                    (g.0[c] / g.2.max(1) as f64).sqrt(),
                    g.2
                );
            }
        }
        Err(e) => eprintln!("  raw open failed: {e}"),
    }
}

#[test]
fn playback_opens_the_default_output() {
    if !mic_test() {
        return;
    }
    // Silence only: the probe must not make a sound on the owner's speakers.
    let sound = oxi_platform::capture::play(&[0.0; 3_200], SAMPLE_RATE).unwrap();
    sleep(Duration::from_millis(300));
    drop(sound);
}
