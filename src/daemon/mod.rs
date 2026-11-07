//! Daemon wiring: one module per thread (ARCHITECTURE section 2).

mod audio;
mod cmd;
mod coordinator;
mod io;
mod ipc;
mod paste;
mod run;
mod stats;
mod ui;
mod watch;
mod whisper;

use std::sync::{Mutex, MutexGuard, PoisonError};

pub use run::run;

/// A mutex guard even when another thread panicked holding it: the daemon's shared state stays
/// usable, and the panic itself is logged by the hook.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
