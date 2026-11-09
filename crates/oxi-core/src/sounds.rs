//! Start and stop sounds (SPEC, Sounds): our own synthesised set, so no third-party rights, and
//! a decoder for a custom `.wav`. Pure: the samples are made here, the file read and the
//! playback belong to the daemon.

use std::f32::consts::TAU;
use std::path::{Path, PathBuf};

/// Sample rate the built-in sounds are made at.
pub const RATE: u32 = 48_000;

/// A custom clip longer than this is refused: a sound, not a song.
pub const MAX_CUSTOM_SECONDS: u32 = 5;

/// Sample rates of a custom file that are accepted; outside this the resampler would be asked
/// for absurd sizes.
pub const CUSTOM_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;

/// Loudest sample of a built-in at volume 1.
const PEAK: f32 = 0.6;

/// A built-in sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Rise,
    Fall,
    Pop,
    Tick,
    /// For `Sound::Error`; not offered for the start or stop sound.
    Fault,
}

impl Builtin {
    /// The sounds the settings offer, in list order.
    pub const PICKABLE: [Self; 4] = [Self::Rise, Self::Fall, Self::Pop, Self::Tick];

    /// The id written in `config.toml`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Rise => "rise",
            Self::Fall => "fall",
            Self::Pop => "pop",
            Self::Tick => "tick",
            Self::Fault => "fault",
        }
    }

    /// Mono samples at [`RATE`]; every sound is under 300 ms and never silent.
    #[must_use]
    pub fn samples(self) -> Vec<f32> {
        // (start Hz, end Hz, milliseconds) per note, played one after the other.
        let notes: &[(f32, f32, u32)] = match self {
            Self::Rise => &[(660.0, 660.0, 80), (880.0, 880.0, 120)],
            Self::Fall => &[(880.0, 880.0, 80), (660.0, 660.0, 120)],
            Self::Pop => &[(420.0, 140.0, 90)],
            Self::Tick => &[(1800.0, 1800.0, 30)],
            Self::Fault => &[(220.0, 220.0, 110), (165.0, 165.0, 150)],
        };
        notes
            .iter()
            .flat_map(|&(from, to, ms)| note(from, to, ms))
            .collect()
    }
}

/// One sine note gliding from `from` to `to` Hz: a short attack, then a quadratic decay.
fn note(from: f32, to: f32, ms: u32) -> impl Iterator<Item = f32> {
    let len = RATE * ms / 1000;
    let attack = (len / 10).max(1) as f32;
    let mut phase = 0.0_f32;
    (0..len).map(move |i| {
        let t = i as f32 / len as f32;
        phase += TAU * (from + (to - from) * t) / RATE as f32;
        let env = (i as f32 / attack).min(1.0) * (1.0 - t) * (1.0 - t);
        phase.sin() * env * PEAK
    })
}

/// What a `sounds.start` or `sounds.stop` value names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Builtin(Builtin),
    Custom(PathBuf),
}

/// The value is neither a built-in id nor a path to a `.wav`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a built-in sound or a .wav path")]
pub struct UnknownSound(pub String);

impl Choice {
    /// A built-in id, or a path (it has a separator or a drive colon, or ends in `.wav`).
    ///
    /// # Errors
    /// [`UnknownSound`] for any other text.
    pub fn parse(text: &str) -> Result<Self, UnknownSound> {
        let text = text.trim();
        if let Some(b) = Builtin::PICKABLE.into_iter().find(|b| b.id() == text) {
            return Ok(Self::Builtin(b));
        }
        let path = Path::new(text);
        let is_wav = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
        if is_wav || text.contains(['/', '\\', ':']) {
            Ok(Self::Custom(path.to_path_buf()))
        } else {
            Err(UnknownSound(text.to_owned()))
        }
    }
}

/// Whether `path` is worth opening as a custom sound: not a UNC, device or verbatim path
/// (`\\nas\a.wav`, `\\.\pipe\x.wav`), no alternate data stream, no reserved device name
/// (`NUL.wav`). Opening any of those can block for ever; the daemon also checks `is_file`.
#[must_use]
pub fn is_plain_local_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\") || text.starts_with("//") {
        return false;
    }
    // A colon is only the drive letter's.
    if text.char_indices().any(|(i, c)| c == ':' && i != 1) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| c.is_ascii_digit() && c != '0');
    !device
}

/// Mono samples and their rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Clip {
    #[must_use]
    pub fn builtin(b: Builtin) -> Self {
        Self {
            samples: b.samples(),
            rate: RATE,
        }
    }

    /// The clip scaled by `volume` (0 to 1).
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a gain between 0 and 1 fits f32"
    )]
    pub fn scaled(&self, volume: f64) -> Vec<f32> {
        let gain = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        } as f32;
        self.samples.iter().map(|s| s * gain).collect()
    }
}

/// Why a custom file is not used. The text is for the settings and the tray, never a path
/// of dictated content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WavError {
    #[error("not a WAV file")]
    NotWav,
    #[error("unsupported WAV format (use 16-bit, 24-bit or 32-bit PCM or 32-bit float)")]
    Unsupported,
    #[error("longer than {MAX_CUSTOM_SECONDS} seconds")]
    TooLong,
    #[error("empty")]
    Empty,
}

/// Decodes a RIFF/WAVE file to mono (channels averaged).
///
/// # Errors
/// [`WavError`] for anything but non-empty 8/16/24/32-bit PCM or 32-bit float of at most
/// [`MAX_CUSTOM_SECONDS`] seconds.
pub fn decode_wav(bytes: &[u8]) -> Result<Clip, WavError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWav);
    }
    let mut fmt = None;
    let mut data = None;
    let mut rest = &bytes[12..];
    while rest.len() >= 8 {
        let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let body = &rest[8..];
        // A truncated last chunk is read as far as it goes.
        let chunk = &body[..size.min(body.len())];
        match &rest[..4] {
            b"fmt " => fmt = Some(chunk),
            b"data" => data = Some(chunk),
            _ => {}
        }
        // Chunks are padded to an even size.
        rest = body.get(size + (size & 1)..).unwrap_or_default();
    }
    let (fmt, data) = fmt.zip(data).ok_or(WavError::NotWav)?;
    if fmt.len() < 16 {
        return Err(WavError::NotWav);
    }
    let le16 = |at: usize| u16::from_le_bytes([fmt[at], fmt[at + 1]]);
    let mut tag = le16(0);
    if tag == 0xFFFE && fmt.len() >= 26 {
        tag = le16(24); // WAVE_FORMAT_EXTENSIBLE: the sub-format starts the GUID
    }
    let (channels, bits) = (usize::from(le16(2)), le16(14));
    let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
    let width = usize::from(bits / 8);
    let decode: fn(&[u8]) -> f32 = match (tag, bits) {
        (1, 8) => |b| (f32::from(b[0]) - 128.0) / 128.0,
        (1, 16) => |b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
        (1, 24) => |b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
        (1, 32) => |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        (3, 32) => |b| {
            let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        },
        _ => return Err(WavError::Unsupported),
    };
    if channels == 0 || !CUSTOM_RATES.contains(&rate) {
        return Err(WavError::Unsupported);
    }
    let frame = width * channels;
    let frames = data.len() / frame;
    if frames == 0 {
        return Err(WavError::Empty);
    }
    if frames > MAX_CUSTOM_SECONDS as usize * rate as usize {
        return Err(WavError::TooLong);
    }
    let samples = data
        .chunks_exact(frame)
        .map(|f| {
            let sum: f32 = f.chunks_exact(width).map(decode).sum();
            sum / channels as f32
        })
        .collect();
    Ok(Clip { samples, rate })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_are_short_and_not_silent() {
        for b in [Builtin::PICKABLE.as_slice(), &[Builtin::Fault]].concat() {
            let clip = Clip::builtin(b);
            let ms = clip.samples.len() * 1000 / RATE as usize;
            assert!((20..500).contains(&ms), "{b:?} is {ms} ms");
            let peak = clip.samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.1 && peak <= PEAK, "{b:?} peak {peak}");
        }
    }

    #[test]
    fn built_in_ids_are_unique_and_parse_back() {
        for b in Builtin::PICKABLE {
            assert_eq!(Choice::parse(b.id()), Ok(Choice::Builtin(b)));
        }
        assert!(Choice::parse("fault").is_err(), "not pickable");
    }

    #[test]
    fn a_choice_is_an_id_or_a_wav_path() {
        assert_eq!(
            Choice::parse(r"C:\Sounds\my.wav"),
            Ok(Choice::Custom(r"C:\Sounds\my.wav".into()))
        );
        assert_eq!(
            Choice::parse("beep.WAV"),
            Ok(Choice::Custom("beep.WAV".into()))
        );
        assert_eq!(Choice::parse("nope"), Err(UnknownSound("nope".into())));
        assert!(Choice::parse("").is_err());
    }

    fn wav(tag: u16, bits: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut out = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        out.extend(16_u32.to_le_bytes());
        out.extend(tag.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        let align = channels * bits / 8;
        out.extend(rate.wrapping_mul(u32::from(align)).to_le_bytes());
        out.extend(align.to_le_bytes());
        out.extend(bits.to_le_bytes());
        out.extend(b"data");
        out.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn decodes_pcm16_and_mixes_stereo_to_mono() {
        let data: Vec<u8> = [16_384_i16, -16_384, 8_192, 8_192]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        let clip = decode_wav(&wav(1, 16, 2, 8_000, &data)).unwrap();
        assert_eq!(clip.rate, 8_000);
        assert_eq!(clip.samples, [0.0, 0.25]);
    }

    #[test]
    fn decodes_float_and_24_bit() {
        let f: Vec<u8> = [0.5_f32, -0.25]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        assert_eq!(
            decode_wav(&wav(3, 32, 1, 8_000, &f)).unwrap().samples,
            [0.5, -0.25]
        );
        let i24 = [0x00, 0x00, 0x40, 0x00, 0x00, 0xC0];
        assert_eq!(
            decode_wav(&wav(1, 24, 1, 8_000, &i24)).unwrap().samples,
            [0.5, -0.5]
        );
    }

    #[test]
    fn refuses_junk_odd_formats_empty_and_long_files() {
        assert_eq!(decode_wav(b"nope"), Err(WavError::NotWav));
        assert_eq!(decode_wav(&[]), Err(WavError::NotWav));
        assert_eq!(
            decode_wav(&wav(2, 4, 1, 8_000, &[0; 8])),
            Err(WavError::Unsupported)
        );
        assert_eq!(decode_wav(&wav(1, 16, 1, 8_000, &[])), Err(WavError::Empty));
        let long = vec![0; 2 * 8_000 * (MAX_CUSTOM_SECONDS as usize + 1)];
        assert_eq!(
            decode_wav(&wav(1, 16, 1, 8_000, &long)),
            Err(WavError::TooLong)
        );
        let ok = vec![0; 2 * 8_000 * MAX_CUSTOM_SECONDS as usize];
        assert!(decode_wav(&wav(1, 16, 1, 8_000, &ok)).is_ok());
    }

    #[test]
    fn refuses_rates_outside_the_supported_range() {
        let one = [0_u8, 0];
        for rate in [0, 1, 7_999, 192_001, u32::MAX - 4] {
            assert_eq!(
                decode_wav(&wav(1, 16, 1, rate, &one)),
                Err(WavError::Unsupported),
                "{rate}"
            );
        }
        for rate in [8_000, 44_100, 192_000] {
            assert!(decode_wav(&wav(1, 16, 1, rate, &one)).is_ok(), "{rate}");
        }
    }

    #[test]
    fn float_samples_are_clamped_and_non_finite_become_silence() {
        let f: Vec<u8> = [
            1000.0_f32,
            -1000.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
        let clip = decode_wav(&wav(3, 32, 1, 8_000, &f)).unwrap();
        assert_eq!(clip.samples, [1.0, -1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn only_plain_local_paths_are_opened() {
        for ok in [
            r"C:\Sounds\my.wav",
            "beep.wav",
            "/home/me/a.wav",
            r"sub\a.b.wav",
            "console.wav",
        ] {
            assert!(is_plain_local_path(Path::new(ok)), "{ok}");
        }
        for bad in [
            r"\\.\pipe\x.wav",
            r"\\nas\share\a.wav",
            r"\\?\C:\a.wav",
            "//nas/share/a.wav",
            r"C:\a.wav:stream",
            r"C:\x\NUL.wav",
            "con.wav",
            "Com1.wav",
            "lpt9",
            "",
        ] {
            assert!(!is_plain_local_path(Path::new(bad)), "{bad}");
        }
    }

    #[test]
    fn a_truncated_data_chunk_is_read_as_far_as_it_goes() {
        let mut bytes = wav(1, 16, 1, 8_000, &[0, 0x40, 0, 0x40]);
        bytes.truncate(bytes.len() - 2);
        assert_eq!(decode_wav(&bytes).unwrap().samples, [0.5]);
    }

    #[test]
    fn scaled_applies_the_volume_and_clamps_it() {
        let clip = Clip {
            samples: vec![1.0, -0.5],
            rate: 8_000,
        };
        assert_eq!(clip.scaled(0.5), [0.5, -0.25]);
        assert_eq!(clip.scaled(2.0), [1.0, -0.5]);
        assert_eq!(clip.scaled(f64::NAN), [0.0, 0.0]);
    }
}
