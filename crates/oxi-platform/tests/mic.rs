//! Real default microphone. Runs only with `OXI_MIC_TEST=1`; audio is never written to disk.

use std::thread::sleep;
use std::time::Duration;

use oxi_core::audio::{SAMPLE_RATE, level};
use oxi_core::config::Audio;
use oxi_platform::capture::{Capture, CpalCapture};

#[test]
fn default_mic_captures_one_second() {
    if std::env::var_os("OXI_MIC_TEST").is_none_or(|v| v != "1") {
        eprintln!("skipped: set OXI_MIC_TEST=1");
        return;
    }
    // No trim or normalise: a quiet room must not turn into NothingHeard here.
    let opts = Audio {
        trim: false,
        normalize: false,
        ..Audio::default()
    };
    let capture = CpalCapture::open("", &opts, |_| {}).unwrap();
    let rate = capture.input_rate();
    sleep(Duration::from_secs(1));
    let dropped = capture.dropped_samples();
    let samples = capture.finish().unwrap();
    let l = level(&samples);
    eprintln!(
        "mic: device rate {rate}, {} samples, peak {:.4}, rms {:.4}, dropped {dropped}",
        samples.len(),
        l.peak,
        l.rms
    );
    assert!(samples.len() > SAMPLE_RATE as usize * 8 / 10);
    assert!(samples.len() <= SAMPLE_RATE as usize * 12 / 10);
}
