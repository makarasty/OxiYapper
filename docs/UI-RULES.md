# UI rules

The owner's permanent rules for the OxiYapper interface. Every UI change obeys them. `docs/SPEC.md` wins a conflict. Other look decisions (visual pickers, confirmations, tray menu, "Made with Slint" badge) are in SPEC, "Settings UI" and "System", and are not repeated here.

How to look:

- **Three sizes:** minimum 720x520, middle 1000x700, the window maximum.
- **Real look:** `scripts/real-shot.ps1` gives a PNG from the runtime renderer. `crates/oxi-settings/tests/shots.rs` covers states, but its software renderer differs, so it is not the final look.
- **Both themes** count, light and dark. Open the PNGs and read them; a passing test is not a look.

## Nothing moves

1. **Rows never change height. No row or card grows, shrinks or moves because of an event, a value or a state.**
   Check: shot the row in each of its states, at minimum and maximum values too. Its height and the y of the next row are the same in all of them.
   Two exceptions. (a) Window banners (daemon offline, config error, `app.slint:313`) are rare whole-window errors above the page: they may push the content down. Rule 1, rule 8 and the check of rule 27 do not apply to them. (b) A panel the user opens on purpose (the hotkey keyboard, `hotkey.slint:81`) may move the content below it; rule 1 covers rows that change on their own or from events. Rule 6 still holds for the panel's width.
2. **A persistent warning is a "!" icon in a reserved slot of the row, with the text in a hover tooltip.** The slot keeps its width when empty. No warning line under the row. This covers warnings only: status badges (Ready, Not downloaded) and the model card's size and removal date stay as text on the row.
   Check: shot with and without the state: the row's other parts do not move. Hover shows the full text. The text is not on the row.
3. **Short events are a toast at the bottom of the window.** The toast lies over the content, pushes nothing and leaves by itself.
   Check: shot with and without the toast: outside the toast's own area the two match.
4. **A caption tied to a value goes inside the control** (for example "Never" inside the field). A caption must not shift a control off its centre.
   Check: set the value to its edge cases (0, minimum, maximum): the control's position and height do not change.
5. **Edge cases are designed in, not added on top.** A row, card or page has a place for all its states before it is built.
   Check: the task lists the states of each new or changed component, and one shot or gallery sheet shows them all at the same size.
6. **Opening a panel does not change the width of the content** (for example the hotkey keyboard).
   Check: shot Hotkey with the keyboard closed and open, at the three sizes: the left and right content edges match.

## Spacing and alignment

7. **One spacing system. All values live in `crates/oxi-settings/ui/theme/tokens.slint`.** Pages and the shell (`ui/app.slint`) hold no pixel numbers and no extra container padding on any side.
   Check: `rg "[1-9]px|[0-9]{2,}px" crates/oxi-settings/ui/pages crates/oxi-settings/ui/app.slint` prints nothing (a `0px` comparison is no value).
8. **Page title and sidebar start on one line.**
   Check: on every page, in a real shot (no window banner shown, see rule 1), the vertical centre of the page title and of the first sidebar item differ by at most 1 px.
9. **Things stacked in one card line up.** Controls start and end at one x; row titles on one page start at one x; a field below a select is exactly as wide as the select.
   Check: measure the edges in a shot at the three sizes; they differ by 0 px.
10. **One layer of surface.** No card inside a card, no frame inside a frame. An action sits at the end of its row or next to what it acts on, never alone in empty space.
    Check: no `Card` nested in a `Card` in `crates/oxi-settings/ui/pages`; the shot shows one border around any control.
11. **Overlay status text is centred horizontally in the pill.**
    Check: pill shot of each status: left and right gaps differ by at most 1 px.

## Shape and colour

12. **Low corner radius, from tokens only.** Circles (the Switch and radio thumbs at `self.height / 2`, the slider thumb) go through the round token `Theme.radius-round`, and so does the focus ring derived from them. A later task adds the token itself.
    Check: every line of `rg "border-radius" crates/oxi-settings/ui` reads a `Theme.radius-*` token (`Theme.radius-round` for circles and for a focus ring derived from them). A focus ring may add a fixed offset to its target's token radius (`root.radius + 3px`, `base.slint`), and the gallery's radius samples are exempt.
13. **Status badges are nearly square.** Radius at most `Theme.radius-control` (4 px); no pill shape.
    Check: the `Badge` component's `border-radius`, and a zoomed shot of Ready, Downloading and Not downloaded.
14. **Sharp corners are minimal.** Blocks and icons have softened corners. Only window edges, full-bleed panels and 1 px dividers may be square.
    Check: a real shot zoomed 2x on each page shows no other square corner; no `border-radius: 0` in `crates/oxi-settings/ui`.
15. **Own teal accent, never the Windows accent.**
    Check: `no_system_accent_on_any_page` in `crates/oxi-settings/tests/shots.rs` passes; `rg "Palette\.accent" crates/oxi-settings/ui` prints nothing.
16. **Danger and cancel buttons turn red on hover** (Cancel, Remove, Delete, Stop and the like).
    Check: each such button uses the danger variant; a forced-hover shot in the gallery shows the `error-*` colours.
17. **Selected means filled, never an outline ring.** A focus ring appears only for keyboard focus.
    Check: shot of selected and unselected: the border does not change colour, the fill does.
18. **The light theme is warm like dawn, slightly yellow, not pure white.**
    Check: every opaque neutral light `bg-*` token in `tokens.slint` has red above blue and none is `#FFFFFF`. Teal and success tints and translucent overlays are exempt.
19. **Text is at least 12 px and at least 4.5:1 against its background, in both themes.** Disabled text is exempt from the 4.5:1 limit (rule 31 still says why it is disabled).
    Exempt: the Slint `AboutSlint` widget on About. Its 10 px text is fixed by Slint, and the Slint royalty-free licence requires the widget.
    Check: `rg "font-size: *[0-9]" crates/oxi-settings/ui` prints nothing; the gallery contrast swatches all read 4.5 or more, except the swatches of disabled text.
20. **The on-screen keyboard's style is the base for every component: bright caps on grey, crisp 1 px strokes. Icons are Fluent UI System Icons, not hand-made.** The own logo mark is the one exception.
    Check: a new icon is listed in `crates/oxi-settings/ui/icons/SOURCE.md` and read through the `Icons` global.

## Motion

21. **Animations are short and calm, and nobody waits for them.** At most 200 ms; a control takes input while it animates. Durations live in `tokens.slint`.
    Check: every `animate` in `crates/oxi-settings/ui` reads a `Theme` duration token of 200 ms or less. Click twice fast during a transition: both clicks count.
22. **Animations respect the OS reduced-motion setting.**
    Check: turn off Windows "Animation effects" and open a list or popup: it appears with no transition.
23. **A still window draws nothing.** Animations cost no CPU when nothing happens.
    Check: on the runtime renderer, with no event running, a still window uses under 1 % of one core averaged over 10 s, read from the Windows performance counters (`\Process(OxiYapper*)\% Processor Time`, where 100 % is one core; the daemon is also `OxiYapper`, so pick the settings process's instance by its `ID Process` counter). Pages with a live meter (Audio) are exempt while shown.

## Window and navigation

24. **The window has a maximum and maximize is off; the content adapts.** It resizes from the minimum up to the maximum in SPEC.
    Check: the maximize button is disabled, dragging an edge stops at the maximum, and shots at the three sizes show no stretched control. An empty area below short content in a large window is allowed: do not stretch controls or cards to fill the height.
25. **No scroll on normal pages. A page that does not fit gets horizontal sub-tabs under the title.** The sidebar stays short and works icons-only. Page scrolling stays only as SPEC's fallback. Two one-view pages that cannot be split into meaningful sub-tabs, Home and Hotkey with the on-screen keyboard open, may use a tight layout at small window sizes under a token-defined height instead of sub-tabs: it hides captions, the legend, headings and instruction text and keeps actions in view (owner, 2026-11-10). System General's tight layout at low height (system.slint:49, token `system-tight-height` in tokens.slint) is also allowed.
    Check: a real shot of every page and sub-tab at 720x520 with default content shows no scrollbar; at the same size the icons-only sidebar shows every section.
26. **No scroll inside a scroll.** A long list scrolls by itself and the page around it does not.
    Check: with a list over its height, wheel over the list moves only the list.

## Edge cases

27. **Many-items and few-items states are checked per screen, using the table below.**
    Check: each cell is rendered and looked at. Nothing leaves its box, overlaps or breaks rules 1 to 3 (window banners are exempt from rule 1, see there). Every screen is also checked with OxiYapper not running and with a read-only config.
28. **Visible text fits at minimum, middle and maximum window size,** and at every width where the layout switches (960 px today, full sidebar to icons-only). Only unbounded user text (device names, paths, app names) may be cut with an ellipsis, and then its tooltip has the full text.
    Check: shots at those sizes, with the longest real strings: no cut, overlapping or clipped text, except an ellipsis on user-supplied text (device names, paths, app names) that has the full text in its tooltip.

| Screen | Few, empty | Many, long |
|---|---|---|
| Home | no dictations yet | very large numbers, a full year of activity |
| Hotkey | one key, no key picked | 15 keys in the chord: chips stay in the Keys row |
| Audio | no microphone, saved device unplugged | many microphones, a very long device name |
| Language popup | search finds nothing | every whisper language |
| Model | nothing downloaded, no GPU | all states at once (ready, downloading, paused, failed), long model and GPU names |
| Text: Dictionary | empty file | a very long file, over the token budget |
| Text: Replacements | empty file | a very long file, invalid lines |
| Per-app rules | no rules, scan finds nothing | over 100 apps, long app names |
| Overlay | overlay turned off | every position selected in turn |
| Sounds gallery | built-in sounds only | own file with a long name |
| Test | no recording, no result | every scenario in the table, a long phrase |
| System | nothing to warn about | all warnings on, a long folder path |
| About | no extra states | narrowest window: blocks stay whole |

## Content

29. **Say a thing once per screen.** A badge, caption, note and banner do not repeat the same state.
    Check: read each state's shot; no fact appears twice.
30. **Human names, not internal ones.** "[" and not BracketLeft; Windows FriendlyName for microphones and GPU; app names and not exe names.
    Check: shot with such values: no raw key code, device id or `.exe` needed to pick something.
31. **A disabled control says why,** in its tooltip or row description.
    Check: hover each disabled control in the offline and read-only states.
32. **A slider's value can be typed.**
    Check: click the value field, type a number, press Enter: the slider moves, and an out-of-range number is clamped.
33. **Things that have more behind them are clickable** (for example a stat card opens its trend by day).
    Check: on Home and any status element with more detail, a click opens that detail and hover shows it can be clicked.
