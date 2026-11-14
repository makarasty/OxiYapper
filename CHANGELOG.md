# Changelog

All notable changes to OxiYapper are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Self-update: once a day (and on Check now in System > Updates) OxiYapper looks for a newer release on GitHub, pre-releases included; with "Install updates automatically" on (default) it downloads the exe, checks it against `SHA256SUMS`, swaps it in when you are not dictating and restarts. A read-only folder or the switch off offers the download page instead.

## [0.1.0] - not released yet

First public version. Windows 10 and 11 only; macOS and Linux builds compile but do not dictate yet.

### Added

- Push-to-talk and toggle dictation on a global hotkey (default Ctrl+Win), double tap to lock recording, optional Esc cancel, a hold gap that continues the same dictation, and a visual keyboard to pick any chord.
- Local transcription with whisper.cpp: Vulkan and CUDA builds, choice of the device the model runs on with CPU fallback, background load on key press, unload after idle or right away, optional load at start.
- Model gallery: Whisper large-v3 turbo, large-v3, a Russian large-v3 (podlodka), their compact files, Distil-Whisper English, small and base; downloads pinned to a Hugging Face commit, resumable and checked by SHA256; a Recommended mark from the graphics memory, system memory, free disk and language; your own `.bin` file; a model per dictation language.
- Paste through the clipboard with your own copy restored, a Type mode for games, terminals and remote desktop, Win+V history control, admin-window detection, optional Enter after paste, line joining for classic consoles, and per-app rules (mode, speed, language, model, text options).
- Audio pipeline: microphone picker that follows the Windows default, boost, loudness levelling, silence trimming, RNNoise noise reduction (light / medium / strong / auto), low-rumble filter, stereo side, speech sensitivity and edge presets, settings per microphone, a 3-second mic check, and suggestions from your recent recordings.
- Text: dictionary in the Whisper prompt with a token budget, replacements, filler-word removal, phrase shortcuts, punctuation from pauses, voice commands (new line, new paragraph, delete the last sentence), style switches, holding back unsure words, and suggestions learned from past dictations and edits. Everything that changes the text is off by default.
- Overlay pill with a live meter that never takes focus, positions, hide over fullscreen apps; start, stop, ready, cancel and error sounds with your own files.
- History: audio and text saved while recording (survives a crash), retention by age and size, player, search, export, transcribe again, transcribe an audio file (wav, ogg, mp3, m4a, flac); documented entry format (`docs/HISTORY-FORMAT.md`).
- Test page: a three-step wizard that compares models and settings on your own recordings and offers the best one.
- Settings window: Home stats (dictations, words, speaking time and speed), light and dark theme, accent colours, search across every setting (Ctrl+F), tray icon with state colours, autostart at sign-in, portable mode.
- Write each dictation to a daily text or Markdown file.
- Release workflow building Windows Vulkan, Windows CUDA, Linux Vulkan and macOS Metal archives with SHA256SUMS and build provenance.

[Unreleased]: https://github.com/makarasty/OxiYapper/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/makarasty/OxiYapper/releases/tag/v0.1.0
