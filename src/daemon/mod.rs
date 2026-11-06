//! Daemon wiring: one module per thread (ARCHITECTURE section 2).

mod audio;
mod cmd;
mod coordinator;
mod io;
mod ipc;
mod paste;
mod run;
mod ui;
mod watch;
mod whisper;

pub use run::run;
