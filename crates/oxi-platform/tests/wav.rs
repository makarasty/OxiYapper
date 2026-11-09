use std::fs;
use std::path::PathBuf;

use oxi_platform::wav::{read_wav_mono_f32, write_wav};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxi-wav-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn written_file_reads_back_at_16k_mono_and_leaves_no_part() {
    let dir = scratch("roundtrip");
    let samples: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    let path = dir.join("a.wav");
    write_wav(&path, &samples).unwrap();

    let (back, rate) = read_wav_mono_f32(&path).unwrap();
    assert_eq!(rate, 16_000);
    assert_eq!(back.len(), samples.len());
    assert!(
        back.iter()
            .zip(&samples)
            .all(|(b, s)| (b - s).abs() < 1.0 / 16_000.0)
    );
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["a.wav"]);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn out_of_range_samples_are_clamped() {
    let dir = scratch("clamp");
    let path = dir.join("c.wav");
    write_wav(&path, &[2.0, -2.0]).unwrap();
    let (back, _) = read_wav_mono_f32(&path).unwrap();
    assert!((back[0] - 1.0).abs() < 1e-3 && (back[1] + 1.0).abs() < 1e-3);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reads_stereo_int_wav_as_mono() {
    let dir = scratch("stereo");
    let path = dir.join("a.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for (l, r) in [(16384_i16, 0_i16), (-16384, -16384)] {
        w.write_sample(l).unwrap();
        w.write_sample(r).unwrap();
    }
    w.finalize().unwrap();
    let (mono, rate) = read_wav_mono_f32(&path).unwrap();
    assert_eq!((rate, mono), (8000, vec![0.25, -0.5]));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rejects_a_path_without_wav_extension() {
    let dir = scratch("ext");
    assert!(write_wav(&dir.join("a.txt"), &[0.0]).is_err());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn io_error_message_leaves_the_cause_to_source() {
    // anyhow's `{:#}` prints message then source: the cause must not be in both.
    let dir = scratch("nodir");
    let err = write_wav(&dir.join("missing").join("a.wav"), &[0.0]).unwrap_err();
    let cause = std::error::Error::source(&err)
        .expect("io cause kept")
        .to_string();
    assert!(!err.to_string().contains(&cause), "{err}");
}
