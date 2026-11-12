//! OS code behind traits, one module per OS chosen by `cfg` (ARCHITECTURE section 4).

pub mod autostart;
pub mod capture;
pub mod decode;
pub mod dirs;
pub mod draw;
pub mod fs;
pub mod ipc;
pub mod open;
#[cfg(test)]
mod scratch;
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

/// The OS asks for less animation; `false` on an OS with no such setting wired up.
#[cfg(target_os = "windows")]
pub fn reduced_motion() -> bool {
    os::reduced_motion()
}

/// The OS asks for less animation; `false` on an OS with no such setting wired up.
#[cfg(not(target_os = "windows"))]
pub const fn reduced_motion() -> bool {
    false
}
