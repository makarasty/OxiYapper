//! OS code behind traits, one module per OS chosen by `cfg` (ARCHITECTURE section 4).

pub mod capture;
pub mod wav;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
