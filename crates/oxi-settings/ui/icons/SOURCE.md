# Vendored icons

Fluent UI System Icons, Microsoft, MIT licence (text in `LICENSE` next to this file).

- Repository: https://github.com/microsoft/fluentui-system-icons
- Commit: `e15fc0de25fab983e3e42bac8798be69a85c23bf` (`main` on 2026-11-07, committed 2026-11-05)
- URL pattern: `https://raw.githubusercontent.com/microsoft/fluentui-system-icons/<commit>/assets/<Folder Name>/SVG/ic_fluent_<name>_<size>_<style>.svg`
- Files are unchanged (single `<path>` with `fill="#212121"`; components recolour them with `colorize`).
  Each was checked to contain only `<svg>` and `<path>` elements before it was copied in.
- `ui/theme/icons.slint` maps every file to a name: `Icons.<name>` is the 20 px regular master,
  `-16`/`-24`/`-12` other masters, `-filled` the filled style.

## Icons (91 files)

| Name | Sizes and styles | Use |
|---|---|---|
| keyboard | 20 regular/filled, 24 regular/filled | nav Hotkey, paste "Type" tile |
| mic | 20 regular/filled | nav Audio, device row, stat card |
| brain_circuit | 20 regular/filled | nav Model |
| text_t | 20 regular/filled | nav Text, replacements |
| slide_text | 20 regular/filled | nav Overlay |
| settings | 20 regular/filled | nav System |
| info | 20 regular/filled, 16 regular | nav About, info banner/note |
| speaker_2 | 20 regular/filled | gain, M10 Sounds |
| history | 20 regular/filled | M10 History |
| home | 20 regular/filled | nav Home (dashboard) |
| edit | 20, 16 regular | change hotkey |
| hand_left | 20 regular/filled, 24 regular/filled | push-to-talk tile |
| toggle_right | 20 regular/filled, 24 regular/filled | toggle tile |
| clipboard_paste | 20 regular/filled, 24 regular/filled | paste "Clipboard" tile |
| dark_theme | 20 regular/filled, 24 regular/filled | theme System tile |
| weather_sunny | 20 regular/filled, 24 regular/filled | theme Light tile |
| weather_moon | 20 regular/filled, 24 regular/filled | theme Dark tile |
| data_usage | 20 regular | level row |
| gauge | 20 regular | meter sensitivity |
| timer | 20 regular | tail, max length, idle unload |
| local_language | 20 regular | language |
| arrow_download | 20, 16 regular | download |
| dismiss | 20, 16 regular | cancel |
| checkmark_circle | 20, 16 regular | success note |
| checkmark | 16, 12 regular | check badge (12) |
| folder_open | 20, 16 regular | browse |
| open_folder | 20, 16 regular | open file / folder |
| cube | 20 regular | own .bin |
| desktop | 20 regular | GPU |
| book_letter | 20 regular | dictionary |
| app_generic | 20 regular | per-app rule |
| add | 20, 16 regular | add rule |
| delete | 20, 16 regular | remove rule |
| save | 20, 16 regular | save |
| eye | 20, 16 regular | overlay preview |
| window | 20 regular | overlay |
| arrow_sync | 20, 16 regular | updates, restart |
| bug | 20 regular | debug |
| warning | 20, 16 regular | warning banner/note |
| error_circle | 20, 16 regular | error banner/note |
| play | 20, 16 regular | start daemon |
| fire | 20 regular | streak stat card |
| text_word_count | 20 regular | words stat card |
| top_speed | 20 regular | speed stat card |
| calendar | 20 regular | activity heatmap header |

Size notes: `_32_` masters do not exist for hand_left, toggle_right, keyboard and dark_theme, so
every tile icon uses the `_24_` master drawn at 32 px (one stroke weight across a picker). All names
marked "verify" in the spec exist (edit, timer, add, save, eye, bug, play); none needed a substitute.
