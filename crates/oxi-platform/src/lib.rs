//! OS code behind traits, one module per OS chosen by `cfg` (ARCHITECTURE section 4).

pub mod capture;
pub mod dirs;
pub mod draw;
pub mod fs;
pub mod ipc;
pub mod open;
pub mod stats_store;
pub mod wav;

// Every OS module is reached as `os`, so callers never name the OS.
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
pub mod os;
#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
pub mod os;
#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
pub mod os;
