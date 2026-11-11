# Design system

The one document every UI task reads before it touches `crates/oxi-settings/ui`. It turns `docs/UI-RULES.md` into tokens, components and numbered checks. SPEC and UI-RULES win a conflict; a rule here that seems to contradict them is a bug in this file: file a finding.

How to use it:

- Tokens live in `crates/oxi-settings/ui/theme/tokens.slint` (`Theme.*`). "New" marks a token the primitives task (t21) adds; "was" marks a value it changes. Pages never hold a px number (UI rule 7).
- Components live in `crates/oxi-settings/ui/theme/components/`. A page uses them as they are. A page that needs a different look files a finding; it never restyles a primitive locally.
- Every rule has an id (DS-n.m) and a check a reviewer can run: a `rg`, a measurement in a real shot (`scripts/real-shot.ps1`, both themes, 720x520 and 1280x860 at least), or a gallery state (`ui/gallery.slint`).
- MISSION item numbers are in brackets, for example [19].

## 1. Spacing [15, 25, 27]

Scale, 4 px base: `space-1` 4, `space-2` 8, `space-3` 12, `space-4` 16, `space-6` 24, `space-8` 32. The named tokens below are the only gaps; each reads a scale step.

| Token | Value | Use |
|---|---|---|
| `page-padding` | 16 (was 24) | content left/right/bottom edge from the rail and window |
| `page-padding-narrow` | 12 (was 16) | the same under `narrow-window` |
| `header-row-height` (new) | = `rail-item-height` (40) | the header row (section 14) |
| `header-bottom` (new) | 12 | header row's tab line to the first card or group |
| `group-gap` | 24 (was 32) | between two groups on a page [27] |
| `header-gap` | 8 | group title to its first card |
| `card-gap` | 4 | between cards of one group |
| `card-padding-h` / `card-padding-v` | 16 / 12 | inside a card |
| `control-gap` | 8 | between controls in one row |
| `icon-gap` | 16 | a row's 20 px icon to the row text |
| `pair-gap` (new) | 8 | icon to its label in any icon+label pair (buttons, stat titles, chips) |
| `tile-gap` | 8 | between tiles of a grid |
| `title-gap` | 2 | title to its caption: the only off-scale gap |

`title-top` and `title-bottom` go away with the page title (R2.4).

- DS-1.1 Every gap, padding and spacing in `ui/` reads a token above. Check: UI rule 7's `rg` prints nothing; `rg "[1-9][0-9]*px" crates/oxi-settings/ui/theme/components` prints only 1 px hairlines, each with a comment.
- DS-1.2 Every gap measured in a real shot is one of 2, 4, 8, 12, 16, 24, 32 px. Check: measure the gaps around each card on every page; any other value is a defect [11].
- DS-1.3 The first card's top edge is `header-bottom` (12 px) below the header row's tab line on every page [25]. Check: measure in a shot; 0 px spread across pages.
- DS-1.4 Content starts `page-padding` (16 px) right of the rail on every page and sub-tab. Check: x of the leftmost card edge, 0 px spread across pages.

## 2. Radii [46]

`radius-small` 2 (cells under 16 px), `radius-control` 4 (controls, badges, chips, tooltips, inline tiles), `radius-card` 8 (cards, tiles, banners, popups, the overlay pill), `radius-round` (circles: switch track and thumb, radio, slider thumb, status dot).

- DS-2.1 Every `border-radius` reads a `Theme.radius-*` token (UI rule 12). Check: `rg "border-radius" crates/oxi-settings/ui` shows only tokens, or a token plus the focus ring offset (section 4).
- DS-2.2 The overlay pill uses the card radius, 8 logical px times the monitor scale, not `h / 2` [46]. One constant in `crates/oxi-core/src/overlay/look.rs` (`CARD_RADIUS`, new) holds 8, and `crates/oxi-platform/src/draw.rs` reads it. Check: `rg "h / 2.0" crates/oxi-platform/src/draw.rs` finds no pill corner; a test in oxi-settings reads `radius-card` from `tokens.slint` and asserts it equals `look::CARD_RADIUS`.
- DS-2.3 A shape with no radius token of its own (an inner fill, a selection indicator, a progress fill) inside a rounded parent with padding p uses radius max(parent radius - p, `radius-small`). Controls, badges, chips and tiles keep their own token above wherever they sit: a Combo in a card is `radius-control` 4. Check: read the gallery state of each nested shape.

## 3. Control column [19]

Tokens: `control-column-min` 240, `control-column-max` 360, `control-height` 32, `number-field-width` (new, 80). Component: `Row` (layout.slint) sizes the column from `Shell.page-width`, the same in every row of a page.

- DS-3.1 Value controls fill the column: Combo, TextField, PathField, Segmented, Meter, and Slider plus NumberField. Their left and right edges are the column's edges. Check: in a shot of Audio, System and Model, the left x of every value control matches to 0 px, and so does the right x.
- DS-3.2 A slider row is `[Slider stretch][control-gap][NumberField number-field-width]`; the NumberField's right edge is the column's right edge. Check: Gain and Mic sensitivity rows: slider left x equals the Combo's left x above, field right x equals the Combo's right x.
- DS-3.3 Compact controls (Switch, one Button, IconButton) pack to the column's right edge. A button beside a value control (Test next to the Meter) takes its width from the column: the Meter shrinks, the right edge stays. Check: right x of every control in a card matches to 0 px.
- DS-3.4 Every control is vertically centred in its row. Check: control box centre y and row centre y differ by at most 0.5 px.

## 4. Focus ring [1, 20]

Tokens: `focus-ring` (new colour: the accent, softened; hex by t20), `focus-ring-width` 2, `focus-ring-offset` (new, 2). `focus-outer` and `focus-inner` go away. Component: `FocusRing` (base.slint), the only focus visual; `KeyFocus` decides when it shows.

- DS-4.1 One ring: 2 px of `focus-ring`, 2 px outside the control; its outer radius = control radius + `focus-ring-offset` + `focus-ring-width` (the fixed offset UI rule 12 allows). No second ring, no white or black ring. Check: zoomed shot of a Tab-focused sub-tab, button, tile and field in both themes: one coloured ring, no white pixels.
- DS-4.2 The ring shows only for keyboard focus (UI rule 17). Check: click a control: no ring; press Tab: ring.
- DS-4.3 No ring while the control's popup is open (Combo list, SearchPopup, menus). It comes back on close if the focus came from the keyboard. Check: Tab to the microphone Combo, press Space: zero pixels of `focus-ring` outside the popup.
- DS-4.4 An open popup covers its field: popup x <= field x and popup right >= field right [20]. Check: shot of the open Combo: no pixel of the field's border or ring left or right of the popup.
- DS-4.5 Nothing clips a ring: a container keeps at least `focus-ring-offset` + `focus-ring-width` (4 px) free around focusable children, or draws the ring inside. Check: Tab through every page; each ring is whole.

## 5. States: hover, pressed, selected [16, 30, 43]

| Surface | Rest | Hover | Pressed | Selected |
|---|---|---|---|---|
| framed (Button, fields) | `bg-control` | `bg-control-hover` | `bg-control-pressed` | n/a |
| subtle (IconButton, tab, list row) | transparent | `bg-subtle-hover` | `bg-subtle-selected` | `bg-subtle-selected` |
| card (ClickCard) | `bg-card` | `bg-control-hover` | `bg-card-pressed` | `accent-tint` |
| card (Tile) | `bg-card` | `bg-control-hover` | = hover fill (no darker press, DS-5.5; `tiles.slint`) | `accent-tint` |

Motion tokens: `motion-quick` 100 ms (hover, pressed, selected, knob), `motion-open` 160 ms (popup, tooltip). Both read 0 under `Shell.reduced-motion`.

- DS-5.1 Every clickable element has a hover look and `mouse-cursor: pointer` (UI rule 36), including the clear "x" in a search field and the "i" tip [43]. Check: `rg -A3 "TouchArea" crates/oxi-settings/ui` shows `mouse-cursor: pointer` on every TouchArea with a `clicked`; hover each in a real run.
- DS-5.2 Inert things have no hover (UI rule 37): labels, plain cards, the status chip in its normal state [33]. Check: hover each: no change.
- DS-5.3 Every fill, border, text colour and opacity change of a state animates with `motion-quick`, `easing: ease-out` [16, 30]. Check: `rg "animate" crates/oxi-settings/ui` lists every stateful component; each reads `Theme.motion-*`.
- DS-5.4 No scale, bounce or overshoot (UI rule 38). Only colour, opacity, and the position of a knob or selection indicator that slides with ease-out. Check: no `animate` on `width`, `height` or a scale; no `ease-out-back`, `ease-in-out-elastic` or `cubic-bezier` with values above 1.
- DS-5.5 Pressing an already selected item keeps the selected fill: no flash [30]. Selection moves as a cross-fade: the old fill fades out while the new fades in [16]. Check: click a selected model row and the Paste tiles in a slow-motion screen capture: no frame is brighter than hover.

## 6. IconButton [26]

Tokens: `control-height` 32 (box), `icon-size-small` 16 (glyph), `radius-control`. Component: `IconButton` (new, button.slint; replaces `ButtonKind.icon` and every page-local icon button: the Hotkey pencil, About links, open-folder buttons).

- DS-6.1 One size: a 32x32 box whose whole area takes hover and click; subtle surface (section 5). Check: `rg -A8 "TouchArea" crates/oxi-settings/ui/pages` shows no `Image`: every icon button is an IconButton.
- DS-6.2 The glyph is 16 px at whole-pixel x and y. Its centre is within 0.5 px of the box centre. Check: gallery geometry: (32 - 16) / 2 = 8 on both axes.
- DS-6.3 Its `label` is the tooltip and the accessible name. A destructive one turns red on hover (UI rule 16). Check: hover each icon button: tooltip shows; Remove shows `error-*`.
- DS-6.4 A field's own adornments (the clear "x", the Combo chevron) belong to the field, `adornment-size` (new, 24) square, inset `space-1`, with the same hover. They are not IconButtons.

## 7. Button with icon [32, 42]

Component: `Button` (button.slint). Height `control-height`, padding `space-3`, icon `icon-size-small`, icon to label `pair-gap`, min width `button-min-width` (new, 88).

- DS-7.1 Icon and label are one group, centred in the button; the gap is exactly 8 px. Check: in the gallery, icon right edge to label left edge = 8 px on every icon button.
- DS-7.2 Icon centre y equals the label's centre y and the button's centre y, within 0.5 px. Check: gallery geometry.
- DS-7.3 Buttons that swap in one slot (Download, Resume, Cancel, Delete on a model) share one width, from a token that fits the widest label (`model-action-column`). The slot never changes width. Check: shot of each model state: the button's x and width match to 0 px.
- DS-7.4 No label carries "..." or "…": the icon says a dialog opens [31, 42]. Browse is a Button with the `folder-open` glyph like every other button. Check: `rg -F '..."' crates/oxi-settings` and `rg '…"' crates/oxi-settings` find no label.

## 8. Inputs [23]

Components: `Combo`, `TextField`, `NumberField`, `TextArea`, `SearchField` (new), `PathField` (section 13). If a std-widget base cannot take this look (the fluent ComboBox and LineEdit draw their own underline and focus), the component is rebuilt on our own primitives.

- DS-8.1 One shape: height `control-height` (TextArea: `editor-min-height`), radius `radius-control`, fill `bg-control`, 1 px `stroke-control` border with a `stroke-control-bottom` bottom edge, text padding `space-3`, text `body-size`, placeholder `text-secondary`. No accent underline. Check: gallery row of all five side by side: same height, corner, fill and border, pixel for pixel.
- DS-8.2 Hover: fill `bg-control-hover`. A text field being edited (any focus reason) shows a 1 px `accent` border; keyboard focus adds the ring (section 4). Check: gallery forced states.
- DS-8.3 Disabled and locked inputs use `text-disabled` ink on the rest fill. No opacity on text [22]. Check: `rg "opacity" crates/oxi-settings/ui/theme/components` touches no Text.

## 9. Badge [29]

Component: `Badge` (feedback.slint). Height `badge-height` (new, 20), padding across `space-2`, radius `radius-control`, `caption-size` text.

- DS-9.1 Text is centred: the visible glyphs' centre is within 0.5 px of the badge centre. The Text fills the badge with `vertical-alignment: center`. Check: zoomed shot of Ready, Paused, Downloading and Not downloaded: equal space above the cap height and below the baseline, within 0.5 px.
- DS-9.2 Ink on fill is at least 4.5:1 for every kind, in both themes, on `bg-card` and on a hovered card. Check: the gallery's contrast swatches (`Contrast.ratio`) for each kind read >= 4.5.
- DS-9.3 Every badge in one column has the same height; badges never wrap. Check: model list shot.

## 10. Switch [6, 18]

Component: `Switch` and `ToggleRow` (switch.slint, layout.slint). Box `control-height`, track `switch-track-width` x `switch-track-height` (new, 40 x 20), `radius-round`, "On"/"Off" caption left of the track.

- DS-10.1 The track is centred in the Switch box: (32 - 20) / 2 = 6 px above and below. Check: gallery geometry.
- DS-10.2 In a row, the track centre y equals the row centre y and the centre y of the other controls in that row ("30 days", "2 GB"), within 0.5 px. Check: System Save recordings and Audio Noise suppression rows, and every other Switch (`rg "Switch|ToggleRow" crates/oxi-settings/ui/pages`).

## 11. Info tip "i" [3, 13]

Component: `InfoTip` (new, base.slint). Glyph `info` 16 px in a 20x20 hover area (`icon-size`), `radius-round`, subtle surface. The text is a `Tip`, at most two short sentences.

- DS-11.1 No explanatory text lies loose on a page (UI rule 35). A hint is an InfoTip right after the title or control it explains, `pair-gap` from it, centred on its first line. Check: read each page shot; `rg "Caption|Note" crates/oxi-settings/ui/pages` finds no sentence that explains instead of naming a value.
- DS-11.2 The tip opens on hover (the Tip delay) and on keyboard focus, and Enter or Space toggles it. Check: Tab to an "i": the tooltip shows.

## 12. Empty state [39]

Component: `EmptyState` (new, feedback.slint). Glyph `icon-size-large` (new, 32) in `text-secondary`, one line in `body-size` `text-primary`, an optional Button `space-3` below.

- DS-12.1 It sits centred in the same box the full list takes: the page around it does not move (UI rule 1). Check: shot empty and full: the list box's rect matches.
- DS-12.2 One line says what is missing; the Button does the fix (Scan for apps, Download a model). Any longer hint is an InfoTip. Check: read the empty shots in UI-RULES' table.

## 13. Paths [44, 45]

Component: `PathField` (new, fields.slint). The elision is pure logic: `elide_path(path, max_chars) -> String` in oxi-core, with tests.

- DS-13.1 A path shows its file or folder name whole, as many trailing folders as fit, and "…" for the rest: `C:\…\models\ggml-large-v3.bin`. Only a name wider than the field alone is cut, at its end. Check: oxi-core tests for a short path, a long path, a long name, a UNC path and a root.
- DS-13.2 The full path shows in the tooltip, and as selectable text while the field has focus. Check: hover and Tab to the model path in a long-path shot.
- DS-13.3 An empty value shows its default in `text-secondary` ("Default folder"). Open folder is an IconButton with the `folder-open` glyph and a tooltip naming the folder [4, 7]. Check: System Recordings folder row.
- DS-13.4 Every file or folder dialog starts at the current value's folder. If that is gone, it starts at the nearest existing parent, else at the default folder (models folder for a model file). Never at Documents by default. Check: `rg "FileDialog::new" crates/oxi-settings/src` shows a `set_directory` on every call.

## 14. Header row [R2.4, 2, 24, 33, 40]

Component: `HeaderRow` (new, layout.slint). Height `header-row-height`. Left: `SubTabs`, or the page name in `body-size` `weight-strong` on a page without tabs. Right: `StatusChip` (new). A 1 px `divider` line at the bottom.

- DS-14.1 No page title and no "OxiYapper" line in the rail (UI rule 34). Check: `rg "PageHeader" crates/oxi-settings/ui/pages` prints nothing.
- DS-14.2 The tab line spans the whole content width, from `page-padding` to `page-padding` [24]. Check: shot of Test and Text: line ends match the card edges to 0 px.
- DS-14.3 The header row's centre y equals the first rail item's centre y, within 1 px (UI rule 8). Check: shot of each page.
- DS-14.4 StatusChip: text, then a dot (`status-dot-size`, new, 8; `radius-round`) at `pair-gap` right of it. Hover and pointer only when the status has an action (no model, no microphone, daemon down); the normal state is inert. Check: hover the chip in the normal and the "no model" state.

## 15. Card grid [41, 12]

Component: `CardGrid` (gallery.slint, from `TileGallery`), shared by the Per-app apps, the model gallery and the sounds. Tokens: `tile-min-width` (new, 160), `tile-height` 104, `tile-height-short` 76, `tile-gap`, `icon-size-large` for the tile glyph or picture.

- DS-15.1 One anatomy: glyph or picture top-left, title (`body-size`, one line, elided with tooltip), caption (`caption-size`), a reserved top-right slot for a badge or check. Check: the three galleries side by side in a shot: same tile height, glyph box and padding.
- DS-15.2 Columns = as many `tile-min-width` columns as fit; cells share the width equally; the grid takes the free height and scrolls inside itself (UI rule 26). Check: Per-app at 1280x860 and full screen shows more rows than at 720x520; wheel moves only the grid.
- DS-15.3 States follow section 5's card row; selected adds the `CheckBadge`. Check: gallery states.
- DS-15.4 Icons in any box sit at equal padding on all four sides, within 0.5 px [11]. Check: geometry of the Built with, stat and tile icon boxes.

## 16. Icon and label pairs [35]

- DS-16.1 In every icon+label pair (stat card titles, group titles, chips, buttons) the icon centre y equals the label's line-box centre y within 0.5 px, and the gap is `pair-gap`. Check: Home stat titles zoomed: measure both centres.

## 17. "More" affordance [31]

- DS-17.1 "More choices" is a glyph, never typed dots: `caret-down` for a trailing segment or list that opens more (the language Segmented), `dots-three` for an overflow menu. Each has a tooltip ("More languages"). Check: `rg '"\.\.\."|"…"' crates/oxi-settings/ui` prints nothing.

## 18. Scrollbar [14]

Component: `ScrollView` (new, layout.slint, over Flickable; std's fluent bar grows off its axis and cannot be restyled). Tokens: `scrollbar-width` (new, 6), `scrollbar-gutter` (new, 12).

- DS-18.1 The thumb keeps its width and x on hover; only its colour changes (`text-disabled` at rest, `text-secondary` on hover and drag). Radius `radius-small`. Check: shot at rest and on hover: thumb rect x and width match.
- DS-18.2 The thumb is centred in the gutter, 3 px from the content edge. Content leaves exactly `scrollbar-gutter` only while it overflows. Check: measure in an overflowing Per-app grid.

## 19. Brand and icons (owner's pick, R5.1)

Brand: concept C "white minimal" from `.fleet/2026-11-10-ui-polish/choice/brand.html`. Ink on paper. The logo mark is an "o" ring with a record dot, one mark everywhere (exe, window, rail, tray), drawn once in `crates/oxi-platform/src/draw.rs` (`logo`, `tray_icon`); `assets/oxi.ico`, `ui/theme/logo.svg` and `ui/assets/logo-window-*.png` are generated from it.

Palette (tokens.slint, set by t20; light / dark):

| Token | Light | Dark |
|---|---|---|
| `bg-page` | #F7F5F0 | #161616 |
| `bg-card` | #FCFBF8 | #1F1F1F |
| `bg-control` | #FFFEFB | #262626 |
| `bg-control-hover` | #F8F6F2 | #2C2C2C |
| `bg-control-pressed` | #F1EEE8 | #222222 |
| `bg-card-pressed` | #F1EEE8 | #1A1A1A |
| `bg-subtle-hover` / `-selected` / `bg-inset` | #1C1B19 at 4 / 6 / 8 % | #FFFFFF at 6 / 8 / 10 % |
| `stroke-card` | #E6E2DA | #2F2F2F |
| `stroke-control` / `-bottom` | #D3CEC4 / #B5AFA4 | #383838 / #444444 |
| `stroke-keycap` | #8A857C | #5A5A5A |
| `divider` | #EAE7E0 | #2A2A2A |
| `text-primary` = `accent` = `accent-text` | #1C1B19 | #F4F2EE |
| `text-secondary` = `focus-ring` | #5C5850 | #B5B1AA |
| `text-disabled` | #A39F97 | #75726D |
| `accent-hover` / `accent-pressed` | #34322E / #4A4740 | #FFFEFA / #D9D6D0 |
| `on-accent` | #FCFBF8 | #161616 |
| `accent-tint` = `accent-badge` | #ECE9E2 | #2E2E2E |
| `info-fill` | #F2F0EB | #292929 |
| `recording` | #E5484D | #E5484D |

Status inks and fills (`error-*`, `warning-*`, `success-*`) keep their earlier values. Overlay and tray (`oxi_core::overlay::palette`): `RECORDING` #E5484D, `CLIP` #FFFFFF (the meter is red, so clipping goes white), tray update badge `UPDATE` #30A46C (green, red is taken), error notices and the tray error badge `ERROR` #C86400 (deep orange: 3.43:1 on a #EEEEEE light taskbar, 4.10 on #202020 dark, 4.13 on the pill), app icon `PAPER` #FAF7F1 with `PAPER_EDGE` #E4DED2 and the ring `MARK` #1C1B19.

Contrast (WCAG 2, computed from the tokens; light / dark): every ink (`text-primary`, `text-secondary`, `accent-text`, `error-text`, `warning-text`, `success-text`) on every surface (`bg-page`, `bg-card`, `bg-control`, `-hover`, `-pressed`, `bg-card-pressed`, `info-fill`) is at least 4.70 / 6.54 (lowest: `success-text`, light; `text-secondary`, dark). `text-primary` 14.86 / 12.49 at least; `text-secondary` 6.11 / 6.54; `on-accent` on `accent` 16.63 / 16.18, on `accent-pressed` 8.95 / 12.48; `accent-text` on `accent-badge` 14.19 / 12.15; `text-secondary` on `bg-inset` over a hovered card 5.63 / 4.71; non-text minimums over the same surfaces (lowest on `bg-control-pressed` light, `bg-control-hover` dark): `accent` 14.86 / 12.49, `focus-ring` 6.11 / 6.54, `recording` 3.38 / 3.57.

- DS-19.1 The accent is the ink: `accent` = the primary text colour of the theme, `on-accent` = the paper colour. Selected fills (`accent-tint`, `accent-badge`) are neutral steps, not a hue. Check: tokens.slint after t20.
- DS-19.2 Red (`recording`, #E5484D) means "recording now" only: the overlay, the tray and the logo dot. Errors keep the `error-*` tokens; in the overlay and tray they are deep orange (`palette::ERROR` #C86400), never red. Check: `rg "recording" crates/oxi-settings/ui` shows no use outside a recording state.
- DS-19.3 Colour lives only in palette tokens, in both themes, so a later colour theme replaces tokens and touches no page or component. Check: `rg "#[0-9A-Fa-f]{6}" crates/oxi-settings/ui --glob '!tokens.slint'` prints nothing (logo assets exempt).
- DS-19.4 Every pair of text and fill passes 4.5:1, and the ink accent on paper passes 3:1 for non-text marks (Switch track, focus ring). Check: the gallery contrast swatches.

Icons: Phosphor, Bold weight, `@phosphor-icons/core@2.1.1` (MIT), vendored with its licence and listed in `crates/oxi-settings/ui/icons/SOURCE.md`; read only through the `Icons` global (`ui/theme/icons.slint`).

- DS-19.5 Sizes: 16 px in buttons, fields and InfoTip; 20 px in rows, group titles and the rail; 32 px in tiles and empty states. Check: `rg "width: Theme.icon-size" crates/oxi-settings/ui` covers every Image of an icon.
- DS-19.6 One glyph per meaning: `folder-open` for every open-folder action [4], `caret-down` and `dots-three` for "more" (section 17), `info` for tips, `warning-circle` for the "!" slot. Check: `rg "Icons\." crates/oxi-settings/ui/pages` shows one name per meaning.
- DS-19.7 Only the Bold weight, except a selected tile's glyph, which uses the Fill weight of the same release. No hand-made icons except the logo mark. Check: `SOURCE.md` lists each file with its weight.

## Open: not in this file

Charts (hover tooltip, bar reveal: an ease-out height reveal without overshoot is a data reveal, not a bounce) are t23's. The overlay's HiDPI sharpness [57] and its progress [49] are t32's. The disabled-text rendering check [22] is t21's (DS-8.3 covers inputs).
