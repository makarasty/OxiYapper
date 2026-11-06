//! Helpers shared by the settings test binaries.

use std::fs;

use oxi_core::paths::Layout;

/// A fresh directory per test and test binary; `config` is written as `config.toml` when given.
pub fn layout(name: &str, config: Option<&str>) -> Layout {
    let dir = std::env::temp_dir().join(format!(
        "oxi-settings-{}-{}-{name}",
        env!("CARGO_CRATE_NAME"),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    if let Some(text) = config {
        fs::write(dir.join("config.toml"), text).unwrap();
    }
    Layout {
        config_dir: dir.clone(),
        data_dir: dir.clone(),
        logs_dir: dir,
    }
}
