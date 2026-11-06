//! whisper.cpp behind one crate, so the GPU features and the slow C++ build stay
//! out of everything else. See docs/ARCHITECTURE.md, "Model lifecycle".

/// whisper.cpp build and CPU/GPU capability line, for `--version` style output.
pub fn system_info() -> &'static str {
    whisper_rs::print_system_info()
}
