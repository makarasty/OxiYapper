//! Sounds (SPEC, Sounds): our own synthesised set, so no third-party rights, and a decoder for a
//! custom `.wav` or `.ogg`. Pure: the samples are made here, the file read and the playback
//! belong to the daemon.

use std::f32::consts::{FRAC_2_PI, TAU};
use std::path::{Path, PathBuf};

use crate::machine::Sound;

/// Sample rate the built-in sounds are made at.
pub const RATE: u32 = 48_000;

/// A custom clip longer than this is refused: a sound, not a song.
pub const MAX_CUSTOM_SECONDS: u32 = 5;

/// Sample rates of a custom file that are accepted; outside this the resampler would be asked
/// for absurd sizes.
pub const CUSTOM_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;

/// A built-in sound. Each one is made in code from [`Builtin::recipe`]. The `apa`, `boop`,
/// `kill`, `rx`, `blip` and `nine` ones are recreations, from measurements, of sounds the owner
/// rated well; no file or sample of an original is in the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    TapLight,
    TapWood,
    BubbleDouble,
    CancelSoft,
    Nine,
    ApaHit2,
    ApaHit3,
    ApaHit4,
    ApaHit5,
    ApaHit6,
    Blip3,
    Boop1,
    Boop2,
    Boop3,
    Kill1,
    Kill2,
    Rx1,
    Rx3,
    Rx3Echo,
    Rx4,
    Rx10,
    Rx12,
    Rx14,
    Rx25,
}

// The names the config and the callers (`config::Sounds::default`, the daemon, the Audio page)
// used before the pack: one per role, now the role's default sound.
#[allow(
    non_upper_case_globals,
    reason = "former enum variants; callers move to the new names with the Audio page (t25)"
)]
impl Builtin {
    /// Default start sound.
    pub const Rise: Self = Self::ApaHit3;
    /// Default stop sound.
    pub const Fall: Self = Self::ApaHit2;
    /// Default "text ready" sound.
    pub const Pop: Self = Self::Rx25;
    /// Default cancel sound.
    pub const Tick: Self = Self::CancelSoft;
    /// Default error sound; the old id `fault` still names it for the error role only.
    pub const Fault: Self = Self::Boop3;
}

impl Builtin {
    /// The four tiles of the Audio page until it lists [`Self::ALL`]: the role defaults.
    pub const PICKABLE: [Self; 4] = [Self::Rise, Self::Fall, Self::Pop, Self::Tick];

    /// Every built-in, in list order.
    pub const ALL: [Self; 24] = [
        Self::ApaHit2,
        Self::ApaHit3,
        Self::ApaHit4,
        Self::ApaHit5,
        Self::ApaHit6,
        Self::Rx25,
        Self::Rx1,
        Self::Rx3,
        Self::Rx3Echo,
        Self::Rx4,
        Self::Rx10,
        Self::Rx12,
        Self::Rx14,
        Self::Nine,
        Self::Blip3,
        Self::Boop1,
        Self::Boop2,
        Self::Boop3,
        Self::Kill1,
        Self::Kill2,
        Self::TapLight,
        Self::TapWood,
        Self::BubbleDouble,
        Self::CancelSoft,
    ];

    /// The id written in `config.toml`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::TapLight => "tap-light",
            Self::TapWood => "tap-wood",
            Self::BubbleDouble => "bubble-double",
            Self::CancelSoft => "cancel-soft",
            Self::Nine => "nine",
            Self::ApaHit2 => "apa-hit-2",
            Self::ApaHit3 => "apa-hit-3",
            Self::ApaHit4 => "apa-hit-4",
            Self::ApaHit5 => "apa-hit-5",
            Self::ApaHit6 => "apa-hit-6",
            Self::Blip3 => "blip-3",
            Self::Boop1 => "boop-1",
            Self::Boop2 => "boop-2",
            Self::Boop3 => "boop-3",
            Self::Kill1 => "kill-1",
            Self::Kill2 => "kill-2",
            Self::Rx1 => "rx-1",
            Self::Rx3 => "rx-3",
            Self::Rx3Echo => "rx-3-echo",
            Self::Rx4 => "rx-4",
            Self::Rx10 => "rx-10",
            Self::Rx12 => "rx-12",
            Self::Rx14 => "rx-14",
            Self::Rx25 => "rx-25",
        }
    }

    /// The new sound a config written before the pack names: `rise`, `fall`, `pop`, `tick`, and
    /// `fault` for the error role only.
    #[must_use]
    pub fn from_legacy_id(text: &str, role: Sound) -> Option<Self> {
        match text {
            "rise" => Some(Self::Rise),
            "fall" => Some(Self::Fall),
            "pop" => Some(Self::Pop),
            "tick" => Some(Self::Tick),
            "fault" if role == Sound::Error => Some(Self::Fault),
            _ => None,
        }
    }

    /// The parts that make the sound and its loudest sample at volume 1. Times are in
    /// milliseconds. The loudness differs by sound as the originals' did, all under -1 dBFS.
    fn recipe(self) -> (Vec<Part>, f32) {
        let hit = |hz: f32, ms: f32| (vec![Part::sine(hz, ms)], 0.3);
        // A bell: inharmonic partials, the higher ones dying sooner.
        let kill = |hz: f32, ms: f32, level: f32| {
            let parts = [
                (1.0, 1.0, 1.0),
                (2.32, 0.5, 0.7),
                (4.1, 0.3, 0.45),
                (6.3, 0.15, 0.3),
            ]
            .map(|(ratio, gain, life)| Part::sine(hz * ratio, ms * life).gain(gain));
            (parts.to_vec(), level)
        };
        match self {
            Self::TapLight => (
                vec![Part::sine(650.0, 50.0).sweep(420.0, 3.0).att(2.0)],
                0.5,
            ),
            Self::TapWood => (
                vec![
                    Part::sine(520.0, 70.0).sweep(380.0, 4.0).att(2.0),
                    Part::sine(1300.0, 30.0)
                        .sweep(1300.0, 5.0)
                        .att(2.0)
                        .gain(0.4),
                ],
                0.5,
            ),
            Self::BubbleDouble => (
                vec![
                    Part::sine(300.0, 60.0).sweep(900.0, 2.0).att(4.0),
                    Part::sine(400.0, 55.0)
                        .sweep(1300.0, 2.0)
                        .att(4.0)
                        .at(100.0),
                ],
                0.5,
            ),
            Self::CancelSoft => (
                vec![Part::sine(600.0, 110.0).tri().sweep(300.0, 2.0).att(4.0)],
                0.5,
            ),
            Self::Nine => (vec![Part::sine(456.0, 30.0).drop_to(607.0, 10.0)], 0.5),
            Self::ApaHit2 => hit(1184.0, 29.0),
            Self::ApaHit3 => hit(1425.0, 25.0),
            Self::ApaHit4 => hit(1659.0, 28.0),
            Self::ApaHit5 => hit(1898.0, 28.0),
            Self::ApaHit6 => hit(2134.0, 29.0),
            Self::Blip3 => (
                vec![Part::sine(855.0, 24.0), Part::noise(10.0).gain(0.6)],
                0.45,
            ),
            Self::Boop1 => (
                vec![Part::sine(388.0, 34.0), Part::noise(6.0).gain(0.5)],
                0.4,
            ),
            Self::Boop2 => (vec![Part::sine(546.0, 300.0).drop_to(70.0, 70.0)], 0.4),
            Self::Boop3 => (vec![Part::sine(734.0, 300.0).drop_to(60.0, 60.0)], 0.4),
            Self::Kill1 => kill(1181.0, 190.0, 0.45),
            Self::Kill2 => kill(1574.0, 260.0, 0.5),
            Self::Rx1 => (vec![Part::sine(255.0, 40.0)], 0.4),
            Self::Rx3 => (vec![Part::sine(839.0, 20.0)], 0.25),
            Self::Rx3Echo => (
                vec![
                    Part::sine(959.0, 17.0),
                    Part::sine(959.0, 17.0).at(60.0).gain(0.4),
                    Part::sine(959.0, 17.0).at(120.0).gain(0.16),
                ],
                0.25,
            ),
            Self::Rx4 => (vec![Part::sine(193.0, 11.0)], 0.45),
            Self::Rx10 => (vec![Part::sine(359.0, 31.0)], 0.5),
            Self::Rx12 => (vec![Part::sine(256.0, 48.0)], 0.5),
            Self::Rx14 => (
                vec![
                    Part::sine(1113.0, 26.0),
                    Part::sine(2226.0, 26.0).gain(0.3),
                ],
                0.25,
            ),
            Self::Rx25 => (vec![Part::sine(2104.0, 91.0)], 0.35),
        }
    }

    /// Mono samples at [`RATE`]; every sound is under 400 ms and never silent.
    #[must_use]
    pub fn samples(self) -> Vec<f32> {
        let (parts, level) = self.recipe();
        let end = parts.iter().fold(0.0_f32, |m, p| m.max(p.at_ms + p.ms));
        let mut out = vec![0.0; frames(end)];
        let mut seed = 0x1234_5678_u32;
        for part in &parts {
            part.render(&mut out, &mut seed);
        }
        let peak = out.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        let gain = if peak > 0.0 { level / peak } else { 0.0 };
        for s in &mut out {
            *s *= gain;
        }
        out
    }
}

/// Samples in `ms` milliseconds.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a few hundred milliseconds of samples"
)]
fn frames(ms: f32) -> usize {
    (ms * RATE as f32 / 1000.0) as usize
}

#[derive(Clone, Copy)]
enum Wave {
    Sine,
    Tri,
    /// White noise through a first difference: a click without the low end.
    Noise,
}

#[derive(Clone, Copy)]
enum Env {
    /// Falls to -60 dB at the end of the part; the pitch follows `drop_to`.
    Exp,
    /// `(1 - t)^p` to silence at the end; the pitch glides linearly.
    Pow(f32),
}

/// One tone or noise burst of a sound.
#[derive(Clone, Copy)]
struct Part {
    wave: Wave,
    hz: f32,
    to_hz: f32,
    glide_ms: f32,
    env: Env,
    ms: f32,
    att_ms: f32,
    gain: f32,
    at_ms: f32,
}

impl Part {
    /// A sine of steady pitch that dies in `ms`.
    fn sine(hz: f32, ms: f32) -> Self {
        Self {
            wave: Wave::Sine,
            hz,
            to_hz: hz,
            glide_ms: 0.0,
            env: Env::Exp,
            ms,
            att_ms: 1.5,
            gain: 1.0,
            at_ms: 0.0,
        }
    }

    fn noise(ms: f32) -> Self {
        Self {
            wave: Wave::Noise,
            ..Self::sine(0.0, ms)
        }
    }

    fn tri(self) -> Self {
        Self {
            wave: Wave::Tri,
            ..self
        }
    }

    /// The pitch moves to `to_hz` with time constant `glide_ms`.
    fn drop_to(self, to_hz: f32, glide_ms: f32) -> Self {
        Self {
            to_hz,
            glide_ms,
            ..self
        }
    }

    /// Linear glide to `to_hz` with a `(1 - t)^power` decay.
    fn sweep(self, to_hz: f32, power: f32) -> Self {
        Self {
            to_hz,
            env: Env::Pow(power),
            ..self
        }
    }

    fn att(self, att_ms: f32) -> Self {
        Self { att_ms, ..self }
    }

    fn gain(self, gain: f32) -> Self {
        Self { gain, ..self }
    }

    fn at(self, at_ms: f32) -> Self {
        Self { at_ms, ..self }
    }

    /// Adds the part to `out`; what falls past its end is dropped.
    fn render(&self, out: &mut [f32], seed: &mut u32) {
        let (start, len) = (frames(self.at_ms), frames(self.ms));
        let attack = (self.att_ms * RATE as f32 / 1000.0).max(1.0);
        let (mut phase, mut last) = (0.0_f32, 0.0_f32);
        for i in 0..len {
            let Some(slot) = out.get_mut(start + i) else {
                break;
            };
            let u = i as f32 / len as f32;
            let (env, hz) = match self.env {
                Env::Exp => {
                    let ms = i as f32 * 1000.0 / RATE as f32;
                    let glide = if self.glide_ms > 0.0 {
                        (-ms / self.glide_ms).exp()
                    } else {
                        1.0
                    };
                    (
                        (-6.9 * u).exp(),
                        self.to_hz + (self.hz - self.to_hz) * glide,
                    )
                }
                Env::Pow(p) => ((1.0 - u).powf(p), self.hz + (self.to_hz - self.hz) * u),
            };
            phase += TAU * hz / RATE as f32;
            let x = match self.wave {
                Wave::Sine => phase.sin(),
                Wave::Tri => FRAC_2_PI * phase.sin().asin(),
                Wave::Noise => {
                    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let white = (*seed >> 8) as f32 / 8_388_608.0 - 1.0;
                    let diff = (white - last) * 0.5;
                    last = white;
                    diff
                }
            };
            *slot += x * env * (i as f32 / attack).min(1.0) * self.gain;
        }
    }
}

/// What a sound setting (`sounds.start`, `.stop`, `.done`, `.cancel`, `.error`) names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Builtin(Builtin),
    Custom(PathBuf),
}

/// The value is neither a built-in id nor a path to a `.wav` or `.ogg`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a built-in sound or a .wav or .ogg path")]
pub struct UnknownSound(pub String);

impl Choice {
    /// What `config::Sounds::normalise` accepts for `role`: a built-in id, an id of the sounds
    /// before the pack ([`Builtin::from_legacy_id`]), or a path (it has a separator or a drive
    /// colon, or ends in `.wav` or `.ogg`).
    ///
    /// # Errors
    /// [`UnknownSound`] for any other text.
    pub fn parse_for(role: Sound, text: &str) -> Result<Self, UnknownSound> {
        let text = text.trim();
        let built_in = Builtin::ALL
            .into_iter()
            .find(|b| b.id() == text)
            .or_else(|| Builtin::from_legacy_id(text, role));
        if let Some(b) = built_in {
            return Ok(Self::Builtin(b));
        }
        let path = Path::new(text);
        let audio = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("ogg"));
        if audio || text.contains(['/', '\\', ':']) {
            Ok(Self::Custom(path.to_path_buf()))
        } else {
            Err(UnknownSound(text.to_owned()))
        }
    }

    /// [`Self::parse_for`] a role that may not use `fault`.
    ///
    /// # Errors
    /// [`UnknownSound`] for any other text.
    pub fn parse(text: &str) -> Result<Self, UnknownSound> {
        Self::parse_for(Sound::Start, text)
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
    #[error("not a readable Ogg Vorbis file")]
    BadOgg,
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

/// Decodes an Ogg Vorbis file to mono (channels averaged).
///
/// # Errors
/// [`WavError`] for anything but a non-empty Vorbis stream of at most [`MAX_CUSTOM_SECONDS`]
/// seconds at a rate in [`CUSTOM_RATES`].
pub fn decode_ogg(bytes: &[u8]) -> Result<Clip, WavError> {
    // lewton sizes its buffers from header counts before reading them: a 300-byte file asked
    // for 55 GB and the allocation failure aborted the process, which no `Result` can catch.
    headers_fit(bytes).ok_or(WavError::BadOgg)?;
    let mut reader = lewton::inside_ogg::OggStreamReader::new(std::io::Cursor::new(bytes))
        .map_err(|_| WavError::BadOgg)?;
    let rate = reader.ident_hdr.audio_sample_rate;
    let channels = usize::from(reader.ident_hdr.audio_channels);
    if channels == 0 || !CUSTOM_RATES.contains(&rate) {
        return Err(WavError::Unsupported);
    }
    let max = MAX_CUSTOM_SECONDS as usize * rate as usize;
    let mut samples = Vec::new();
    while let Some(packet) = reader.read_dec_packet_itl().map_err(|_| WavError::BadOgg)? {
        samples.extend(packet.chunks_exact(channels).map(|frame| {
            let sum: f32 = frame.iter().map(|&s| f32::from(s)).sum();
            sum / channels as f32 / 32_768.0
        }));
        // Stops early: a few MB of Vorbis can hold minutes of audio.
        if samples.len() > max {
            return Err(WavError::TooLong);
        }
    }
    if samples.is_empty() {
        return Err(WavError::Empty);
    }
    Ok(Clip { samples, rate })
}

/// Most codebook entries plus lookup values a setup header may declare in all. Real encoders
/// stay far below; past it lewton would reserve memory the file cannot back.
const MAX_CODEBOOK_VALUES: u64 = 1 << 22;

/// `Some` when every count in the comment and setup headers of the stream fits the bytes that
/// follow it, so lewton's allocations stay bounded by the file. The packets come from the same
/// `ogg` reader lewton uses, picked the way `lewton::inside_ogg::read_headers` picks them.
fn headers_fit(bytes: &[u8]) -> Option<()> {
    let mut reader = ogg::PacketReader::new(std::io::Cursor::new(bytes));
    let mut packets = std::iter::from_fn(|| reader.read_packet().ok().flatten());
    let serial = packets.next()?.stream_serial();
    let mut headers = packets.filter(|p| p.stream_serial() == serial);
    comment_fits(&headers.next()?.data)?;
    codebooks_fit(&headers.next()?.data)
}

/// A little-endian `u32` length off the front of `rest`.
fn le32(rest: &mut &[u8]) -> Option<usize> {
    let (n, tail) = rest.split_first_chunk::<4>()?;
    *rest = tail;
    usize::try_from(u32::from_le_bytes(*n)).ok()
}

/// The vendor string and every comment lie inside the packet (Vorbis I, section 5.2.1).
fn comment_fits(packet: &[u8]) -> Option<()> {
    let mut rest = packet.strip_prefix(b"\x03vorbis")?;
    let vendor = le32(&mut rest)?;
    rest = rest.get(vendor..)?;
    // Each pass eats 4 bytes or fails, so the loop ends with the packet.
    for _ in 0..le32(&mut rest)? {
        let len = le32(&mut rest)?;
        rest = rest.get(len..)?;
    }
    Some(())
}

/// Bits of a packet, least significant first (Vorbis I, section 2.1.2).
struct BitReader<'a> {
    data: &'a [u8],
    at: usize,
}

impl BitReader<'_> {
    fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for i in 0..count {
            let byte = self.data.get(self.at / 8)?;
            value |= u32::from(byte >> (self.at % 8) & 1) << i;
            self.at += 1;
        }
        Some(value)
    }

    fn skip(&mut self, count: u64) -> Option<()> {
        let to = u64::try_from(self.at).ok()?.checked_add(count)?;
        self.at = usize::try_from(to).ok()?;
        (self.at <= self.data.len() * 8).then_some(())
    }
}

/// Largest `r` with `r^dims <= entries` (Vorbis I, section 9.2.3); `None` for no dimensions.
fn lookup1_values(entries: u32, dims: u32) -> Option<u64> {
    let fits = |r: u64| r.checked_pow(dims).is_some_and(|p| p <= u64::from(entries));
    if dims == 0 {
        return None;
    }
    let (mut low, mut high) = (0, u64::from(entries));
    while low < high {
        let mid = (low + high).div_ceil(2);
        if fits(mid) { low = mid } else { high = mid - 1 }
    }
    Some(low)
}

/// Walks the codebooks of the setup header (Vorbis I, section 3.2.1) as lewton reads them and
/// checks that their entries and lookup tables stay within [`MAX_CODEBOOK_VALUES`] and the
/// packet. The rest of the header holds only small counts.
fn codebooks_fit(packet: &[u8]) -> Option<()> {
    let mut bits = BitReader {
        data: packet.strip_prefix(b"\x05vorbis")?,
        at: 0,
    };
    let mut budget = MAX_CODEBOOK_VALUES;
    for _ in 0..=bits.read(8)? {
        if bits.read(24)? != 0x56_4342 {
            return None;
        }
        let dims = bits.read(16)?;
        let entries = bits.read(24)?;
        if bits.read(1)? == 0 {
            let sparse = bits.read(1)? == 1;
            // Each entry takes at least one bit, so this ends with the packet.
            for _ in 0..entries {
                if !sparse || bits.read(1)? == 1 {
                    bits.read(5)?;
                }
            }
        } else {
            // Ordered: runs of growing length; lewton keeps the length in a u8.
            let mut length = bits.read(5)? + 1;
            let mut done = 0;
            while done < entries {
                done += bits.read(u32::BITS - (entries - done).leading_zeros())?;
                length += 1;
                if length > 33 || done > entries {
                    return None;
                }
            }
        }
        let lookup = bits.read(4)?;
        let vectors = u64::from(entries) * u64::from(dims);
        let values = match lookup {
            0 => 0,
            1 => lookup1_values(entries, dims)?,
            2 => vectors,
            _ => return None,
        };
        let held = u64::from(entries) + if lookup == 0 { 0 } else { vectors };
        budget = budget.checked_sub(held)?;
        if lookup != 0 {
            bits.skip(64)?; // minimum and delta
            let width = u64::from(bits.read(4)? + 1);
            bits.skip(1)?;
            bits.skip(values.checked_mul(width)?)?;
        }
    }
    Some(())
}

/// Decodes a custom file by its content: Ogg when it starts with `OggS`, else WAV.
///
/// # Errors
/// As [`decode_wav`] and [`decode_ogg`].
pub fn decode_custom(bytes: &[u8]) -> Result<Clip, WavError> {
    if bytes.starts_with(b"OggS") {
        decode_ogg(bytes)
    } else {
        decode_wav(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_render_short_quiet_enough_and_without_clicks() {
        for b in Builtin::ALL {
            let s = Clip::builtin(b).samples;
            let ms = s.len() * 1000 / RATE as usize;
            assert!((10..400).contains(&ms), "{b:?} is {ms} ms");
            let peak = s.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
            // -1 dBFS is 0.891.
            assert!(peak > 0.1 && peak <= 0.891, "{b:?} peak {peak}");
            assert!(s.iter().all(|x| x.is_finite()), "{b:?}");
            let (first, last) = (s[0].abs(), s[s.len() - 1].abs());
            assert!(first < 0.02 && last < 0.02, "{b:?} edges {first} {last}");
        }
    }

    #[test]
    fn built_in_ids_are_unique_and_parse_back() {
        let mut seen = std::collections::HashSet::new();
        for b in Builtin::ALL {
            assert!(seen.insert(b.id()), "{b:?}");
            for role in [Sound::Start, Sound::Error] {
                assert_eq!(Choice::parse_for(role, b.id()), Ok(Choice::Builtin(b)));
            }
        }
        assert!(Builtin::PICKABLE.iter().all(|b| Builtin::ALL.contains(b)));
        assert!(Choice::parse("fault").is_err(), "legacy fault is for errors only");
    }

    // Configs written before the pack keep working: each old id is the role's new default.
    #[test]
    fn old_ids_map_to_the_nearest_new_sound() {
        for (old, new) in [
            ("rise", Builtin::ApaHit3),
            ("fall", Builtin::ApaHit2),
            ("pop", Builtin::Rx25),
            ("tick", Builtin::CancelSoft),
        ] {
            assert_eq!(Choice::parse(old), Ok(Choice::Builtin(new)), "{old}");
        }
        assert_eq!(
            Choice::parse_for(Sound::Error, "fault"),
            Ok(Choice::Builtin(Builtin::Boop3))
        );
    }

    // Renders every built-in to `$OXI_SOUND_OUT/<id>.wav` (16-bit mono) for listening:
    // `OXI_SOUND_OUT=<dir> cargo test -p oxi-core render_built_ins -- --ignored`.
    #[test]
    #[ignore = "writes files for listening, only when asked"]
    #[allow(clippy::cast_possible_truncation, reason = "a sample in -1..1 times 32767")]
    fn render_built_ins_to_wav() {
        let dir = std::env::var_os("OXI_SOUND_OUT").expect("set OXI_SOUND_OUT to a folder");
        for b in Builtin::ALL {
            let pcm: Vec<u8> = Clip::builtin(b)
                .samples
                .iter()
                .flat_map(|s| ((s * 32_767.0).round() as i16).to_le_bytes())
                .collect();
            let mut file = wav(1, 16, 1, RATE, &pcm);
            let riff = u32::try_from(file.len() - 8).unwrap();
            file[4..8].copy_from_slice(&riff.to_le_bytes());
            std::fs::write(Path::new(&dir).join(format!("{}.wav", b.id())), file).unwrap();
        }
    }

    // `Choice::parse_for` takes what `config::Sounds::normalise` keeps, so a valid config never
    // reaches the daemon or the settings as an Err.
    #[test]
    fn parse_for_accepts_ogg_and_fault_only_for_the_error_role() {
        assert_eq!(
            Choice::parse_for(Sound::Done, "x.OGG"),
            Ok(Choice::Custom("x.OGG".into()))
        );
        assert_eq!(
            Choice::parse_for(Sound::Error, "fault"),
            Ok(Choice::Builtin(Builtin::Fault))
        );
        for role in [Sound::Start, Sound::Stop, Sound::Done, Sound::Cancel] {
            assert!(Choice::parse_for(role, "fault").is_err(), "{role:?}");
        }
        assert!(Choice::parse_for(Sound::Error, "tick").is_ok());
    }

    #[test]
    fn the_config_and_parse_for_agree() {
        let mut sounds = crate::config::Sounds {
            start: "a.ogg".into(),
            stop: "fault".into(),
            done: "nope".into(),
            cancel: r"C:\s\c.wav".into(),
            error: "fault".into(),
            ..crate::config::Sounds::default()
        };
        sounds.normalise();
        for (role, text) in [
            (Sound::Start, &sounds.start),
            (Sound::Stop, &sounds.stop),
            (Sound::Done, &sounds.done),
            (Sound::Cancel, &sounds.cancel),
            (Sound::Error, &sounds.error),
        ] {
            assert!(Choice::parse_for(role, text).is_ok(), "{role:?} {text}");
        }
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

    /// Bits of a Vorbis header, least significant bit first (Vorbis I spec, section 2.1.2).
    struct Bits(Vec<u8>, u32);

    impl Bits {
        fn put(&mut self, value: u32, count: u32) {
            for i in 0..count {
                if self.1.is_multiple_of(8) {
                    self.0.push(0);
                }
                let last = self.0.len() - 1;
                self.0[last] |= u8::from(u64::from(value) >> i & 1 == 1) << (self.1 % 8);
                self.1 += 1;
            }
        }
    }

    fn ogg_crc(data: &[u8]) -> u32 {
        data.iter().fold(0_u32, |mut crc, &b| {
            crc ^= u32::from(b) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 == 0 {
                    crc << 1
                } else {
                    crc << 1 ^ 0x04C1_1DB7
                };
            }
            crc
        })
    }

    /// One page holding `packets` (at most 255 segments in all).
    fn ogg_page(flags: u8, seq: u32, packets: &[Vec<u8>]) -> Vec<u8> {
        let mut lacing = Vec::new();
        for p in packets {
            lacing.extend(std::iter::repeat_n(255_u8, p.len() / 255));
            lacing.push(u8::try_from(p.len() % 255).unwrap());
        }
        let mut page = b"OggS\0".to_vec();
        page.push(flags);
        page.extend(0_u64.to_le_bytes()); // granule position
        page.extend(1_u32.to_le_bytes()); // stream serial
        page.extend(seq.to_le_bytes());
        page.extend([0; 4]); // checksum, set below
        page.push(u8::try_from(lacing.len()).unwrap());
        page.extend(lacing);
        page.extend(packets.concat());
        let crc = ogg_crc(&page).to_le_bytes();
        page[22..26].copy_from_slice(&crc);
        page
    }

    /// An Ogg Vorbis stream made here, no encoder needed: `channels` channels at `rate`,
    /// blocksize 64, `n` audio packets that all mark their floors unused, so it decodes to
    /// silence of a known length (32 frames per packet after the first).
    fn ogg(channels: u8, rate: u32, n: usize) -> Vec<u8> {
        ogg_from(&ogg_headers(channels, rate), n)
    }

    /// The identification, comment and setup packets of [`ogg`].
    fn ogg_headers(channels: u8, rate: u32) -> [Vec<u8>; 3] {
        let mut ident = b"\x01vorbis".to_vec();
        ident.extend(0_u32.to_le_bytes());
        ident.push(channels);
        ident.extend(rate.to_le_bytes());
        ident.extend([0; 12]); // bitrates
        ident.extend([0x66, 1]); // blocksizes 2^6 and 2^6, framing
        let mut comment = b"\x03vorbis".to_vec();
        comment.extend([0; 8]); // no vendor, no comments
        comment.push(1);
        let mut b = Bits(b"\x05vorbis".to_vec(), 56);
        b.put(0, 8); // one codebook: two entries of length 1, no lookup
        b.put(0x56_4342, 24);
        b.put(1, 16);
        b.put(2, 24);
        b.put(0, 2); // not ordered, not sparse
        b.put(0, 10);
        b.put(0, 4);
        b.put(0, 6); // one time transform, 0
        b.put(0, 16);
        b.put(0, 6); // one floor: type 1 with no partitions
        b.put(1, 16);
        b.put(0, 5);
        b.put(0, 2);
        b.put(6, 4);
        b.put(0, 6); // one residue: type 0, empty range, one class without books
        b.put(0, 16);
        b.put(0, 24); // begin
        b.put(0, 24); // end
        b.put(0, 24); // partition size - 1
        b.put(0, 6);
        b.put(0, 8);
        b.put(0, 4);
        b.put(0, 6); // one mapping: one submap, no coupling
        b.put(0, 16);
        b.put(0, 4);
        b.put(0, 24);
        b.put(0, 6); // one short-block mode
        b.put(0, 1);
        b.put(0, 32);
        b.put(0, 8);
        b.put(1, 1); // framing
        [ident, comment, b.0]
    }

    /// A stream of `headers` and `n` silent audio packets, every page with a good checksum.
    fn ogg_from(headers: &[Vec<u8>; 3], n: usize) -> Vec<u8> {
        let [ident, comment, setup] = headers.clone();
        let mut out = ogg_page(2, 0, &[ident]);
        out.extend(ogg_page(0, 1, &[comment, setup]));
        // A zero byte is the packet-type bit and one "floor unused" bit for each channel.
        let packets = vec![vec![0_u8]; n];
        let pages = packets.chunks(200).collect::<Vec<_>>();
        for (seq, chunk) in (2..).zip(&pages) {
            let last = seq as usize == pages.len() + 1;
            out.extend(ogg_page(u8::from(last) << 2, seq, chunk));
        }
        out
    }

    #[test]
    fn decodes_an_ogg_stream_built_in_the_test() {
        for channels in [1, 2] {
            let clip = decode_ogg(&ogg(channels, 8_000, 100)).unwrap();
            assert_eq!(clip.rate, 8_000);
            assert_eq!(clip.samples.len(), 99 * 32, "{channels} channels");
        }
        assert_eq!(decode_custom(&ogg(1, 8_000, 10)).unwrap().rate, 8_000);
    }

    #[test]
    fn refuses_junk_odd_rates_and_long_ogg_files() {
        assert_eq!(decode_ogg(b"OggS nope"), Err(WavError::BadOgg));
        assert_eq!(decode_ogg(&[]), Err(WavError::BadOgg));
        assert_eq!(decode_ogg(&ogg(1, 7_000, 10)), Err(WavError::Unsupported));
        let seconds = MAX_CUSTOM_SECONDS as usize;
        let long = ogg(1, 8_000, 8_000 * (seconds + 1) / 32 + 2);
        assert_eq!(decode_ogg(&long), Err(WavError::TooLong));
        let mut bad = ogg(1, 8_000, 10);
        let last = bad.len() - 1;
        bad[last] ^= 0xFF; // the last page no longer matches its checksum
        assert_eq!(decode_ogg(&bad), Err(WavError::BadOgg));
    }

    // A comment count of 0x0501_4500 made lewton reserve 55 GB and abort the process at
    // config time (review of t09). The header check refuses it before lewton reads it.
    #[test]
    fn a_comment_count_past_the_packet_is_refused_not_allocated() {
        let mut headers = ogg_headers(2, 8_000);
        headers[1][11..15].copy_from_slice(&0x0501_4500_u32.to_le_bytes());
        assert_eq!(decode_ogg(&ogg_from(&headers, 50)), Err(WavError::BadOgg));
        headers[1][7..11].copy_from_slice(&u32::MAX.to_le_bytes()); // vendor length
        assert_eq!(decode_ogg(&ogg_from(&headers, 50)), Err(WavError::BadOgg));
    }

    // A real setup packet: one ordered codebook of `entries` one-dimensional vectors (lookup
    // type 2, one-bit values), its value bits really there. At 2^22 entries the 2^23 values and
    // entries are past the budget while the packet itself is big enough to hold them, so only
    // the budget check refuses it; at 2^20 the same packet passes.
    #[test]
    fn a_codebook_past_the_budget_is_refused() {
        let book = |entries: u32| {
            let mut b = Bits(b"\x05vorbis".to_vec(), 56);
            b.put(0, 8);
            b.put(0x56_4342, 24);
            b.put(1, 16); // dimensions
            b.put(entries, 24);
            b.put(1, 1); // ordered: one run covers every entry
            b.put(0, 5);
            b.put(entries, u32::BITS - entries.leading_zeros());
            b.put(2, 4); // lookup type 2
            b.put(0, 32); // minimum
            b.put(0, 32); // delta
            b.put(0, 4); // value width - 1
            b.put(0, 1); // sequence
            b.0.resize(b.0.len() + entries as usize / 8 + 1, 0); // the values
            b.0
        };
        assert_eq!(codebooks_fit(&book(1 << 20)), Some(()));
        assert_eq!(codebooks_fit(&book(1 << 22)), None);
        // 2^24 - 1 entries of 65535 dimensions ask for 4 TB of vectors.
        let mut b = Bits(b"\x05vorbis".to_vec(), 56);
        b.put(0, 8);
        b.put(0x56_4342, 24);
        b.put(0xFFFF, 16);
        b.put(0xFF_FFFF, 24);
        b.put(1, 1);
        b.put(0, 5);
        b.put(0xFF_FFFF, 24);
        b.put(2, 4);
        assert_eq!(codebooks_fit(&b.0), None);
        let mut headers = ogg_headers(1, 8_000);
        headers[2] = b.0;
        assert_eq!(decode_ogg(&ogg_from(&headers, 5)), Err(WavError::BadOgg));
        assert_eq!(lookup1_values(16, 2), Some(4));
        assert_eq!(lookup1_values(17, 2), Some(4));
        assert_eq!(lookup1_values(5, 40), Some(1));
        assert_eq!(lookup1_values(5, 0), None);
    }

    // Random byte changes to the three headers, each stream with good checksums so lewton
    // reads it: none may panic or abort, whatever the result. Fixed seed, about 0.1 s.
    #[test]
    fn mutated_ogg_headers_never_panic_or_abort() {
        let clean = ogg_headers(2, 8_000);
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3_000 {
            let mut headers = clean.clone();
            for _ in 0..=next() % 4 {
                let packet = &mut headers[usize::try_from(next() % 3).unwrap()];
                let at = usize::try_from(next()).unwrap() % packet.len();
                packet[at] = next().to_le_bytes()[0];
            }
            let _ = decode_ogg(&ogg_from(&headers, 50));
        }
    }
}
