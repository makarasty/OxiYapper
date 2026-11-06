# Rules for AI agents

1. Read `docs/SPEC.md`, `docs/ARCHITECTURE.md` and `docs/ROADMAP.md` before changing anything. SPEC is the owner's decisions and is binding; change it only when the owner says so. Work one ROADMAP milestone at a time and finish its "done when" checks.
2. Where things live:
   - `crates/oxi-core`: pure logic (state machine, config, text, dictionary, IPC types, model registry, audio DSP). No OS calls, no `-sys` crates, `#![forbid(unsafe_code)]`. Put every decision you can here, with tests.
   - `crates/oxi-platform` (from M4): OS code behind traits, one module per OS chosen by `cfg`.
   - `crates/oxi-whisper`: whisper-rs and the `vulkan`/`cuda`/`metal` features.
   - `crates/oxi-settings` (from M9): Slint UI.
   - `src/`: the binary and daemon wiring.
3. Commands (Windows: set `CARGO_TARGET_DIR=C:\t\oxi` and `LIBCLANG_PATH` first, see CONTRIBUTING):
   - fast loop: `cargo test -p oxi-core`
   - before finishing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace` (or `cargo test --workspace`)
   - GPU build: `cargo build --release --features vulkan`
4. Never commit owner data or models: no `.wav`, `.bin`, `.gguf`, dictation history, dictionary or replacement files from a real person, no reference phrase text. Bench data stays in the private folder passed as `OXI_BENCH_DIR`. Fixtures are synthetic.
5. No network calls outside the model download and update check modules. No telemetry. Dictated text never goes into logs.
6. Every GitHub Action is pinned to a full commit SHA with a version comment; no `pull_request_target`; `permissions: contents: read` unless a job needs more.
7. New dependencies: add the version to `[workspace.dependencies]` in the root `Cargo.toml`; licence must pass `deny.toml`.
8. Pedantic clippy is on. Fix the lint; an `#[allow]` needs a one-line reason.
