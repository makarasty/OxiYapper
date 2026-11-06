# oxi-yapper architecture

How the app is built. `docs/SPEC.md` holds the owner's decisions and wins any conflict with this file. `docs/ROADMAP.md` orders the work. Numbers marked (target) are budgets to measure, not measurements.

## 1. Processes

One executable, three modes:

- `oxi-yapper` - the **daemon**. Resident, autostarted. Owns the tray, the global hotkey, the mic, whisper, paste, the overlay, model downloads and the update check. It is the only process that touches the network.
- `oxi-yapper settings` - the **settings** window (Slint, software renderer). Spawned by the daemon from the tray, exits on close. The only writer of `config.toml`, `dictionary.txt` and `replace.txt`.
- `oxi-yapper bench <dir>` - hidden developer mode: runs the audio and text pipeline plus whisper on a folder of wav files and prints text and timings. Used for the reference phrases and the GPU measurement. Never touches the hotkey or the clipboard.

The overlay lives inside the daemon. It is a raw OS window (layered window on Windows, `NSPanel` on macOS, X11 override-redirect or wlr-layer-shell on Linux), not Slint, because Slint cannot do no-activate, click-through and no-taskbar.

Single instance: the daemon binds a per-user local socket at start. If the bind fails, the new process connects, sends `OpenSettings` and exits 0. `oxi-yapper settings` started by hand starts the daemon first if nothing answers on the socket.

## 2. Threads

The daemon runs these threads. Each owns its data; they talk only through channels and two atomics.

| Thread | Owns | Notes |
|---|---|---|
| main (UI) | tray, overlay window, OS event loop | Windows: Win32 message loop. macOS: `NSApplication` run loop (AppKit is main-thread only). Linux: overlay X11/Wayland event loop; `ksni` runs its own D-Bus thread. |
| hook | the global hotkey | Windows: dedicated thread with its own `GetMessage` loop and `WH_KEYBOARD_LL`, so a busy main thread can never push the hook past `LowLevelHooksTimeout` (Windows silently removes slow hooks). macOS: `CGEventTap` run loop thread (handy-keys). Linux: evdev reader thread or the portal D-Bus thread. |
| coordinator | the state machine, config, timers | Single inbox, `recv_timeout` until the next deadline. Never blocks on I/O. |
| audio | cpal stream (callback runs on the OS audio thread) plus a drain loop | Callback copies samples into a preallocated `rtrb` ring and updates the level atomic. No allocation, no lock in the callback. The drain loop downmixes and resamples in 20 ms chunks while recording. |
| whisper | `WhisperContext`, the loaded model | Takes one job at a time. Loads on `Preload`, unloads on `Unload`. |
| paste | clipboard and synthetic input | Waits for modifiers, snapshots and restores the clipboard. Kept off the coordinator because it sleeps (restore delay). |
| io | history writes, model download, update check | One worker with a job channel; downloads report progress over IPC. |
| ipc | local socket server | One connection at a time is the settings window; other connections only send `OpenSettings`. |

Shared atomics, read lock-free by the UI thread at 30 Hz: `LEVEL` (peak and RMS of the last 20 ms, for the overlay meter) and `PHASE` (current phase as `u8`, read by the hook callback to decide whether Esc is ours).

### Channels

- Everything that happens goes into the coordinator inbox as `Event` (one `std::sync::mpsc` channel, many senders): hook events, audio stopped or failed, job audio ready, transcription done, paste done, model loaded or failed, config changed, IPC request, suspend/resume/lock, timer tick.
- The coordinator sends commands out on one channel per worker: `AudioCmd`, `WhisperCmd`, `PasteCmd`, `IoCmd`, and `UiCmd` (overlay show/hide/state, tray state, notification). `UiCmd` also wakes the UI loop: `PostMessage(WM_APP)` on Windows, a main-queue dispatch on macOS, an eventfd or X11 client message on Linux.
- Workers never call each other. Audio hands finished audio to the coordinator, which forwards it to whisper. That keeps every decision in one place and testable.

## 3. State machine

Lives in `oxi-core::machine`, pure: `step(&mut State, Event, now: Millis) -> Effects`. `now` comes from the caller, so tests drive time by hand. The coordinator executes the returned effects (start capture, play sound, show overlay, send a job, notify) and asks `State::next_deadline()` how long to sleep.

Phases: `Idle -> Recording -> Transcribing -> Pasting -> Idle`, plus two outcomes, `Cancelled` and `Error`, which emit their feedback (sound, overlay text, notification) and fold back into whatever is left to do.

- Key-down in Idle: `Recording`, emit `StartCapture`, `Preload` and the start sound. In push-to-talk the press is first a candidate tap (see double tap below).
- Key-up (push-to-talk) or second press (toggle): stop capture, the audio becomes a job, phase `Transcribing`.
- Transcription done: phase `Pasting`, the paste worker gets the text. Paste done: `Idle`, schedule the idle unload.
- A new key-down during `Transcribing` or `Pasting` starts a new recording at once: no lost words. Jobs run and paste strictly in order. At most two jobs wait; a third key-down gets the error sound. The phase shown is the most urgent one: Recording, then Transcribing, then Pasting.
- Esc during Recording: `Cancelled`, the audio is dropped, Esc is swallowed. Esc at any other time is not ours and passes through.

Timers the machine owns: double-tap window, max recording length, speech watchdog (push-to-talk key-up recovery), idle model unload, overlay hide delay.

### Latency budget, key-up to text in the field

For a 5 s phrase, turbo model, RTX 4070 SUPER, Vulkan, model already loaded (it loads on key-down, during speech).

| Step | Budget (target) |
|---|---|
| hook to coordinator | < 1 ms |
| release tail (keeps the last syllable; configurable) | 100 ms |
| flush last 20 ms chunk, trim, normalise (resampling already done while recording) | < 10 ms |
| whisper encode + decode | 250-450 ms (measure in M5; `audio_ctx` shrink is an option, off by default) |
| text pipeline | < 1 ms |
| wait for modifiers, set clipboard, Ctrl+V | 5-60 ms |
| **total** | **p50 <= 600 ms, p95 <= 1000 ms** |

Clipboard restore happens 300 ms after Ctrl+V and is outside the budget. A cold model load (first press after idle unload) adds 1-4 s; the overlay says "loading model".

## 4. Crates

Cargo workspace. The root package is the app binary, so `cargo build --release --features vulkan` at the root keeps working.

```
Cargo.toml              workspace + app package (bin oxi-yapper)
src/main.rs             CLI dispatch; src/daemon/ wires threads and runs effects
crates/oxi-core         pure logic: machine, config, text, dictionary, history policy,
                        ipc messages, model registry, audio DSP (pure Rust only)
crates/oxi-platform     OS code behind traits: hotkey, paste, clipboard, overlay, tray,
                        autostart, capture, power/session events, notifications, paths
crates/oxi-whisper      whisper-rs wrapper: load/unload, device choice, transcribe;
                        owns the vulkan/cuda/metal features
crates/oxi-settings     Slint UI (settings window and first-run wizard)
```

Only `oxi-core` and `oxi-whisper` exist today; the other two arrive with their milestones (`members = ["crates/*"]` picks them up).

Why a workspace and not one crate:

- **Testability.** `oxi-core` has no OS calls and no `-sys` crate (CI fails if one appears in its tree), so its tests run on all three OSes in seconds without CMake, a GPU SDK or a display. Almost every decision in this file is in it.
- **Compile time.** whisper.cpp (CMake, minutes) and Slint (build script, large) sit in their own crates. Editing the state machine or paste code recompiles neither, and `cargo test -p oxi-core` never builds them.
- **Boundaries agents can see.** A change to paste behaviour on macOS touches `oxi-platform/src/macos/paste.rs`; a change to what happens on Esc touches `oxi-core/src/machine.rs`.

Platform code: each concern is a trait in `oxi-platform` (`Hotkey`, `Paster`, `Clipboard`, `Overlay`, `Tray`, `Autostart`, `Capture`, `SessionEvents`). One implementation per OS in `src/windows/`, `src/macos/`, `src/linux/`, picked with `#[cfg(target_os = ...)]` and re-exported as `oxi_platform::Platform`. The app holds the traits generically, so daemon wiring tests use fakes. No `dyn` dispatch where one implementation exists per build.

## 5. IPC, daemon <-> settings

Transport: `interprocess` local socket. Windows: named pipe `\\.\pipe\oxi-yapper-<user SID>` created with a DACL for the current user only and `PIPE_REJECT_REMOTE_CLIENTS` (the default pipe DACL lets Everyone read). macOS/Linux: Unix socket `oxi-yapper.sock` in `$XDG_RUNTIME_DIR` (Linux) or the per-user `$TMPDIR` (macOS), directory mode 0700. A stale socket file (connect refused) is removed before bind.

Framing: one JSON object per line (`serde_json`), max 1 MiB per line, tagged enum `{"t": "...", ...}`. Types live in `oxi-core::ipc` and are snapshot-tested so a field rename shows up in review.

| From settings | Reply / pushed by daemon |
|---|---|
| `Hello { proto, version }` | `HelloAck { proto, version }` |
| `OpenSettings` (also sent by a second daemon launch) | `Raise` to the open settings window, or spawn one |
| `Status` | `Status { phase, model, device, backend, warnings }` |
| `ListDevices` | `Devices { mics, gpus }` |
| `DownloadModel { id }`, `CancelDownload { id }` | `DownloadProgress { id, done, total }`, `DownloadDone { id, result }` |
| `CaptureHotkey { on }` | `Key { code, down }` while on |
| `PreviewOverlay { position }` | overlay shows for 2 s |
| `Shutdown` | daemon exits |
| | `ConfigError { file, line, message }` pushed on a bad reload |

Versioning: `proto` is an integer, starting at 1. Adding an optional field (serde default) does not bump it; renaming or removing a field or changing meaning does. `Hello`, `HelloAck` and `Shutdown` never change shape. On a mismatch (exe updated while the old daemon runs) settings shows "Restart oxi-yapper to finish the update" and a button that sends `Shutdown` and relaunches the daemon.

Hotkey capture: while `CaptureHotkey { on: true }` the daemon pauses the chord, forwards raw key events and swallows them only while the settings window is foreground. Capture ends when settings sends `on: false`, loses focus, or the connection drops. This is the one place keystrokes leave the hook, which is why the socket is owner-only.

## 6. Config

`config.toml`, written only by settings: serialise, write `config.toml.tmp` in the same directory, fsync, rename over the old file. The daemon watches the directory (not the file: the rename replaces the inode) with a 200 ms debounce and reloads. The daemon itself writes `state.toml` (last update check, models marked bad, GPU fallback note), never `config.toml`, except creating it with defaults when it is missing (`create_new`). One writer per file, so no merge logic.

Loading: parse to a `toml_edit::DocumentMut`, read `version`, run migrations `v1 -> v2 -> ...` (each a pure function on the document, unit-tested with a fixture per version), then deserialise into `Config` where every field has `#[serde(default)]`. Saving writes the typed values into the document's existing items, so comments, key order and formatting survive, an unchanged file is written back byte-identical (a partial file, CRLF line endings and a UTF-8 BOM included; a file mixing LF and CRLF is normalised to its dominant ending), a key the file lacks is written only when its value differs from the default, and unknown keys stay in the document, so a newer field survives a round trip through an older settings build. Paste rules are matched by `app`. A file with `version` newer than this build is read for the fields it knows and never written; settings shows it read-only. A parse error keeps the last good config in the daemon and shows the file and line in the tray and settings.

```toml
version = 1
[hotkey]
chord = ["Ctrl", "Win"]        # "Win" is Cmd on macOS
mode = "push"                  # push | toggle
[audio]
device = ""                    # empty = system default
gain_db = 0.0
normalize = true
denoise = false
trim = true
tail_ms = 100
max_seconds = 600              # SPEC round 4: 10 min cap
[model]
id = "turbo"                   # registry id, or "custom"
custom_path = ""
gpu = "auto"                   # auto | cpu | device name (names survive index changes)
language = "ru"                # ru | en | auto
idle_unload_minutes = 5        # 0 = never unload
[paste]
mode = "clipboard"             # clipboard | type
restore_delay_ms = 300
trailing_space = true
rules = [{ app = "mstsc.exe", mode = "type" }]   # per-app overrides: mode, chord, delay
[overlay]
enabled = true
position = "bottom-right"      # bottom-centre, top-left, ...
hide_over_fullscreen = false
[sounds]
enabled = true
volume = 0.5
[history]
enabled = false                # owner turns it on; off for everyone else
max_bytes = 2_147_483_648
[system]
autostart = true
update_check = true
[debug]
log_text = false               # dictated text in logs only when true
```

Per-app paste rules answer "games, terminals, remote desktop" without a toggle the user flips by hand. App identity: exe name on Windows, bundle id on macOS, `WM_CLASS` on X11.

## 7. Files on disk

From `directories::ProjectDirs`. Portable mode: if a file named `portable` sits next to the exe, everything goes under `<exe dir>/data/` instead (the portable zip ships it).

| What | Windows | macOS | Linux |
|---|---|---|---|
| `config.toml`, `dictionary.txt`, `replace.txt` | `%APPDATA%\oxi-yapper\config\` | `~/Library/Application Support/<bundle id>/` | `$XDG_CONFIG_HOME/oxi-yapper/` |
| `state.toml`, `models/`, `history/` | `%LOCALAPPDATA%\oxi-yapper\data\` | `~/Library/Application Support/<bundle id>/` | `$XDG_DATA_HOME/oxi-yapper/` |
| `logs/` (daily files, 7 kept) | `%LOCALAPPDATA%\oxi-yapper\data\logs\` | `~/Library/Logs/oxi-yapper/` | `$XDG_STATE_HOME/oxi-yapper/logs/` |

Models are local, not roaming (gigabytes). History entries are `history/YYYY-MM-DD/HHMMSS-mmm.{wav,txt}`. On Unix the data directory is created 0700. The macOS bundle id is chosen in ROADMAP M12 and never changes after the first release: TCC permissions and paths hang on it.

## 8. Model lifecycle

- **Registry** in `oxi-core::models`: id, display name, file name, URL pinned to a Hugging Face commit (`/resolve/<commit>/...`, never `main`), size, SHA256. Curated: podlodka (Russian), large-v3, turbo. Plus "custom .bin": loaded as is, no hash.
- **Download** (daemon, io thread): `ureq` to `models/<file>.part` with `Range` resume, SHA256 computed while streaming, rename on match, delete `.part` on mismatch. If the CDN redirect drops `Range`, restart from zero. A verified file gets a `<file>.ok` stamp (size + mtime); a changed stamp means re-hash before the next load.
- **Load** on key-down (`Preload`), in the background while the user speaks. If key-up comes first, the job waits for the load. Load failure is an `Error` with the reason.
- **Unload** after `idle_unload_minutes` without a job (default 5), and on suspend. Drop the context; M5 measures whether Vulkan really returns the VRAM.
- **Device choice**: `gpu = "auto"` picks the first discrete GPU in ggml's device list, then integrated, then CPU. On the owner's machine ggml's Vulkan backend lists `Vulkan0 = RTX 4070 SUPER (uma: 0)` and `Vulkan1 = AMD Radeon iGPU (uma: 1)`; UMA marks the integrated one. A named device that is gone falls back to auto with a warning. The name-to-index mapping uses ggml's backend device API through `whisper-rs-sys` (unverified that it is exported; fallback is the index in the settings list).
- **CPU fallback**: GPU init failure or OOM retries the load once on CPU, keeps CPU until the config or the model changes, and says so in the tray ("running on CPU: <reason>").
- **Proposal, needs owner OK after M5**: run whisper in a child process (`oxi-yapper worker`, same exe) if M5 shows ggml aborting the process on GPU OOM or device loss. A child gives crash isolation and a guaranteed memory release on unload. The whisper thread already talks only through `WhisperCmd`/`Event`, so the move is local to `oxi-whisper` and the coordinator.

## 9. Audio pipeline

Order, and where each step can fail:

1. **Open** the device (configured or system default) on key-down, in its native format. Fails: device gone, no permission, exclusive mode held by another app. Result: `Error` with a deep link to the OS privacy page. The mic is not kept open between dictations (privacy indicator, Bluetooth headsets stuck in call quality).
2. **Capture** callback: convert i16/u16/f32 to f32, push to the ring, update `LEVEL`. Fails: ring full (drain starved) - samples dropped and counted; device lost mid-stream - capture stops, what was captured is kept.
3. **Mono**: average the channels. Manual gain applied here, so the meter shows it.
4. **Denoise** (optional, off): RNNoise needs 48 kHz mono in 480-sample frames, so it runs before the 16 kHz resample, after a resample to 48 kHz if the device rate differs.
5. **Resample** to 16 kHz with rubato, streaming in 20 ms chunks during recording. Fails only on a bad config (unsupported rate) at stream start.
6. **Trim** silence at both ends with an energy gate (earshot), keeping 200 ms of padding. Fewer than 250 ms of voiced audio left: "nothing heard", no whisper call.
7. **Normalise**: RMS toward a target with a +20 dB gain cap and a peak limit. Pure silence (all zeros) is not normalised; it raises "microphone gives silence".

Steps 3-7 are pure functions in `oxi-core::audio`, tested on synthetic signals. Defaults (normalise on, trim on, denoise off, tail 100 ms) are confirmed against the 8 reference phrases in M5 before they ship.

History (when on): the 16 kHz audio after step 7 goes to `<name>.wav.part` through hound, `finalize()`, then rename. Startup deletes leftover `.part` files.

## 10. Paste pipeline

Text pipeline first (`oxi-core::text`, pure): remove `\n` without adding spaces, trim, drop hallucinated segments, apply `replace.txt`, add the trailing space. Empty result: "nothing heard", nothing pasted.

Then, in the paste worker (Windows shown; other OSes below):

1. **Pick the mode**: per-app rule for the foreground app, else `paste.mode`.
2. **Wait for modifiers**: poll the physical state of Ctrl, Shift, Alt, Win (both sides) every 5 ms. A pending Ctrl+V with Win still held would open Win+V. If they are still down after 1.5 s: put the text on the clipboard, do not restore, notify "Text is on the clipboard, press Ctrl+V".
3. **Elevated target (UIPI)**: compare the foreground process integrity level with ours. Higher: same as above ("Admin window: press Ctrl+V"). SendInput into it fails silently, so we never try.
4. **Snapshot the clipboard**: open with 10 retries 20 ms apart. Copy every HGLOBAL format up to 32 MiB total; skip GDI-handle formats (CF_BITMAP has CF_DIB beside it), synthesised formats and delayed-render formats the owner fails to render in 500 ms. Locked after retries: switch to type mode. Snapshot incomplete: type mode if the text has 300 characters or fewer, else paste and skip the restore, with a one-time note.
5. **Set** the text as CF_UNICODETEXT plus `ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory = 0`, `CanUploadToCloudClipboard = 0`. Remember `GetClipboardSequenceNumber`.
6. **Send** Ctrl down, V down, V up, Ctrl up with SendInput (VK codes, which are layout-independent on Windows; the per-app chord may be Shift+Insert or Ctrl+Shift+V).
7. **Restore** after `restore_delay_ms` (default 300; RDP and some Electron apps read late), only if the sequence number is unchanged (someone else wrote the clipboard: leave theirs). The restored data also carries the history-exclusion formats: it is already in the history from the user's own copy.

**Type mode**: SendInput with `KEYEVENTF_UNICODE`, one event pair per UTF-16 unit (surrogate pairs as two), 5 ms between characters by default (games drop faster input), clipboard untouched. It bypasses the IME composition. Games that read raw input or DirectInput ignore it; nothing can be done there, and the QA checklist records which games work.

**IME**: clipboard mode with an open IME composition usually commits the composition first, then pastes. Recorded per IME in the QA checklist; type mode is the fallback rule.

**macOS**: NSPasteboard snapshot of all items and types, write with `org.nspasteboard.TransientType` and `ConcealedType` (clipboard managers skip them), CGEvent Cmd+V (needs Accessibility), `changeCount` instead of the sequence number. The V key code is looked up for the current layout (Dvorak moves it), falling back to `kVK_ANSI_V`. Secure Event Input on (a password field somewhere): posting is blocked, so clipboard-only with a note.

**Linux X11**: arboard sets the clipboard and keeps serving it until the restore; XTest Ctrl+V via enigo; terminals get default rules for Ctrl+Shift+V; KDE managers skip data with `x-kde-passwordManagerHint=secret`.

**Wayland** (OPEN in the spec, settled in ROADMAP M14): no global synthetic input without help. Chain: xdg RemoteDesktop portal keyboard (GNOME, KDE; one permission prompt, restore token kept) -> `wtype` (wlroots) -> `ydotool` if its socket exists -> clipboard-only with "press Ctrl+V". Clipboard through `wlr-data-control` where offered.

## 11. Edge cases

Recording and hotkey
1. Hotkey during transcription or paste: a new recording starts; jobs paste in order; a third waiting job is refused with the error sound.
2. Double tap (push-to-talk): two presses shorter than 250 ms with a gap under 300 ms lock recording on; audio counts from the second press; the next full press stops it.
3. Single press under 300 ms: a tap, discarded without transcription (taps produce "Thank you." hallucinations).
4. Toggle mode: key-up ignored, no double tap; under 300 ms of audio between presses is discarded.
5. Recording reaches `max_seconds` (default 600, SPEC round 4): the overlay warns 30 s before, then stop and transcribe, never discard. Long audio goes to whisper in chunks so one job never holds minutes of state (chunk length set in M5).
6. Esc while recording: cancel, swallow Esc. Esc while transcribing: not ours, passes through.
7. Another key pressed while the chord is held (Ctrl+Win+D and friends): the user meant a shortcut; cancel silently and pass the key through.
8. Key-up lost (UAC prompt, Ctrl+Alt+Del, secure desktop): a 100 ms watchdog reads the physical key state while recording; chord not held means key-up.
9. Win release would open Start (Win11 too): inject VK 0xE8 down/up before the Win key-up passes.
10. Chord keys are never swallowed, so other apps still see Ctrl and Win; only Esc during recording is swallowed.
11. Hotkey changed in settings while recording: applied when the phase returns to Idle.
12. Hook removed by Windows after a stall: the hook thread does nothing but post events, and the hook is reinstalled on resume and on session unlock.
13. Foreground app runs elevated: whether the low-level hook still sees keys is unverified (QA item); paste handles it per section 10.

Audio
14. Mic unplugged mid-recording: stop, transcribe what was captured, notify; the next key-down opens the system default.
15. Configured mic missing at key-down: use the system default, notify once per session.
16. No mic permission (Windows privacy switch, macOS denied): error with a link to the OS page; no whisper call.
17. All-zero audio (hardware mute, privacy switch on some drivers): "microphone gives silence".
18. Bluetooth headset switches profile on open and loses the first half second: known; documented, measured in QA.
19. Start sound picked up by the mic: sounds are short and quiet; trim and the speech gate remove them (checked in M5).
20. Clipping: the overlay meter turns red; nothing is changed automatically.
21. Ring overrun: dropped samples are counted and logged, recording continues.

Model and GPU
22. Model missing at key-down: error sound, notification "No model, open settings", no recording.
23. Registry model fails its hash or whisper init: marked bad in `state.toml`, notification with Re-download. Custom file: error text only, never deleted.
24. Model still loading at key-up: the job waits, overlay says "loading model".
25. GPU OOM or device lost at load: one retry on CPU; at transcription: this job retried on CPU, then CPU until config change.
26. Machine with iGPU and dGPU: explicit device by name; the default is the first discrete GPU.
27. Download interrupted: resume from `.part`; offline: error in settings, nothing else retries in the background.
28. Disk full: download removes its `.part`; a history write is skipped; the dictation is still pasted.

Text
29. Dictionary over 223 tokens (whisper.cpp keeps half of the 448-token text context less one slot for its PREV marker; SPEC rounds this to ~224): settings shows the count with the exact whisper tokenizer and marks the lines past the limit; the daemon sends only the lines that fit, first lines win (whisper.cpp would silently drop from the front).
30. Empty transcript: nothing pasted, overlay "nothing heard".
31. Hallucinated segments ("Субтитры сделал DimaTorzok", "Продолжение следует...", "Редактор субтитров ..."): segments matching the shipped list or user lines in it are dropped; empty after that = case 30.
32. Whisper wraps mid-word with `\n`: removed without a space.
33. `replace.txt` rules: literal, whole word, case-insensitive, longest match first, one pass (a rule never rewrites another rule's output). A line without `=>` is ignored and reported with its line number.
34. Text files saved as UTF-16 or with a BOM (Notepad): BOM stripped, UTF-16 decoded; invalid UTF-8 skips the file with a warning.

Paste and clipboard
35. Modifiers still held after 1.5 s: clipboard-only plus "press Ctrl+V".
36. Elevated target: clipboard-only plus "Admin window: press Ctrl+V".
37. Clipboard locked by another app: type mode.
38. Huge or delayed-render clipboard: section 10 step 4.
39. Clipboard changed between paste and restore: no restore.
40. Clipboard history and cloud clipboard: excluded for both our text and the restored data.
41. Focus moved between key-up and paste: the text goes where focus is at paste time; we do not chase windows.
42. Paste lands nowhere (no text field): the last text stays in memory (never on disk unless history is on); tray item "Copy last text".
43. Non-Latin layout (Russian): VK_V works on Windows; macOS looks the key code up per layout.

System
44. Overlay over a fullscreen game: no-activate, click-through, topmost, tool window, shown with `SW_SHOWNOACTIVATE`, drawn with `UpdateLayeredWindow` (no WM_PAINT, no flicker). `hide_over_fullscreen` uses `SHQueryUserNotificationState` (D3D fullscreen, busy, presentation).
45. Two monitors, DPI change: position and scale are computed on every show from the monitor of the foreground window, its work area (taskbar excluded) and its DPI (per-monitor v2 manifest).
46. Sleep: cancel recording, unload the model. Resume: reinstall the hook; audio reopens on the next key-down anyway.
47. Screen locked while recording: cancel and discard (the user walked away on purpose).
48. Settings and daemon writing config at once: cannot happen, one writer per file (section 6).
49. Hand-edited config with an error: daemon keeps the last good config; settings shows the line and offers "Open file" or "Reset to defaults" (old file kept as `config.toml.bak`).
50. Config from a newer build (downgrade): read-only, never written.
51. Crash during a wav write: `.part` removed at next start; the text was already pasted.
52. History over its cap: oldest entries removed after each save and at start; only files matching the history naming inside `history/` are ever deleted.
53. Update check offline or rate-limited: silent, try again the next day. Asset for this build variant missing: the badge opens the release page instead of Install.
54. Autostart path moved (portable zip moved): each start re-registers autostart if it points elsewhere.
55. Installer and portable copy both present: same per-user socket, the second opens the first one's settings.
56. Two users logged in on one machine: socket name and data are per user; hooks are per session.
57. Stale Unix socket after a crash: connect refused, unlink, bind.
58. Daemon crash (`panic = "abort"`): the panic hook writes the message and flushes the log first; no auto-restart in v1; settings shows "not running" with Start.
59. Linux without `/dev/input` access: GlobalShortcuts portal (chord needs a non-modifier key there), else the tray shows "hotkey unavailable" with the `input` group instructions.
60. macOS Accessibility not granted: clipboard-only plus "press Cmd+V". Input Monitoring not granted: no hotkey, the wizard step reopens. Unsigned updates change the code signature and macOS drops the grants: checked at every start until Developer ID signing.

## 12. Security and privacy

- Network: only the model download (huggingface.co and its CDN) and the update check (api.github.com, github.com release assets). Both go through one `net` module with a host allowlist; both are visible in settings; the update check can be turned off. No telemetry, no crash upload.
- Downloads: models by SHA256 pinned in code; updates by the SHA256 from `SHA256SUMS` of the same release (integrity, not authenticity, until code signing; build provenance attestations let a user verify with `gh attestation verify`). Never self-replace: Install runs the per-user installer the user can see.
- Dictated text never goes to logs; logs hold lengths, timings and errors. `debug.log_text = true` adds text and settings shows a warning while it is on.
- History is off by default; when on it is a plain local folder in the user's data directory, removable from settings.
- IPC is owner-only (section 5), because hotkey capture forwards keystrokes.
- TLS through rustls; `cargo deny` bans OpenSSL and unknown registries or git sources, and fails on RustSec advisories.
- No secrets in the repo (`docs/SECRETS.md`). No owner audio, dictionary or replacement list in the repo: reference phrases live in a private folder passed by path (`OXI_BENCH_DIR`).

## 13. Test strategy

Pure core (`oxi-core`, every OS, seconds):
- State machine: table tests per transition, including every hotkey edge case above, driven with fake time. Property test: any event sequence never pastes after Esc, never runs two whisper jobs at once, always returns to Idle when events stop.
- Text: newline strip, replace engine, hallucination filter. Property tests (proptest): replace with no rules is identity; output never contains `\n`; whole-word rules never fire inside a word. Snapshot tests (insta) over a fixture corpus of synthetic sentences (never owner text).
- Dictionary token budget: the tokenizer in core implements whisper.cpp's greedy longest-match over the multilingual vocab; a regression test in the whisper job compares its counts with `whisper_tokenize` on the tiny model, and checks the embedded vocab and the 223-token budget (`n_text_ctx/2 - 1`) against the same model.
- Config: a fixture per schema version migrates to current; defaults round-trip; unknown keys survive; a newer version loads read-only.
- History retention: deletes oldest first, never deletes a non-history file (temp dir fixture).
- Audio DSP: synthetic sine plus silence for trim, known gains for normalise, resample length and energy checks.
- IPC: message snapshots.

Whisper (`oxi-whisper`, Linux CPU in CI): ggml-tiny.bin cached and checked against a pinned SHA256, transcribe whisper.cpp's `samples/jfk.wav`, word error rate under 0.2 after normalising case and punctuation. Never an exact-text snapshot. Skips with a message when `OXI_TINY_MODEL` is not set, so local runs need no download.

Settings UI (`oxi-settings`): `i-slint-backend-testing` (pinned to the slint version) for the hotkey picker, token counter warning and wizard steps, without a display.

Platform smoke tests (`oxi-platform`, per OS in CI where the runner allows): clipboard snapshot/restore round trip, exclusion formats present, autostart register/unregister in a temp entry, overlay window flags. Input injection and hooks cannot run on CI runners.

Owner acceptance (local, never in CI): `oxi-yapper bench "$OXI_BENCH_DIR"` on the 8 reference phrases; text must equal the bench text for all 8.

Manual QA checklist per OS, run before each release, results kept in the release PR: hotkey in a normal app, an elevated app and a fullscreen game; double tap; Esc; paste in Notepad, a browser, Windows Terminal, an RDP window, a game chat (type mode), with an IME on; clipboard restored and absent from Win+V; overlay never takes focus in the game (open chat, dictate, Enter); two monitors with different DPI; sleep and lock during recording; first-run wizard; macOS permissions; Linux X11 and one Wayland compositor.

Coverage: `cargo llvm-cov` report on `oxi-core`, expected above 85% lines; reported, not a gate. Platform crates are not measured.

## 14. Quality gates

All in `.github/workflows/ci.yml`. Branch protection requires one check, `ci-ok`, which fails if any blocking job failed, was cancelled or skipped.

| Job | Runs | Blocks merge |
|---|---|---|
| hygiene | PR title is a Conventional Commit, `cargo fmt --check`, `typos`, `cargo machete`, `cargo deny check` | yes |
| clippy | `cargo clippy --workspace --all-targets -D warnings` (pedantic, CPU build) | yes |
| core | `cargo nextest run -p oxi-core` on Windows, Linux, macOS; no `-sys` crate in its tree | yes |
| test | `cargo nextest run --workspace` on Linux CPU; whisper tiny regression once M3 lands | yes |
| build | release build + `--version` for Windows Vulkan, Linux Vulkan | yes |
| build-macos | macOS Metal | no, until M12 (macOS runners were sometimes never acquired) |
| build-cuda | Windows CUDA | no, until M5 decides to keep CUDA |

Release (`release.yml`, on a `v*` tag): builds, packages, SHA256SUMS, provenance attestation, GitHub release. Packaging with cargo-packager arrives in ROADMAP M11.
