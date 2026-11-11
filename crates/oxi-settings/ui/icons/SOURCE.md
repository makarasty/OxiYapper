# Vendored icons

Phosphor Icons, `@phosphor-icons/core@2.1.1`, MIT licence (text in `LICENSE` next to this file,
copied from the same package version). Owner's pick, 2026-11-10 (DESIGN-SYSTEM section 19).

- Package: https://www.npmjs.com/package/@phosphor-icons/core/v/2.1.1
- URL pattern: `https://cdn.jsdelivr.net/npm/@phosphor-icons/core@2.1.1/assets/<weight>/<name>-<weight>.svg`
- Files are unchanged: one `<svg viewBox="0 0 256 256" fill="currentColor">` with one `<path>`;
  components recolour them with `colorize`. Each was checked to contain only `<svg>` and `<path>`
  elements (no `href`, `style` or script) before it was copied in.
- Weights: `-bold` everywhere; `-fill` only for a selected item's glyph (DS-19.7). One drawing per
  icon, scaled to the 16, 20 or 32 px box.
- `ui/theme/icons.slint` maps every file to a name. The names of the earlier Fluent set (`-16`,
  `-24`, `-12`, `-filled`) stay as aliases, so pages keep compiling; new code uses the plain name.

## Project marks and the avatar

`si_<name>.svg` come from simple-icons, CC0 1.0 (logos stay their owners' trademarks), unchanged;
each was checked to contain only `<svg>`, `<title>` and `<path>` elements.

- Repository: https://github.com/simple-icons/simple-icons
- Commit: `1bb2f6e6ea50f920de8d2f3d273ae48564a5eaee` (`develop` on 2026-11-09)
- URL pattern: `https://raw.githubusercontent.com/simple-icons/simple-icons/<commit>/icons/<name>.svg`
  (`si_claude.svg` fetched through `https://cdn.jsdelivr.net/gh/simple-icons/simple-icons@<commit>/icons/claude.svg`, same bytes)
- Files: `si_rust.svg`, `si_slint.svg`, `si_github.svg`, `si_claude.svg` (About tiles and buttons).
- No mark at that commit for whisper.cpp / ggml, whisper-rs, nnnoiseless, cpal or tiny-skia, and
  none of those projects publishes a logo under a licence that allows bundling, so their About
  tiles keep a Phosphor glyph. The Phosphor tile can use any Phosphor glyph (MIT, ours to ship).

`../assets/avatar-makarasty.png` is the public GitHub avatar of makarasty (owner approved), with one edit: pixels in the
purple hue band (245-315 degrees) were desaturated to under 5% chroma, so the page passes the
no-system-accent-purple shot test. No file here is hand-drawn.
`../assets/logo-window-{16,20,24,32}.png` are the app's own mark: the 16, 20, 24 and 32 px PNG
entries of `assets/oxi.ico`, which `regen_icons` (`crates/oxi-platform/tests/draw.rs`) writes from
`oxi_platform::draw::logo`. After a change of the mark, run `regen_icons` and copy those four
entries out again.

## Icons (61 files)

| `Icons.` name (aliases) | File | Use |
|---|---|---|
| home, home-filled | house bold, fill | nav Home |
| brain-circuit, -filled | brain bold, fill | nav Model |
| keyboard, -filled, keyboard-24, -24-filled | keyboard bold, fill | nav Hotkey, paste "Type" tile |
| mic, mic-filled | microphone bold, fill | nav Audio, device row, stat card |
| text-t, text-t-filled | text-aa bold, fill | nav Text, replacements |
| slide-text, slide-text-filled | picture-in-picture bold, fill | nav Overlay |
| settings, settings-filled | gear-six bold, fill | nav System |
| info, info-16, info-filled | info bold, fill | nav About, info tip, note |
| warning, warning-16 | warning bold | warning banner/note |
| warning-circle | warning-circle bold | the "!" slot (DS-19.6) |
| error-circle, error-circle-16 | x-circle bold | error banner/note |
| checkmark-circle, -16 | check-circle bold | success note |
| checkmark-12 | check bold | check badge |
| folder-open, folder-open-16, open-folder-16 | folder-open bold | every open-folder action (item 4) |
| open-16 | arrow-square-out bold | external link only |
| caret-down | caret-down bold | "more" in a segment or list (DS-17.1) |
| dots-three | dots-three bold | overflow menu (DS-17.1) |
| add, add-16 | plus bold | add rule |
| delete, delete-16 | trash bold | remove |
| edit-16 | pencil-simple bold | change hotkey |
| dismiss-16 | x bold | cancel, clear |
| save-16 | floppy-disk bold | save |
| eye-16 | eye bold | preview |
| play | play bold | start daemon, play |
| arrow-download, -16 | download-simple bold | download |
| speaker-2 | speaker-high bold | gain, sounds |
| history, history-filled | clock-counter-clockwise bold, fill | nav History |
| stop | stop bold | stop playing |
| redo | arrow-clockwise bold | transcribe again |
| path | path bold | copy path |
| hand-left-24, -filled | hand-palm bold, fill | push-to-talk tile |
| toggle-right-24, -filled | toggle-right bold, fill | toggle tile |
| clipboard-paste-24, -filled | clipboard-text bold, fill | paste "Clipboard" tile |
| dark-theme-24, -filled | circle-half bold, fill | theme System tile |
| weather-sunny-24, -filled | sun bold, fill | theme Light tile |
| weather-moon-24, -filled | moon bold, fill | theme Dark tile |
| timer | timer bold | tail, max length, idle unload |
| top-speed | gauge bold | speed stat card |
| fire | fire bold | streak stat card |
| text-word-count | article bold | words stat card |
| calendar | calendar-blank bold | activity heatmap |
| desktop | desktop bold | GPU |
| book-letter | book-open-text bold | dictionary |
| app-generic | app-window bold | per-app rule |
| code | code bold | Model > Advanced, Decoding row |
| person-voice | user-sound bold | About whisper.cpp tile |
| plug-connected | plugs-connected bold | About whisper-rs tile |
| sound-wave-circle | waveform bold | About nnnoiseless tile |
| icons | shapes bold | About icon set tile |
| paint-brush | paint-brush bold | About tiny-skia tile |
