//! Per-user config, data, log and runtime directories (ARCHITECTURE section 7).
//! The layout itself is `oxi_core::paths`; this module only asks the OS for the base directories.

use std::io;
use std::path::Path;

use directories::BaseDirs as OsDirs;
use oxi_core::paths::{self, BaseDirs, Layout, Os, PORTABLE_MARKER};

// shortcut: placeholder id, ROADMAP M12 fixes the real one; it is frozen after the first release.
const BUNDLE_ID: &str = "io.github.makarasty.OxiYapper";

/// The layout for this run: portable when the `portable` marker sits next to the exe.
pub fn layout() -> io::Result<Layout> {
    let exe = std::env::current_exe()?;
    let exe_dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("the exe path has no parent directory"))?;
    layout_for(exe_dir)
}

/// `layout` for an exe in `exe_dir` (split out so tests need no real exe).
pub fn layout_for(exe_dir: &Path) -> io::Result<Layout> {
    if exe_dir.join(PORTABLE_MARKER).is_file() {
        return Ok(paths::portable_layout(exe_dir));
    }
    let dirs = OsDirs::new().ok_or_else(|| io::Error::other("no home directory"))?;
    let base = BaseDirs {
        config: dirs.config_dir().to_path_buf(),
        // Windows: %LOCALAPPDATA%, not roaming (models are gigabytes). Same as `data_dir` elsewhere.
        data: dirs.data_local_dir().to_path_buf(),
        // Linux: $XDG_STATE_HOME. macOS: ~/Library/Logs. Windows: unused (logs sit under data).
        logs: match Os::current() {
            Os::MacOs => dirs.home_dir().join("Library").join("Logs"),
            Os::Linux => dirs
                .state_dir()
                .map_or_else(|| dirs.data_local_dir().to_path_buf(), Path::to_path_buf),
            Os::Windows => dirs.data_local_dir().to_path_buf(),
        },
    };
    Ok(paths::layout(Os::current(), &base, BUNDLE_ID))
}

/// Create the config, data and log directories; the data directory is 0700 on Unix.
pub fn ensure(layout: &Layout) -> io::Result<()> {
    let mut data = std::fs::DirBuilder::new();
    data.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut data, 0o700);
    data.create(&layout.data_dir)?;
    std::fs::create_dir_all(&layout.config_dir)?;
    std::fs::create_dir_all(&layout.logs_dir)
}
