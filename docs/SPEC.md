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
- Esc cancels a recording only if the setting "Esc cancels" is on; it is off by default, because Esc may be pressed by accident or meant for another app (owner 2026-11-10). A cancelled recording is never pasted.
- A key outside the chord pressed during a recording always reaches the system. Whether it also cancels is decided by voice activity (VAD) and the setting "Other key": auto (default), never cancel, always cancel. Auto: no speech heard yet means a shortcut (Ctrl+Win+D works) and a quiet cancel; speech already heard means the recording goes on, so a very short phrase is not lost. Any cancel after speech saves the audio to history and shows the notice "cancelled, can be restored" (owner 2026-11-10).
- Microphone picked from a list (default: system device). Dictation language: auto or any language whisper supports (owner 2026-11-08). The picker shows the 2 last used languages (start: ru, en), Auto, and a "..." popup with every whisper language and a search field. The app (UI) language is a separate picker, independent of the dictation language.

## Audio before whisper

All measured against the 8 owner reference phrases before becoming defaults.
- Auto level normalisation (on).
- Manual gain slider plus level-meter sensitivity, with a live meter in settings.
- Noise suppression, RNNoise (`nnnoiseless`), off by default: Whisper can get worse on denoised audio.
- Trim silence at both ends (cuts hallucinations on silence and latency).

## Feedback

- Start and stop sounds, a "text ready and pasted" sound (on by default), and a selectable sound for cancel and for error. A sound that fails to play is shown in the status, never silent (owner 2026-11-10).
- Tray icon: monochrome shapes for idle and transcribing; colour only for recording and the update badge.
- Overlay pill with a live volume meter. Position chosen in settings (bottom-right default, bottom-centre, other corners), never at the mouse cursor. Can be turned off.
- Overlay must never take focus or flicker: no-activate, click-through, topmost, no taskbar entry. Case to hold: a player in a fullscreen game opens chat, dictates, presses Enter. Option to hide the overlay over fullscreen apps.

## Text

- `dictionary.txt` words go into the whisper prompt. whisper.cpp keeps only the last ~224 tokens: show the token count and warn past the limit.
- `replace.txt` lines `heard => wanted`, whole word, case-insensitive, applied after transcription, never in the prompt.
- Strip `\n` from whisper output without adding spaces (it wraps mid-word).
- Paste: save clipboard, set text, synthetic Ctrl/Cmd+V, restore clipboard after a delay. Paste always pastes, except in Type mode and when the clipboard is locked by another program; there is no automatic typing of short texts (owner 2026-11-08). What can be restored is restored; a complex clipboard object may be lost (accepted).
- Windows clipboard history (Win+V): setting "keep dictation in Win+V history", default on (owner 2026-11-08). Ctrl+V returns the previous clipboard. Order in Win+V (owner 2026-11-10): the user's own copy stays current and on top, the dictation is second, and the previous dictation of ours is removed from the history. The dictated text is never uploaded to the cloud clipboard, whatever the setting. Elevated target window (UIPI): say "press Ctrl+V" instead of failing silently.

## Models

- Short curated list (podlodka for Russian, large-v3, turbo) downloaded from Hugging Face with SHA256 check, plus "use my own .bin".
- Load in the background on key-down, unload after idle (default 5 min, configurable, with an "immediately" option; "never" stays; owner 2026-11-08).
- Partial downloads (`.part`) are kept 7 days from the last write, then removed at start (owner 2026-11-08). The model card shows the size and when it will be removed; buttons "Delete partial downloads" and "Open models folder".
- GPU: Vulkan build is the default for everyone; the CUDA build stays and is the default on the owner's machine (owner decision 2026-11-06; M5 flash attention on: p50 -20..-41 % Vulkan, -14..-34 % CUDA; one installer per variant, M11). Pick the GPU explicitly (machines have an iGPU too).

## History

- Recording storage is a setting, on by default (30 days / 2 GB; owner 2026-11-10, replaces the off-by-default rule of 2026-11-08) and can be turned off. Each dictation saves wav + text + json meta to a folder the person chooses, with retention limits by size and by age (see History in round 3). The audio is written to disk while recording, so a crash or power loss keeps it. The entry format (wav + txt + json) is stable and documented, so other tools can read it.
- Usage stats for the Home page (owner request 2026-11-07), always on: per local day only the number of dictations, their word count and audio length. Never the text, app names or a time finer than the day; nothing of it goes to logs or the network.

## Settings UI

- Look modelled on Windows 11 Settings: opens large; resizable from a minimum (720x520) with no maximum size; maximize is allowed (owner 2026-11-10); lists and galleries stretch and scroll inside themselves; content adapts to the size, and the hotkey keyboard no longer changes the content width in a jarring way: the content adapts (owner 2026-11-08). Page scrolling is a fallback only: normal pages fit without it. Brand "white minimal" (owner pick 2026-11-10, concept C): ink on paper, the accent is the ink of the theme (#1C1B19 light, #F4F2EE dark), never the Windows accent; red (#E5484D) only for "recording now". Palette in `docs/DESIGN-SYSTEM.md` section 19.
- Look modelled on XClicker: compact, with few controls per screen. No single-panel layout, except Home and Hotkey (with the on-screen keyboard open), which are one-view pages with a tight layout (owner, 2026-11-10): pages have horizontal sub-tabs in the header row, no separate page title (owner 2026-11-08, R2.4): Model has choice / settings / Advanced; Text has Dictionary / Replacements; Per-app rules has its own sub-tab or page. The sidebar stays short and keeps working in icons-only mode.
- A Test tab (owner 2026-11-08): ready phrases in the person's language or their own phrase; scenario profiles (fast speech, bad microphone, long dictation, others) choose which settings sets to try (normalisation, noise suppression, model, tuning); a table with error percent per set; an "Apply best" button. No AI. Recordings made for a test stay in memory and are deleted on close; recordings from History can be used too (owner 2026-11-10).
- Rows never change height (owner 2026-11-08). A persistent state shows a "!" icon in a reserved slot of the row with the text in a hover tooltip. Short events (for example the mic test recording) are toasts at the bottom of the window.
- Visual pickers instead of long lists: Phosphor icons in tiles and a drawn screen picker for overlay position, mode and the like.
- Hotkey picker: a minimal field plus a button that opens a full visual keyboard; click keys or press them, held keys light up.
- Confirmations only for important actions (example: turning off update checks shows a short note why it is recommended to keep it).
- First-run wizard: microphone, model, hotkey; on macOS also the permissions.
- About screen carries the "Made with Slint" badge (Slint royalty-free licence).
- Icon: own logo mark, an ink "o" ring with a red record dot on a paper tile (own drawing, no licence issue), the same mark in the exe icon, window, sidebar and tray; UI icons from Phosphor, Bold weight (`@phosphor-icons/core@2.1.1`, MIT), pinned to that version (owner pick 2026-11-10).

## System

- Autostart on by default, toggle in settings.
- Tray menu is the native menu, dark with our icons (owner 2026-11-08); if the OS call is unavailable it stays light. No custom window.
- Update check: once a day against GitHub Releases, badge on the tray icon plus an "Install" button; never installs on its own; verifies SHA256; can be turned off.

## Build and CI

- Windows: cargo target dir must be short (`C:\t\...`), the vulkan-shaders-gen ExternalProject exceeds MAX_PATH otherwise; Ninja does not help.
- CI matrix: Windows Vulkan, Windows CUDA, Linux Vulkan, macOS Metal; every action pinned by SHA; no `pull_request_target`.

## Decided in round 3

- Paste: restore the previous clipboard (see Text); add a trailing space; a "type character by character" mode for fields where Ctrl+V does not work (games, terminals, remote desktop). No auto-Enter.
- Cancel by Esc is a setting, off by default (see Recording). No "paste last again" hotkey, no second-language hotkey.
- History (recording storage, see History): retention by size cap and by age, both configurable (default 2 GB and the last 30 days; whichever limit is hit first removes the oldest entries). The age limit is strict by date; only entries dated after today (clock set back) are spared. Both limits apply to the folder in use also while saving is off; a former folder is never touched (owner 2026-11-10). A history view in settings (owner 2026-11-10): play, copy, Transcribe again, add to replacements, delete, Copy path. The Test page can run on several recordings from the history, each with its own editable reference text.
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
