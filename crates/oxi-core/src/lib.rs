//! Pure logic of `OxiYapper`. Rule: no OS calls and no `-sys` crate in the normal
//! dependency tree (CI checks it), so `cargo test -p oxi-core` runs anywhere in seconds.
#![forbid(unsafe_code)]

/// The product name: executable, per-user directories, IPC names and usage text all use it.
pub const APP_NAME: &str = "OxiYapper";

pub mod audio;
pub mod cli;
pub mod config;
pub mod device;
pub mod dictionary;
pub mod hotkey;
pub mod ipc;
pub mod machine;
pub mod models;
pub mod overlay;
pub mod paste;
pub mod paths;
pub mod stats;
pub mod strings;
pub mod text;
pub mod tray;
pub mod types;

pub use cli::{Command, parse_command};
