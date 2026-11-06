//! Pure logic of oxi-yapper. Rule: no OS calls and no `-sys` crate in the normal
//! dependency tree (CI checks it), so `cargo test -p oxi-core` runs anywhere in seconds.
#![forbid(unsafe_code)]

pub mod audio;
pub mod cli;
pub mod config;
pub mod dictionary;
pub mod ipc;
pub mod machine;
pub mod models;
pub mod paths;
pub mod text;
pub mod types;

pub use cli::{Command, parse_command};
