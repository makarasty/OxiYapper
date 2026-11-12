# OxiYapper architecture

How the app is built. `docs/SPEC.md` holds the owner's decisions and wins any conflict with this file. `docs/ROADMAP.md` orders the work. Numbers marked (target) are budgets to measure, not measurements.

## 1. Processes

One executable, three modes:

- `OxiYapper` - the **daemon**. Resident, autostarted. Owns the tray, the global hotkey, the mic, whisper, paste, the overlay, model downloads and the update check. It is the only process that touches the network.
- `OxiYapper settings` - the **settings** window (Slint, Skia CPU surface with the software renderer as fallback). Spawned by the daemon from the tray, exits on close. The only writer of `config.toml`, `dictionary.txt` and `replace.txt`.
- `OxiYapper bench <dir>` - hidden developer mode: runs the audio and text pipeline plus whisper on a folder of wav files and prints text and timings. Used for the reference phrases and the GPU measurement. Never touches the hotkey or the clipboard.

The overlay lives inside the daemon. It is a raw OS window (layered window on Windows, `NSPanel` on macOS, X11 override-redirect or wlr-layer-shell on Linux), not Slint, because Slint cannot do no-activate, click-through and no-taskbar.

Single instance: the daemon binds a per-user local socket at start. If the bind fails, the new process connects, sends `OpenSettings` and exits 0. `OxiYapper settings` started by hand starts the daemon first if nothing answers on the socket.

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
| io | model download, update check | One worker with a job channel; downloads report progress over IPC. |
| history | history writes and retention (`src/daemon/history.rs`) | One worker with a job channel; a failed write comes back as a status warning. |
| ipc | local socket server | One connection at a time is the settings window; other connections only send `OpenSettings`. |

Shared atomics, read lock-free by the UI thread at 30 Hz: `LEVEL` (peak and RMS of the last 20 ms, for the overlay meter) and `PHASE` (current phase as `u8`, read by the hook callback to decide whether Esc is ours; Esc is ours only while recording and `hotkey.esc_cancels` is on).

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
- Esc during Recording, only with `hotkey.esc_cancels` on (default off, R2.1): `Cancelled`, Esc is swallowed. With it off, Esc always passes through. A cancel keeps the audio in history when speech was heard (see section 11, item 7); with no speech it is dropped. Esc at any other time is not ours and passes through.

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
Cargo.toml              workspace + app package (bin OxiYapper)
src/main.rs             CLI dispatch; src/daemon/ wires threads and runs effects
crates/oxi-core         pure logic: machine, config, text, dictionary, history policy,
                        ipc messages, model registry, GPU device choice, audio DSP
                        (pure Rust only)
crates/oxi-platform     OS code, one module per OS: hook, clipboard and input (implement the
                        paste traits of oxi-core), overlay, tray, pipe, capture (trait
                        `Capture`), paths, file helpers
crates/oxi-whisper      whisper-rs wrapper: load/unload, ggml device list, transcribe;
                        owns the vulkan/cuda/metal features
crates/oxi-settings     Slint UI (settings window and first-run wizard)
```

All four crates exist (`members = ["crates/*"]` picks up new ones). `oxi-platform` has only the Windows module so far; macOS and Linux arrive with their milestones. The daemon (`src/daemon/`) is `#[cfg(windows)]`: on other OSes `OxiYapper` prints "not supported on this OS yet" and exits, so the workspace still builds and tests there.

Why a workspace and not one crate:

- **Testability.** `oxi-core` has no OS calls and no `-sys` crate (CI fails if one appears in its tree), so its tests run on all three OSes in seconds without CMake, a GPU SDK or a display. Almost every decision in this file is in it.
- **Compile time.** whisper.cpp (CMake, minutes) and Slint (build script, large) sit in their own crates. Editing the state machine or paste code recompiles neither, and `cargo test -p oxi-core` never builds them.
- **Boundaries agents can see.** A change to paste behaviour on macOS touches `oxi-platform/src/macos/paste.rs`; a change to what happens on Esc touches `oxi-core/src/machine.rs`.

Platform code: one module per OS in `src/windows/`, `src/macos/`, `src/linux/`, picked with `#[cfg(target_os = ...)]` and re-exported as `oxi_platform::os`. A concern gets a trait where a fake is useful: `Capture` is in `oxi-platform`; the paste traits `Clipboard` and `Input` are in `oxi-core::paste` (below). The other Windows pieces (hook, tray, overlay, named pipe) are plain types for now. No `dyn` dispatch where one implementation exists per build.

Paste in core: `oxi-core::paste` holds the traits `Clipboard` (snapshot, set text, sequence number, restore) and `Input` (modifier state, foreground app, chord, Unicode typing, sleep) and the whole algorithm of section 10 as `plan` (pick mode, chord, delay from the rules) and `paste`, plus `copy_only`. `oxi-platform` only implements the two traits per OS, so every branch of section 10 is tested with fakes and no clipboard. Result type: `PasteOutcome`.

Tray: raw `Shell_NotifyIconW` through `windows-sys` (icon, menu, balloons, `TaskbarCreated`, power and session messages) on the main thread loop. No `tray-icon` and no `clipboard-win` crate; `windows-sys` features are listed in `crates/oxi-platform/Cargo.toml`.

Overlay text: the label font is an `ab_glyph` `FontRef` handed to `Overlay::new`, meant to be the system Segoe UI. Without a font the pill shows its icon and meter only; the daemon passes no font yet.

## 5. IPC, daemon <-> settings

Transport: `interprocess` local socket. Windows: named pipe `\\.\pipe\OxiYapper-<user SID>` created with a DACL for the current user only and `PIPE_REJECT_REMOTE_CLIENTS` (the default pipe DACL lets Everyone read). macOS/Linux: Unix socket `OxiYapper.sock` in `$XDG_RUNTIME_DIR` (Linux) or the per-user `$TMPDIR` (macOS), directory mode 0700. A stale socket file (connect refused) is removed before bind.

Windows clients open the pipe at `SECURITY_IDENTIFICATION` (the server can never act as the user) and, before sending anything, check the pipe's owner and mandatory integrity label. The daemon always sets owner = the user's SID and label = Medium (`S:(ML;;NW;;;ME)`), whether or not it runs elevated, so a normal second launch reaches an elevated daemon and the other way round. Outcomes, each with its own message: owner is another SID, or the pipe denies us -> `ForeignServer` ("held by another account"); owner is us but label below Medium (a sandboxed or low-integrity program squatting the name) -> `Untrusted` ("held by an untrusted program"); no instance free within 2 s -> `Busy` ("running but not answering"; busy alone is no evidence of a squatter). On `ForeignServer` or `Untrusted` the daemon refuses to start with that message and never treats the pipe as a running instance.

Connection close rule: the daemon closes a connection whose first message is `OpenSettings` after acting on it; the settings window starts with `Hello` and keeps its connection. A second launch connects, sends `OpenSettings` and waits for that close within 3 s in total (connect, a stalled write and pushes in between all count against it), then exits (a pipe closed before the daemon accepts it is discarded unread, so it must not close first).

Framing: one JSON object per line (`serde_json`), max 1 MiB per line, tagged enum `{"t": "...", ...}`. Types live in `oxi-core::ipc` and are snapshot-tested so a field rename shows up in review.

| From settings | Reply / pushed by daemon |
|---|---|
| `Hello { proto, version }` | `HelloAck { proto, version }` |
| `OpenSettings` (also sent by a second daemon launch) | `Raise` to the open settings window, or spawn one |
| `Status` | `Status { phase, model, device, backend, loaded, warnings }`; `loaded`: a model is in memory (it loads with the first dictation and unloads when idle) |
| `ListDevices` | `Devices { mics, gpus }` |
| `DownloadModel { id }`, `CancelDownload { id }` | `DownloadProgress { id, done, total }`, `DownloadDone { id, result }` |
| `CaptureHotkey { on }` | `Key { code, down }` while on |
| `PreviewOverlay { position }` | overlay shows for 2 s |
| `Stats` | `Stats { dictations, words, wpm, streak, longest_streak, heat_start, heat }`, read from `stats.json` |
| `PreviewSound { choice, volume_percent }` | the daemon plays `choice` once; a sound that cannot be used shows as a `sounds.preview: ...` line in `Status.warnings` |
| `TestAudio { reset, pcm }` | no reply; the connection holds the raw 16 kHz mono recording in memory until it closes, never on disk |
| `TestTranscribe { id, set, language }` | `TestResult { id, outcome }`: `Heard { text, ms }` or `Failed { why }` for one settings set on the held recording, preceded by `TestProgress { id, percent, est_ms }` (0 to 100 while whisper runs; the first carries `est_ms`, the estimated length of the run, so the bar moves before whisper reports; optional, serde default); one run in flight per connection, the rest and any run a dictation overtakes answer `Busy`. A model only a test loaded is dropped after 60 s with no test request |
| `Shutdown` | daemon exits |
| | `ConfigError { file, line, message }` pushed on a bad reload |

Versioning: `proto` is an integer, starting at 1. Adding an optional field (serde default) does not bump it; renaming or removing a field or changing meaning does, and so does a new request (an older daemon drops a connection that sends one). Proto 2 added `Stats`. Proto 3 added `PreviewSound`. Proto 4 added `TestAudio` and `TestTranscribe`. `Hello`, `HelloAck` and `Shutdown` never change shape. On a mismatch (exe updated while the old daemon runs) settings shows "Restart OxiYapper to finish the update" and a button that sends `Shutdown` and relaunches the daemon.

Live updates (planned, run 2026-11-11, task c04; proto 6): the daemon pushes what changed, and the settings window does not poll. Pushed unasked while a settings connection exists: `HistoryAdded { item }`, `HistoryChanged { item }`, `HistoryRemoved { ids }` from the history worker after the files are on disk; `Stats` after each rewrite of `stats.json`; `Status` and `Attention` when the phase, the model load state, the backend or the warnings change; `FileChanged { file }` (config, dictionary, replace) from the config watcher; `Woke` on resume and unlock. With no connection a push is dropped at no cost. The 1 s timer of the window (file stats, redraw) goes away: file checks follow `FileChanged`, and the whole-frame redraw that repairs pixels the OS dropped follows window focus, un-occlusion, resize, theme change, pointer entry and `Woke`. Devices are listed again when their page is shown. `HistoryList` pages come from an in-memory id list built by one directory listing; only the files of the page are read. Proto 5 added `Attention`, the history requests, `DeleteModel` and `TestProgress`.

Live meter: the settings Audio page does not read `LEVEL` over IPC. While the page is shown, settings opens its own capture (`CpalCapture`) and polls its `LevelMeter` at 30 Hz; the capture closes when the page goes away. The daemon's mic is still opened only on key-down. Settings reopens the stream every 60 s to bound the memory the capture keeps.

Hotkey capture: while `CaptureHotkey { on: true }` the daemon pauses the chord, forwards raw key events and swallows them only while the settings window is foreground. Capture ends when settings sends `on: false`, loses focus, or the connection drops. This is the one place keystrokes leave the hook, which is why the socket is owner-only.

## 6. Config

`config.toml`, written only by settings: serialise, write `config.toml.tmp` in the same directory, fsync, rename over the old file. The daemon watches the directory (not the file: the rename replaces the inode) with a 200 ms debounce and reloads. The daemon itself writes `state.toml` (last update check, models marked bad, GPU fallback note), never `config.toml`, except creating it with defaults when it is missing (`create_new`). One writer per file, so no merge logic.

Loading: parse to a `toml_edit::DocumentMut`, read `version`, run migrations `v1 -> v2 -> ...` (each a pure function on the document, unit-tested with a fixture per version), then deserialise into `Config` where every field has `#[serde(default)]`. Saving writes the typed values into the document's existing items, so comments, key order and formatting survive, an unchanged file is written back byte-identical (a partial file, CRLF line endings and a UTF-8 BOM included; a file mixing LF and CRLF is normalised to its dominant ending), a key the file lacks is written only when its value differs from the default, and unknown keys stay in the document, so a newer field survives a round trip through an older settings build. Paste rules are matched by `app`. A file with `version` newer than this build is read for the fields it knows and never written; settings shows it read-only. A parse error keeps the last good config in the daemon and shows the file and line in the tray and settings.

```toml
version = 2
[hotkey]
chord = ["Ctrl", "Win"]        # "Win" is Cmd on macOS
mode = "push"                  # push | toggle
esc_cancels = false            # Esc cancels a recording; off by default (R2.1)
other_key = "auto"             # key outside the chord while recording: auto | never | always
[audio]
device = ""                    # empty = system default
gain_db = 0.0
normalize = true
denoise = false
trim = true
tail_ms = 100
max_seconds = 3600             # 60 min cap (owner, 2026-11-11; was 10 min in SPEC round 4)
meter_sensitivity_db = 0.0     # level meter display only (overlay, settings); -20..+20
[model]
id = "turbo"                   # registry id, or "custom"
custom_path = ""
gpu = "auto"                   # auto | cpu | device name (names survive index changes)
language = "ru"                # any whisper language code, or auto
idle_unload_minutes = 5        # 0 = never unload
unload_immediately = false    # unload as soon as a dictation ends; wins over idle_unload_minutes
[paste]
mode = "clipboard"             # clipboard | type
restore_delay_ms = 300
trailing_space = true
keep_in_clipboard_history = true   # dictation stays in Win+V history; false excludes it (cloud is always excluded)
rules = [{ app = "mstsc.exe", mode = "type" }]   # per-app overrides: mode, chord, delay
[overlay]
enabled = true
position = "bottom-right"      # bottom-centre, top-left, ...
hide_over_fullscreen = false
[sounds]
enabled = true
volume = 0.5                  # 0 to 1
start = "apa-hit-3"            # a built-in id (apa-hit-3, apa-hit-2, rx-25, cancel-soft, boop-3, ...) or the path of a .wav or .ogg file, or "off" (any sound); the old ids rise, fall, pop, tick, fault still load as the default of their sound
stop = "apa-hit-2"
done_enabled = true            # false = no sound when the text is ready (the Off tile of that sound)
done = "rx-25"
cancel = "cancel-soft"                # sound when a recording is cancelled
error = "boop-3"
[history]
enabled = true                 # recording storage is on by default (R1.3); a file with an explicit value keeps it
max_bytes = 2_147_483_648      # size cap, oldest entries go first; 0 = no cap
max_days = 30                  # keep the last N days, today included; 0 = no age limit
folder = ""                    # absolute path, may be another drive, gets an "OxiYapper history" subfolder; empty = history/ in the data dir
[system]
autostart = true
update_check = true
[debug]
log_text = false               # dictated text in logs only when true
[ui]
theme = "system"               # system | dark | light
```

`audio.meter_sensitivity_db` scales only the meter that is drawn, never the audio whisper gets. A value outside -20..+20 is clamped (NaN becomes 0.0) and the load adds a message to `ConfigFile.warnings`. `[ui] theme` is the colour scheme of the settings window: `system` follows the OS light/dark setting. Both keys are optional, and an older file without them loads with the defaults.

Keys added by the 2026-11-11 run (planned, task a04; `version` stays 2, every key is optional):

```toml
[hotkey]
hold_gap_ms = 400              # push mode: a re-press this soon after a release continues the dictation; 0 = off, else 100..1500
[audio]
channel = "mix"                # mix | left | right
vad_sensitivity = "normal"     # low | normal | high: how quiet a sound still counts as voice for the trim
[model]
preload_at_start = false       # load the model when the daemon starts
[model.by_language]            # empty = off; an explicit dictation language picks its model
# ru = "podlodka"
[model.advanced]               # "auto" keeps the engine's own choice
threads = "auto"               # or 1..32
flash_attn = "auto"            # or true | false
decoding = "auto"              # or "greedy" | "beam"
best_of = 1                    # greedy, 1..5
beam_size = 5                  # beam, 2..8
temperature = "auto"           # or 0.0..1.0
temperature_fallback = "auto"  # or true | false
initial_prompt = ""            # put before the dictionary words
audio_ctx = "auto"             # or "full" | "fit" (encoder window fitted to the clip)
no_speech_threshold = "auto"   # or 0.0..1.0
logprob_threshold = "auto"     # or -5.0..0.0
entropy_threshold = "auto"     # or 0.0..10.0
suppress_non_speech = "auto"   # or true | false
[paste]
type_speed = "fast"            # fast (16 units per call, 2 ms apart) | safe (one character, 5 ms apart)
# a rule may also set: type_speed, language, model, to_file, dictionary, replacements,
# voice_commands, filler_filter, pause_punctuation
# pace of an app: the rule's type_speed; else safe when a rule for the app (the user's, or the
# built-in mstsc.exe one) has mode = "type"; else the type_speed above. No file is migrated.
[text]                         # everything here changes the text and is off by default
pause_punctuation = false
pause_sentence_ms = 800        # 300..3000
pause_paragraph_ms = 0         # 0 = off, else 1000..10000
filler_filter = false
fillers = ["э", "ээ", "эм", "мм", "um", "uh", "erm"]
voice_commands = false
confidence_filter = false      # holds a segment back when its mean word confidence is under confidence_min, or is
                               # under 0.6 while its 30 s window got 0.6 or more for "no speech" (the model scores
                               # a window, not a segment, so the window alone never holds anything); held text stays in the history
confidence_min = 0.35          # 0.05..0.95
# [[text.snippets]] with trigger = "..." and text = "..."
[output]
file_mode = "off"              # off | also | only: append each dictation to a daily file
file_folder = ""
file_format = "md"             # md | txt
[history]
skip_pauses = false            # the History player jumps over pauses of 1 s or more (playback only)
[sounds]
folder = ""                    # a folder of .wav/.ogg files offered as sounds
[ui]
accent = "default"             # default | "#RRGGBB"
```

Per-app paste rules answer "games, terminals, remote desktop" without a toggle the user flips by hand. App identity: exe name on Windows, bundle id on macOS, `WM_CLASS` on X11.

## 7. Files on disk

From `directories::ProjectDirs`. Portable mode: if a file named `portable` sits next to the exe, everything goes under `<exe dir>/data/` instead (the portable zip ships it).

| What | Windows | macOS | Linux |
|---|---|---|---|
| `config.toml`, `dictionary.txt`, `replace.txt` | `%APPDATA%\OxiYapper\config\` | `~/Library/Application Support/<bundle id>/` | `$XDG_CONFIG_HOME/OxiYapper/` |
| `state.toml`, `stats.json`, `models/`, `history/` | `%LOCALAPPDATA%\OxiYapper\data\` | `~/Library/Application Support/<bundle id>/` | `$XDG_DATA_HOME/OxiYapper/` |
| `logs/` (daily files, 7 kept) | `%LOCALAPPDATA%\OxiYapper\data\logs\` | `~/Library/Logs/OxiYapper/` | `$XDG_STATE_HOME/OxiYapper/logs/` |

Models are local, not roaming (gigabytes). History entries are `history/YYYY-MM-DD/HHMMSS-mmm.{wav,txt,json}`. `stats.json` (usage stats for the Home page, `oxi-core::stats`, versioned) holds one row per local day: date, dictations, words, audio ms; rows older than about 3 years fold into one totals row. The daemon's stats thread rewrites it atomically after each dictation that reached the field or the clipboard; a corrupt file is renamed `stats.bad` and counting starts fresh. On Unix the data directory is created 0700. The macOS bundle id is chosen in ROADMAP M12 and never changes after the first release: TCC permissions and paths hang on it.

## 8. Model lifecycle

- **Registry** in `oxi-core::models`: id, display name, file name, URL pinned to a Hugging Face commit (`/resolve/<commit>/...`, never `main`), size, SHA256. Curated: podlodka (Russian), large-v3, turbo. Plus "custom .bin": loaded as is, no hash.
- **Download** (daemon, io thread): `ureq` to `models/<file>.part` with `Range` resume, SHA256 computed while streaming, rename on match, delete `.part` on mismatch. If the CDN redirect drops `Range`, restart from zero. A verified file gets a `<file>.ok` stamp (size + mtime); a changed stamp means re-hash before the next load.
- **Load** on key-down (`Preload`), in the background while the user speaks. If key-up comes first, the job waits for the load. Load failure is an `Error` with the reason.
- **Unload** after `idle_unload_minutes` without a job (default 5), and on suspend. Drop the context; M5 measures whether Vulkan really returns the VRAM.
- **Device choice**: `gpu = "auto"` picks the first discrete GPU in ggml's device list, then integrated, then CPU. On the owner's machine ggml's Vulkan backend lists `Vulkan0 = RTX 4070 SUPER (uma: 0)` and `Vulkan1 = AMD Radeon iGPU (uma: 1)`; UMA marks the integrated one. A named device that is gone falls back to auto with a warning. The name-to-index mapping uses ggml's backend device API through `whisper-rs-sys` (unverified that it is exported; fallback is the index in the settings list).
- **CPU fallback**: GPU init failure or OOM retries the load once on CPU, keeps CPU until the config or the model changes, and says so in the tray ("running on CPU: <reason>").
- **Proposal, needs owner OK after M5**: run whisper in a child process (`OxiYapper worker`, same exe) if M5 shows ggml aborting the process on GPU OOM or device loss. A child gives crash isolation and a guaranteed memory release on unload. The whisper thread already talks only through `WhisperCmd`/`Event`, so the move is local to `oxi-whisper` and the coordinator.

## 9. Audio pipeline

Order, and where each step can fail:

1. **Open** the device (configured or system default) on key-down, in its native format. Fails: device gone, no permission, exclusive mode held by another app. Result: `Error` with a deep link to the OS privacy page. The mic is not kept open between dictations (privacy indicator, Bluetooth headsets stuck in call quality).
2. **Capture** callback: convert i16/u16/f32 to f32, push to the ring, update `LEVEL`. Fails: ring full (drain starved) - samples dropped and counted; device lost mid-stream - capture stops, what was captured is kept.
3. **Mono**: average the channels. Manual gain applied here, so the meter shows it.
4. **Denoise** (optional, off): RNNoise needs 48 kHz mono in 480-sample frames, so it runs before the 16 kHz resample, after a resample to 48 kHz if the device rate differs.
5. **Resample** to 16 kHz with rubato, streaming in 20 ms chunks during recording. Fails only on a bad config (unsupported rate) at stream start.
6. **Trim** silence at both ends with an energy gate (earshot), keeping 200 ms of padding. Fewer than 250 ms of voiced audio left (and the level detector never fired): "nothing heard", no whisper call; the raw recording is kept as `failed`, and "Transcribe again" sends it whole.
7. **Normalise**: RMS toward a target with a +20 dB gain cap and a peak limit. Pure silence (all zeros) is not normalised; it raises "microphone gives silence".

Steps 3-7 are pure functions in `oxi-core::audio`, tested on synthetic signals. Defaults (normalise on, trim on, denoise off, tail 100 ms) are confirmed against the 8 reference phrases in M5 before they ship.

History (on by default, R1.3): the audio is written to disk while recording (streaming, `<name>.wav.part` through hound), so a crash or power loss keeps what was recorded; at start the daemon recovers a leftover `.part`. On a normal stop the file is finalized (`finalize()`) and renamed. The coordinator hands the final text to the history thread (`src/daemon/history.rs`) once it is known; it writes the `.txt` and a `.json` meta file beside the `.wav` (a documented, stable wav + txt + json entry format, so other tools can read it; the settings History page offers Copy path), then applies `oxi-core::history::expired` (age, then size, oldest first) to the day folders. Everything lives in one app-owned folder: `OxiYapper history` inside `[history] folder`, or `history/` in the data dir when that is empty or relative. It is created with a `.oxiyapper-history` marker; a folder without the marker is never listed or changed, and a non-empty one of that name is refused. Inside it, only `YYYY-MM-DD/HHMMSS-mmm.wav|txt|json` names with a real date and time are ever listed or deleted, and at start (and on every config change) the leftovers of a killed write directly in a day folder are handled: a `HHMMSS-mmm.wav.part` with audio in it is recovered (finalized and renamed, the entry has no text), an empty one and any `HHMMSS-mmm.txt.tmp` are swept. The age limit sits out a pass with an entry dated after today, or with today more than 31 days past every earlier entry (a clock jump); the size limit always applies. A clean-up failure is logged per entry, the pass goes on, and it shows as its own "Old history not removed" warning. A write error becomes a status warning and never stops the paste; the text is never logged.

## 10. Paste pipeline

Text pipeline first (`oxi-core::text`, pure): remove `\n` without adding spaces, trim, drop hallucinated segments, apply `replace.txt`, add the trailing space. Empty result: "nothing heard", nothing pasted.

Then, in the paste worker (Windows shown; other OSes below):

1. **Pick the mode**: per-app rule for the foreground app, else `paste.mode`.
2. **Wait for modifiers**: poll the physical state of Ctrl, Shift, Alt, Win (both sides) every 5 ms. A pending Ctrl+V with Win still held would open Win+V. If they are still down after 1.5 s: put the text on the clipboard, do not restore, notify "Text is on the clipboard, press Ctrl+V".
3. **Elevated target (UIPI)**: compare the foreground process integrity level with ours. Higher: same as above ("Admin window: press Ctrl+V"). SendInput into it fails silently, so we never try.
4. **Snapshot the clipboard**: open with 10 retries 20 ms apart. Copy every HGLOBAL format up to 32 MiB total; skip GDI-handle formats (CF_BITMAP has CF_DIB beside it), synthesised formats and delayed-render formats the owner fails to render in 500 ms. Locked after retries: switch to type mode. Snapshot incomplete (too big, an owner that hangs or renders too slowly): type the text instead and leave the clipboard alone, so the user's copy is never lost (owner 2026-11-10).
5. **Set** the text as CF_UNICODETEXT plus `CanUploadToCloudClipboard = 0` always, and `CanIncludeInClipboardHistory = 0` and `ExcludeClipboardContentFromMonitorProcessing` when the setting "keep dictation in Win+V history" is off (default on), and also when Win+V is full (25 entries), unreadable, or the history work ran out of time (see the sequence below): then the dictation is pasted but not kept in Win+V. Remember `GetClipboardSequenceNumber`.
6. **Send** Ctrl down, V down, V up, Ctrl up with SendInput (VK codes, which are layout-independent on Windows; the per-app chord may be Shift+Insert or Ctrl+Shift+V).
7. **Restore** after `restore_delay_ms` (default 300; RDP and some Electron apps read late), only if the sequence number is unchanged (someone else wrote the clipboard: leave theirs). The restored data never enters history: it always carries the three exclusion formats. Win+V still has its own entry of the copy, and a restore into history would add a twin and push the oldest entry out of a full history.

**Clipboard history sequence (item 52, Windows, owner 2026-11-10: no entry of the user's is ever pushed out)**: Ctrl+V gives the user's copy, Win+V shows the latest dictation on top, and no entry is pushed out. Win+V holds one entry more than before the first dictation (the dictation itself), and one more again while a delete of ours failed or was put off and waits for the next paste; it never passes 25 through us. Done through the WinRT clipboard history API (`Clipboard.GetHistoryItemsAsync`, `DeleteItemFromHistory`, Windows 10 1809+; windows crate feature `ApplicationModel_DataTransfer`), every call on its own STA thread, limited to 2 s or the time left of the 3 s that all history work of one paste may take. Order: (1) delete our earlier dictation entries by id (one list call, one delete each); (2) set the dictation, into the history only while Win+V holds fewer than 25 entries (it pushes the oldest out for a 26th), and paste it; (3) wait until the history shows it and remember its id for the next paste; (4) put the user's copy back outside the history. Our entry is the new top entry whose text hash is the dictation's; a new entry with other text is a copy of the user's that registered late, and ours goes above it. A delete that fails or times out keeps its id for the next paste; a write not seen in time is remembered by its place and a hash of its text and deleted by the next paste once it shows, unless the user copied in between (a same-text copy could sit in that place: the mark is dropped). The user's copies are never lost to a delete (the OS drops a write it is still registering when an entry is deleted): a copy made since our last paste is waited for until it shows (up to 10 looks); one that does not show keeps our deletes for the next paste and a slot free for itself; no delete runs once the clipboard number moved during the history work, and a copy made then is what the restore puts back (the dictation then stays out of Win+V). The user's copy on top of Win+V is deferred (owner 2026-11-10). A failed restore is reported, never ignored.

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
5. Recording reaches `max_seconds` (default 3600, set by the owner on 2026-11-11): the overlay warns 30 s before, then stop and transcribe, never discard. Long audio goes to whisper in chunks so one job never holds minutes of state (chunk length set in M5).
6. Esc while recording: cancels and is swallowed only when `hotkey.esc_cancels` is on (default off: Esc may be meant for another app); otherwise it passes through. A cancel after speech is saved to history with the notice of item 7. Esc while transcribing: not ours, passes through.
7. Another key pressed while the chord is held (Ctrl+Win+D and friends): the key always passes through to the system (R2.1). What happens to the recording is decided by VAD and `hotkey.other_key`. `auto` (default): no speech heard yet, so the user meant a shortcut and the recording is cancelled silently; speech already heard, so the recording goes on (a short phrase is not lost). `never`: never cancels. `always`: always cancels. A cancel after speech saves the audio to history and shows the notice "cancelled, can be restored". The logic is pure, in `oxi-core::machine`, with a test per edge case.
8. Key-up lost (UAC prompt, Ctrl+Alt+Del, secure desktop): a 100 ms watchdog reads the physical key state while recording; chord not held means key-up.
9. Win release would open Start (Win11 too): inject VK 0xE8 down/up before the Win key-up passes.
10. Chord keys are never swallowed, so other apps still see Ctrl and Win; only Esc during recording is swallowed, and only with `esc_cancels` on.
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
40. Clipboard history and cloud clipboard: our text stays in Win+V history by default (setting, owner 2026-11-08) and is excluded when the setting is off; the cloud clipboard is always excluded, for both our text and the restored data.
41. Focus moved between key-up and paste: the text goes where focus is at paste time; we do not chase windows.
42. Paste lands nowhere (no text field): the last text stays in memory (never on disk unless history is on); tray item "Copy last text".
43. Non-Latin layout (Russian): VK_V works on Windows; macOS looks the key code up per layout.

System
44. Overlay over a fullscreen game: no-activate, click-through, topmost, tool window, shown with `SW_SHOWNOACTIVATE`, drawn with `UpdateLayeredWindow` (no WM_PAINT, no flicker). `hide_over_fullscreen` uses `SHQueryUserNotificationState` (D3D fullscreen, busy, presentation).
45. Two monitors, DPI change: position and scale are computed on every show from the monitor of the foreground window, its work area (taskbar excluded) and its DPI. `Overlay::new` calls `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`, so DPI awareness does not depend on a manifest; the call failing because a manifest (not planned yet) or an earlier call already set it is fine.
46. Sleep: cancel recording, unload the model. Resume: reinstall the hook; audio reopens on the next key-down anyway.
47. Screen locked while recording: cancel and discard (the user walked away on purpose).
48. Settings and daemon writing config at once: cannot happen, one writer per file (section 6).
49. Hand-edited config with an error: daemon keeps the last good config; settings shows the line and offers "Open file" or "Reset to defaults" (old file kept as `config.toml.bak`).
50. Config from a newer build (downgrade): read-only, never written.
51. Crash during a wav write: a non-empty `.wav.part` is kept and recovered as "interrupted" at next start (see 52), never deleted; the text was already pasted.
52. History over its cap: oldest entries removed after each dictation and at start, saving on or off, in the current folder only (a former folder is never touched; a `.txt.tmp` leftover is swept and a non-empty `.wav.part` is recovered as "interrupted" on the same pass, never deleted); age is strict by date, entries dated after today are kept; only files matching the history naming inside the app-owned folder that holds the marker are ever deleted.
53. Update check offline or rate-limited: silent, try again the next day. Asset for this build variant missing: the badge opens the release page instead of Install.
54. Autostart path moved (portable zip moved): each daemon start and config reload re-registers autostart (`HKCU\...\Run\OxiYapper`, quoted exe path) if it points elsewhere, and removes it when off and it is this exe; a test daemon (`OXI_PIPE_SUFFIX` set) never touches it.
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
- History is on by default (R1.3), can be turned off, and is a plain local folder in the user's data directory, removable from settings.
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

Live tests need a real desktop and are off unless their env var is `1` (list in `CONTRIBUTING.md`): hook, `SendInput`, clipboard, overlay window, tray icon, microphone and the whole daemon. Live tests never inject Ctrl+Win, because the owner's dictation tool listens on it; hook tests use F13 chords. Only the clipboard tests run in CI (Windows clippy job).

Owner acceptance (local, never in CI): `OxiYapper bench "$OXI_BENCH_DIR"` on the 8 reference phrases; text must equal the bench text for all 8.

Manual QA checklist per OS, run before each release, results kept in the release PR: hotkey in a normal app, an elevated app and a fullscreen game; double tap; Esc; paste in Notepad, a browser, Windows Terminal, an RDP window, a game chat (type mode), with an IME on; clipboard restored (the dictation is in Win+V with the default on, absent with the setting off); overlay never takes focus in the game (open chat, dictate, Enter); two monitors with different DPI; sleep and lock during recording; first-run wizard; macOS permissions; Linux X11 and one Wayland compositor.

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

## 15. Progress (planned, run 2026-11-11)

One model for every progress the user sees (overlay pill, downloads, test and history runs): `oxi-core::overlay::progress::Smooth`. Its rules and constants are `docs/DESIGN-SYSTEM.md` section 26. It is fed an estimate of the whole job and, when they exist, real fractions, and answers the value to draw: it moves from the first frames, never goes back, never jumps, and runs to 100 % on finish. Transcription: the estimate is a fixed cost plus a rate per audio second, learned per model and backend during the run (first guess 282 ms + 88 ms per second of audio); the percent whisper reports per 30 s window is a real fraction. Typing: the real fraction is the units typed. A clipboard paste has no bar. The paste worker reports "landed" as soon as the text is in the field, before the clipboard restore, and the pill shows done then. Animation never delays work. The overlay reads the value on its 33 ms timer, which runs only while the pill animates; the settings window drives its bars with one 33 ms timer that runs only while a job is shown.

## 16. Word data in the history (planned, run 2026-11-11)

- At dictation time each word gets a probability (the lowest of its tokens), stored in the json of the entry. No extra whisper work.
- Word times are computed only when a recording is opened or exported, never during dictation: DTW needs a context without flash attention. The whisper worker runs the saved wav through a second context (flash attention off, the DTW preset of the model) in the slot that test and history runs use, so a dictation always goes first; the context is dropped 60 s after the last request. The result is cached in the json (`docs/HISTORY-FORMAT.md`).
- The timing pass is its own transcription, so its words are lined up with the shown text by a word diff (`oxi-core::history::align`); a word with no partner gets an interpolated time.
- Before any UI is built on it, the times are checked on Russian recordings against the voice gate (monotone, inside the clip, 85 % of word time voiced, pause edges within 150 ms median). If DTW fails the check, plain token timestamps are tried; if both fail, the player works per segment.
