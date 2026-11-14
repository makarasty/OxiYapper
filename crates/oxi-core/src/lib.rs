//! Pure logic of `OxiYapper`. Rule: no OS calls and no `-sys` crate in the normal
//! dependency tree (CI checks it), so `cargo test -p oxi-core` runs anywhere in seconds.
#![forbid(unsafe_code)]

/// The product name: executable, per-user directories, IPC names and usage text all use it.
pub const APP_NAME: &str = "OxiYapper";

/// The version of this build: the release tag CI builds from (`OXI_VERSION`, `0.2.0-pre.57`), else
/// the crate version. The update check compares it with the releases (`update`).
pub const VERSION: &str = match option_env!("OXI_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod audio;
pub mod autostart;
pub mod cli;
pub mod config;
pub mod constants;
pub mod device;
pub mod dictionary;
pub mod history;
pub mod hotkey;
pub mod ipc;
pub mod languages;
pub mod learn;
pub mod machine;
pub mod models;
pub mod overlay;
pub mod paste;
pub mod paths;
pub mod sounds;
pub mod stats;
pub mod strings;
pub mod suggest;
pub mod text;
pub mod tray;
pub mod tuning;
pub mod types;
pub mod update;

pub use cli::{Command, parse_command};
