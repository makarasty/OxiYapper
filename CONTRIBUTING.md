# Contributing

Read `docs/SPEC.md` (what the app must do), `docs/ARCHITECTURE.md` (how) and `docs/ROADMAP.md` (what is next) first.

## Build

Rust 1.92+ (Slint 1.18 needs it), CMake, LLVM/clang (for bindgen). GPU backend by feature: `vulkan`, `cuda`, `metal`; none = CPU.

Windows (PowerShell), MSVC Build Tools and the Vulkan SDK installed:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
$env:CARGO_TARGET_DIR = "C:\t\oxi"   # keep it short, see below
cargo build --release --features vulkan
```

The short target dir is required, not a preference: with a target path longer than about 30 characters the nested whisper.cpp `vulkan-shaders-gen` CMake build passes `MAX_PATH` and fails with `FileTracker : error FTK1011` or `No CMAKE_C_COMPILER could be found`. Ninja does not help.

macOS: Xcode command line tools, `brew install cmake`, then `cargo build --release --features metal`. Microphone and Accessibility permissions attach to the app that runs the binary: run from a terminal and the terminal gets the prompt.

Linux (Debian/Ubuntu): `sudo apt install cmake clang libclang-dev libasound2-dev libvulkan-dev glslc`, then `cargo build --release --features vulkan`.

## Test

```
cargo test -p oxi-core                     # pure logic, seconds, no CMake
cargo nextest run --workspace              # everything, CPU whisper build
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
typos && cargo machete && cargo deny check # if installed; CI runs them
```

Live tests touch the real desktop, so each is off unless its variable is `1`. Run them one at a time (`-- --test-threads=1`) in an interactive Windows session, with nothing else typing:

| Variable | Test | What it does |
|---|---|---|
| `OXI_HOOK_TEST` | `hook_live` | installs `WH_KEYBOARD_LL`, injects F13 chords and a swallowed Esc |
| `OXI_INPUT_TEST` | `input_live` | real `SendInput` |
| `OXI_CLIPBOARD_TEST` | `clipboard_live` | real clipboard; saves and restores what was on it (CI sets it) |
| `OXI_OVERLAY_TEST` | `overlay_live` | puts the overlay pill on screen for a moment |
| `OXI_TRAY_TEST` | `tray_live` | adds and removes tray icons |
| `OXI_DAEMON_TEST` | `daemon_smoke` | runs a copy of the real daemon: hook, tray icon, pipe |
| `OXI_MIC_TEST` | `mic`, settings `page_audio` | opens the default microphone; audio is never written to disk |
| `OXI_SHOTS_DIR` | draw, overlay, settings shots | a folder; when set, tests save PNG files of overlay states, tray icons and settings screens there for review |

Live tests never use Ctrl+Win: the owner's dictation tool listens on it. Use F13 chords. Quit any other dictation tool before running them.

Whisper regression (from ROADMAP M3): set `OXI_TINY_MODEL` to a downloaded `ggml-tiny.bin`, otherwise the test skips. Owner acceptance runs `OxiYapper bench <private dir>` locally; that data never enters the repo.

The product and the executable are `OxiYapper`; the crates (`oxi-core`, `oxi-whisper`, `oxi-platform`, the `oxi-yapper` package) stay lowercase because crates.io requires it.

## Commits and PRs

Conventional Commits, for commits and PR titles (CI checks the title): `feat(paste): restore clipboard after RDP paste`, `fix(hotkey): ...`, `docs: ...`. Scopes are crate or area names: `core`, `whisper`, `platform`, `settings`, `paste`, `hotkey`, `overlay`, `audio`, `ci`. One logical change per PR; `ci-ok` must be green.

Never commit audio, models, dictionaries or replacement lists from real users, and no secrets (`docs/SECRETS.md`).
