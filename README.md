# oxi-yapper

Push-to-talk dictation for Windows, macOS and Linux. Hold a hotkey, speak, release: the audio is transcribed locally on your GPU with [whisper.cpp](https://github.com/ggml-org/whisper.cpp) (through `whisper-rs`) and pasted into the focused field. Nothing leaves your machine.

Status: **pre-alpha skeleton**. The binary only prints its version and the whisper.cpp system info.

Design: one executable. `oxi-yapper` runs the small resident daemon (tray, hotkey, audio, transcription, paste). `oxi-yapper settings` opens a separate settings window ([Slint](https://slint.dev), software renderer) that exits when closed.

## Build

Needs Rust 1.85+, CMake and LLVM/clang (bindgen). Pick a GPU backend with a feature flag:

| Feature  | Backend | Extra requirements |
|----------|---------|--------------------|
| `vulkan` | Vulkan (Windows, Linux) | Vulkan SDK (`VULKAN_SDK` set; on Linux `libvulkan-dev` and `glslc`) |
| `cuda`   | NVIDIA CUDA | CUDA toolkit |
| `metal`  | Apple Metal (macOS) | Xcode command line tools |

No feature = CPU only.

```
cargo build --release --features vulkan
```

Windows also needs the MSVC Build Tools and `LIBCLANG_PATH` pointing to the LLVM `bin` folder. Linux also needs `libasound2-dev`.

### Windows: keep the target path short

The `vulkan` build fails with `FileTracker : error FTK1011` or `No CMAKE_C_COMPILER could be found` when the cargo target directory path is longer than about 30 characters (a nested CMake build exceeds `MAX_PATH`). Use a short one:

```powershell
$env:CARGO_TARGET_DIR="C:\t\oxi"
cargo build --release --features vulkan
```

The Ninja generator (`CMAKE_GENERATOR=Ninja`) does not avoid it. Enabling long paths in Windows was not tested.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
