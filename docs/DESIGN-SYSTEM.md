# Design system

The one document every UI task reads before it touches `crates/oxi-settings/ui` or the overlay's look. It turns `docs/UI-RULES.md` into tokens, components and numbered checks. SPEC and UI-RULES win a conflict; a rule here that seems to contradict them is a bug in this file: file a finding.

How to use it:

- Tokens live in `crates/oxi-settings/ui/theme/tokens.slint` (`Theme.*`). "New" marks a token the primitives task adds; "was" marks a value it changes. Pages never hold a px number (UI rule 7).
- Components live in `crates/oxi-settings/ui/theme/components/`. A page uses them as they are. A page that needs a different look files a finding; it never restyles a primitive locally.
- Every rule has an id (DS-n.m) and a check a reviewer can run: a `rg`, a measurement in a real shot (`scripts/real-shot.ps1`, both themes, 720x520 and 1280x860 at least), or a gallery state (`ui/gallery.slint`).
- `[M7]` is item 7 of the owner's list in `.fleet/2026-11-11-ui-overhaul/mission.md`. Section 29 maps every item to its section.
- Sections 1 to 19 keep their numbers, and their rule ids keep their meaning, from the earlier version (about 80 code comments cite them). A rule added on 2026-11-11 took the next free id of its section. Two ids changed meaning by owner decision (DS-19.1, DS-19.2): "Rule ids" at the end lists the code lines to reword. Sections 20 to 28 are new. Section 19 holds the neutral base; the accent on top of it is section 20.
- "Today" lines are measurements from the dark 1280x860 real shots of 2026-11-11: they say what the rule changes.

## 1. Spacing [M7, M35]

Scale, 4 px base: `space-1` 4, `space-2` 8, `space-3` 12, `space-4` 16, `space-6` 24, `space-8` 32. The named tokens below are the only gaps; each reads a scale step.

| Token | Value | Use |
|---|---|---|
| `page-padding` | 16 | content left/right/bottom edge from the rail and window |
| `page-padding-narrow` | 12 | the same under `narrow-window` |
| `header-row-height` | = `rail-item-height` (40) | the header row (section 14) |
| `header-bottom` | 12 | header row's bottom line to the first card or group title |
| `group-gap` | 24 | between two groups on a page |
| `header-gap` | 8 | group title to its first card |
| `card-gap` | 4 | between cards of one group |
| `card-padding-h` | 16 | inside a card, left and right |
| `card-padding-v` | 12 | inside a card that holds no rows (a tile, an editor, a detail pane) |
| `row-height` (new) | 48 | a settings row: `control-height` 32 plus 8 above and below |
| `row-height-tall` (new) | 56 | a row with a description line |
| `card-padding-rows` (new) | 4 | top and bottom padding of a card that holds rows |
| `control-gap` | 8 | between controls in one row |
| `icon-gap` | 16 | a row's 20 px icon to the row text |
| `pair-gap` | 8 | icon to its label in any icon+label pair (buttons, stat titles, chips) |
| `tile-gap` | 8 | between tiles of a grid |
| `title-gap` | 2 | title to its caption: the only off-scale gap |

`title-top` and `title-bottom` go away (no page title, UI rule 34).

- DS-1.1 Every gap, padding and spacing in `ui/` reads a token above. Check: UI rule 7's `rg` prints nothing; `rg "[1-9][0-9]*px" crates/oxi-settings/ui/theme/components` prints only 1 px hairlines, each with a comment.
- DS-1.2 Every gap measured in a real shot is one of 2, 4, 8, 12, 16, 24, 32 px. Check: measure the gaps around each card on every page; any other value is a defect.
- DS-1.3 The first element under the header row (a card or a group title) starts exactly `header-bottom` (12 px) below the header's bottom line on every page and sub-tab. Check: measure in a shot; 0 px spread across pages. Today: 60 (Home, Model, Overlay, History, Test), 64 (Audio, Text), group title text at 70 (Hotkey) and 74 (System).
- DS-1.4 Content starts `page-padding` (16 px) right of the rail on every page and sub-tab. Check: x of the leftmost card edge, 0 px spread across pages.
- DS-1.5 Row pitch. In every card, two neighbouring rows are 49 px apart (48 + the 1 px divider); with a description, 57. A one-row card is 56 px high (4 + 48 + 4). `Row`, `ToggleRow` and `SliderRow` share this. The one other height is a picker row: a row whose control is a wide picker (DS-3.7) is as high as the picker plus 16 (8 above and below), from a token per picker (`position-row-height` 88 for the position grid of 28.4; a `TilePicker` row: its tile height + 16); its pitch is that height + 1, the same in every state. Check: centre y of consecutive 32 px controls on Audio, Hotkey, Overlay, Test and System differ by 49; a picker row's height equals its token. Today: 49 on Audio, 40 on Hotkey, 49 and 52 on Overlay.

## 2. Radii

`radius-small` 2 (cells under 16 px), `radius-control` 4 (controls, badges, chips, tooltips, inline tiles), `radius-card` 8 (cards, tiles, banners, popups, the overlay pill), `radius-round` (circles: switch track and thumb, radio, slider thumb, status dot).

- DS-2.1 Every `border-radius` reads a `Theme.radius-*` token (UI rule 12). Check: `rg "border-radius" crates/oxi-settings/ui` shows only tokens, or a token plus the focus ring offset (section 4).
- DS-2.2 The overlay pill uses the card radius, 8 logical px times the monitor scale (`look::CARD_RADIUS` in oxi-core). Check: the oxi-settings test that reads `radius-card` from `tokens.slint` and asserts it equals `look::CARD_RADIUS`.
- DS-2.3 A shape with no radius token of its own (an inner fill, a selection indicator, a progress fill) inside a rounded parent with padding p uses radius max(parent radius - p, `radius-small`). Controls, badges, chips and tiles keep their own token wherever they sit. Check: read the gallery state of each nested shape.

## 3. Control grid [M7, M35]

The right side of every settings row is one column, cut the same way on every page, so controls line up top to bottom.

Tokens: `control-height` 32; `control-column` (new) = 360 px when `Shell.page-width` >= 720, else 240 (two steps, never a fluid value; replaces `control-column-min`/`-max`); `control-side` (new, 88; `number-field-width` was 80 and `button-min-width` 88: both read it); `control-main` = `control-column` - `control-gap` - `control-side` = 264 (or 144). Component: `Row` (layout.slint) places the column; pages never size a control.

```
|<----------------- control-column 360 ----------------->|
|<--------- control-main 264 --------->|<-8->|<- side 88 ->|
[ Combo / TextField / PathField / Segmented, full column  ]
[ Slider rail                          ]     [ NumberField ]
[ Meter                                ]     [ Button      ]
                                    On  [ Switch track 40 ]
                                             [ Button >=88 ]
```

- DS-3.1 Full-column controls (Combo, TextField, PathField, Segmented, a Meter with no button) span the column exactly: left x = column left, right x = column right. Check: left and right x of every such control on Audio, System, Model, Test and Hotkey match to 0 px. Today: App language on System is 240 wide (x 1008) while every other page's Combo is 360 (x 889).
- DS-3.2 Split rows are `[main][control-gap][side]`: Slider + NumberField, Meter + Button, Combo + Button, PathField + Button. The side slot is `control-side` (88) and the main slot `control-main` (264). A side Button's label must fit 88: a longer label is reworded, the slot is never widened. One named exception: the Model action row (28.2), alone in its card, has the side `model-action-column` (112, the widest of Download, Resume, Cancel and Delete with its icon), so its side holds the button, 8 and the 32 px Delete slot, and the bar's slot is 360 - 2 x 8 - 112 - 32 = 200 (80 in the 240 column). Check: on Audio, the Test button and both dB fields share left x and right x; the Meter and both slider rails share left x and right x. Today: Test is 88 wide, the dB fields 80; the Meter ends at 1152, the rails at 1150.
- DS-3.3 Compact controls pack to the column's right edge: Switch (track right edge = column right; the On/Off caption left of it), one Button, IconButtons, several buttons 8 px apart in the order of section 23. A Button is max(`control-side`, content + 24) wide: content is the label's width, plus 24 (icon 16 + `pair-gap` 8) when the button has an icon; the other 24 is the padding (2 x `space-3`). A button beside a value control sits in the side slot of DS-3.2. Check: right x of every control in a card matches to 0 px; gallery: a button with an icon and a 70 px label is 118 wide (70 + 24 + 24), and no label or icon is cut.
- DS-3.4 Every control is vertically centred in its row, and a row holds one line of controls, never a second. A confirm button belongs in the side slot or is merged away (section 27). Check: no control's centre y differs from its row's centre y by more than 0.5 px. Today: Test, "From history": Add sits on a line of its own under the Combo.
- DS-3.5 One height: Combo, TextField, NumberField, PathField, SearchField, Segmented, Button, IconButton, Switch box, Slider box and Meter are all `control-height` (32). `meter-height` was 24: it becomes 32. Check: gallery row of all of them side by side: equal top and bottom y. Today: Meter 24 beside a 32 px Test button.
- DS-3.6 A Slider's rail spans its whole slot: rail left x = slot left, rail right x = slot right. The 20 px thumb's centre travels from 10 px inside each end; the thumb never leaves the slot. Check: Gain rail left x = Device Combo left x. Today: rail starts 10 px right of the column (old backlog J80).
- DS-3.7 Wide pickers are the one exemption from the column's left edge: `TilePicker`, the position grid (section 28.4) and the accent swatches (DS-20.7). Their right edge is still the column's, and their rows have the picker height of DS-1.5.

## 4. Focus ring [M9]

Tokens: `focus-ring` = `accent` (section 20), `focus-ring-width` 2, `focus-ring-offset` 2. `focus-outer` and `focus-inner` go away. Component: `FocusRing` (base.slint), the only focus visual; `KeyFocus` decides when it shows.

- DS-4.1 Look: one 2 px stroke of `focus-ring`, 2 px outside the control, outer radius = control radius + 4. No second ring, no white or black ring, no glow. Check: zoomed shot of a Tab-focused sub-tab, button, tile and field in both themes.
- DS-4.2 The ring shows only when the focus came from a key: Tab, Shift+Tab, an arrow key, or an access key. Pointer click, window activation, popup activation and programmatic focus never show it. `KeyFocus.from-key` must return false for `FocusReason.programmatic`; a control that hands focus on passes the answer along (`take-focus(ring)`). Check: click a control: no ring; press Tab: ring. Today: `KeyFocus.from-key` (base.slint:45) treats programmatic focus as a key press, which rings the search field of a popup opened by mouse.
- DS-4.3 No ring on a control while its popup is open (Combo list, `SearchPopup`, menus). It comes back on close only if the control had it before. Check: Tab to the microphone Combo, press Space: zero pixels of `focus-ring` outside the popup; Esc: the ring is back.
- DS-4.4 An open popup lines up with its field: popup x = field x and popup width = field width. The rest of its geometry is DS-8.6. Check: shot of the open Combo: the popup's left and right x equal the field's, to 0 px.
- DS-4.5 Nothing clips a ring: a container keeps 4 px free around focusable children, or draws the ring inside. Check: Tab through every page; each ring is whole.
- DS-4.6 Inside a popup that holds a text field (Combo with search, `SearchPopup`, the apps popup), the field takes the caret when the popup opens and shows only its 1 px `accent` editing border (DS-8.2), never the ring, whether the popup was opened by mouse or by key. Arrow keys move the list highlight (`bg-subtle-selected`), not a ring. Check: open the language list by mouse and by keyboard: zero ring pixels inside the popup, one 1 px border on the field.

## 5. States: hover, pressed, selected

| Surface | Rest | Hover | Pressed | Selected |
|---|---|---|---|---|
| framed (secondary Button, fields) | `bg-control` | `bg-control-hover` | `bg-control-pressed` | n/a |
| soft (soft Button, IconButton) | `bg-soft` | `bg-soft-hover` | `bg-soft-pressed` | `accent-tint` (a toggled IconButton) |
| list row, rail item | transparent | `bg-subtle-hover` | `bg-subtle-selected` | `accent-tint` |
| sub-tab | no fill, `text-secondary` | no fill, `text-primary` | no fill, `text-primary` | `text-primary` + 2 px `accent` underline |
| card (ClickCard, Tile) | `bg-card` | `bg-control-hover` | `bg-card-pressed` (Tile: = hover) | `accent-tint` |
| primary Button | `accent` | `accent-hover` | `accent-pressed` | n/a |

New tokens: `bg-soft` = #FFFFFF at 8 % (dark) / #1C1B19 at 6 % (light); `bg-soft-hover` 12 % / 10 %; `bg-soft-pressed` 6 % / 4 %.

Motion tokens: `motion-quick` 100 ms (hover, pressed, selected, knob), `motion-open` 160 ms (popup, tooltip). Both read 0 under `Shell.reduced-motion`.

- DS-5.1 Every clickable element has a hover look and `mouse-cursor: pointer` (UI rule 36). Check: `rg -A3 "TouchArea" crates/oxi-settings/ui` shows `mouse-cursor: pointer` on every TouchArea with a `clicked`.
- DS-5.2 Inert things have no hover (UI rule 37).
- DS-5.3 Every fill, border, text colour and opacity change of a state animates with `motion-quick`, `easing: ease-out`. Check: `rg "animate" crates/oxi-settings/ui`: each reads `Theme.motion-*`.
- DS-5.4 No scale, bounce or overshoot (UI rule 38).
- DS-5.5 Pressing an already selected item keeps the selected fill: no flash. Selection moves as a cross-fade.
- DS-5.6 A state changes colour only. Hover, press, focus, selection, enable and disable never change a width, height, padding, border width, font weight or icon size (section 25).

## 6. IconButton [M33]

Tokens: `control-height` 32 (box), `icon-size-small` 16 (glyph), `radius-control`. Component: `IconButton` (button.slint).

- DS-6.1 One size: a 32x32 box with the soft surface (section 5): the fill is visible at rest, so the hit area is seen. Check: zoomed shot of History's and Hotkey's icon buttons at rest: a filled 32x32 box. Today: transparent at rest.
- DS-6.2 The glyph is 16 px at whole-pixel x and y, its centre within 0.5 px of the box centre.
- DS-6.3 Its `label` is the tooltip and the accessible name; every IconButton has one. A destructive one turns `error-fill` + `error-text` on hover (UI rule 16).
- DS-6.4 A field's own adornments (the clear "x", the Combo chevron) belong to the field, `adornment-size` 24 square, inset `space-1`, with a hover fill. They are not IconButtons.
- DS-6.5 A toggled IconButton (the Hotkey pencil while its panel is open) shows `accent-tint` fill and an `accent-text` glyph.

## 7. Button [M33, M34]

Component: `Button` (button.slint). Height `control-height`, padding `space-3`, icon `icon-size-small`, icon to label `pair-gap`, min width `control-side` (88); the width formula is in DS-3.3.

| Kind | Rest | Use |
|---|---|---|
| `primary` | `accent` fill, `on-accent` ink | the one main action of a card or popup (Apply, Download, Run) |
| `secondary` | `bg-control` fill, 1 px `stroke-control` border | the default |
| `soft` (new; `subtle` is its deprecated alias) | `bg-soft` fill, no border | a low-weight action beside content: Open file, History folder, Reset |
| `danger` | as `secondary`; hover and pressed `error-fill` + `error-text` | Delete, Remove, Cancel of a running job, Stop |
| IconButton | `bg-soft` fill, 32x32 | section 6 |

- DS-7.1 Icon and label are one group, centred in the button; the gap is exactly 8 px (`pair-gap`). Check: gallery: icon right edge to label left edge = 8 px on every button with an icon.
- DS-7.2 Icon centre y = label centre y = button centre y, within 0.5 px. Check: gallery geometry.
- DS-7.3 Buttons that swap in one slot (Download, Resume, Cancel, Delete on a model; Record, Stop) share one width, from a token that fits the widest label. The slot never changes width.
- DS-7.4 No label carries "..." or "…". Browse is a Button with its glyph of section 24 (`browse-folder`), like every other button.
- DS-7.5 Every button has a fill at rest. No transparent, text-only button exists. Check: `rg "ButtonKind.subtle" crates/oxi-settings/ui/pages` prints nothing after wave B; a shot of Home, System and Text shows a box behind History folder, Open and Open file. Today: those three are bare text.
- DS-7.6 Allowed without a rest fill, because they are not buttons: sub-tabs, rail items, list rows, tiles and cards (their own surface), a field's adornments, the `InfoTip`, a link inside running text (About). Nothing else.
- DS-7.7 At most one `primary` button is visible in a card or popup. A page may have none. A destructive action is never `primary` (DS-21.6).
- DS-7.8 When a button carries an icon: a labelled Button has an icon if and only if its action is in the table of section 24; then it always has it, on every screen. Flow words never have one: Apply, Cancel, Save, OK, Done, Run, Test, Reset, Add (as a confirm), Keep. Check: `rg -B4 "icon:" crates/oxi-settings/ui/pages` against the table. Today: Open has an icon and Delete has none; Save has one.
- DS-7.9 Action glyphs are for actions only: a row, card or group title never uses a glyph of section 24 as its own leading icon, so a button's icon never repeats its row's icon. Check: read each page shot: no glyph appears twice in one row.

## 8. Inputs

Components: `Combo`, `TextField`, `NumberField`, `TextArea`, `SearchField`, `PathField` (section 13).

- DS-8.1 One shape: height `control-height` (TextArea: `editor-min-height`), radius `radius-control`, fill `bg-control`, 1 px `stroke-control` border with a `stroke-control-bottom` bottom edge, text padding `space-3`, text `body-size`, placeholder `text-secondary`. Check: gallery row of all five side by side.
- DS-8.2 Hover: fill `bg-control-hover`. A text field being edited (any focus reason) shows a 1 px `accent` border, the same 1 px as at rest (colour only). Keyboard focus adds the ring (section 4), except inside a popup (DS-4.6).
- DS-8.3 Disabled and locked inputs use `text-disabled` ink on the rest fill. No opacity on text.
- DS-8.4 Selected text: `accent` fill, `on-accent` ink.
- DS-8.5 `SearchField` has the `magnifying-glass` glyph (16 px) at its left, inset `space-2`, and the clear "x" adornment at its right only while it holds text; the text box keeps its width either way.
- DS-8.6 Combo popup geometry: x = field x, width = field width, top = field bottom + 4 (or bottom = field top - 4 when there is no room below); rows 32 px, list padding 4 px top and bottom, row text inset 12 px = the field's text inset; at most `combo-visible-rows` (6) rows, then the thin scrollbar (section 18). The popup never covers its field and never changes size while open. Check: shot open and closed: the field's pixels match, the list's text x = the field's text x.
- DS-8.7 An invalid field (a value the app refuses) keeps its size and shows two things: a 1 px `error-text` border, and the `error` glyph in `error-text` in its row's `WarningSlot` (UI rule 2) with the reason as the tooltip of the glyph and of the field. A field outside a Row shows the glyph in its own right adornment slot (DS-6.4). The border's colour alone never says "invalid": the editing border is the accent, which is red by default (DS-21.1). Check: gallery: an invalid NumberField and hex field in both themes show the glyph; no neighbour moves.

## 9. Badge

Component: `Badge` (feedback.slint). Height `badge-height` 20, padding across `space-2`, radius `radius-control`, `caption-size` text. Kinds: `neutral` (`bg-inset`, `text-secondary`), `accent` (`accent-tint`, `accent-text`), `success` (`success-tint`, `success-text`).

- DS-9.1 Text is centred within 0.5 px.
- DS-9.2 Ink on fill is at least 4.5:1 for every kind, in both themes, on `bg-card` and on a hovered card.
- DS-9.3 Every badge in one column has the same height; badges never wrap.

## 10. Switch

Component: `Switch` and `ToggleRow`. Box `control-height`, track 40 x 20, `radius-round`, "On"/"Off" caption left of the track in a fixed 24 px slot. On: track `accent`, knob `on-accent`. Off: track `bg-inset` with a 1 px `stroke-keycap` border, knob `text-secondary`.

- DS-10.1 The track is centred in the Switch box: 6 px above and below.
- DS-10.2 In a row, the track centre y equals the row centre y within 0.5 px, and the track's right edge is the column's right edge (DS-3.3).

## 11. Info tip "i" and hints [M27]

Component: `InfoTip` (base.slint). Glyph `info` 16 px in a 20x20 hover area, `radius-round`. The text is a `Tip`: at most two sentences and 140 characters; sentence one says what the setting does, sentence two when to change it. Tooltips open after the existing Tip delay, are at most `tooltip-max-width` (320) wide, and never take focus.

- DS-11.1 No explanatory text lies loose on a page (UI rule 35). Check: `rg "Caption|Note" crates/oxi-settings/ui/pages` finds no sentence that explains instead of naming a value.
- DS-11.2 It opens on hover and on keyboard focus; Enter or Space toggles it.
- DS-11.3 Slots: `Row.info` (exists) and `Group.info` (new) put the InfoTip right after the title, `pair-gap` from it, centred on the title's first line. An InfoTip never sits beside the control. Check: every "i" in a shot follows a title. Today: on Test, "New recording", the "i" sits left of the Record button.
- DS-11.4 A hint is required on: (a) every setting that changes the dictated text; (b) every number (its unit and what a larger value does); (c) every setting that acts outside the app (clipboard, Win+V, autostart, files on disk, network); (d) every disabled control (why, UI rule 31); (e) every IconButton (its label); (f) every text that can be cut with an ellipsis (the full text); (g) every stat card and chart (what is counted); (h) every "Auto" choice (what Auto picks now); (i) Test's recording row: "Recordings here use the microphone settings from Audio." [M26]. A plain switch whose title says everything needs none. Check: the per-page list in each wave B task; the blind test (e01) reports any control a naive user could not explain.
- DS-11.5 A button's own tooltip replaces an InfoTip beside it: never both for one fact. Today: Home has "History folder" plus a separate "i".

## 12. Empty state

Component: `EmptyState` (feedback.slint). Glyph 32 px in `text-secondary`, one line in `body-size` `text-primary`, an optional Button `space-3` below.

- DS-12.1 It sits centred in the same box the full list takes: the page around it does not move.
- DS-12.2 One line says what is missing; the Button does the fix. Any longer hint is an InfoTip.

## 13. Paths

Component: `PathField` (fields.slint); elision is `elide_path` in oxi-core.

- DS-13.1 A path shows its file or folder name whole, as many trailing folders as fit, and "…" for the rest.
- DS-13.2 The full path shows in the tooltip, and as selectable text while the field has focus.
- DS-13.3 An empty value shows its default in `text-secondary` ("Default folder"). Open folder is an IconButton with `folder-open` and a tooltip naming the folder.
- DS-13.4 Every file or folder dialog starts at the current value's folder, else the nearest existing parent, else the default folder. Never at Documents by default.

## 14. Header row [M13]

Component: `HeaderRow` (layout.slint), the one header of every page: the old `PageHeader` is deleted. It draws either a title (`title`) or tabs (`SubTabs` as its child) with the same geometry, so switching pages moves nothing.

Geometry (y from the header's top, x from the content's left edge = the cards' left edge):

| Part | Value |
|---|---|
| height | `header-row-height` 40; its centre y = the first rail item's centre y (UI rule 8) |
| title, or first tab label | text left x = 0; text centre y = 20; `body-size`, `weight-strong` |
| other tabs | label to label gap `tab-gap` (new, 24); `text-secondary` at rest, `text-primary` on hover and when selected; no fill |
| tab width | each tab reserves its label's `weight-strong` width, so selecting a tab moves no neighbour |
| tab count ("Per-app rules 1") | `caption-size`, `text-secondary`, `tab-count-gap` 6 right of the label |
| selected underline | 2 px (`tab-underline`, was 3) `accent`, exactly the label's width, flush with the header's bottom |
| bottom line | 1 px `divider` from x = 0 to the content's right edge |
| `StatusChip` | box right edge = content right edge; box centre y = 20 |
| tab hit area | the label plus 12 px left and right, the full 40 px height (invisible) |

- DS-14.1 No page title and no "OxiYapper" line in the rail (UI rule 34). Check: `rg "PageHeader" crates/oxi-settings/ui` prints nothing.
- DS-14.2 The bottom line spans the whole content width, from the cards' left edge to their right edge, on title pages and on tab pages. Check: shot of Test and Text: the line's ends match the card edges to 0 px.
- DS-14.3 The header row's centre y equals the first rail item's centre y, within 1 px (UI rule 8). Check: shot of each page.
- DS-14.4 StatusChip: text, then a dot (8 px, `radius-round`) `pair-gap` right of it. Dot colour: `success-text` running, `warning-text` attention, `error-text` down; the chip's text names the state, so the dot's colour is never the only cue (DS-21.1). Hover and pointer only when the status has an action.
- DS-14.5 Same geometry on title pages and tab pages. Check: in shots of Home (title) and Model (tabs): first label left x, label centre y, chip centre y, bottom line y and first content y each match to 0 px. Today: label x 236 against 248, label and chip centre y 29 against 26.
- DS-14.6 A title is inert (no hover, no underline). Tabs follow section 5's sub-tab row.
- DS-14.7 Toasts show in the header row, between the tabs and the StatusChip (DS-25.9).

## 15. Card grid

Component: `CardGrid` (gallery.slint), shared by the apps popup, the model gallery and the sounds. Tokens: `tile-min-width` 160, `tile-max-width` (new, 320), `tile-height` 104, `tile-height-short` 76, `tile-gap`, `icon-size-large` 32.

- DS-15.1 One anatomy: glyph or picture top-left, title (`body-size` strong, one line, elided with tooltip), caption (`caption-size`), a reserved 20x20 top-right slot for a badge or check.
- DS-15.2 Columns = as many `tile-min-width` columns as fit; cells share the width equally up to `tile-max-width`, then pack left (old backlog: one tile stretched over 2560 px); the grid takes the free height and scrolls inside itself (UI rule 26).
- DS-15.3 States follow section 5's card row; selected adds the `CheckBadge` (`accent` fill, `on-accent` check).
- DS-15.4 Icons in any box sit at equal padding on all four sides, within 0.5 px.

## 16. Icon and label pairs

- DS-16.1 In every icon+label pair the icon centre y equals the label's line-box centre y within 0.5 px, and the gap is `pair-gap`.

## 17. "More" affordance

- DS-17.1 "More choices" is a glyph, never typed dots: `caret-down` for a trailing segment or list that opens more, `dots-three` for an overflow menu. Each has a tooltip.

## 18. Scrollbar [M5]

Component: `ScrollBox` (base.slint) with its `Bar`: the only scroller in the app. `ScrollView` (layout.slint) stays as the page-level wrapper built on it. No std `ScrollView`, `ListView` scrollbar or bare `Flickable` anywhere else.

Tokens: `scrollbar-width` 6 (regular), `scrollbar-width-thin` (new, 4), `scrollbar-gutter` 12, `scrollbar-min-thumb` 24. Properties (new): `thin: bool`, `inset: length`, `reserve-gutter: bool`.

- DS-18.1 States: the thumb keeps its rect in every state; only its colour changes: `text-disabled` at rest, `text-secondary` on hover and drag, `motion-quick`. Radius `radius-small`; track transparent. Drag moves it; a click on the track pages toward the click. Check: shot at rest and on hover: thumb x and width match.
- DS-18.2 The thumb is centred in the 12 px gutter. Content never lies under the gutter: it leaves exactly `scrollbar-gutter` while the box overflows (a list of DS-18.6 leaves it always). Check: rightmost content pixel < gutter left x.
- DS-18.3 One component. Check: `rg "ListView|ScrollView \{|Flickable" crates/oxi-settings/ui --glob '!gallery.slint'` shows only `ScrollBox`'s own Flickable and `ScrollView`'s definition and uses. Today: `SearchPopup` (search_popup.slint:86) uses std `ListView` with the fluent bar.
- DS-18.4 Inset: the bar's track starts `inset` below the box's top and ends `inset` above its bottom, and `inset` is the container's own padding (4 in a Combo list and a list card, 8 in a popup, 12 in a card, 0 for a page). The thumb never touches the container's edge when the content does not. Check: thumb top y at scroll 0 = first item's top y, to 0 px, in History, the apps popup, a Combo list and Per-app rules. Today: the bar runs the box's full height while the items are padded.
- DS-18.5 Only thickness varies: `thin` (4 px) inside a control (Combo list, SearchPopup, TextArea), regular (6 px) everywhere else. Gutter, colours, radius and behaviour are the same.
- DS-18.6 Gutter and jumps: a list whose length changes while shown (History, Per-app rules, apps popup, Test takes and results) sets `reserve-gutter`, so the content width is the same with and without the bar. A fixed-content page reserves the gutter only while it overflows. Check: add items until History overflows: no row changes width.

## 19. Neutral base, brand and icons

Brand: concept C "white minimal". Ink on paper is the neutral base; the user's accent (section 20) is added on top, red by default: the colour of the logo's record dot. The logo mark is an "o" ring with a record dot, drawn once in `crates/oxi-platform/src/draw.rs`. Inside the app (the About mark, the settings window's icon) the dot is drawn in the accent and follows the user's pick; the exe icon file `assets/oxi.ico` keeps its current dot (DS-20.8).

Neutral palette (tokens.slint; light / dark), unchanged:

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
| `text-primary` | #1C1B19 | #F4F2EE |
| `text-secondary` | #5C5850 | #B5B1AA |
| `text-disabled` | #A39F97 | #75726D |
| `info-fill` | #F2F0EB | #292929 |

Status inks and fills (`error-*`, `warning-*`, `success-*`) keep their values. The `recording` token (#E5484D) is deleted: recording shows the accent (section 21).

- DS-19.1 (meaning changed 2026-11-11; was "the accent is the ink") The accent is the user's colour of section 20, red by default. `text-primary` is the ink; no accent token is defined as a text token.
- DS-19.2 (meaning changed 2026-11-11; was "red means recording only") Red is the default accent, so red by itself means nothing: recording shows the accent, and an error or an overload is told by its mark and words as well as its colour (DS-21.1).
- DS-19.3 Colour lives only in palette tokens, in both themes. Check: `rg "#[0-9A-Fa-f]{6}" crates/oxi-settings/ui --glob '!tokens.slint'` prints nothing (logo assets exempt).
- DS-19.4 Every pair of text and fill passes 4.5:1; every non-text mark passes 3:1. Check: the gallery contrast swatches.

Icons: Phosphor, Bold weight, `@phosphor-icons/core@2.1.1` (MIT), vendored and listed in `crates/oxi-settings/ui/icons/SOURCE.md`; read only through the `Icons` global.

- DS-19.5 Sizes: 16 px in buttons, fields and InfoTip; 20 px in rows, group titles and the rail; 32 px in tiles and empty states.
- DS-19.6 One glyph per meaning: section 24.
- DS-19.7 Only the Bold weight, except a selected tile's or rail item's glyph, which uses the Fill weight. No hand-made icons except the logo mark.

## 20. Accent [M1, M17]

The user picks one seed colour: a preset or a custom hex. Everything else is derived from the seed by one pure function in oxi-core (pure colour maths, with tests), per theme. The settings window sets the results into `Theme` at run time; the overlay derives its own (section 21). Nothing in `ui/` computes a colour.

Presets (seed, sRGB), in swatch order. Red is the brand default (owner, 2026-11-11): it is the logo's record dot. A fresh config and a missing or invalid value use it.

| # | Name | Seed | | # | Name | Seed |
|---|---|---|---|---|---|---|
| 1 | Red (default) | #D93A3F | | 6 | Cyan | #00A2C7 |
| 2 | Orange | #F76B15 | | 7 | Blue | #0090FF |
| 3 | Amber | #FFC53D | | 8 | Iris | #5B5BD6 |
| 4 | Green | #46A758 | | 9 | Purple | #8E4EC6 |
| 5 | Teal | #12A594 | | 10 | Pink | #D6409F |

Custom: `#RRGGBB` typed by the user. The accent can be any hue, red, amber and green included, so no state anywhere is told by its colour alone (DS-21.1).

Derivation. `contrast` is WCAG 2 relative-luminance contrast. `mix(a, b, t)` is per sRGB channel `floor(a + (b - a) * t + 0.5)`. `toward(set)` is whichever of #FFFFFF and #000000 has the higher lowest contrast against the colours of `set` (white on a tie): white for the dark theme's surfaces and for the pill, black for the light theme's surfaces, and whichever the taskbar needs (DS-21.4). `surfaces` are the theme's opaque fills: `bg-page`, `bg-card`, `bg-control`, `bg-control-hover`, `bg-control-pressed`, `bg-card-pressed`. `lift(seed, set, need)` = `mix(seed, toward(set), 0.04 * k)` for the smallest k in 0..=25 whose contrast against every colour of `set` is at least `need`. It always ends: at k = 25 the result is `toward(set)` itself, which has at least 4.58:1 (the square root of 21) against any one colour and over 13:1 against either theme's surfaces. A `set` never mixes light and dark colours.

| Token | Formula | Guarantee |
|---|---|---|
| `accent` | `lift(seed, surfaces, 3.0)` | 3:1 as a fill or mark on every surface |
| `accent-tint` (= `accent-badge`) | `mix(bg-card, accent, 0.16)` dark, `0.12` light | `text-primary` and `text-secondary` keep 4.5:1 on it |
| `accent-text` | `lift(seed, surfaces + accent-tint, 4.5)` | 4.5:1 as text or glyph on every surface and on the tint |
| `on-accent` | #FFFFFF if `contrast(#FFFFFF, accent)` >= 4.5, else #000000 | always >= 4.5:1 on `accent` |
| `accent-hover` / `accent-pressed` | `mix(accent, dir, 0.10)` / `mix(accent, dir, 0.20)`. `dir` is `away`: #000000 when `on-accent` is white, else #FFFFFF. But when `contrast(mix(accent, away, 0.20), accent)` < 1.15 (the accent already sits at that end: a #000000 seed in the light theme, a #FFFFFF seed in the dark one) and `contrast(on-accent, mix(accent, on-accent, 0.20))` >= 4.5, `dir` is `on-accent` | `on-accent` keeps >= 4.5:1 on both; both always differ from `accent` |
| `focus-ring`, the edited field's border | = `accent` | 3:1 |
| heat scale 1..4 | `mix(bg-card, accent, 0.25 / 0.45 / 0.70 / 1.00)`; level 0 is `bg-inset` | level 4 is 3:1 on the card |

Default values in tokens.slint (Red), which are also the first test vectors:

| Token | Dark | Light |
|---|---|---|
| `accent` | #D93A3F (k = 0) | #D93A3F (k = 0) |
| `accent-hover` / `accent-pressed` | #C33439 / #AE2E32 | #C33439 / #AE2E32 |
| `on-accent` | #FFFFFF (4.54:1) | #FFFFFF (4.54:1) |
| `accent-text` | #E47175 (k = 7) | #BF3337 (k = 3) |
| `accent-tint` | #3D2324 | #F8E4E2 |
| heat scale 1..4 | #4E2627 / #732B2D / #A13235 / #D93A3F | #F3CBCA / #ECA4A5 / #E47477 / #D93A3F |

White on #D93A3F is 4.54:1, so the ink on the default accent is white. (The seed was #E5484D, where white is 3.91:1; the coordinator moved it to #D93A3F, 2026-11-11, so the default needs no black ink.)

More vectors for the oxi-core tests. Iris dark: `accent` #6868D9 (k = 2), hover #5E5EC3, pressed #5353AE, `on-accent` #FFFFFF (4.59:1), `accent-text` #8F8FE3 (k = 8), tint #2B2B3D. Iris light: `accent` #5B5BD6 (k = 0), hover #5252C1, pressed #4949AB, `on-accent` #FFFFFF (5.37:1), `accent-text` #5757CD (k = 1), tint #E9E8F4. Amber light: `accent` #A37E27 (k = 9), `accent-text` #856620 (k = 12), `on-accent` #000000. Amber dark: `accent` #FFC53D, `dir` = `on-accent`: hover #E6B137, pressed #CC9E31. Blue light: `accent` #008AF5 (k = 1), `on-accent` #000000. Seed #000000 dark: `accent` #7A7A7A (k = 12), `accent-text` #999999 (k = 15). Seed #000000 light: `accent` #000000, `dir` = `on-accent`: hover #1A1A1A, pressed #333333. Seed #FFFFFF dark: `accent` #FFFFFF, hover #E6E6E6, pressed #CCCCCC.

Computed over the 10 presets, the seeds #FFFFFF, #000000, #000080, #FFFF00, #808080, #0A0A0A and #F5F5F5, and a grid of 4096 seeds (16 steps per channel), all in both themes, the lowest results are: `accent` on a surface 3.00, `accent-text` 4.50, `on-accent` on `accent` 4.50, on `accent-hover` 4.94 and on `accent-pressed` 4.50, `text-primary` on the tint 8.85, `text-secondary` on the tint 4.63. Hover never equals the accent (the weakest step is seed #FF00FF, dark: 1.02:1, the green channel still moves by 26).

- DS-20.1 A custom hex that fails contrast is not refused: `lift` moves it toward white (dark) or black (light) in 4 % steps until it passes, and the UI shows the adjusted colour. The saved value stays the seed the user typed. Check: oxi-core tests for the vectors above, and a property test over the 4096-seed grid for the five guarantees of the table; type #000000 in the dark theme: the switch tracks turn #7A7A7A.
- DS-20.2 A custom hex close to the error colour is allowed, with a warning (owner, 2026-11-11). Close means: HSL hue in 345°..360° or 0°..15° and saturation at least 0.45, and either lightness in 0.25..0.75 or a contrast under 1.5:1 against `error-text` of either theme (#FF99A4 or #B9291A), which catches pale tints such as #FF99A4 (lightness 0.80). The value is applied and saved like any other, and the Accent row's `WarningSlot` (UI rule 2) shows the `warning` glyph with the tooltip "Close to the error colour. Errors keep their icon and text." The presets never warn (the default Red is in that range by design), and a hex equal to a preset's seed selects that preset. A string that is not `#RRGGBB` or `RRGGBB` (case-insensitive) is refused: the field is invalid (DS-8.7) with "Use six hex digits, like #D93A3F", and nothing is saved. Check: #CC0000 and #FF99A4 apply and show the warning; #F76B15 (hue 23°) and #D6409F (hue 322°) apply without it; "12345" is refused.
- DS-20.3 The accent tokens are `in-out` properties of `Theme` set from Rust, with the Red values as defaults; an accent change recolours the open window without a restart. Never `Palette.accent` (UI rule 15: that name is the Windows accent). Check: `rg "Palette\.accent" crates/oxi-settings/ui` prints nothing.
- DS-20.4 Where the accent goes. `accent` fill or mark: the primary Button; the Switch's "on" track; the Slider's filled rail and its thumb dot; the selected Radio; the `CheckBadge`; the selected Segmented segment (ink `on-accent`); the selected sub-tab's underline; the selected rail item's 3x16 pill; ProgressBar fill; the Meter's bars; the heatmap scale and trend bars; the focus ring; the edited field's 1 px border; selected text; the logo's dot (DS-20.8); the 6 px kept-draft dot on the Hotkey pencil (28.3). `accent-text`: links, the selected rail item's glyph, the `accent` Badge's ink, a toggled IconButton's glyph, the "new" keycaps of the hotkey editor. `accent-tint`: the fill of a selected tile, card, list row, rail item and `accent` Badge, and the 600 ms flash of a rule found as a duplicate (28.1).
- DS-20.5 Where it must not go: body text and titles; row, group and unselected rail icons; any border, card or control fill at rest; dividers; secondary, soft and danger buttons; keycaps at rest; tooltips and toasts; the scrollbar; the InfoTip glyph; disabled controls (they use `text-disabled` and `bg-inset`); success, warning and error states (their own tokens and marks, section 21); the StatusChip dot. Check: in a shot with the Blue preset, count the blue things on each page: every one is in DS-20.4's list.
- DS-20.6 Selected means the tint fill plus an accent mark (check, pill, underline), never a coloured outline (UI rule 17).
- DS-20.7 Accent picker (System, General, in the Appearance card under the theme tiles): a Row "Accent" of the standard height. Its control block packs to the column's right edge (DS-3.7): ten 20 px round swatches 6 px apart (10 x 20 + 9 x 6 = 254), `control-gap`, a hex `TextField` of `control-side` (88) showing the current seed, 350 px in all, so the field sits in the side slot. A swatch is filled with that preset's derived `accent`; its hit area is 26 x 32 (half a gap each side, the full control height); the selected one holds a 12 px `check` glyph in its `on-accent` (a custom seed selects none); hover adds a 2 px ring of `stroke-keycap` 2 px outside the swatch (28 px across, inside the row: no size change); each swatch's tooltip is the preset's name. Picking a swatch or pressing Enter on a valid hex applies and saves at once (live preview is the application itself). Tab order: swatches (arrow keys move, Space picks), then the field.
- DS-20.8 The logo's dot follows the accent inside the app (owner, 2026-11-11): the About mark draws its ring in ink and its dot in `accent`; the settings window's icon draws its dot as `lift(seed, {PAPER #FAF7F1}, 3.0)`; both are redrawn when the accent changes. The default dot is #D93A3F, the default Red seed (task a05 updates `crates/oxi-platform/src/draw.rs` and the window and About logo to it). The exe icon file `assets/oxi.ico`, the installer and shortcuts are not redrawn and keep their current dot (a file cannot follow a setting); this is accepted, and the difference from #D93A3F is small. Check: pick Blue: the About mark's dot and the title-bar icon's dot turn blue; `assets/oxi.ico` is unchanged.

## 21. State marks and colours [M1, M11]

The accent is red by default and can be any hue, so a state's colour can coincide with the accent. With the default accent in the light theme `accent-text` (#BF3337) and `error-text` (#B9291A) are 1.10:1 apart and `accent-tint` and `error-fill` 1.05:1: the eye cannot tell them apart. So a state has three carriers: a mark (a glyph or a shape), words, and a colour. The colour supports the other two and is never needed to read the state. This replaces the earlier "red only for errors and overload".

| Meaning | Mark | Words | Colour, settings | Colour, overlay and tray |
|---|---|---|---|---|
| working: recording, transcribing, pasting, typing, downloading | the live thing itself: wave bars, the progress fill | the job's name, a percent or a timer | `accent` | `pill_accent` = `lift(seed, {SURFACE #1E1F24}, 3.0)` (Red: #D93A3F, 3.62:1; Iris: #5B5BD6, 3.06:1) |
| done, ready, running | `done`: a check in a circle | "Done" or the result | `success-text` / `success-tint` | `OK` #30A46C (5.21:1 on the pill) |
| attention, a notice that needs no action now | `warning`: a triangle | the notice | `warning-text` / `warning-fill` | `WARN` #F7B01B (8.76:1) |
| error, failed | `error`: an x in a circle | the short reason | `error-text` / `error-fill` | `ERROR` #FF99A4 (was #C86400; 8.10:1) |
| microphone overload | the clip line (DS-21.3) | "Too loud" | `error-text` | `ERROR` |
| neutral information | `info` | the text | `text-secondary` | `INFO` #8B8D98 (4.98:1) |

The three state glyphs have three outlines (a circle with a check, a triangle, a circle with an x), so they read in greyscale. `ERROR` is the dark theme's `error-text`: a pale red, 2.24:1 away from the default `pill_accent`, and never shown without its mark.

- DS-21.1 No state is told by colour alone. Every state shows its mark, and its words are on screen or in the mark's tooltip. An `error-fill` surface always holds the `error` glyph (or is the hover of a danger control, DS-21.6); an `accent-tint` surface always holds an accent mark (check, pill, underline). Check: a greyscale copy of every state shot (settings pages, pill, tray) still tells each state; `rg "error-text|error-fill" crates/oxi-settings/ui/pages`: every use sits beside `Icons.error`, the clip line or a danger control.
- DS-21.2 Overlay states. Each state has one mark and one label; the surface (`SURFACE`), the text (`TEXT` #EEEFF2) and the pill's size are the same in all of them (UI rules 1, 11). `RECORDING`, `RECORDING_RIM`, `BUSY` and `CLIP` in `oxi_core::overlay::look::palette` go away: the wave and the rim use `pill_accent` (rim: `mix(SURFACE, pill_accent, 0.5)`, Red: #7C2D32). Check: `rg "RECORDING|CLIP|BUSY" crates/oxi-core/src/overlay` finds no colour constant; a pill shot of each state, in colour and in greyscale.

| State | Mark | Colour | Label |
|---|---|---|---|
| recording | live wave bars; the microphone or lock glyph | `pill_accent` | the recording label or the timer |
| recording, time nearly up | the wave, and the `warning` glyph in the glyph slot | `WARN` | the state's label |
| recording, overloaded | the wave under the clip line | `ERROR` | "Too loud", in the same label slot |
| transcribing | progress fill (section 26) | `pill_accent` | "Transcribing 42 %" |
| pasting / typing | progress fill | `pill_accent` | "Pasting" / "Typing 42 %" |
| done | `done` glyph | `OK` | "Done" |
| error | `error` glyph | `ERROR` | the short reason |
| notice | `warning` glyph | `WARN` | the notice |

- DS-21.3 Overload is a level, not a single clipped sample: a 20 ms frame is hot when its peak is at or above -1.0 dBFS (0.891 of full scale); the state is "overloaded" from the moment hot frames add up to 60 ms within the last 300 ms, and for 600 ms after the last hot frame. The rule is one pure function in oxi-core with tests, used by the overlay wave and by the Audio page's `Meter`. While overloaded both show the same three things: (1) the clip line: a 2 px line across the full width of the wave or Meter, at its top edge, inside its box (nothing moves); (2) the bars and the line in the error colour (`ERROR` in the pill, `error-text` in the Meter) instead of the accent; (3) the words: the pill's label reads "Too loud"; the Level row's `WarningSlot` shows the `warning` glyph with the tooltip "Too loud: lower the gain." The clip line is the mark: it is there with any accent and in greyscale. Check: oxi-core tests: one hot frame is not overload; three in 300 ms are; the state clears 600 ms after the last; a greyscale pill shot of normal and overloaded recording differ by the line and the label.
- DS-21.4 The tray's live states use the same table. Every tray colour C is drawn as `lift(C, {the taskbar colour under it}, 3.0)`; `toward` is black on a light taskbar and white on a dark one, so the lift always ends. On a #EEEEEE taskbar: Red #D93A3F (k = 0, 3.91:1), Amber #A37E27 (k = 9, 3.25:1), `ERROR` #B86E76 (k = 7, 3.27:1), `OK` #2C9763 (k = 2, 3.17:1). On #202020: Red #D93A3F (3.59:1), Iris #5B5BD6 (3.04:1), `ERROR` #FF99A4 (8.03:1), `OK` #30A46C (5.16:1). The marks differ by shape and place, not only by colour: recording is the whole mark drawn in the accent; the error badge is an `x-circle` at the bottom right (the triangle is the warning mark); the update badge is a round dot at the top right; the icon's tooltip names the state. Check: oxi-core tests for the eight values above; tray shots on a light and a dark taskbar, in greyscale too.
- DS-21.5 In the settings window a running job (Test recording, History "Transcribe again", a model download) shows the accent; its success shows `success-*` with the `done` glyph, its failure `error-*` with the `error` glyph and the reason. The `recording` token is deleted (the earlier draft's `overload` token is not added): the Meter's overload uses `error-text`. Check: `rg "recording:" crates/oxi-settings/ui/theme/tokens.slint` prints nothing.
- DS-21.6 Danger. A destructive or stopping control (Delete, Remove, Stop, Cancel of a running job) is told by its glyph (`delete`, `stop`) or its label and by the `danger` kind's `error-*` hover (UI rule 16), never by a red fill at rest: with the default accent a red fill at rest is the primary button. A destructive confirm's Delete is `danger`, never `primary`. Check: `rg -B3 "Icons.delete" crates/oxi-settings/ui/pages` shows no `ButtonKind.primary`.

## 22. Visual hierarchy [M17]

A page must read at a glance, without reading every label.

- DS-22.1 Five rules: (1) one accent-filled action at most per card (DS-7.7); (2) the accent has four jobs: the main action; what is selected or on; live work or data (a progress fill, the Meter, the heat scale, trend bars); and markers of place or identity (the focus ring, the edited field's border, links, the logo's dot, the kept-draft dot). DS-20.4 is the full list and nothing outside it is accent-coloured; (3) three text levels only: group title `body-size` strong `text-primary`, row title `body-size` `text-primary`, caption or secondary value `caption-size` `text-secondary`; stat values use `subtitle-size` strong; (4) a state is told by its mark and its words; its colour only supports them (DS-21.1); (5) the same thing has the same glyph, position and colour on every page (sections 23, 24). Check: the blind test (e01) names the selected item and the main action of each page within 3 seconds.
- DS-22.2 No page shows only neutrals: every page has at least one selected or "on" mark in the accent (the rail pill at least). Check: every page shot holds `accent` pixels.

## 23. Action placement [M22, M25, M34]

An action bar (a card's header or footer, a list row's trailing controls, a popup's footer) has two ends.

- DS-23.1 Start (left): the media action first (Play / Stop, Record), then the object's main action if it has one. Check: History detail, Test take card: Play is the leftmost control of its bar.
- DS-23.2 End (right), in this order from left to right: Copy, Open / Show in folder, Edit, Retry (Transcribe again), other utilities, and Delete last. Delete (Remove, trash) is always the rightmost control of its bar; in a card with a header bar it is at the top right. Check: `rg -A30 "Icons.delete" crates/oxi-settings/ui/pages`: nothing follows it in its layout; every shot with a trash glyph has it at the right end. Today: on the Test take card Delete sits mid-row after "Recording 0:01".
- DS-23.3 A confirm footer is right-aligned: `[Cancel][primary]`, the primary at the right end. A destructive confirm is `[Cancel][Delete]`, Delete of kind `danger` (DS-21.6).
- DS-23.4 Controls in a bar are `control-gap` (8) apart; the start and end groups are separated by the stretch, never by a fixed gap.
- DS-23.5 The same action sits at the same end on every screen, and a slot that can be empty (Delete on a model that is not downloaded) keeps its width (DS-7.3).

## 24. Icon semantics [M22, M34]

One glyph per meaning, on every screen. `Icons` gets one constant per row (name = the "Constant" column); pages use only these for actions. "Vendor" marks a Phosphor Bold file the primitives task adds to `ui/icons/` and `SOURCE.md`.

| Meaning | Constant | Phosphor (Bold) | Note |
|---|---|---|---|
| copy to the clipboard | `copy` | `copy` | vendor. Today: `clipboard-text`, which stays for the Paste mode tile only |
| open a folder in Explorer; show a file in its folder | `folder-open` | `folder-open` | both are "the folder". Today: History uses `path` for it |
| choose a folder (dialog) | `browse-folder` | `folder` | vendor |
| choose a file (dialog) | `browse-file` | `file` | vendor |
| open a file or link in another app | `open-external` | `arrow-square-out` | |
| delete, remove | `delete` | `trash` | always the `danger` kind (DS-21.6) |
| retry, transcribe again | `retry` | `arrow-clockwise` | |
| scan for installed apps, scan again | `scan` | `scan` | vendor. Never `search` or `retry` |
| revert, restore the old version | `revert` | `arrow-counter-clockwise` | vendor |
| play / stop | `play` / `stop` | `play` / `stop` | one slot, swaps |
| record | `record` | `record` | vendor |
| edit | `edit` | `pencil-simple` | |
| search | `search` | `magnifying-glass` | vendor. The SearchField's own glyph only (DS-8.5), never on a button |
| close a popup, clear a field | `close` | `x` | |
| hint | `info` | `info` | InfoTip only |
| warning, attention, overload | `warning` | `warning` | the triangle; `warning-text` (the `WarningSlot`'s "!") |
| error, invalid value | `error` | `x-circle` | `error-text` |
| done | `done` | `check-circle` | `success-text` |
| download | `download` | `download-simple` | |
| add | `add` | `plus` | |
| preview on the desktop | `preview` | `eye` | |
| "becomes" (current to new) | `becomes` | `arrow-right` | vendor |
| more | `caret-down`, `dots-three` | the same | section 17 |
| model class: most accurate | `model-accurate` | `brain` | |
| model class: fast | `model-fast` | `lightning` | vendor |
| model class: tuned for one language | `model-language` | `translate` | vendor |
| model class: small | `model-small` | `feather` | vendor |
| own model file | `model-own` | `folder-user` | vendor. A folder, as the owner asked [M8]; not the plain `folder`, which is `browse-folder` |
| always replace (History selection) | `replace` | `swap` | vendor |
| add to dictionary | `dictionary` | `book-open-text` | |

- DS-24.1 A meaning never uses another glyph, and a glyph never carries another meaning. Check: `rg "Icons\.[a-z0-9-]+" -o crates/oxi-settings/ui/pages | sort | uniq -c` against this table; old constants (`path`, `redo`, `open-16`, `clipboard-paste-24` as copy) have no use left in pages.
- DS-24.2 Nouns (row and rail icons: microphone, keyboard, gear and the like) are not in this table and are never used on a button (DS-7.9).

## 25. No layout jumps [M25]

UI rules 1 to 6 hold. These add the cases found on screen:

- DS-25.1 A state changes colour only (DS-5.6): no border-width, weight, padding or size change on hover, press, focus, select, enable or disable.
- DS-25.2 Status text has a reserved place: a caption slot of fixed height (`caption-line` 16) that stays when empty, the `WarningSlot`, a tooltip, or the toast. Text never appears between two controls and pushes them.
- DS-25.3 A Combo keeps its field unchanged while open: the same Text element shows the value or the placeholder, at the same x and y, open and closed; the list is a separate layer with the geometry of DS-8.6. Check: frame capture of opening and closing "Pick a recording to add": the field's text does not move by a pixel. Today: its first item's text jumps.
- DS-25.4 A popup has its final size and position in its first frame; only opacity animates (`motion-open`). Its content never resizes it after that: lists scroll inside (section 18).
- DS-25.5 Selecting an item never adds or removes a panel that resizes a list or grid. Details of the selection show in a place reserved from the start, or in a popup. Today: picking an app in the Per-app gallery adds a bottom panel and the grid jumps.
- DS-25.6 A scrollbar's arrival moves nothing (DS-18.6).
- DS-25.7 Numbers that tick (percent, timer, counts, dB) sit in a slot as wide as their widest value ("100 %", "-60 dB", "59:59"), right-aligned. Check: a progress label going 9 % to 10 % to 100 % moves nothing beside it.
- DS-25.8 Content that loads later (app icons, history rows, model sizes) takes its final box at once: a placeholder of the same size, never a row that grows.
- DS-25.9 Toast: one line, 32 px high, on its own layer in the header row, 4 px inside the header's 40 px, between the tabs and the StatusChip. Its right edge is `control-gap` (8) left of the chip's box; it is as wide as its text plus 2 x `space-3`, at most 360; its left edge stays at least 8 px right of the last tab (or the title), and a text longer than that span is elided. It covers no tab, no chip, no field and no button on any page, pushes nothing, takes no click, and leaves by itself after 3 s with a `motion-open` fade both ways. Toast texts are short enough to show whole on Text (the page with the most tabs) at 720 x 520 in every shipped language. Check: shot of each page with a toast at 720 x 520 and 1280 x 860: its rect lies inside the header row and overlaps neither a tab nor the chip; with an actionable chip ("No model"), the chip still takes the click while the toast shows. Today: the toast "Transcribed again." at the bottom covers History's fields. A result that has a place on the page (a diff bar, a row's status slot) is shown there instead of a toast.
- DS-25.10 An item that is added, removed or reordered by an event while the user points at a list does not move the row under the pointer: new History rows come in at the top only while the list is at scroll 0; otherwise the list keeps its offset.

## 26. Progress [M12, M31]

One model for every bar and percent: model download, transcription (History, Test, overlay), pasting and typing (overlay), apps scan. It is one pure type in oxi-core with tests; `ProgressBar` (feedback.slint) and the overlay only draw its `shown` value. Inputs: elapsed time; an optional estimate `est` of the whole job in ms; an optional real fraction `real` in 0..1 (bytes, whisper's callback, characters typed).

| Constant | Value | Meaning |
|---|---|---|
| kick | to 0.03 within 200 ms, ease-out | the bar moves in its first frames, whatever is known |
| estimate curve | `e(t) = 0.90 * (1 - 2^(-t / (0.30 * est)))` | eased guess while no real fraction exists: 0.81 at t = est, never above 0.90 |
| default `est` | 4000 ms | when the caller has no estimate |
| real projection | `real + (real / t_real) * (t - t_real)`, at most `real + 0.10` | between two real updates the bar keeps the measured pace, at most 10 points ahead of the last real value |
| target | the projection when `real` >= 0.01, else `e(t)`; capped at 0.99 | |
| chase | `shown += (target - shown) * (1 - exp(-dt / 200 ms))` | smooths a coarse update into a glide |
| speed limit | at most 0.04 per 33 ms frame (1.2 per second) | a 0 to 67 jump takes at least 560 ms |
| floor | `shown` never decreases; a lower target only slows it | |
| finish | from the current value to 1.0 in 150 ms, ease-out; the full bar stays 250 ms | "done" is never shown below 100 |
| fail / cancel | the bar stops where it is and never runs to 100; on a fail the fill turns `error-text` and the label slot shows the `error` glyph and the short reason (DS-21.1) | |
| label | `floor(shown * 100)` and " %", at most 99 until finish; slot of DS-25.7 | |
| tick | every 33 ms, only while a job is shown | a still window draws nothing (UI rule 23) |
| real updates | pushed by events from the worker, at most 10 per second; no polling | |

- DS-26.1 Never at 0 while work runs: `shown` >= 0.02 at 200 ms after the start. Check: oxi-core test; a frame capture of a transcription start.
- DS-26.2 Never backwards and never a jump: for every step, 0 <= `shown(t + 33 ms) - shown(t)` <= 0.04, except the finish. Check: oxi-core property test with real fractions 0, 0.67, 1.0 fed at random times.
- DS-26.3 Indeterminate fallback, when the job has neither an estimate that means anything nor a real fraction (the apps scan, loading a model): a segment 30 % of the rail wide runs left to right in 1200 ms, linear, looping; under reduced motion the rail shows a still 30 % segment at its centre. No percent label. It becomes determinate, starting at the kick, the moment a real fraction arrives.
- DS-26.4 Estimates: transcription `est` = audio length times the measured speed of recent runs (the source is named in arch-plan), at least 400 ms; typing `est` = characters times the per-character delay, with `real` = characters typed; clipboard paste has no bar (it is one step): the pill shows "Pasting" with the accent mark and goes to "Done"; download uses `real` = bytes / total from the first byte.
- DS-26.5 A new phase is a new bar from 0 with its own label (transcribing, then typing); a bar never continues into another phase. A phase shorter than 200 ms still shows its kick and its finish.
- DS-26.6 Look: rail 4 px (`rail-height`), `bg-inset`, fill `accent`, `radius-round`; in the overlay the fill is `pill_accent`. The bar's place is reserved before the job starts (DS-25.2).

## 27. Merge principle [M20]

When two rows or controls do one job, make one control. Before adding a row, apply these tests; if one fits, merge.

- DS-27.1 A button that only shows or applies what the picker beside it selects: the picker does it on click. Overlay: clicking a position tile, the selected one too, selects it and shows the preview; no Preview row.
- DS-27.2 A confirm button after a select that has only one sensible outcome: picking does it. Test, "From history": picking a recording adds it and the Combo returns to its placeholder; no Add button. Keep a confirm only when undoing costs the user work.
- DS-27.3 "Current" and "new" value in two rows: one row, `current`, the `becomes` glyph, `new` (Hotkey, section 28.3).
- DS-27.4 A read-only row that repeats a value editable elsewhere: remove it, or make it the editor.
- DS-27.5 A title, a caption, a badge and a tooltip that say the same fact: keep one (UI rule 29).
- DS-27.6 A button and an InfoTip about the same thing: the button's tooltip carries the hint (DS-11.5).
- DS-27.7 An input that adds and an input that searches the same list: one field does both (Per-app, section 28.1).

Check for all: read each page shot and name, for every row, the one job it does; two rows with one job is a defect.

## 28. Screens

What correct looks like, per screen. Geometry not given here comes from sections 1 to 27.

### 28.1 Text, Per-app rules [M3, M4, M6]

- One card. Top bar, 32 px: `[SearchField, stretch][8][Button secondary "Scan for apps" + `scan` glyph]`. The button opens the apps popup.
- The SearchField ("Search or add an app") filters the rules list as the user types (name and exe, case-insensitive substring). Under it, a result list (Combo popup geometry, DS-8.6) offers installed apps that match and have no rule yet; clicking one, or Enter on the first, adds its rule and clears the field. Text ending in `.exe` that matches nothing offers "Add <text>".
- Rules list: `ScrollBox` with `reserve-gutter`, rows of `row-height` 48: `[app icon 20][16][name, elided, exe as caption][mode Combo, rule-mode-width 160][8][IconButton delete]`. Delete is rightmost.
- Apps popup: modal, centred, width min(960, window - 64), height min(640, window - 64), padding 16, `radius-card`. Header 32 px: `[SearchField, stretch, takes the caret, no ring][8][IconButton close]`. Body: `CardGrid` of `tile-height-short` tiles (icon 32, name, exe caption), `reserve-gutter`, inset 8. Clicking a tile adds or removes its rule and toggles its `CheckBadge`; the popup stays open and the grid never resizes: there is no bottom panel. The mode is set in the rules list. Esc, the close button and a click outside close it.
- Edge cases: an app with no icon shows `app-window`; a duplicate is never added: the existing rule scrolls into view and shows `accent-tint` for 600 ms; names are elided with a tooltip; no rules: `EmptyState` with the Scan button; scan running: the indeterminate bar (DS-26.3) in a reserved 4 px strip under the popup's header; scan found nothing: `EmptyState` "No apps found" with a "Scan again" button (the same `scan` glyph); over 100 apps: the grid scrolls inside the popup.

### 28.2 Model [M8, M10]

- Model tiles (`CardGrid`, `tile-height` 104): the 32 px class glyph top-left (`model-accurate`, `model-fast`, `model-language`, `model-small`, `model-own`), the state in the top-right slot as a 20 px mark with a tooltip (`CheckBadge` in use; `done` in `success-text` downloaded; `download` in `text-secondary` not downloaded; the percent, right-aligned in a "100 %" slot, while downloading), title, then one caption line of facts joined by " · ": size, speed class, languages ("1.6 GB · Fast · 99 languages"). The state is not repeated as words on the tile (UI rule 29). No two tiles of different classes share a glyph.
- The action row under the grid belongs to the selected tile: `[class glyph 20][name, caption][WarningSlot][ProgressBar slot, 200 wide (80 in the 240 column), while downloading][8][button, model-action-column 112][8][Delete slot, 32]`: 200 + 8 + 112 + 8 + 32 = the 360 column (80 + 8 + 112 + 8 + 32 = 240). The bar's place is reserved when no download runs. Delete is a 32 px IconButton (`delete` glyph, tooltip and accessible label "Delete", destructive, DS-6.3), rightmost (DS-23.2); its slot is always there, empty when there is nothing to delete or a download runs, so no button moves (DS-23.5).
- Advanced: every engine parameter is a Row with an InfoTip and a control whose first choice is "Auto"; the tip says what Auto picks now (DS-11.4 h). The group title's trailing slot holds a soft "Reset all to Auto" button.

### 28.3 Hotkey [M14, M15, M16]

- One row does it all: `[keyboard icon][Keys + InfoTip] ... [current keycaps][IconButton edit]`. The InfoTip carries the text that was the popup's description.
- Editing (pencil toggled, DS-6.5): the same row shows `[current keycaps in text-secondary][becomes glyph 16][new keycaps: accent-tint fill, accent-text ink]`; before a key is pressed the new side shows the placeholder "Press keys" in `text-secondary` in a slot one keycap wide at least. No second title, no "Press the new hotkey" line.
- The panel under the row (UI rule 1b) holds only the on-screen keyboard and a footer: `[status slot, caption-line 16, reserved: conflicts and errors, each after its `warning` or `error` glyph] ... [Cancel][Apply primary]`.
- The pencil is a toggle: clicking it again hides the panel and keeps the draft; a 6 px `accent` dot at the pencil's top-right corner (inside its box) marks a kept draft. Leaving the page drops the draft.

### 28.4 Overlay [M11, M12, M20]

- Position and preview are one control (DS-27.1): a 3 x 2 grid of tiles in the control column, matching the screen's corners and centres: tile 112 x 32, column gap 12, row gap 8 (3 x 112 + 2 x 12 = 360; in the 240 column: 72 wide). The row is a picker row (DS-1.5): `position-row-height` 88 (the 72 px grid + 16), so the next row starts 89 px below its top; it is the one row on Overlay off the 49 px pitch. Each tile shows a small pill mark where the overlay will sit; the selected tile has the `accent-tint` fill and its mark in `accent`.
- Clicking any tile, the selected one too, selects it and shows the real pill there for 2000 ms (one-shot timer; a new click restarts it). The "Try it on the desktop" row is gone. The row's InfoTip: "Click a position to see the overlay there."
- The pill's colours and progress follow sections 21 and 26.

### 28.5 History [M21, M22, M23]

- List (left): rows 56 px, selected `accent-tint`, `ScrollBox` with `reserve-gutter` and inset 4.
- Detail (right), top to bottom: (1) action bar, 32 px: start `[Play / Stop Button]` and the duration; end `[copy][folder-open][retry "Transcribe again"][delete]` as IconButtons with tooltips, Delete rightmost; (2) the text, filling the free height; (3) a reserved 32 px status strip. The Heard / Replace with / Add row is gone.
- Status strip: while transcribing again, the ProgressBar and its percent; after it, "New version applied" with a soft `[revert "Restore old"]` button at the right end. The new text is applied at once; the text area shows the diff until the user edits, picks another recording, or restores: removed words struck through in `text-secondary` on `bg-inset`, added words underlined on `success-tint` (strike and underline tell them apart without colour). One click on Restore puts the old text back (and then offers "Use new" the same way). No toast.
- Selection action bar: selecting text in the detail shows a bar on its own layer (nothing moves): 40 px high (32 px controls, 4 px padding), `bg-control`, 1 px `stroke-control`, `radius-card`, its bottom 8 px above the selection's first line (below the last line if there is no room), kept inside the detail card. In the order of section 23: start `[play]`; end `[copy][edit "Fix here only"]`, then the other utilities `[replace "Always replace"][dictionary "Add to dictionary"]`. Play and Copy are IconButtons, the other three labelled Buttons; under 520 px of card width all five are IconButtons. The bar is as wide as its controls, so the stretch between start and end is 8 px here. Play shows only when the recording has word timings (absent, not disabled). Esc or an empty selection hides the bar.

### 28.6 Home [M1, M29]

- The activity group's title row is 32 px: `[calendar icon][Speaking activity][InfoTip]`, stretch, `[Button soft "History folder" + folder-open]` at the right end (`Group` gets a trailing `actions` slot). The button's tooltip names the folder; the separate "i" beside it is gone (DS-11.5). Nothing sits under the heatmap card.
- Heatmap: the last 26 weeks (was 53). Cell = floor((card inner width - `heat-label-width` 32 - 25 x `heat-gap` 3) / 26), at most `heat-cell-max` 36 (was 30); at 1280 wide that is 34 px. The card is as high as its content (month labels 20, 7 rows, a 16 px legend line, `card-padding-v`): no empty band above or below; free space stays under the card (UI rule 24).
- Colour: the heat scale of section 20 (level 0 `bg-inset`, levels 1 to 4 the accent mixes). Legend "Less", five cells, "More" at the bottom right.
- Clickable: a day with dictations has a pointer, a 1 px `stroke-keycap` inner outline on hover and a tooltip ("12 Nov · 34 dictations · 1,240 words"); a click opens History filtered to that day. Empty days are inert.

### 28.7 Audio and Test [M7, M25, M26]

- Audio, Microphone, exactly the grid of section 3: Device: Combo over the full column. Level: `[Meter 264 x 32][8][Test 88]`. Gain and Meter sensitivity: `[Slider rail 264][8][NumberField 88]`. So Combo, Meter and both rails share their left x; Test and both fields share left and right x; all four rows are 49 px apart.
- Test, Recordings: Scenario: Combo, full column. Test: `[Run primary, 88]` at the column's right. New recording: title + InfoTip (DS-11.4 i), `[Record, 88]`. From history: Combo, full column, picking adds (DS-27.2). Four rows, 49 px apart, no second line.
- Take card: `[Play / Stop][wave, stretch][duration] ... [delete]`: Play leftmost, Delete rightmost (section 23).
- Test records with the device, gain and processing set on Audio; there is no second set of microphone settings.

## 29. Mission items to sections

| Item | Where |
|---|---|
| M1 accent colour for the UI and the overlay | 20, 21, 28.6 |
| M5 one scrollbar, inset as the container | 18 |
| M7 right-column controls line up | 3, 1 (DS-1.5), 28.7 |
| M9 focus ring only for the keyboard | 4 |
| M11 one system of state marks and colours, overload | 21 |
| M13 one page header | 14 |
| M17 readable at a glance | 22, 20 (DS-20.4, DS-20.5) |
| M12 overlay percent for transcription and typing | 26, 21 (DS-21.2) |
| M20 merge rows that do one job | 27, 28.4 |
| M22 icons by meaning, action placement | 24, 23 |
| M25 no layout jumps | 25, 8 (DS-8.6), 5 (DS-5.6) |
| M27 hints everywhere | 11 |
| M31 progress never at 0, never jumping | 26 |
| M33 every button has a fill | 7 (DS-7.5, DS-7.6), 6 (DS-6.1), 5 |
| M34 one rule for icons in buttons | 7 (DS-7.8, DS-7.9), 24 |
| M35 one style across screens | 1, 3, 23, 24, 28 |
| Screens: M3, M4, M6 / M8, M10 / M14 to M16 / M12, M20 / M21 to M23 / M29 / M26 | 28.1 / 28.2 / 28.3 / 28.4 / 28.5 / 28.6 / 28.7 |
| Old backlog: J80 slider offset, CardGrid single tile, M63 toast | DS-3.6, DS-15.2, DS-25.9 |

## Rule ids

Rule ids of sections 1 to 19 keep the meaning they had before 2026-11-11, so the code comments that cite them stay true. A rule added on 2026-11-11 took the next free id of its section. Two ids changed meaning by owner decision: DS-19.1 and DS-19.2. These lines contradict this file and are reworded by the task that owns the file (a doc task edits no code):

| Where | Says | Becomes |
|---|---|---|
| `crates/oxi-settings/src/lib.rs:193` | the accent is "the theme's ink, DS-19.1" | the user's accent (section 20) |
| `crates/oxi-settings/ui/theme/tokens.slint:258` | "Accent: the ink itself (DS-19.1)" | the same |
| `crates/oxi-settings/ui/theme/tokens.slint:269` | "Red means recording now only (DS-19.2)" | the `recording` token is deleted (DS-21.5) |
| `crates/oxi-settings/ui/theme/tokens.slint:251` | the focus ring is "the ink softened to the secondary text" (DS-4) | `focus-ring` = `accent` (section 4) |
| `crates/oxi-settings/ui/theme/components/fields.slint:661` | the list is "over the field ... it covers the field" (DS-4.4) | same x and width, below the field (DS-4.4, DS-8.6) |
| `crates/oxi-settings/tests/gallery.rs:1020` | "DS-7.4: Browse is a Button with the folder-open glyph" | Browse has the `browse-folder` glyph (section 24); the test's expected glyph changes with it |
| `crates/oxi-settings/ui/icons/SOURCE.md:51` | `warning-circle` is "the "!" slot (DS-19.6)" | the slot uses `warning` (section 24) |
| `crates/oxi-core/src/overlay/look.rs:13-41` | palette comments: "red means recording now and nothing else", errors "deep orange, since red means recording", clipping white | section 21 |

## Open: not in this file

Logic and performance architecture (where the progress type, the accent derivation and the overload rule live; reactivity; estimates) is `arch-plan.md`'s. The owner answered the four questions of `.fleet/2026-11-11-ui-overhaul/ask/a01-ds-spec.md` on 2026-11-11: red is the default accent, the logo's dot follows the accent inside the app, a red custom hex is allowed with a warning, the toast lives in the header row. They are in sections 19 to 21 and DS-25.9, and they replace the mission line "red only for errors and overload".
