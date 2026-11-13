//! Every `Notice`, for the tests that walk them all. `ALL_NOTICES` and the exhaustive `slot`
//! match are both generated from the one list below, so a variant missing from it stops the
//! build (non-exhaustive match) until it is added.
//! Shared by oxi-core's tests, `src/tray.rs` and oxi-platform's `tests/draw.rs` (via `#[path]`).
#![allow(dead_code, reason = "each includer uses part of it")]

use super::Notice; // the includer imports it (a lib test has no `oxi_core` name)

macro_rules! notices {
    ($($n:ident),+ $(,)?) => {
        pub const ALL_NOTICES: [Notice; [$(Notice::$n),+].len()] = [$(Notice::$n),+];

        /// The place of a notice in `ALL_NOTICES`; the match is exhaustive on purpose.
        pub fn slot(n: Notice) -> usize {
            let at = |m: Notice| ALL_NOTICES.iter().position(|s| *s == m).expect("listed");
            match n {
                $(Notice::$n => at(Notice::$n)),+
            }
        }
    };
}

notices! {
    Cancelled,
    CancelledKept,
    NothingHeard,
    NoModel,
    ModelBad,
    DeviceLost,
    CaptureFailed,
    NoMicrophone,
    TranscriptionFailed,
    PressPaste,
    AdminWindow,
    RestoreSkipped,
    PasteFailed,
    FileFailed,
    ModelFellBack,
    MicFallback,
    ConfigInvalid,
    ConfigAdjusted,
    MicSilence,
    OutOfMemory,
    HistoryOn,
    FailedKept,
    Recovered,
}

#[test]
fn all_notices_lists_every_notice_once() {
    for (i, n) in ALL_NOTICES.into_iter().enumerate() {
        assert_eq!(slot(n), i, "{n:?} is out of place in ALL_NOTICES");
    }
}
