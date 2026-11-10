//! Hotkey: key names, the configured chord, and the hook's per-key decisions
//! (ARCHITECTURE section 11 items 6-10).
//!
//! The OS hook maps its native key code to a [`Key`], feeds it to [`Tracker::on_key`]
//! and does what the [`Verdict`] says: post the event, swallow the key, inject the
//! Start-menu blocker, forward the key to hotkey capture.

mod chord;
mod key;
mod tracker;

pub use chord::{Chord, ChordError};
pub use key::Key;
pub use tracker::{Ctx, KeyId, KeyInput, Tracker, Verdict};

#[cfg(test)]
mod edge_tests;
