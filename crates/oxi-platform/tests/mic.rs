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
