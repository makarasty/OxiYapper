# OxiYapper spec

Hold a hotkey, speak, release: the text appears in the focused field. Local whisper.cpp on GPU, no cloud, no AI post-processing. Windows, macOS, Linux.

Status: decisions from the owner interview (2026-11-06). Open items are marked OPEN.

## Processes

- **Daemon** (default mode, autostart): tray, global hotkey hook, mic capture, whisper, paste, overlay. No GUI toolkit beyond what the overlay needs.
- **Settings** (`OxiYapper settings`): separate process, Slint (software or Skia renderer, picked by the text-quality spike), spawned from the tray, exits on close. Costs zero RAM when closed.
- One exe, one bundle id on macOS (one permission entry). Config is a TOML file; the settings process writes it (temp file + atomic rename), the daemon watches and reloads. While the settings window captures a hotkey it tells the daemon over IPC to pause the hook.
- Single instance: a second launch asks the running daemon to open settings and exits.

## Recording

- Modes, chosen in settings: push-to-talk (hold, release = stop) and toggle (press = start, press = stop). In push-to-talk a double tap locks recording on until the next press.
- Default hotkey: Ctrl+Win (Ctrl+Cmd on macOS), modifier-only chord. Win release must not open Start (inject VK 0xE8).
- Esc cancels a recording: nothing is pasted.
- Microphone picked from a list (default: system device). Language: ru, en, auto.

## Audio before whisper

All measured against the 8 owner reference phrases before becoming defaults.
- Auto level normalisation (on).
- Manual gain slider plus level-meter sensitivity, with a live meter in settings.
- Noise suppression, RNNoise (`nnnoiseless`), off by default: Whisper can get worse on denoised audio.
- Trim silence at both ends (cuts hallucinations on silence and latency).

## Feedback

- Start and stop sounds.
- Tray icon: monochrome shapes for idle and transcribing; colour only for recording and the update badge.
- Overlay pill with a live volume meter. Position chosen in settings (bottom-right default, bottom-centre, other corners), never at the mouse cursor. Can be turned off.
- Overlay must never take focus or flicker: no-activate, click-through, topmost, no taskbar entry. Case to hold: a player in a fullscreen game opens chat, dictates, presses Enter. Option to hide the overlay over fullscreen apps.

## Text

- `dictionary.txt` words go into the whisper prompt. whisper.cpp keeps only the last ~224 tokens: show the token count and warn past the limit.
- `replace.txt` lines `heard => wanted`, whole word, case-insensitive, applied after transcription, never in the prompt.
- Strip `\n` from whisper output without adding spaces (it wraps mid-word).
- Paste: save clipboard, set text, synthetic Ctrl/Cmd+V, restore clipboard after a delay; exclude from Windows clipboard history. Elevated target window (UIPI): say "press Ctrl+V" instead of failing silently.

## Models

- Short curated list (podlodka for Russian, large-v3, turbo) downloaded from Hugging Face with SHA256 check, plus "use my own .bin".
- Load in the background on key-down, unload after idle (default 5 min, configurable).
- GPU: Vulkan build is the default for everyone; the CUDA build stays and is the default on the owner's machine (owner decision 2026-11-06; M5 flash attention on: p50 -20..-41 % Vulkan, -14..-34 % CUDA; one installer per variant, M11). Pick the GPU explicitly (machines have an iGPU too).

## History

- Each dictation saves wav + text to a folder. On for the owner; off by default for other users (privacy).

## Settings UI

- Look modelled on Windows 11 Settings: opens large and resizable (min 720x520); page scrolling allowed as a fallback. Own teal accent, not the Windows accent.
- Look modelled on XClicker: one compact panel.
- Visual pickers instead of long lists: Fluent icons in tiles and a drawn screen picker for overlay position, mode and the like.
- Hotkey picker: a minimal field plus a button that opens a full visual keyboard; click keys or press them, held keys light up.
- Confirmations only for important actions (example: turning off update checks shows a short note why it is recommended to keep it).
- First-run wizard: microphone, model, hotkey; on macOS also the permissions.
- About screen carries the "Made with Slint" badge (Slint royalty-free licence).
- Icon: own logo mark (own drawing, no licence issue); UI icons from Fluent UI System Icons (MIT), pinned to a commit.

## System

- Autostart on by default, toggle in settings.
- Update check: once a day against GitHub Releases, badge on the tray icon plus an "Install" button; never installs on its own; verifies SHA256; can be turned off.

## Build and CI

- Windows: cargo target dir must be short (`C:\t\...`), the vulkan-shaders-gen ExternalProject exceeds MAX_PATH otherwise; Ninja does not help.
- CI matrix: Windows Vulkan, Windows CUDA, Linux Vulkan, macOS Metal; every action pinned by SHA; no `pull_request_target`.

## Decided in round 3

- Paste: restore the previous clipboard; add a trailing space; a "type character by character" mode for fields where Ctrl+V does not work (games, terminals, remote desktop). No auto-Enter.
- Cancel key is Esc, fixed. No "paste last again" hotkey, no second-language hotkey.
- History: size cap, configurable (default 2 GB), oldest removed first. A history view in settings: play, copy, add to replacements.
- Distribution: free only. Windows per-user installer (no admin) plus portable zip; macOS .dmg; Linux AppImage and .deb. Package managers and code signing later.

## Decided in round 4

- Theme follows the OS, with a visual picker (System, Light, Dark; Windows order).
- Sounds: a set of 3-4 own synthesised sounds (no third-party rights), volume slider, preview in settings, custom wav allowed.
- Recording cap: 10 minutes, configurable; the overlay warns 30 s before; long audio is transcribed in chunks.
- Moving from the previous tool (owner's machine, never in the repo): copy the 59-word `C:\Users\mky\Desktop\dictionary.txt` into the local config; reuse the podlodka model already on disk (SHA256 checked, no 3 GB re-download); import its history (transcriptions.db + audio) as an app feature or a one-off script.

## OPEN

- Linux Wayland paste and hotkey path.
- Test strategy and quality gates.
- Architecture (module map, threads, IPC messages).
