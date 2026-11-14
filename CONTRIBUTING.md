# Contributing

Read `docs/SPEC.md` (what the app must do), `docs/ARCHITECTURE.md` (how) and `docs/ROADMAP.md` (what is next) first. AI agents also follow `AGENTS.md`.

## Layout

One executable, `OxiYapper`. Without arguments it runs the resident daemon (tray, hotkey, audio, transcription, paste, overlay). `OxiYapper settings` opens the separate settings window ([Slint](https://slint.dev)), which exits when closed. `OxiYapper --version` prints the version.

| Path | What |
|---|---|
| `crates/oxi-core` | Pure logic: state machine, config, text, dictionary, IPC types, model registry, audio DSP. No OS calls, no unsafe. |
| `crates/oxi-platform` | OS code behind traits, one module per OS. |
| `crates/oxi-whisper` | whisper-rs and the GPU features. |
| `crates/oxi-settings` | The Slint settings window. |
| `src/` | The binary and the daemon wiring; `src/net` is the only module that may use the network. |

## Build

Rust 1.92+ (Slint 1.18 needs it), CMake, LLVM/clang (for bindgen). Pick the GPU backend with a feature:

| Feature | Backend | Extra requirements |
|---|---|---|
| `vulkan` | Vulkan (Windows, Linux) | Vulkan SDK (`VULKAN_SDK` set; on Linux `libvulkan-dev` and `glslc`) |
| `cuda` | NVIDIA CUDA | CUDA toolkit (`CUDA_PATH` set) |
| `metal` | Apple Metal (macOS) | Xcode command line tools |

No feature = CPU only.

### Windows

PowerShell, with the MSVC Build Tools, LLVM and the Vulkan SDK installed:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
$env:CARGO_TARGET_DIR = "C:\t\oxi"   # keep it short, see below
cargo build --release --features vulkan
```

The exe is then `C:\t\oxi\release\OxiYapper.exe`.

**Keep the target path short.** This is required, not a preference: with a cargo target path longer than about 30 characters the nested whisper.cpp `vulkan-shaders-gen` CMake build passes `MAX_PATH` and fails with `FileTracker : error FTK1011` or `No CMAKE_C_COMPILER could be found`. The Ninja generator (`CMAKE_GENERATOR=Ninja`) does not help. Enabling long paths in Windows was not tested.

### macOS

Xcode command line tools, `brew install cmake`, then `cargo build --release --features metal`. Microphone and Accessibility permissions attach to the app that runs the binary: run it from a terminal and the terminal gets the prompt. The daemon is not wired up on macOS yet (ROADMAP M12).

### Linux (Debian/Ubuntu)

`sudo apt install cmake clang libclang-dev libasound2-dev libvulkan-dev glslc`, then `cargo build --release --features vulkan`. The daemon is not wired up on Linux yet (ROADMAP M13, M14).

## Test

```
cargo test -p oxi-core                     # fast loop: pure logic, seconds, no CMake
cargo fmt --all --check                    # before finishing
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace              # everything, CPU whisper build (or cargo test --workspace)
typos && cargo machete && cargo deny check # if installed; CI runs them
```

Pedantic clippy is on: fix the lint; an `#[allow]` needs a one-line reason.

Tests leave no trace on disk: a temp folder is removed by a guard's `Drop` when the test ends, and fake model or recording files stay a few KB (never `set_len` to a real size).

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

The product and the executable are `OxiYapper`; the crates (`oxi-core`, `oxi-whisper`, `oxi-platform`, `oxi-settings`, the `oxi-yapper` package) stay lowercase because crates.io requires it.

## Commits and PRs

Conventional Commits, for commits and PR titles (CI checks the title): `feat(paste): restore clipboard after RDP paste`, `fix(hotkey): ...`, `docs: ...`. Scopes are crate or area names: `core`, `whisper`, `platform`, `settings`, `paste`, `hotkey`, `overlay`, `audio`, `ci`. One logical change per PR; `ci-ok` must be green. Add a line to the `Unreleased` section of `CHANGELOG.md` for a change users will notice.

New dependencies: add the version to `[workspace.dependencies]` in the root `Cargo.toml`; the licence must pass `deny.toml`. Every GitHub Action is pinned to a full commit SHA with a version comment.

Never commit audio, models, dictionaries or replacement lists from real users, and no secrets (`docs/SECRETS.md`). No network calls outside `src/net`, no telemetry, and dictated text never goes into logs unless the user turns on "Log dictated text".

## Releases

See [docs/RELEASING.md](docs/RELEASING.md).
