//! Values shared by more than one page (or by the tests). A value used by one page stays beside
//! its code.

use std::time::Duration;

/// How often the window looks for an edit of `config.toml` from outside (an editor, "Open
/// file"): a fixed file clears the banner and refreshes the pages without reopening.
// shortcut: a metadata poll, not a file watcher (`notify` is not a dependency of this crate);
// switch to a watcher if the second of delay matters.
pub const FILE_POLL: Duration = Duration::from_secs(1);

/// How often a page looks for the answer of a check on its worker (the History folder, a model path).
pub const FOLDER_POLL: Duration = Duration::from_millis(25);

/// Default window size: `min(1280, 85 %)` x `min(800, 88 %)` of the work area, at least the
/// minimum (redesign spec section 2). The shot tests render at both.
pub const DEFAULT: (f32, f32) = (1280.0, 800.0);
/// Minimum window size.
pub const MIN: (f32, f32) = (720.0, 520.0);
