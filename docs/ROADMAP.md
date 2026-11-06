# oxi-yapper roadmap

Each milestone fits one agent session and ends with checks anyone can run. Windows first: it is the owner's daily driver and v0.x must replace the owner's current dictation tool there. Then macOS, then Linux.

"Parallel with" means the milestones touch different crates or modules and can run in separate sessions at once. Every milestone ends with the CI gates green (`docs/ARCHITECTURE.md` section 14) and the docs it changes updated.

Owner data rule for every milestone: the 8 reference phrases, the owner's dictionary and replacements live in a private folder outside the repo, passed by path (`OXI_BENCH_DIR`). Nothing from it is ever committed, pasted into a test, or used as a fixture.

## M0 - Workspace and gates (this change)

Workspace (`oxi-core`, `oxi-whisper`, app at the root), lints, `deny.toml`, `_typos.toml`, `rustfmt.toml`, nextest config, CI gates, CONTRIBUTING, AGENTS.

Done when: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo build --release --features vulkan` pass on Windows and the exe prints the whisper system info; the first push to GitHub turns `ci-ok` green (owner pushes).

## M1 - Core state machine and config

`oxi-core::machine` (phases, events, effects, timers, double tap, taps, Esc, queue of two jobs, watchdog key-up), `oxi-core::config` (schema v1, defaults, migrations framework, unknown keys kept, newer version read-only), `oxi-core::paths` (per-OS layout as pure functions of base dirs, portable marker).

Done when: table tests cover edge cases 1-12 of ARCHITECTURE section 11; the proptest invariants (no paste after Esc, one whisper job at a time, back to Idle) pass with 10 000 cases; a v1 fixture round-trips byte-identical.

## M2 - Text pipeline and dictionary budget

Parallel with M1 and M3. `oxi-core::text` (newline strip, trim, hallucination filter with a shipped default list, replace engine, trailing space, BOM/UTF-16 file reading), `oxi-core::dictionary` (whisper.cpp greedy tokenizer over the embedded multilingual vocab, first-lines-that-fit prompt builder).

Done when: proptest and insta suites from ARCHITECTURE section 13 pass; the token count of a synthetic 300-word list matches `whisper_tokenize` (checked in M3's regression job).

## M3 - Whisper worker and models

Parallel with M1 and M2. `oxi-whisper`: load/unload, device list and choice by name, CPU fallback, transcribe with language and prompt, segment `no_speech` probabilities exposed. `oxi-core::models` registry with commit-pinned URLs and SHA256. Download with Range resume and hash check. Hidden `oxi-yapper bench <dir>` subcommand (wav in, text and timings out). CI: cache ggml-tiny.bin with a pinned SHA256 and run the jfk.wav WER test.

Done when: `bench` on jfk.wav prints the right sentence on CPU and on the RTX 4070 SUPER via Vulkan; the CI test job runs the WER test (WER under 0.2) instead of skipping; a corrupted model file is refused with a hash error.

## M4 - Audio pipeline

Parallel with M3. `oxi-platform` crate appears with `Capture` (cpal, rtrb ring, level atomic, device list, device-lost event). `oxi-core::audio`: mono, gain, 48 kHz RNNoise path, streaming 16 kHz resample, trim with speech gate, normalise. Crash-safe wav writer (`.part` + rename).

Done when: DSP unit tests on synthetic signals pass; `oxi-yapper bench --record 5 out.wav` (debug flag) records from the default mic on Windows and the wav plays back at 16 kHz mono; unplugging a USB mic mid-recording keeps the captured part.

## M5 - Measurement: Vulkan vs CUDA, audio defaults, reference phrases

Needs M3 and M4. On the owner's machine, through `bench` on the private phrases:
- Vulkan vs CUDA on the RTX 4070 SUPER: p50/p95 latency for turbo, large-v3 and podlodka, cold load time, VRAM after unload, `flash_attn` on/off, and whether GPU OOM aborts the process (decides the whisper child-process proposal, ARCHITECTURE section 8).
- Audio defaults: normalise, trim, denoise, tail length, A/B on the same phrases.
- Results go into `docs/research/bench-<date>.md` as numbers only (no phrase text, no audio).

Done when: the document states keep or drop CUDA with the numbers; the CI CUDA job becomes blocking or is removed; defaults in `oxi-core::config` match the measured winners; **all 8 reference phrases give the same text as the owner's bench**.

## M6 - Windows hotkey, tray, daemon wiring

Needs M1. Hook thread with `WH_KEYBOARD_LL`, chord tracking, VK 0xE8 Start suppression, Esc swallow, physical-state watchdog, reinstall on resume/unlock. Tray with three state icons. `src/daemon/` coordinator executing effects. Minimal clipboard paste (set + Ctrl+V, no restore yet). Single instance socket. Logs.

Done when: on the owner's PC, holding Ctrl+Win, speaking and releasing pastes text into Notepad; Start never opens; Esc cancels; a second launch exits and the first stays.

## M7 - Windows paste, all edge cases

Needs M6. Modifier wait, UIPI check, clipboard snapshot/restore with sequence number, history and cloud exclusion, type mode, per-app rules, "Copy last text" tray item.

Done when: QA checklist paste rows pass on Windows (Notepad, browser, Windows Terminal, RDP, an elevated Notepad, a game chat in type mode); Win+V history shows neither the dictation nor the restored item; platform smoke test for clipboard round trip runs in CI on Windows.

## M8 - Windows overlay

Parallel with M7. Layered no-activate click-through window, tiny-skia pill with live meter, positions, per-monitor DPI, hide over fullscreen.

Done when: the fullscreen game case passes (open chat, dictate, Enter sends the message, the game never loses focus); no flicker on show/hide recorded at 60 fps; two monitors with different DPI place the pill in the right corner of the foreground window's monitor.

## M9 - Settings UI, IPC, config watch

Needs M1; parallel with M6-M8. `oxi-settings` crate (Slint, software renderer), IPC messages, config writer (temp + rename), daemon watcher, hotkey picker with visual keyboard and capture mode, mic picker with live meter, model page with download progress, dictionary token counter, replacements editor, overlay position pickers, About with the "Made with Slint" badge. Add the Slint licence exception to `deny.toml`.

Done when: Slint UI tests pass headless; changing the hotkey in settings takes effect in the daemon without restart; killing settings during hotkey capture restores the hotkey; a broken hand-edited config shows its line number.

## M10 - History, sounds, autostart, updates, first-run wizard

Needs M7 and M9. History save and retention, history view (play, copy, add to replacements), start/stop sounds, autostart with path re-registration, daily update check with badge and Install, first-run wizard (mic, model, hotkey).

Done when: retention test deletes oldest first and nothing else; autostart survives moving the portable folder after one manual start; update check offline produces no notification.

## M11 - Windows release (owner switches)

Needs M5-M10. cargo-packager per-user NSIS installer (no admin) and portable zip with the `portable` marker, in `release.yml`. Full Windows QA checklist. Owner migration (SPEC round 4): copy the owner's dictionary into the local config, reuse the podlodka model already on disk after a SHA256 check, import the previous tool's history (its SQLite db + audio) through an import command; none of it enters the repo.

Done when: a tagged pre-release installs without admin on a clean Windows user, the full Windows QA checklist passes, and the owner uses oxi-yapper as the only dictation tool for 3 days with no blocking bug.

## M12 - macOS

Needs M11. Hotkey (handy-keys event tap), paste (NSPasteboard, CGEvent, layout key code, secure input), non-activating NSPanel overlay, tray, permission checks in the wizard (Microphone, Accessibility, Input Monitoring), Metal build, .app and .dmg, bundle id fixed.

Done when: the macOS QA checklist passes on Apple Silicon; the 8 reference phrases give the bench text with Metal.

## M13 - Linux X11

Parallel with M12. evdev hotkey with the `input` group check, XTest paste with terminal rules, override-redirect overlay with empty input shape, ksni tray, Vulkan build, AppImage and .deb.

Done when: the Linux QA checklist passes on an X11 session (Ubuntu LTS); missing `/dev/input` access shows the instructions instead of failing silently.

## M14 - Linux Wayland

Needs M13. Spike first, then build: GlobalShortcuts portal hotkey, RemoteDesktop portal keyboard, wtype/ydotool fallbacks, wlr-layer-shell overlay, GNOME behaviour without layer-shell. Close the OPEN item in SPEC.md with what was measured.

Done when: dictation pastes on GNOME Wayland and KDE Wayland, or the unsupported path says "press Ctrl+V" with the text on the clipboard; SPEC.md has no Wayland OPEN item left.

## M15 - v1.0

Needs M11-M14. Full QA checklist on all three OSes, README with install and permissions per OS, CHANGELOG, SECURITY contact verified, release notes, tag `v1.0.0`.

Done when: every QA row passes on Windows, macOS and Linux X11; CI `ci-ok` is green on the tag; the release has artifacts, SHA256SUMS and attestations for every target.
