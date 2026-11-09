//! Constants used by more than one module (or crate). A value used by one module stays beside
//! its code.

use std::time::Duration;

use crate::types::Millis;

/// The whisper vocab (`data/vocab`), read by the dictionary budget and by the whisper tokenizer
/// regression test.
pub const VOCAB_FILE: &[u8] = include_bytes!("../data/vocab/multilingual.vocab");

/// The widest gap in days between two dated records that history age retention and the stats
/// fold still trust: more than this is probably a clock that jumped.
pub const MAX_CLOCK_JUMP: i32 = 31;

/// A playback stays open this much longer than its clip, so wireless outputs do not cut the tail.
pub const PLAY_TAIL: Millis = Millis(300);

/// One hour, the unit of the `.part` age tests in oxi-core and oxi-platform.
pub const HOUR: Duration = Duration::from_secs(3600);
