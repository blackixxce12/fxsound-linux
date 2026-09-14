# 06 — Secondary windows and transient UI

Reverse-engineering spec for the FxSound Windows/JUCE app, written for a from-scratch
re-implementation in **Rust 1.98.1 + egui/eframe 0.36.0** (winit/Wayland, wgpu or glow) with
**PipeWire** audio.

Scope of this document: everything that is *not* the main window — the Settings dialog and its
three panes, the output-device preference list, the preset import/export dialogs, the modal
message boxes, and the transient toast/notification widget.

Every number below is cited as `path:line` against the C++ tree at
`/home/blackixxce/Загрузки/fxsound-app-main`. Paths are given relative to `fxsound/Source/`.

---

## 0. Common chrome: `FxWindow`

All of the dialogs in this document derive from `FxWindow` (`GUI/FxWindow.h:27`), a borderless
`juce::Component` that is pushed straight onto the desktop with `addToDesktop()` and draws its own
title bar, close button, rounded corners and drop shadow. There is **no OS title bar**.

### 0.1 Geometry constants

| Constant | Value | Source |
|---|---|---|
| `FxWindow::SHADOW_WIDTH` | `5` px | `GUI/FxWindow.h:44` |
| `FxWindow::CLOSE_BUTTON_WIDTH` | `15` px | `GUI/FxWindow.h:45` |
| `FxTheme::WINDOW_CORNER_RADIUS` | `21` px | `GUI/FxTheme.h:42` |
| `FxTheme::TITLE_BAR_HEIGHT` | `57` px | `GUI/FxTheme.h:43` |
| title bar component height | `TITLE_BAR_HEIGHT - 1` = **56** px | `GUI/FxWindow.cpp:29` |
| `TitleBar::ICON_WIDTH` | `106` px | `GUI/FxWindow.h:71` |
| `TitleBar::ICON_HEIGHT` | `15` px | `GUI/FxWindow.h:72` |

### 0.2 The outer-window size formula (load-bearing)

`FxWindow::setContent()` sizes the whole window from the content component
(`GUI/FxWindow.cpp:74-82`):

```
outer_width  = content_width  + SHADOW_WIDTH*2                                   = content_w + 10
outer_height = content_height + title_bar_height + WINDOW_CORNER_RADIUS + SHADOW*2
             = content_h + 56 + 21 + 10                                          = content_h + 87
```

Content is placed at `(SHADOW_WIDTH, title_bar_.getBottom() + 1)` = `(5, 62)` in window
coordinates (`GUI/FxWindow.cpp:74`, `GUI/FxWindow.cpp:150`).

`FxWindow::resized()` (`GUI/FxWindow.cpp:147`) lays the title bar at
`x = WINDOW_CORNER_RADIUS + SHADOW_WIDTH = 26`, `y = SHADOW_WIDTH = 5`,
`w = outer_width - 21*2 - 5*2`, `h = 56`. Therefore **title bar bottom = y 61**, content top = y 62.

Applying the formula to every dialog in this document:

| Dialog | Content size | Outer window size |
|---|---|---|
| `FxSettingsDialog` | 600 × 510 (`GUI/FxSettingsDialog.h:190-191`) | **610 × 597** |
| `FxPresetImportDialog` | 400 × 400 (`GUI/FxPresetImportDialog.h:51-52`) | **410 × 487** |
| `FxPresetExportDialog` | 400 × 405 (`GUI/FxPresetExportDialog.h:67-68`) | **410 × 492** |
| `FxImportCompleteMessage` | 350 × 340 (`GUI/FxPresetImportDialog.cpp:104-105`) | **360 × 427** |
| `FxConfirmationMessage` | 450 × 142 (`GUI/FxMessage.h:177-178`) | **460 × 229** |
| `FxMessage` | 400 × 80 (`GUI/FxMessage.h:56-57`) | **410 × 167** |

### 0.3 Painting

`FxWindow::paint()` (`GUI/FxWindow.cpp:118-143`):
1. `DropShadow` with `radius = shadow_width_ (5)` over a rounded rect inset by 5 on all sides,
   corner radius 21.
2. Fill: `ColourScheme::windowBackground` — rounded-rect (radius 21) when the component is not
   opaque, plain rect when opaque.
3. A 1 px horizontal rule in `FXCOLOR(ControlBackground)` @ alpha 1.0 across `y = title_bar bottom`
   (`GUI/FxWindow.cpp:141-142`).

Title bar (`GUI/FxWindow.cpp:236-259`, `261-310`):
- Fills with `windowBackground`.
- If the window was constructed with an **empty** name (`FxMessage`, `FxConfirmationMessage`,
  `FxImportCompleteMessage`): draws the big `DefaultLogo` SVG at 106 × 15, left-aligned, vertically
  centred in the 57 px band, plus a second `HighlightedLogo` overlay at alpha 0 used for the
  "processing" cross-fade (`GUI/FxWindow.cpp:364-370`, fade duration 600 ms at
  `GUI/FxWindow.cpp:197-207`).
- If the window has a name (`"Settings"`, `"Import Presets"`, `"Export Presets"`): draws the
  narrow `IconLogo` scaled to height `ICON_HEIGHT - 1 = 14`, then the title label at
  `x = icon_width + 2`, vertically centred, colour `ColourScheme::highlightedText`, font
  `FxTheme::getNormalFont()` (`GUI/FxWindow.cpp:374-387`, `GUI/FxWindow.cpp:280-283`).
  The title string is re-translated on every paint via `TRANS(name_)` (`GUI/FxWindow.cpp:241`).
- Close button: 15 × 15, right-aligned, vertically centred (`GUI/FxWindow.cpp:188`,
  `GUI/FxWindow.cpp:263-264`). It is an "X" made of two line segments of thickness `0.08`
  in a unit square scaled to fit a square of side = button height, painted in
  `FXCOLOR(ImageButton)` @ alpha 1.0 over a `windowBackground` fill (`GUI/FxWindow.cpp:154-169`).
  Cursor: `PointingHandCursor`.
- Dragging the title bar drags the whole window via `ComponentDragger`
  (`GUI/FxWindow.cpp:339-358`).

### 0.4 Theme palette (needed to paint any of these dialogs)

`FxColor` enum order (`GUI/FxTheme.h:29-31`) with the two palettes from
`GUI/FxTheme.cpp:23-25` (Dark) and `GUI/FxTheme.cpp:27-29` (Light). `FxThemeMode::Dark = 0`,
`Light = 1`, default is `Dark` (`GUI/FxTheme.h:28`, `GUI/FxTheme.cpp:61`).

| # | `FxColor` | Dark | Light |
|---|---|---|---|
| 0 | `WindowBackground` | `#181818` | `#f5f5f5` |
| 1 | `WidgetBackground` | `#181818` | `#f5f5f5` |
| 2 | `MenuBackground` | `#383838` | `#c7c7c7` |
| 3 | `Outline` | `#2b2b2b` | `#fafafa` |
| 4 | `DefaultText` | `#b1b1b1` | `#4e4e4e` |
| 5 | `DefaultFill` | `#000000` | `#ffffff` |
| 6 | `HighlightedText` | `#ffffff` | `#000000` |
| 7 | `HighlightedFill` | `#0c0c0c` | `#e0e0e0` |
| 8 | `MenuText` | `#ffffff` | `#000000` |
| 9 | `ComboBoxBackground` | `#000000` | `#d7d7d7` |
| 10 | `TextButtonBackground` | `#d51535` | `#1ac1ff` |
| 11 | `ImageButton` | `#e63462` | `#23b6eb` |
| 12 | `HintText` | `#7f7f7f` | `#7f7f7f` |
| 13 | `ValidTextBorder` | `#009cdd` | `#009cdd` |
| 14 | `InvalidTextBorder` | `#d51535` | `#d51535` |
| 15 | `ControlBackground` | `#0f0f0f` | `#e0e0e0` |
| 16 | `SliderTrack` | `#e33250` | `#0a4d66` |
| 17 | `SliderHighlight` | `#f7546f` | `#53ccff` |
| 18 | `GraphHigh` | `#d51535` | `#1ac1ff` |
| 19 | `GraphLow` | `#fe566a` | `#72d8ff` |
| 20 | `EqStart` | `#ef4b65` | `#33c8ff` |
| 21 | `EqEnd` | `#742834` | `#063244` |
| 22 | `VerticalSliderLow` | `#f3f3f3` | `#1c1c1c` |
| 23 | `MenuHighlightBackground` | `#414141` | `#b9b9b9` |
| 24 | `PanelBackground` | `#000000` | `#c0c0c0` |
| 25 | `RowOutline` | `#b1b1b1` | `#4e4e4e` |
| 26 | `SelectedRowOutline` | `#e63462` | `#23b6eb` |

Fonts (`GUI/FxTheme.cpp:466-479`) — the family is Gilroy, embedded as three TTFs
(`GUI/FxTheme.cpp:95-97`):

| Accessor | Typeface | Height |
|---|---|---|
| `getNormalFont()` | Gilroy **Semibold** (`GilroyRegular`→400, `GilroySemibold`→600, `GilroyBold`→700) | **17.0** px |
| `getSmallFont()` | Gilroy Regular (400) | **14.0** px |
| `getTitleFont()` | Gilroy Bold (700) | **17.0** px |
| `getTextButtonFont()` | Gilroy Semibold, `min(17.0, button_height)` (`GUI/FxTheme.cpp:463`) | ≤ 17.0 |
| `getComboBoxFont()` | Gilroy Semibold, **14.0** if box height ≤ 30, else **17.0** (`GUI/FxTheme.cpp:120-126`) | 14 / 17 |
| tooltip text | `getNormalFont().withHeight(14.0f)`, wrap at **400** px (`GUI/FxTheme.cpp:678-687`) | 14.0 |

Tooltip box: width = text width + 20, height = text height + 12, corner radius 5, 1 px outline;
placed at `screenPos.x + 36` (or `x - (w+18)` on the right half of the screen) and `y + 12`
(or `y - (h+12)` on the bottom half), clamped to the parent area
(`GUI/FxTheme.cpp:515-541`).

---

## 1. `FxSettingsDialog` — the Settings window

Files: `GUI/FxSettingsDialog.h`, `GUI/FxSettingsDialog.cpp`.

### 1.1 Window-level facts

| Property | Value | Source |
|---|---|---|
| Title (string id) | `"Settings"` | `GUI/FxSettingsDialog.cpp:24` |
| Content size | 600 × 510 | `GUI/FxSettingsDialog.h:190-191`, `:116` |
| Outer size | 610 × 597 | derived, §0.2 |
| Placement | `centreWithSize(w, h)` — centred on the primary display | `GUI/FxSettingsDialog.cpp:27` |
| Desktop flags | `addToDesktop(0)` (no taskbar button, no native title bar) | `GUI/FxSettingsDialog.cpp:28` |
| Raise | `toFront(true)` | `GUI/FxSettingsDialog.cpp:29` |
| Always-on-top | **not** set | — |
| Modality | `runModalLoop()` from the caller; on close `exitModalState(0); removeFromDesktop();` | `GUI/FxMainWindow.cpp:452-453`, `GUI/FxSettingsDialog.cpp:32-36` |
| Escape key | closes (same path as close button) | `GUI/FxSettingsDialog.cpp:78-88` |
| Tooltips | owns a `TooltipWindow` scoped to this dialog | `GUI/FxSettingsDialog.h:217`, `:24` |
| On dismiss | caller runs `FxController::refreshOutputList()` | `GUI/FxMainWindow.cpp:454`, `GUI/FxSystemTrayView.cpp:264` |

Two entry points: the main window's hamburger menu (`GUI/FxMainWindow.cpp:451-455`) and the
system-tray context menu (`GUI/FxSystemTrayView.cpp:261-265`). Both construct the dialog **on the
stack** and block in `runModalLoop()`.

### 1.2 Extra paint

`FxSettingsDialog::paint()` (`GUI/FxSettingsDialog.cpp:38-44`) draws a vertical separator in
`FXCOLOR(Outline)` @ alpha 1.0 at `x = SEPARATOR_X = 152` (`GUI/FxSettingsDialog.h:50`), from
`y = title_bar_.getBottom()` (61) to the bottom of the window (597).

> **Quirk to reproduce or fix deliberately.** This `SEPARATOR_X` is in *window* coordinates, while
> `SettingsComponent::SEPARATOR_X = 152` (`GUI/FxSettingsDialog.h:205`) is in *content* coordinates
> and the content is offset by `SHADOW_WIDTH = 5`. The painted line therefore sits 5 px to the left
> of the pane edge, and it is drawn *behind* the tab buttons (which span content x 20…170, i.e.
> window x 25…175), so it shows through the buttons' text area but is covered by their 40 × 40 icon
> squares. In the Rust port, draw **one** divider at the pane's left edge.

### 1.3 Layout — `SettingsComponent` (600 × 510)

Constants: `BUTTON_X = 20`, `BUTTON_Y = 50`, `BUTTON_WIDTH = 150`, `BUTTON_HEIGHT = 40`,
`SEPARATOR_X = 152` (`GUI/FxSettingsDialog.h:201-205`). Vertical gap between buttons = 20
(`GUI/FxSettingsDialog.cpp:122-123`).

```
content (600 x 510), window coords = content + (5, 62)
+-------------------------------------------------------------------------------+
|            :                                                                   |
|  (20,50)   :  pane_rect = (153, 1, 449, 509)                                    |
|  +-------+ :  +-------------------------------------------------------------+  |
|  |[icon] | :  | (20,5,429,24)  Pane title, getTitleFont() 17px bold          |  |
|  | Audio | :  |                                                             |  |
|  +-------+ :  |                                                             |  |
|  (20,110)  :  |                                                             |  |
|  +-------+ :  |                                                             |  |
|  |[icon] | :  |                     ACTIVE PANE                             |  |
|  |General| :  |  AudioSettingsPane / GeneralSettingsPane / HelpSettingsPane  |  |
|  +-------+ :  |                                                             |  |
|  (20,170)  :  |                                                             |  |
|  +-------+ :  |                                                             |  |
|  |[icon] | :  |                                                             |  |
|  | Help  | :  |                                                             |  |
|  +-------+ :  +-------------------------------------------------------------+  |
|            :                                                                   |
+-------------------------------------------------------------------------------+
             ^ separator drawn at window x=152 (= content x 147)
```

Tab button bounds: Audio `(20, 50, 150, 40)`, General `(20, 110, 150, 40)`, Help `(20, 170, 150, 40)`
(`GUI/FxSettingsDialog.cpp:121-123`).

Pane bounds `(SEPARATOR_X + 1, 1, getWidth() - SEPARATOR_X + 1, getHeight() - 1)` =
**`(153, 1, 449, 509)`** (`GUI/FxSettingsDialog.cpp:125`).

> **Quirk.** `153 + 449 = 602 > 600`: the pane overhangs the content by 2 px. Almost certainly a
> sign error (`- SEPARATOR_X + 1` should be `- SEPARATOR_X - 1`). Use **447** wide in the port and
> note the 2 px difference if you are pixel-diffing against the original.

### 1.4 Tab button rendering — `SettingsButton`

`GUI/FxSettingsDialog.cpp:46-76`:
- Rounded square of side = button height (40), corner radius `height / 4 = 10`, at local `(0,0)`.
- Fill: `FXCOLOR(MenuHighlightBackground)` when selected, `FXCOLOR(MenuBackground)` when not.
- Icon drawn inside that square inset by `10, 10` (so a 20 × 20 drawing area), centred, alpha 1.0.
- Label: `TRANS(getName())`, `getNormalFont()` (17 px), left-aligned, drawn in the rect
  `(height + 5, 0, width - height + 5, height)` = `(45, 0, 115, 40)`.
- Text colour: `FXCOLOR(HighlightedText)` when selected, `FXCOLOR(DefaultText)` otherwise.
- Cursor `PointingHandCursor` (`GUI/FxSettingsDialog.h:57`).
- `paintButton()` is a no-op; everything is in `paint()` (`GUI/FxSettingsDialog.h:66`).

Icons (SVG, from `BinaryData`, present in `fxsound/Images/`):

| Tab | String id | Icon asset | Source |
|---|---|---|---|
| Audio | `"Audio"` | `speaker.svg` | `GUI/FxSettingsDialog.cpp:92-94` |
| General | `"General"` | `settings.svg` | `GUI/FxSettingsDialog.cpp:98-100` |
| Help | `"Help"` | `question.svg` | `GUI/FxSettingsDialog.cpp:103-105` |

Selection semantics (`GUI/FxSettingsDialog.cpp:131-163`): plain radio behaviour — clicking a
button sets its own toggle state true, the other two false, and shows exactly one pane. **Audio is
selected at construction** (`GUI/FxSettingsDialog.cpp:93`, `:112-114`).

### 1.5 `SettingsPane` base

`GUI/FxSettingsDialog.h:73-89`, `GUI/FxSettingsDialog.cpp:165-183`.

| Constant | Value |
|---|---|
| `X_MARGIN` | 20 |
| `Y_MARGIN` | 5 |
| `TITLE_HEIGHT` | 24 |

Title label: `getTitleFont()` (Gilroy Bold 17), colour `TextButton::textColourOnId`
(= `FXCOLOR(HighlightedText)`, see `GUI/FxTheme.cpp:86`), left-aligned, bounds
`(20, 5, paneWidth - 20, 24)` = `(20, 5, 429, 24)`.

The title text is re-translated on *every* `paint()` (`GUI/FxSettingsDialog.cpp:177-183`) — this is
how live language switching works without rebuilding the UI.

---

### 1.6 Audio pane — `AudioSettingsPane`

Constants (`GUI/FxSettingsDialog.h:101-109`): `GROUP_MARGIN = 10`, `ENDPOINT_Y = 50`,
`LABEL_WIDTH = 220`, `OUTPUT_PREFERENCE_HEIGHT = 260`, `LABEL_HEIGHT = 14`,
`TOGGLE_BUTTON_HEIGHT = 30`, `RESET_PRESETS_BUTTON_WIDTH = 220`, `BUTTON_HEIGHT = 24`,
`MAX_BUTTON_WIDTH = 315`.

Resolved layout in pane-local coordinates (pane is 449 × 509), from
`GUI/FxSettingsDialog.cpp:241-262`:

| Widget | Bounds (x, y, w, h) | Notes |
|---|---|---|
| `title_` ("Audio") | `(20, 5, 429, 24)` | `getTitleFont()` |
| `output_preference_title_` | `(20, 50, 220, 14)` | `getNormalFont()` 17 px, left |
| `output_preference_` (`FxOutputPreference`) | `(20, 74, 399, 260)` | width = `paneW - (X_MARGIN+5)*2` = 449 − 50 |
| `prioritize_new_output_toggle_` | `(20, 344, 399, 30)` | |
| group backdrop | `(10, 40, 419, 344)` rounded r = 8 | `FXCOLOR(DefaultFill)` @ **alpha 0.2** |
| `reset_presets_button_` | `(20, 404, W, 24·lines)` | `220 ≤ W ≤ 315` |

Group backdrop maths (`GUI/FxSettingsDialog.cpp:254-258`):
`x = title.x − 10`, `y = title.y − 10`, `w = output_preference.right − x + 10`,
`h = toggle.bottom − y + 10`. Painted before the pane title, in
`AudioSettingsPane::paint()` (`GUI/FxSettingsDialog.cpp:264-274`), on top of a full-pane fill with
`ResizableWindow::backgroundColourId`.

Reset button auto-sizing (`GUI/FxSettingsDialog.cpp:289-315`):
counts `\n` in the translated label, up to **3** lines; height = `24 × lineCount`;
width = `min(bestWidthForHeight(24 × lines), 315)` clamped up to at least `220`.

#### Settings exposed by the Audio pane

| Control | String id | Type | Default | Range | Persistence key | Source |
|---|---|---|---|---|---|---|
| Output priority list | `"Output Device Preference"` (heading) | ordered list of `DeviceConfig` | seeded from enumerated devices, default device first | n/a | `device_configs` (JSON array), plus `device_configs_version` = `2` | `GUI/FxSettingsDialog.cpp:281`; `Utils/Settings/DeviceConfig.cpp:51`; `GUI/FxController.cpp:1467-1487` |
| Prioritize-new checkbox | `"Prioritize new output devices"` | bool | **false** | — | `prioritize_new_output` | `GUI/FxSettingsDialog.cpp:187,283`; `GUI/FxController.cpp:2747,2752` |
| Reset presets button | `"Reset presets to factory defaults"` | action | — | — | (none; deletes preset files) | `GUI/FxSettingsDialog.cpp:188,285` |

`prioritize_new_output` is read with `settings_.getBool("prioritize_new_output", false)`
(`GUI/FxController.cpp:2747`) and written immediately on click (`GUI/FxSettingsDialog.cpp:208`).
Its only consumer is `DeviceConfig::updateDeviceConfigs()`, which inserts a newly seen device at
index 0 when true and appends when false (`Utils/Settings/DeviceConfig.cpp:57`, `:78-85`).

**Reset button enablement** (`GUI/FxSettingsDialog.cpp:210-220`): enabled iff
`FxModel::getUserPresetCount() > 0` **or** any preset has `modified == true`. After a successful
click it disables itself unconditionally (`GUI/FxSettingsDialog.cpp:226`).

`FxController::resetPresets()` (`GUI/FxController.cpp:1334-1382`) does, in order:
1. `setNumEqBands(10)`, `setVolumeLeveling(0.0f)`, `setBalance(0.0f)`, `setFilterQ(1.0f)`,
   `setMasterGain(0.0f)` — the defaults from `GUI/FxController.h:46-51`.
2. For every preset: delete its auto-saved copy if modified; **permanently delete the `.fac` file**
   for every `UserPreset` (Win32 `SHFileOperation`/`FO_DELETE`, silent, no confirmation).
3. `initPresets()`, then select the preset bound to the current output device, else preset 0.
4. Pushes the toast `"Presets are restored to factory defaults"`.

There is **no confirmation dialog** in front of this destructive action. Recommend adding one in
the Rust port (see Open questions).

Pane refresh: `visibilityChanged()` calls `output_preference_.update()` whenever the pane becomes
visible (`GUI/FxSettingsDialog.cpp:317-323`). `mouseEnter`/`mouseExit` are overridden but
functionally no-ops — note `mouseExit` erroneously calls `Component::mouseEnter`
(`GUI/FxSettingsDialog.cpp:330-333`); do not port the bug.

---

### 1.7 `FxOutputPreference` — the device-priority list

Files: `GUI/FxOutputPreference.h`, `GUI/FxOutputPreference.cpp`.

Container (`GUI/FxOutputPreference.cpp:356-366`):
- Background: rounded rect over the full local bounds (399 × 260), radius **8.0**, fill
  `FXCOLOR(WidgetBackground)` @ alpha 1.0.
- The `ListBox` is inset by `reduced(5, 10)` → `(5, 10, 389, 240)`.
- `ROW_HEIGHT = 40` (`GUI/FxOutputPreference.h:103`), single selection only
  (`GUI/FxOutputPreference.cpp:320-321`).
- List tooltip string id: **`"Use Shift+Up or Shift+Down to change the device priority"`**
  (`GUI/FxOutputPreference.cpp:316`).

Row constants (`GUI/FxOutputPreference.h:35-37`): `BUTTON_WIDTH = 18`, `MARGIN = 5`,
`PRESET_LIST_WIDTH = 150`.

Row layout for a 389 × 40 row (`GUI/FxOutputPreference.cpp:104-138`), `bounds = localBounds.reduced(2)`
= `(2, 2, 385, 36)`, `y = (40 − 18) / 2 = 11`:

```
 0    5   14   23              46                        207            362   389
 |    |    |    |               |                         |              |     |
 +----+----+----+---------------+-------------------------+--------------+-----+
 |    [^] [v]   | 1. Speakers (Realtek…)  |  [ Select preset  v ]        | [x] |
 +--------------+-------------------------+------------------------------+-----+
      18x18 @y11                                150 x 36 @ y2           18x18 @y11
                 underline at y = 37.5 across x 46..202
```

| Element | Rule | Resolved (middle row) |
|---|---|---|
| `up_button_` x | `MARGIN` if `row < numRows-1` else `MARGIN + BUTTON_WIDTH/2` | `5` (last row: `14`) |
| `down_button_` x | `up_button_.right` if `row != 0` else `MARGIN + BUTTON_WIDTH/2` | `23` (first row: `14`) |
| both buttons | `(x, 11, 18, 18)` | |
| `up_button_` visible | `row_index_ > 0` | |
| `down_button_` visible | `row_index_ < numRows - 1` | |
| `remove_button_` | `(bounds.width − 18 − 5, 11, 18, 18)` = `(362, 11, 18, 18)` | |
| `remove_button_` visible | `!FxController::isOutputDevicePresent(name)` — i.e. only for stale entries | `GUI/FxOutputPreference.cpp:130` |
| `preset_list_` (ComboBox) | `(remove.x − 150 − 5, bounds.y, 150, bounds.h)` = `(207, 2, 150, 36)` | |
| `device_name_` | `(MARGIN*2 + BUTTON_WIDTH*2, bounds.y, preset_list.x − x − MARGIN, bounds.h)` = `(46, 2, 156, 36)` | |

> **Quirk.** `remove_button_` uses `bounds.getWidth()` (385) rather than `bounds.getRight()` (387),
> so it sits 4 px left of where the 5 px margin implies. Reproduce or correct knowingly.

Row content:
- Device label text = `sprintf("%d. ", row_index_ + 1) + device_config.device_name`
  (`GUI/FxOutputPreference.cpp:140`) — a 1-based rank prefix.
- The label is **greyed out** (`setEnabled(false)`) when
  `FxController::isOutputDeviceConnected(name)` is false (`GUI/FxOutputPreference.cpp:141`).
- Label font `getNormalFont()` (17 px), `setMinimumHorizontalScale(1.0f)` (no squeezing — it
  ellipsises instead) (`GUI/FxOutputPreference.cpp:79-80`).
- The combo box is populated once with **all** presets, item ids `1..N`
  (`GUI/FxOutputPreference.cpp:161-166`), placeholder text id **`"Select preset"`**
  (`GUI/FxOutputPreference.cpp:61`). Selection is always re-applied (id `0` clears it) because rows
  are recycled (`GUI/FxOutputPreference.cpp:178-180`).
- ComboBox colours: background `FXCOLOR(WidgetBackground)`, outline `FXCOLOR(RowOutline)`,
  focused outline `FXCOLOR(SelectedRowOutline)` (`GUI/FxOutputPreference.cpp:56-58`). On selection
  the outline flips to `SelectedRowOutline` @ 1.0; on deselection to `RowOutline` @ **0.5**
  (`GUI/FxOutputPreference.cpp:150`, `:156`).
- Arrow button images swap to the "selected" variants when the row is selected
  (`GUI/FxOutputPreference.cpp:148-155`); assets `arrow_up.svg` / `arrow_up_white.svg` /
  `arrow_down.svg` / `arrow_down_white.svg` / `remove.svg` (`GUI/FxTheme.cpp:36`).

Row separator (`GUI/FxOutputPreference.cpp:183-195`):
- Selected: `FXCOLOR(SelectedRowOutline)` @ 1.0, thickness 1.0, from
  `(device_name.x, device_name.bottom − 0.5)` to `(device_name.right, device_name.bottom − 1.0)`.
  **The two y values differ — the line is drawn with a 0.5 px slope. That is a bug**; draw it flat.
- Not selected: `FXCOLOR(RowOutline)` @ 1.0, thickness 0.5, flat at `bottom − 0.5`.

Behaviour:

| Action | Effect | Source |
|---|---|---|
| ▲ button | `moveRowUp(index)`: swap with `index−1`, persist, reselect at `index−1` | `GUI/FxOutputPreference.cpp:36-38`, `:238-248` |
| ▼ button | `moveRowDown(index)`: swap with `index+1`, persist, reselect at `index+1` | `:44-46`, `:250-260` |
| ✕ button | `deleteRow(index)`: remove entry, persist, refresh | `:52-54`, `:262-272` |
| **Shift+Up** on the focused list | same as ▲, then re-grab keyboard focus | `:339-344` |
| **Shift+Down** | same as ▼ | `:346-351` |
| Preset combo change | writes `device_config.preset`; if the row is the *current* output device, also applies that preset live via `FxController::setPreset()` | `:63-75` |

Persistence: every mutation calls `FxController::saveDeviceConfigs()`
(`GUI/FxOutputPreference.cpp:297-300`), which serialises the whole array to the settings key
**`device_configs`** as a JSON array of objects with fields
`device_id`, `device_name`, `preset`, `device_form_factor`
(`Utils/Settings/DeviceConfig.h:29-32`, `Utils/Settings/DeviceConfig.cpp:125-134`).

The list also listens to `FxModel::Event::OutputListUpdated` and
`FxModel::Event::PresetListUpdated` and to the global
`DeviceConfig::onDeviceConfigsUpdate` callback (`GUI/FxOutputPreference.cpp:199-208`, `:274-281`).

---

### 1.8 General pane — `GeneralSettingsPane`

Title string id: **`"General Preferences"`** (`GUI/FxSettingsDialog.cpp:336`).

Constants (`GUI/FxSettingsDialog.h:137-143`): `LANGUAGE_SWITCH_Y = 50`,
`TOGGLE_BUTTON_HEIGHT = 30`, `HOTKEY_LABEL_X = X_MARGIN + 25 = 45`, `HOTKEY_LABEL_HEIGHT = 20`,
`LANGUAGE_LABEL_HEIGHT = 24` (unused), `LANGUAGE_LIST_WIDTH = 120` (unused),
`LANGUAGE_LIST_HEIGHT = 30` (unused).

Resolved layout (pane 449 × 509), **with `launch_toggle_` hidden** (the normal case — see below),
from `GUI/FxSettingsDialog.cpp:420-448`:

| Widget | Bounds |
|---|---|
| `title_` | `(20, 5, 429, 24)` |
| `language_switch_` (`FxLanguage`) | `(20, 50, 180, 30)` |
| `hide_help_tips_toggle_` | `(20, 100, 429, 30)` |
| `hide_notifications_toggle_` | `(20, 140, 429, 30)` |
| `hotkeys_toggle_` | `(20, 180, 429, 30)` |
| hotkey row 1 | `(45, 215, 404, 20)` |
| hotkey row 2 | `(45, 245, 404, 20)` |
| hotkey row 3 | `(45, 275, 404, 20)` |
| hotkey row 4 | `(45, 305, 404, 20)` |
| hotkey row 5 | `(45, 335, 404, 20)` |

Gaps: 20 px after the language switch, 10 px between the second/third/fourth toggles, 5 px before
the first hotkey row, then `HOTKEY_LABEL_HEIGHT + 10 = 30` px pitch
(`GUI/FxSettingsDialog.cpp:427`, `:436`, `:439`, `:442`, `:446`).

If `launch_toggle_` **is** visible it takes `(20, 100, 429, 30)` and everything below shifts down by
50 px (`GUI/FxSettingsDialog.cpp:428-432`).

> **Dead branch.** `launch_toggle_` is only added to the pane when
> `SystemStats::getOperatingSystemType() == Windows7` (`GUI/FxSettingsDialog.cpp:402-406`). On any
> modern Windows it never appears. The Linux port should show it unconditionally.

Toggle styling (each of the four, e.g. `GUI/FxSettingsDialog.cpp:348-364`): `PointingHandCursor`,
tick colour and text colour both `TextButton::textColourOnId` (= `FXCOLOR(HighlightedText)`),
`setWantsKeyboardFocus(true)`. The tick-box geometry itself comes from JUCE's
`LookAndFeel_V4::drawToggleButton` (not overridden in `FxTheme`, and the JUCE modules are not
vendored in this tree — see Open questions).

#### Settings exposed by the General pane

| Control | String id | Type | Default | Persistence | Applied by | Source |
|---|---|---|---|---|---|---|
| Language chooser | (shows the native language name, not translated) | enum of 30 codes | `"en"` | `language` (string) | `FxController::setLanguage()` — swaps `LocalisedStrings`, reloads the font, and sends a look-and-feel change to the main window | `GUI/FxLanguage.cpp:25`, `GUI/FxController.cpp:2330-2338`, `:2459-2468` |
| Launch on startup | `"Launch on system startup"` | bool | read live from the registry | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `FxSound` | `GUI/FxController.cpp:2789-2812` | `GUI/FxSettingsDialog.cpp:337, 464` |
| Hide help tips | `"Hide help tips for audio controls"` | bool | **false** | `hide_help_tooltips` | repaints the main window | `GUI/FxSettingsDialog.cpp:338, 465`, `GUI/FxController.cpp:2307-2312` |
| Hide notifications | `"Hide notifications"` | bool | **false** | `hide_notifications` | suppresses the toast path | `GUI/FxSettingsDialog.cpp:339, 466`, `GUI/FxController.cpp:2319-2323` |
| Disable keyboard shortcuts | `"Disable keyboard shortcuts"` | bool (**inverted**) | `hotkeys` default = `1`, so the checkbox starts **unchecked** | `hotkeys` | `FxController::enableHotkeys(!checked)` → registers/unregisters Win32 hotkeys and enables/disables the five editors | `GUI/FxSettingsDialog.cpp:340, 468, 377, 384-391`, `GUI/FxController.cpp:2162-2174`, `Utils/Settings/Settings.cpp:32` |

Note the inversion: the stored key is `hotkeys` (`true` = shortcuts *enabled*), the checkbox reads
"Disable keyboard shortcuts" and is set to `!getHotkeySupport()`
(`GUI/FxSettingsDialog.cpp:377`).

When `SysInfo::canSupportHotkeys()` returns false, the checkbox is forced checked and disabled
(`GUI/FxSettingsDialog.cpp:379-383`). In this tree that function **always returns `true`**
(`Utils/SysInfo/SysInfo.cpp:133-136`) — but it is exactly the hook a Wayland port needs
(see §7).

#### Language list

30 codes, in this exact order (`GUI/FxLanguage.cpp:25`):

```
en, ar, ba, hr, cs, de, es, fi, fr, hu, id, it, ja, ko, nl, no, fa, pl, pt,
pt-br, ro, ru, sl, sv, th, tr, ua, vi, zh-CN, zh-TW
```

`FxLanguage` widget (`GUI/FxLanguage.h:29-38`, `GUI/FxLanguage.cpp:40-48`): size **180 × 30**,
rounded rect radius **5.0** filled with `FXCOLOR(ControlBackground)` @ 1.0. Prev arrow at
`(10, 4, 14, 22)`, next arrow at `(180 − 14 − 10 = 156, 4, 14, 22)`, centred label filling
`(24, 4, 132, 22)`. Arrows are `arrow_prev.svg` / `arrow_next.svg` with `_bw` disabled variants.
Wrap-around in both directions (`GUI/FxLanguage.cpp:80-95`, `:97-111`).

Display names returned by `FxController::getLanguageName()` (`GUI/FxController.cpp:2471-2594`):

| Code | Display name | Code | Display name |
|---|---|---|---|
| `en` | English | `it` | Italiano |
| `ko` | 한국어 | `ru` | русский |
| `vi` | Tiếng Việt | `ro` | Română |
| `id` | bahasa Indonesia | `tr` | Türk |
| `pt-br` | português brasileiro | `pl` | Polski |
| `pt` | Português | `de` | Deutsch |
| `es` | Español | `hu` | Magyar |
| `zh-CN` | 简体中文 | `th` | แบบไทย |
| `zh-TW` | 繁體中文 | `nl` | Nederlands |
| `sv` | svenska | `ja` | 日本語 |
| `fr` | français | `ar` | العربية |
| `hr` | hrvatski | `ba` | bosanski |
| `fa` | فارسی | `ua` | українська |
| `no` | Norsk | `sl` | Slovenščina |
| `fi` | Suomi | `cs` | Česky |

Anything unmatched falls back to `"English"` (`GUI/FxController.cpp:2593`).

> **Startup index bug worth not porting.** `FxLanguage`'s constructor matches with
> `language_code.startsWith(lng)` over the whole list without breaking
> (`GUI/FxLanguage.cpp:63-71`), so `"pt-br"` matches both `"pt"` (index 18) and `"pt-br"`
> (index 19) and keeps the *last* match — fine here, but `"en"` is also a prefix of nothing else,
> and an unknown code leaves `language_index_ = -1`, after which "next" jumps to index 0 and
> "previous" jumps to the last entry. Use an explicit exact-match lookup in Rust.

#### Hotkeys

Five commands, in this order (`GUI/FxSettingsDialog.cpp:342-344`):

| # | Label string id | Settings key (`FxController::HK_CMD_*`) | Win32 hotkey id | Stored default | Decoded default |
|---|---|---|---|---|---|
| 1 | `"Turn FxSound On/Off"` | `cmd_on_off` | `1001` | `393297` = `0x00060051` | **Ctrl + Shift + Q** |
| 2 | `"Open/Close FxSound"` | `cmd_open_close` | `1002` | `393285` = `0x00060045` | **Ctrl + Shift + E** |
| 3 | `"Use Next Preset"` | `cmd_next_preset` | `1003` | `393281` = `0x00060041` | **Ctrl + Shift + A** |
| 4 | `"Use Previous Preset"` | `cmd_previous_preset` | `1004` | `393306` = `0x0006005A` | **Ctrl + Shift + Z** |
| 5 | `"Change Playback Device"` | `cmd_change_output` | `1005` | `393303` = `0x00060057` | **Ctrl + Shift + W** |

Sources: `GUI/FxController.h:54-58` (keys), `GUI/FxController.h:219-223` (ids),
`Utils/Settings/Settings.cpp:34-38` (defaults).

Encoding (`GUI/FxController.cpp:2178-2180`, `:2213`):
`code = (mod << 16) | vk`; on read `mod = (code >> 16) & 0x7`, `vk = code & 0xff`.
`mod` uses the Win32 constants `MOD_ALT = 0x1`, `MOD_CONTROL = 0x2`, `MOD_SHIFT = 0x4`, so `0x6`
= Ctrl+Shift. `vk` is an ASCII code point.

Validation rules:
- `getHotkey()` only returns a hotkey as valid when `mod` is exactly `Ctrl+Alt` **or** exactly
  `Ctrl+Shift`, and `vk` is `'0'..'9'` (`0x30..0x39`) or `'A'..'Z'` (`GUI/FxController.cpp:2181`).
- The editor rejects any chord without Ctrl and without (Alt or Shift)
  (`GUI/FxHotkeyLabel.cpp:104-107`).
- It rejects duplicates against all five commands, checked one by one, *before* assigning
  (`GUI/FxHotkeyLabel.cpp:115-152`); `FxController::setHotkey()` re-checks the same thing
  (`GUI/FxController.cpp:2194-2211`).
- **Delete** clears the binding: `mod = vk = 0`, stored as `0` (`GUI/FxHotkeyLabel.cpp:78-85`).
- If `RegisterHotKey` fails the key is stored as `0` and `setHotkey` returns false, which makes the
  editor revert to unassigned (`GUI/FxController.cpp:2256-2265`, `GUI/FxHotkeyLabel.cpp:159-163`).

`FxHotkeyLabel` (`GUI/FxHotkeyLabel.h:44-58`, `GUI/FxHotkeyLabel.cpp:34-43`):
`HOTKEY_LABEL_WIDTH = 170`; the name label occupies `(0, 0, 170, 20)` and the editor is moved to
`(171, 0, 120, 20)`. So each row is **291 px** of live content inside its 404 px slot. Name font:
`getSmallFont()` (14 px), top-left justified.

`FxHotkeyEditor` (`GUI/FxHotkeyLabel.h:22-42`, `GUI/FxHotkeyLabel.cpp:56-222`):
- `HOTKEY_EDITOR_WIDTH = 120`, `HOTKEY_EDITOR_HEIGHT = 20`, font `getSmallFont()` (14 px),
  centred.
- Border: rounded rect, radius **5.0**, thickness **2** when focused, **1** otherwise (and always 1
  when disabled), colour `TextEditor::textColourId` (= `FXCOLOR(DefaultText)`).
- Text colour: `TextEditor::highlightedTextColourId` (= `FXCOLOR(HighlightedText)`) when the
  binding is valid, otherwise `TextEditor::textColourId`.
- Tooltip string id: **`"Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey"`**.
- Rendered text: `" " + TRANS("Ctrl") + " + "` then optionally `" " + TRANS("Alt") + " + "` and/or
  `" " + TRANS("Shift") + " + "`, then the literal character. When `mod == 0` the text is
  `TRANS("Not configured")` (`GUI/FxHotkeyLabel.cpp:224-256`).
  Note the leading space and the spaces around `+` are part of the rendered string.
- `setMouseClickGrabsKeyboardFocus(true)` — click to focus, then type the chord.

---

### 1.9 Help pane — `HelpSettingsPane`

Title string id: **`"Help"`** (`GUI/FxSettingsDialog.cpp:471`).

Constants (`GUI/FxSettingsDialog.h:165-170`): `TEXT_Y = 50`, `TITLE_HEIGHT = 24` (shadows the
base's, same value), `TEXT_HEIGHT = 20`, `HYPERLINK_HEIGHT = 24`, `TOGGLE_BUTTON_HEIGHT = **24**`
(differs from the other panes' 30), `BUTTON_WIDTH = 220`.

Resolved layout (pane 449 × 509), from `GUI/FxSettingsDialog.cpp:512-524`:

| Widget | String id | Bounds |
|---|---|---|
| `title_` | `"Help"` | `(20, 5, 429, 24)` |
| `version_title_` | `"Version"` | `(20, 50, 429, 24)` |
| `version_text_` | `"v" + applicationVersion` (**not** translated) | `(20, 84, 429, 20)` |
| `changelog_link_` | `"Changelog"` → `https://www.fxsound.com/changelog` | `(25, 114, 429, 24)` |
| `support_title_` | `"Support"` | `(20, 158, 429, 24)` |
| `helpcenter_link_` | `"Help center"` → `https://www.fxsound.com/learning-center` | `(25, 192, 429, 24)` |
| `maintenance_title_` | `"Maintenance"` | `(20, 236, 429, 24)` |
| `auto_updates_toggle_` | `"Automatic updates"` | `(25, 270, 220, 24)` |

Gaps: 10 px title→text, 10 px text→link, 20 px link→next section title
(`GUI/FxSettingsDialog.cpp:517-523`).

Fonts: section titles `getNormalFont()` (17 px); `version_text_` uses `getSmallFont()` (14 px)
(`GUI/FxSettingsDialog.cpp:539-549`). Section-title colour `TextButton::textColourOnId`.

Hyperlink rendering (`GUI/FxHyperlink.cpp:27-42`): `getNormalFont()` with `underline = true`,
colour `ColourScheme::defaultText` (alpha × 0.4 when disabled), justification `topLeft` for all
four links in this pane.

**Declared but never shown** — the port should decide whether to implement or drop them:

| Member | String id | URL | Status |
|---|---|---|---|
| `quicktour_link_` | `"Quick tour"` | (none set) | `addChildComponent` only → invisible (`GUI/FxSettingsDialog.cpp:506`) |
| `submitlogs_link_` | `"Submit debug logs"` | (none set) | invisible (`GUI/FxSettingsDialog.cpp:507`) |
| `feedback_link_` | `"Feedback"` | `https://james722808.typeform.com/to/QfEP5QrP` | **never added to the pane at all** (`GUI/FxSettingsDialog.cpp:487-488`, absent from `:501-509`) |
| `debug_log_toggle_` | — | — | declared at `GUI/FxSettingsDialog.h:184`, never constructed, laid out or added |

#### Settings exposed by the Help pane

| Control | String id | Type | Default | Persistence key | Source |
|---|---|---|---|---|---|
| Automatic updates | `"Automatic updates"` | bool | **true** | `automatic_updates` | `GUI/FxController.cpp:193` (`getBool("automatic_updates", true)`), `:2606-2610` |

The update mechanism itself launches `updater.exe /silent`, throttled to once per
`24*60*60` seconds via the `last_update_time` key (`GUI/FxController.cpp:2612-2627`). The menu's
"check for updates" runs `updater.exe /checknow` (`GUI/FxMainWindow.cpp:485-487`). Neither has a
Linux analogue — see §7.

---

## 2. `FxPresetImportDialog` — Import Presets

Files: `GUI/FxPresetImportDialog.h`, `GUI/FxPresetImportDialog.cpp`.

| Property | Value | Source |
|---|---|---|
| Title (string id) | `"Import Presets"` | `GUI/FxPresetImportDialog.cpp:171` |
| Content size | 400 × 400 | `GUI/FxPresetImportDialog.h:51-52`, `:226` |
| Outer size | 410 × 487 | derived |
| Placement | centred | `GUI/FxPresetImportDialog.cpp:175` |
| Desktop | `addToDesktop(0)`, `toFront(true)`, **`setAlwaysOnTop(true)`** | `:176-178` |
| Escape | closes | `:181-191` |
| Close button | `exitModalState(0); removeFromDesktop();` | `:193-197` |
| Invoked from | main-window menu → `runModalLoop()` | `GUI/FxMainWindow.cpp:474-478` |

Constants (`GUI/FxPresetImportDialog.h:53-56`): `BUTTON_WIDTH = 80`, `BUTTON_HEIGHT = 30`,
`TEXT_HEIGHT = 20`, `DIR_SELECTION_HEIGHT = 310`.

Layout (`GUI/FxPresetImportDialog.cpp:229-253`): working rect starts at `top = 10` then
`reduce(20, 0)` → inner x range 20…380, width **360**.

```
content 400 x 400
+----------------------------------------------------------+
| 20                                                    380 |
|   (20,10,360,20)  "Select the folder which contains the   |
|                    presets..."                            |
|                                                           |
|   (20,40,360,310)  FileBrowserComponent                   |
|   +-----------------------------------------------------+ |
|   | current path combo                                  | |
|   | ........ directory tree / file list .............   | |
|   | Folder: [ .......................... ]              | |
|   +-----------------------------------------------------+ |
|                                                           |
|                             (300,360,80,30)  [  Import  ] |
+----------------------------------------------------------+
```

| Widget | Bounds | String id |
|---|---|---|
| `select_dir_label_` | `(20, 10, 360, 20)` | `"Select the folder which contains the presets..."` |
| `import_dir_select_` | `(20, 40, 360, 310)` | — |
| `import_button_` | `(300, 360, 80, 30)`, right-aligned | `"Import"` |

File browser configuration (`GUI/FxPresetImportDialog.cpp:199-214`):
- Flags: `openMode | canSelectDirectories` — it is a **directory** picker.
- Start directory: `File::SpecialLocationType::userDocumentsDirectory`
  (Windows `%USERPROFILE%\Documents`).
- No file filter object, no preview component.
- Colours: `currentPathBoxBackgroundColourId` and `filenameBoxBackgroundColourId` →
  `FXCOLOR(DefaultFill)` @ 1.0; the two matching text colours → `FXCOLOR(DefaultText)` @ 1.0.
- Filename-box label string id: **`"Folder:"`**.
- Label font `getNormalFont()` (17 px), colour `TextButton::textColourOnId`, centred-left.

### 2.1 Import action and validation

`GUI/FxPresetImportDialog.cpp:255-278`:

1. `import_path = import_dir_select_.getSelectedFile(0)`.
2. Non-recursive glob for `*.fac` in that directory
   (`FileSearchPath::findChildFiles(File::findFiles, false, "*.fac")`).
3. **Error case** — if the glob is empty: show
   `FxConfirmationMessage::showMessage(TRANS("Preset files not found in the selected folder."), Style::OK)`
   and **return without closing** the import dialog.
4. Otherwise `FxController::importPresets(paths, imported, skipped)`.
5. Close the import dialog (`exitModalState(0)`, `removeFromDesktop()`), then construct
   `FxImportCompleteMessage` on the stack and `runModalLoop()` it.

`FxController::importPresets()` (`GUI/FxController.cpp:1419-1458`):
- Destination directory: `userApplicationDataDirectory` + `\FxSound\Presets`
  (Windows `%APPDATA%\FxSound\Presets`), created if missing.
- For each `.fac`: read the embedded preset name via `DfxDsp::getPresetInfo()`. If
  `FxModel::isPresetNameValid(name)` (i.e. no existing preset with that name,
  **case-insensitive** — `GUI/FxModel.cpp:142-153`), copy the file to
  `<dest>\<name>.fac` and add to `imported_presets`; otherwise add to `skipped_presets`.
- If anything was imported: `initPresets()` and re-select the preset named by the `preset` setting.

> **Re-entrancy hazard.** Step 5 tears down the modal dialog from *inside* its own button handler
> and then starts a second nested modal loop. In Rust this is a straightforward state machine —
> model it as `ImportState::{Browsing, Complete{imported, skipped}}` rather than nested loops.

### 2.2 `FxImportCompleteMessage` — the import summary

Defined privately in `GUI/FxPresetImportDialog.cpp:21-169`. `FxWindow` with an **empty** name, so
the title bar shows the big logo, no title text.

| Property | Value | Source |
|---|---|---|
| Content size | 350 × 340 | `:104-105`, `:99` |
| Outer size | 360 × 427 | derived |
| Placement | centred | `:28` |
| Desktop | `addToDesktop(0)`, `toFront(true)`, `setAlwaysOnTop(true)` | `:29-31` |
| Escape | **not handled** (no `keyPressed` override) | — |

Constants (`:104-109`): `BUTTON_WIDTH = 50`, `BUTTON_HEIGHT = 30`, `TEXT_HEIGHT = 20`,
`LIST_HEIGHT = 100`. (`WIDTH`/`HEIGHT` are the 350/340 above.)

Layout (`:111-146`): working rect `top = 10`, `reduce(20, 0)` → x 20…330, width **310**.

| Widget | Bounds | String id / content |
|---|---|---|
| `imported_presets_label_` | `(20, 10, 310, 20)` | `"Presets successfully imported"` |
| `imported_presets_text_` | `(20, 40, 310, 100)` | newline-joined imported names, read-only |
| `skipped_presets_label_` | `(20, 150, 310, 20)` | `"Duplicate presets not imported"` |
| `skipped_presets_text_` | `(20, 180, 310, 100)` | newline-joined skipped names, read-only |
| `ok_button_` | `(150, 300, 50, 30)`, horizontally centred | `"OK"` |

Both text boxes: multi-line, read-only, caret hidden, scrollbars shown, scrollbar thickness
**10** (`:55-59`, `:77-81`). Labels use `getNormalFont()` (17 px), colour
`TextButton::textColourOnId`, top-left.

OK dismisses via `getParentComponent()->exitModalState(0)` + `removeFromDesktop()`
(`:148-155`).

> **Off-by-one in the text builder** (`:61-69` and `:83-91`): the guard is
> `if (i != imported_presets.size())`, which is always true inside a `i < size` loop, so **every**
> line including the last gets a trailing `"\n"`. Harmless, but do not replicate the confusion —
> just `join("\n")`.

---

## 3. `FxPresetExportDialog` — Export Presets

Files: `GUI/FxPresetExportDialog.h`, `GUI/FxPresetExportDialog.cpp`.

| Property | Value | Source |
|---|---|---|
| Title (string id) | `"Export Presets"` | `GUI/FxPresetExportDialog.cpp:21` |
| Content size | 400 × 405 | `GUI/FxPresetExportDialog.h:67-68`, `:105` |
| Outer size | 410 × 492 | derived |
| Placement | centred | `:25` |
| Desktop | `addToDesktop(0)`, `toFront(true)`, `setAlwaysOnTop(true)` | `:26-28` |
| Escape | closes | `:31-41` |
| Invoked from | main-window menu → `runModalLoop()` | `GUI/FxMainWindow.cpp:469-473` |

Constants (`GUI/FxPresetExportDialog.h:69-72`): `BUTTON_WIDTH = 80`, `BUTTON_HEIGHT = 30`,
`TEXT_HEIGHT = 20`, `LIST_HEIGHT = 310`.

Layout (`GUI/FxPresetExportDialog.cpp:108-136`): working rect `top = 10`, `reduce(20, 0)` →
x 20…380, width **360**.

```
content 400 x 405
+----------------------------------------------------------+
|   (20,10,360,20)   "Select the presets to export..."      |
|                                                           |
|   (20,40,360,310)  multi-select preset ListBox            |
|   +-----------------------------------------------------+ |
|   | Preset A                                            | |
|   | Preset B          <- selected rows fill ImageButton | |
|   | ...                                                 | |
|   +-----------------------------------------------------+ |
|   (0,360,400,2)    animated progress bar (hidden)         |
|                             (300,372,80,30)  [  Export  ] |
+----------------------------------------------------------+
```

| Widget | Bounds | String id |
|---|---|---|
| `select_presets_label_` | `(20, 10, 360, 20)` | `"Select the presets to export..."` |
| `preset_list_` | `(20, 40, 360, 310)` | — |
| `preset_export_progress_` | `(0, 360, 400, 2)` — spans the full content width, ignoring the 20 px margins | — |
| `export_button_` | `(300, 372, 80, 30)`, right-aligned | `"Export"` |

List configuration (`GUI/FxPresetExportDialog.cpp:83-89`):
- Background, outline: `FXCOLOR(DefaultFill)` @ 1.0; text `FXCOLOR(DefaultText)` @ 1.0.
- Row height `TEXT_HEIGHT + 6` = **26**.
- Multiple selection enabled, click toggles row selection.
- Row painting (`:143-156`): selected rows fill with `FXCOLOR(ImageButton)` @ 1.0; text is drawn in
  `FXCOLOR(HighlightedText)` @ 1.0 with `getNormalFont()` (17 px), centred-left, in a rect inset by
  `reduce(10, 0)`, with ellipsis on overflow.
- Rows come straight from `FxModel::getPresetCount()` / `getPreset(i)` — **all** presets, both
  factory and user (`:138-141`, `:150`).

Button enablement (`GUI/FxPresetExportDialog.cpp:92`, `:158-168`): disabled at construction, then
enabled iff `getNumSelectedRows() > 0`, checked on every selection change.

Progress bar `PresetExportProgress` (`GUI/FxPresetExportDialog.h:42-52`,
`GUI/FxPresetExportDialog.cpp:49-72`): an `AnimatedAppComponent` at **30 fps**; each frame advances
`colour_gradient_start_` by `0.01` and wraps at `1.0` (so a full cycle takes 100 frames ≈ 3.33 s).
It paints a horizontal `ColourGradient` from `FXCOLOR(ImageButton)` at
`x = colour_gradient_start_ * width` to `FXCOLOR(VerticalSliderLow)` at `x = width`, filled into a
rounded rect of radius `height / 2` (= 1 with height 2). Hidden until Export is pressed
(`addChildComponent`, `:102`; `setVisible(true)`, `:180`).

### 3.1 Export action, confirmation and error paths

`GUI/FxPresetExportDialog.cpp:170-201`:

1. Disable the Export button, show the progress bar, raise the parent window
   (`toFront(true)`).
2. Collect the selected `FxModel::Preset`s and call `FxController::exportPresets(presets)`.
3. If it returns true, show
   `FxConfirmationMessage::showMessage(TRANS("Presets are exported successfully!"), Style::OK)`
   then `File(...).revealToUser()` on
   `%USERPROFILE%\Documents\FxSound\Presets\Export\` — i.e. open Explorer at that folder.
4. Close the export dialog unconditionally.

`FxController::exportPresets()` (`GUI/FxController.cpp:1384-1417`):
- Destination `Documents\FxSound\Presets\Export\`, created if missing.
- Per preset, if `<name>.fac` already exists there, show a **Yes/No** confirmation with the format
  string **`"Preset file %s already exists in the export path, do you want to overwrite the preset file?"`**
  (`%s` ← the preset name, substituted by `FxController::FormatString`). "No" skips that preset.
- Writes through `DfxDsp::exportPreset()`.
- Returns true if at least one preset was written.

So a multi-preset export with several collisions opens **several sequential nested modal
Yes/No dialogs**, one per colliding file.

---

## 4. `FxConfirmationMessage` — the modal message box

Defined entirely inline in `GUI/FxMessage.h:72-241`.

| Property | Value | Source |
|---|---|---|
| Window name | **empty** → logo title bar, no title text | `GUI/FxMessage.h:77` |
| Content size | 450 × 142 | `GUI/FxMessage.h:177-178`, `:165` |
| Outer size | 460 × 229 | derived |
| Placement | `setTopLeftPosition(x, y)` if both ≥ 0, else centred | `GUI/FxMessage.h:80`, `:105-107` |
| Desktop | `addToDesktop(0)`, `setAlwaysOnTop(true)` — note **no** `toFront` | `GUI/FxMessage.h:82-83` |
| Escape | **not handled** | — |
| Close button | `exitModalState(0)` + `removeFromDesktop()` (returns "No"/false for `YesNo`) | `GUI/FxMessage.h:90-94` |

Styles: `enum Style { YesNo = 1, OK }` (`GUI/FxMessage.h:75`) — note `YesNo` is **1**, `OK` is 2.

`showMessage(message, style = YesNo, x = -1, y = -1) -> bool` (`GUI/FxMessage.h:99-122`)
constructs the window on the stack, centres it when `x == -1 || y == -1`, runs a modal loop, and
returns `yes_clicked_` for `YesNo` or unconditional `true` for `OK`.

Constants (`GUI/FxMessage.h:179-181`): `MESSAGE_HEIGHT = (24 + 2) * 2 = 52`,
`BUTTON_WIDTH = 120`, `BUTTON_HEIGHT = 30`.

Layout (`GUI/FxMessage.h:183-215`): working rect `top = 20`, `reduce(20, 0)` → x 20…430,
width **410**.

```
content 450 x 142            Style::YesNo
+------------------------------------------------------------+
|                                                            |
|   (20,20,410,52)   message, getNormalFont() 17px, CENTRED  |
|                    (two lines' worth of box)               |
|                                                            |
|      (95,92,120,30) [   Yes   ]   (235,92,120,30) [  No  ] |
+------------------------------------------------------------+

content 450 x 142            Style::OK
|      (165,92,120,30)          [    OK    ]                 |
```

| Widget | Bounds | String id |
|---|---|---|
| `message_` | `(20, 20, 410, 52)`, `Justification::centred` | runtime text |
| `yes_button_` | `((450 − (120*2 + 20)) / 2, 92, 120, 30)` = `(95, 92, 120, 30)` | `"Yes"` |
| `no_button_` | `(yes.right + 20, 92, 120, 30)` = `(235, 92, 120, 30)` | `"No"` |
| `ok_button_` | horizontally centred in the 410-wide rect → `(165, 92, 120, 30)` | `"OK"` |

All three buttons: `PointingHandCursor`; `yes_button_` also `setWantsKeyboardFocus(true)`
(`GUI/FxMessage.h:141`). Any button click sets `yes_clicked_` only for Yes and then dismisses
(`GUI/FxMessage.h:217-226`).

**Call sites** (every user-visible use of this class):

| Message string id | Style | Where |
|---|---|---|
| `"Preset file %s already exists in the export path, do you want to overwrite the preset file?"` | YesNo | `GUI/FxController.cpp:1403` |
| `"Preset files not found in the selected folder."` | OK | `GUI/FxPresetImportDialog.cpp:264` |
| `"Presets are exported successfully!"` | OK | `GUI/FxPresetExportDialog.cpp:195` |

---

## 5. `FxMessage` — link-carrying message window (currently dead code)

Files: `GUI/FxMessage.h:33-70`, `GUI/FxMessage.cpp`.

| Property | Value | Source |
|---|---|---|
| Window name | empty → logo title bar | `GUI/FxMessage.cpp:22` |
| Content size | 400 × 80 | `GUI/FxMessage.h:56-57`, `GUI/FxMessage.cpp:72` |
| Outer size | 410 × 167 | derived |
| Placement | centred | `GUI/FxMessage.cpp:25` |
| Desktop | `addToDesktop(ComponentPeer::windowAppearsOnTaskbar)` — **the only dialog here that gets a taskbar entry**, plus `toFront(true)` and `setAlwaysOnTop(true)` | `GUI/FxMessage.cpp:26-28` |
| Escape | closes | `GUI/FxMessage.cpp:31-41` |

Constants (`GUI/FxMessage.h:58-59`): `MESSAGE_HEIGHT = 24 + 2 = 26`, `HYPERLINK_HEIGHT = 24`.

Layout (`GUI/FxMessage.cpp:75-93`): working rect `top = 10`, `reduce(20, 0)` → width **360**.

| Widget | Bounds | Notes |
|---|---|---|
| `message_` | `(20, 10, 360, 26)`, `centredTop` | font `getSmallFont().withHeight(17.0f)` — Gilroy **Regular** at 17 px, *not* `getNormalFont()` |
| `link_` | `(20, 46, 360, 24)`, `centredTop` | only laid out when the link text is non-empty |

The link is added only when **both** `link.first` (text) and `link.second` (URL) are non-empty;
`link.first` is passed through `TRANS()` (`GUI/FxMessage.cpp:64-70`).

> **Status: unused.** `FxMessage::showMessage()` (`GUI/FxMessage.cpp:49-53`) has no call site in
> the tree; `FxMessage.h` is included only for its `FxConfirmationMessage` half
> (`GUI/FxController.cpp:25`, `GUI/FxPresetImportDialog.h:26`, `GUI/FxPresetExportDialog.h:26`).
> Do not port it unless you want a modal variant of the toast.

---

## 6. `FxNotification` — the transient toast

Files: `GUI/FxNotification.h`, `GUI/FxNotification.cpp`. This is **not** an `FxWindow` — it is a
bare `Component` that is either pushed to the desktop as its own borderless always-on-top window
(autohide mode) or parented inside the main view (persistent mode).

### 6.1 Constants

| Constant | Value | Source |
|---|---|---|
| `WIDTH` | 216 | `GUI/FxNotification.h:33` |
| `HEIGHT` | 80 | `GUI/FxNotification.h:34` |
| `MAX_WIDTH` | 560 | `GUI/FxNotification.h:35` |
| `MAX_HEIGHT` | 120 | `GUI/FxNotification.h:36` |
| `ICON_WIDTH` | 79 | `GUI/FxNotification.h:42` |
| `ICON_HEIGHT` | 12 | `GUI/FxNotification.h:43` |
| `AD_WIDTH` / `AD_HEIGHT` | 216 / 36 (declared, unused) | `GUI/FxNotification.h:44-45` |
| `TITLE_HEIGHT` / `HYPERLINK_HEIGHT` | 24 / 24 (declared, unused) | `GUI/FxNotification.h:46-47` |
| max message lines | **3** | `GUI/FxNotification.cpp:53`, `:103`, `:153` |

### 6.2 Painting

`GUI/FxNotification.cpp:202-213`: `DropShadow` with `radius = 5` over a rounded rect of the full
size with corner radius **16**, then fill the same rounded rect with `FXCOLOR(DefaultFill)` @ 1.0.

Logo: `DefaultLogo` SVG transformed to fit `(15.0, 10.0, 79, 12)`, x-mid/y-mid
(`GUI/FxNotification.cpp:48-50`). Hidden in non-autohide mode (`:196`).

Message lines: three `Label`s, colour `ColourScheme::defaultText`, border
`BorderSize<int>(1, 0, 2, 0)` (top 1, left 0, bottom 2, right 0),
`setMinimumHorizontalScale(1.0)`, font `getSmallFont().withHeight(17.0f)`
(`GUI/FxNotification.cpp:55-58`, `:80`).

### 6.3 Auto-sizing (`setMessage`, `GUI/FxNotification.cpp:42-145`)

- The message is split on newlines; at most 3 lines are used.
- Per-line justification: `centred` when there is no link, `centredLeft` when there is
  (`:113`, `:122`).
- `margin = 80` when `autohide`, else `40` (`:125`).
- `line_width = font.getStringWidth(line) + link_width`.
- If `line_width > WIDTH − margin` (216 − 80 = 136, or 216 − 40 = 176):
  - if `line_width > MAX_WIDTH − margin` (560 − 80 = 480, or 560 − 40 = 520) → `width = MAX_WIDTH`
    (560) and, if there is a link, the link is pushed onto line `i + 1`;
  - else `width = line_width + margin`.
- Final size: `setSize(width, line_count * 20 + 60)` — so 1 line → 80 px, 2 → 100, 3 → 120
  (= `MAX_HEIGHT`).

### 6.4 Showing (`showMessage(bool autohide = true)`, `GUI/FxNotification.cpp:147-200`)

- `x = 40` when autohide, else `20`.
- Line *i* is placed at `(x, i * 20 + 30, width − 2x, 20)`.
- The link is placed at `(x [+ width of the last line's text if link_line_ == last_line], link_line_ * 20 + 30, link_text_width, 20)`,
  i.e. inline after the final text line, or on its own line if the text was too wide.
- **autohide = true**: `addToDesktop(0)`, `toFront(true)`, fade in over **200 ms**, then start a
  one-shot timer:
  - **7000 ms** when there is no link text *and* no URL;
  - **8000 ms** otherwise.
  `timerCallback()` stops the timer, hides and `removeFromDesktop()` (`:215-220`).
- **autohide = false**: hides the logo, fades in over 200 ms, `setVisible(true)`; **no timer** —
  the caller hides it.

Re-entrancy guard (`GUI/FxNotification.cpp:65-75`): if a timer is already running, a *new*
message replaces the old one only when the running interval is exactly `7000`; an 8000 ms
(link-bearing) notification cannot be pre-empted.

### 6.5 Placement and the two call paths

**Tray toast (autohide).** `FxSystemTrayView::showNotification()`
(`GUI/FxSystemTrayView.cpp:383-416`):
1. `FxModel::popMessage(message, link)`.
2. If `custom_notification_` (set true at `GUI/FxSystemTrayView.cpp:32`) **or** the link text is
   non-empty → use `FxNotification`, but first gate on
   `SHQueryUserNotificationState() == QUNS_ACCEPTS_NOTIFICATIONS` (suppresses toasts during
   presentations/full-screen/quiet hours).
3. Position via `getSystemTrayWindowPosition(w, h)` (`GUI/FxSystemTrayView.cpp:123-165`): finds the
   tray icon's screen rect with `Shell_NotifyIconGetRect`, converts physical→logical, then snaps to
   the corner of the display's `userArea` nearest the tray icon, with a **10 px** inset on both
   axes.
4. Otherwise fall back to a native balloon tip (`Shell_NotifyIcon(NIM_MODIFY, …)` with
   `NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME`), title `"FxSound"`.

**In-window error banner (persistent).** `FxView::showErrorNotification(bool)`
(`GUI/FxView.cpp:58-77`): a second `FxNotification` instance added as a **child component** of the
view, positioned directly under the playback-device combo box, right-aligned to it, forced to
`MAX_WIDTH × MAX_HEIGHT` = **560 × 120**, offset `+5` px below the combo, shown with
`showMessage(false)`. It is shown/hidden on mouse-enter/exit over the combo or the banner itself
(`GUI/FxView.cpp:207-214`) and hidden on other model events (`GUI/FxView.cpp:140`).

Its string id (a 3-line message, note the embedded `\n`s and the trailing space before the link):

```
FxSound is unable to play processed audio through the selected output device.
Another application could be using it in exclusive mode or the device could be
disconnected. To disable exclusive mode follow these 
```
link text `"steps."` → `https://www.fxsound.com/learning-center/no-sound-with-fxsound-realtek`
(`GUI/FxView.cpp:62-63`).

### 6.6 Every message that can reach the toast

`FxModel::pushMessage(message, link)` (`GUI/FxModel.h:170-175`) stores one pending message and
fires `Event::Notification`. Producers:

| String id (format) | Argument | Source |
|---|---|---|
| `" "` with link `"Click here to see what's new on this version!"` → `https://www.fxsound.com/changelog` | — | `GUI/FxController.cpp:719` |
| `"FxSound in system tray\r\nClick FxSound icon to reopen"` | — | `GUI/FxController.cpp:924` |
| (survey message) with link `"Take the survey."` → `https://forms.gle/ATx1ayXDWRaMdiR59` | — | `GUI/FxController.cpp:957` |
| `"Preset: "` + preset name | — | `GUI/FxController.cpp:1101` |
| `"Output Disconnected"` | — | `GUI/FxController.cpp:1170` |
| `"Changes to preset %s are saved."` | preset name | `GUI/FxController.cpp:1221` |
| `"New preset %s is saved."` | preset name | `GUI/FxController.cpp:1234` |
| `"Reached the limit on new presets."` | — | `GUI/FxController.cpp:1239` |
| `"Preset %s is deleted."` | preset name | `GUI/FxController.cpp:1313` |
| `"Presets are restored to factory defaults"` | — | `GUI/FxController.cpp:1381` |
| `"FxSound is %s."` | command-line parameter | `GUI/FxController.cpp:1933` |

---

## 7. Complete list of user-visible string ids in this subsystem

These are the exact literals passed to `TRANS()`; they double as the msgid in the JUCE
`LocalisedStrings` `.txt` translation files (loaded from `BinaryData`, one per language, at
`GUI/FxController.cpp:2340-2457`). There are **no translation files in this source tree** — only
the `BinaryData` symbol names — so the English literal *is* the key.

**Settings window**
```
Settings
Audio
General
Help
General Preferences
Output Device Preference
Prioritize new output devices
Reset presets to factory defaults
Launch on system startup
Hide help tips for audio controls
Hide notifications
Disable keyboard shortcuts
Turn FxSound On/Off
Open/Close FxSound
Use Next Preset
Use Previous Preset
Change Playback Device
Version
Support
Maintenance
Changelog
Quick tour
Submit debug logs
Help center
Feedback
Automatic updates
```

**Output preference list / hotkey editor**
```
Select preset
Use Shift+Up or Shift+Down to change the device priority
Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey
Not configured
Ctrl
Alt
Shift
```

**Import**
```
Import Presets
Import
Select the folder which contains the presets...
Folder:
Preset files not found in the selected folder.
Presets successfully imported
Duplicate presets not imported
OK
```

**Export**
```
Export Presets
Export
Select the presets to export...
Presets are exported successfully!
Preset file %s already exists in the export path, do you want to overwrite the preset file?
```

**Confirmation buttons**
```
Yes
No
OK
```

**Toasts** — see the table in §6.6.

Not translated: `"v" + applicationVersion` (`GUI/FxSettingsDialog.cpp:542`), device names, preset
names, language display names.

---

## 8. Persistence summary

All settings go through `FxSound::Settings` (`Utils/Settings/Settings.h:34-58`), a JUCE
`PropertiesFile` with `applicationName = "FxSound"`, `folderName = "FxSound"`, suffix
`"settings"` (plus a parallel `"secure"` store) — on Windows that resolves to
`%APPDATA%\FxSound\FxSound.settings`, an XML key/value file. A read-only fallback set supplies
defaults (`Utils/Settings/Settings.cpp:28-65`).

Keys touched by this subsystem:

| Key | Type | Default | Written by |
|---|---|---|---|
| `device_configs` | JSON array of `{device_id, device_name, preset, device_form_factor}` | seeded from enumerated devices | output-preference list, device hot-plug |
| `device_configs_version` | int | `2` | `FxController::initOutputs` |
| `prioritize_new_output` | bool | `false` | Audio pane checkbox |
| `language` | string | `"en"` | language switch |
| `hide_help_tooltips` | bool | `false` | General pane |
| `hide_notifications` | bool | `false` | General pane |
| `hotkeys` | bool | `true` (XML default `1`) | General pane (inverted checkbox) |
| `cmd_on_off` | int | `393297` (Ctrl+Shift+Q) | hotkey editor |
| `cmd_open_close` | int | `393285` (Ctrl+Shift+E) | hotkey editor |
| `cmd_next_preset` | int | `393281` (Ctrl+Shift+A) | hotkey editor |
| `cmd_previous_preset` | int | `393306` (Ctrl+Shift+Z) | hotkey editor |
| `cmd_change_output` | int | `393303` (Ctrl+Shift+W) | hotkey editor |
| `automatic_updates` | bool | `true` | Help pane |
| `last_update_time` | int (unix seconds) | `0` | update check throttle |
| `preset` | string | `"General"` | preset selection (read back after import) |
| `max_user_presets` | int | `120` | seeded at first run (`GUI/FxController.cpp:194-197`) |

Registry (Windows-only): `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `FxSound`
for "Launch on system startup" (`GUI/FxController.cpp:2789-2812`).

Filesystem paths:

| Purpose | Windows path | Source |
|---|---|---|
| Imported presets land here | `%APPDATA%\FxSound\Presets\<name>.fac` | `GUI/FxController.cpp:1421`, `:1435` |
| Exported presets land here | `%USERPROFILE%\Documents\FxSound\Presets\Export\<name>.fac` | `GUI/FxController.cpp:1386`, `:1400` |
| Import browser start dir | `%USERPROFILE%\Documents` | `GUI/FxPresetImportDialog.cpp:201` |
| Preset file extension | `.fac` | `GUI/FxPresetImportDialog.cpp:261` |

---

## 9. Porting to Rust + egui/eframe 0.36 on Wayland

### 9.1 Window model — one deferred viewport per dialog

Every dialog here is an **OS-level, undecorated, self-dragged, centred, (mostly) always-on-top
window** that blocks its caller. Two viable mappings:

**Recommended: `egui::Modal` inside the main viewport for the small message boxes, and a deferred
viewport for the three big dialogs.**

```rust
// Settings / Import / Export: real windows.
ctx.show_viewport_deferred(
    egui::ViewportId::from_hash_of("fxsound.settings"),
    egui::ViewportBuilder::default()
        .with_title("Settings")                  // shown in the taskbar / app switcher
        .with_inner_size([610.0, 597.0])         // §0.2
        .with_min_inner_size([610.0, 597.0])
        .with_resizable(false)
        .with_decorations(false)                 // FxSound draws its own title bar
        .with_transparent(true)                  // needed for the 21px rounded corners
        .with_app_id("fxsound"),                 // Wayland app_id -> icon + window rules
    move |ctx, _class| { /* draw title bar + panes */ },
);
```

Key differences from Win32/JUCE you must design around:

| JUCE / Win32 behaviour | Wayland reality | What to do |
|---|---|---|
| `centreWithSize()` | A Wayland client **cannot position its own toplevel**. `winit`'s `set_outer_position` is a no-op. | Drop explicit centring; let the compositor place it. For a true "centred over parent" feel, use `xdg_dialog_v1` / `ViewportBuilder::with_parent` so the compositor centres the child itself. |
| `FxConfirmationMessage(x, y)` explicit placement | same | The only call sites use the default `-1, -1` (centred), so nothing is lost. |
| `setAlwaysOnTop(true)` (import/export/message windows) | No client-side always-on-top on Wayland. | Use a modal child window (`with_parent` + the `xdg_dialog_v1` "modal" hint where available); the compositor will keep it above its parent. Otherwise accept the loss. |
| `runModalLoop()` (blocking nested event loop) | egui is immediate-mode; there is no nested loop. | Model as state: `enum DialogState { None, Settings, Import(ImportState), Export(ExportState), Confirm{ text, style, on_result: …} }`. `FxConfirmationMessage::showMessage() -> bool` becomes an async/continuation: the caller stores what to do with `true`/`false`. |
| Custom title bar dragging via `ComponentDragger` | Must be a **compositor-initiated** move. | On drag start in your title-bar rect call `ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag)`. Do not track pointer deltas yourself — it will not work under Wayland. |
| `addToDesktop(ComponentPeer::windowAppearsOnTaskbar)` (only `FxMessage`) | `app_id` governs grouping; there is no per-window taskbar opt-in. | Ignore; `FxMessage` is dead code anyway. |
| Drop shadow painted by the app (`radius 5`) | The compositor may or may not draw one for CSD windows. | Keep painting your own inside a transparent window, exactly as JUCE does: 5 px transparent gutter, content inset by 5. |
| Escape closes | Same. | Handle `Key::Escape` in the viewport's input; send `ViewportCommand::Close`. |

If you decide multi-window CSD is more trouble than it is worth, the fallback is a **single**
viewport with `egui::Modal` (egui 0.36 has `egui::Modal`, which dims the backdrop and traps focus)
for every dialog. That maps the modality exactly and costs you the ability to drag Settings away
from the main window. Given that the Settings window is 610 × 597 and the main window is small,
separate viewports are the better product decision.

### 9.2 Per-dialog mapping

| C++ | egui construct | Notes |
|---|---|---|
| `FxSettingsDialog` | deferred viewport, 610 × 597, not resizable | Left rail = three custom `Button`s drawn with `Painter` (rounded 40 × 40 icon square, radius 10, + 17 px label); right pane = `CentralPanel` with a `match` on the selected tab. Use `egui::containers::Frame` with a `Rounding::same(8.0)` and the `DefaultFill` @ 20 % fill for the Audio-pane group backdrop. |
| `FxOutputPreference` | `ScrollArea::vertical()` + manual 40 px rows, or `egui_extras::TableBuilder` with a fixed 40 px row height | Reordering: prefer real drag-and-drop (`egui::DragAndDrop` payload = row index) **in addition to** the ▲/▼ buttons and Shift+Up/Shift+Down, which must be kept. |
| preset `ComboBox` per row | `egui::ComboBox::from_id_salt((row_idx, "preset"))` | Width 150, height 36; placeholder `"Select preset"`. Salt by a **stable device id**, not the row index, or the popup state will follow the wrong row after a reorder. |
| `FxPresetImportDialog` | **Do not** re-implement `FileBrowserComponent** | Replace the whole 360 × 310 embedded browser with a native folder picker — see §9.3. The dialog shrinks to a label + "Choose folder…" + the chosen path + Import. |
| `FxImportCompleteMessage` | `egui::Modal` or a small viewport, 360 × 427 | Two `ScrollArea`s of 310 × 100 with read-only, selectable text (`ui.add(egui::Label::new(txt).selectable(true))` inside a scroll area). |
| `FxPresetExportDialog` | deferred viewport, 410 × 492 | Multi-select list: track `HashSet<PresetId>`; row height 26; selected row background `ImageButton`. Progress bar = a 2 px `Painter` rect with an animated `Mesh` gradient; call `ctx.request_repaint()` each frame (the original runs at 30 fps, `+0.01` per frame). |
| `FxConfirmationMessage` | `egui::Modal` | 450 × 142 content. Return value via a callback/`enum`, not a blocking call. Add Escape = No/Cancel (the original does not handle Escape — fix it). |
| `FxNotification` (autohide) | **not** an egui window | See §9.4. |
| `FxNotification` (error banner) | `egui::Area` + `Frame` with `Rounding::same(16.0)` and a shadow, anchored under the device combo | 560 × 120, `Order::Foreground`, `Sense::hover()` so hover keeps it open. |
| `TooltipWindow` | `response.on_hover_text()` with a styled `Frame` | Corner radius 5, padding 10 h / 6 v, wrap at 400 px, 14 px font (§0.4). |

### 9.3 What needs a real OS file dialog

Exactly one place: the import folder chooser.

Use **[`rfd`](https://crates.io/crates/rfd)** with its XDG desktop portal backend
(`rfd` default features on Linux use `ashpd`/`org.freedesktop.portal.FileChooser`, which is
mandatory under Flatpak/Snap and is the right choice on bare Wayland too — it gets you the user's
own file manager dialog, bookmarks, and recent files):

```rust
// Import: pick a directory (JUCE: openMode | canSelectDirectories, start = ~/Documents)
let dir = rfd::AsyncFileDialog::new()
    .set_title("Select the folder which contains the presets…")
    .set_directory(dirs::document_dir().unwrap_or_else(|| dirs::home_dir().unwrap()))
    .pick_folder()
    .await;
```

Do **not** use the blocking `rfd::FileDialog` from inside the egui update loop — it will deadlock or
stutter; spawn the async variant and poll a `oneshot` channel, or run it on a side task and
`ctx.request_repaint()` on completion.

The other filesystem touchpoint is `File::revealToUser()` after a successful export
(`GUI/FxPresetExportDialog.cpp:196`). On Linux use the portal's
`org.freedesktop.portal.OpenURI.OpenDirectory` (via `ashpd::desktop::open_uri`), falling back to
`xdg-open <dir>`. Never shell out to a hardcoded file manager.

Export itself writes to a fixed directory in the original, so it needs no dialog — but consider
offering a `save_file()`/`pick_folder()` for the export destination, since a fixed
`~/Documents/FxSound/Presets/Export/` is un-Linux-y.

### 9.4 Notifications

The autohide toast is a floating, always-on-top, tray-anchored window. **None of that is available
to a Wayland client.**

Recommended: send a real desktop notification over D-Bus
(`org.freedesktop.Notifications`) using **`notify-rust`**:

```rust
notify_rust::Notification::new()
    .summary("FxSound")
    .body(&message)                    // the 1–3 lines, joined with '\n'
    .appname("FxSound")
    .icon("fxsound")
    .timeout(notify_rust::Timeout::Milliseconds(if has_link { 8000 } else { 7000 }))  // §6.4
    .action("open", &link_text)        // the hyperlink becomes a notification action
    .show()?;
```

This preserves the two timeouts (7 s / 8 s) and the clickable link (as an action button), and it
respects the user's do-not-disturb setting — which is exactly what
`SHQueryUserNotificationState() == QUNS_ACCEPTS_NOTIFICATIONS` was doing on Windows
(`GUI/FxSystemTrayView.cpp:393-397`). The `hide_notifications` setting gates it as before.

Keep the hand-drawn `FxNotification` *only* for the in-window error banner (§6.5), where it is a
child component anyway and Wayland places no constraints.

The tray icon itself (and hence `getSystemTrayWindowPosition`) has no Wayland equivalent; use
**StatusNotifierItem** via the `ksni` crate (works on KDE, and on GNOME with the AppIndicator
extension). The corner-snapping placement logic at `GUI/FxSystemTrayView.cpp:141-165` is then moot.

### 9.5 Global hotkeys

Five global hotkeys are registered with Win32 `RegisterHotKey` against a hidden message-only window
(`GUI/FxController.h:180-217`, `GUI/FxController.cpp:2248-2256`). **A Wayland client cannot grab
global keys.** Options, in order of preference:

1. **`org.freedesktop.portal.GlobalShortcuts`** (xdg-desktop-portal ≥ 1.17; implemented by KDE
   Plasma 6 and GNOME 46+). Use the `ashpd::desktop::global_shortcuts` API. Important behavioural
   change: *the portal, not the app, owns the binding UI*. The five hotkey editor rows must become
   **read-only displays** of what the portal reports, plus a "Configure shortcuts…" button that
   calls the portal's own binding dialog. Register these five shortcut ids, preserving the command
   keys as ids: `cmd_on_off`, `cmd_open_close`, `cmd_next_preset`, `cmd_previous_preset`,
   `cmd_change_output`, with the defaults `Ctrl+Shift+Q / E / A / Z / W`.
2. **MPRIS2** (`org.mpris.MediaPlayer2.Player`) for next/previous — gets you the media keys for
   preset cycling for free on every desktop. Map `Next`→`cmd_next_preset`,
   `Previous`→`cmd_previous_preset`.
3. **Compositor config** — document the D-Bus methods and let users bind them in their compositor
   (`hyprctl`/`kwinrc`/`custom keybindings`). Expose a small D-Bus interface
   (`com.fxsound.App` with `TogglePower`, `ToggleWindow`, `NextPreset`, `PreviousPreset`,
   `NextOutput`) so a compositor keybind can call it.

The existing `SysInfo::canSupportHotkeys()` hook (`Utils/SysInfo/SysInfo.cpp:133`) is the natural
place to return "false" when no portal is present, which already makes the UI force the
"Disable keyboard shortcuts" checkbox on and grey out the five editors
(`GUI/FxSettingsDialog.cpp:379-383`). Reuse that path.

### 9.6 Other Windows-isms in this subsystem

| Windows mechanism | Achieves | Linux substitute |
|---|---|---|
| `HKCU\…\CurrentVersion\Run` value `FxSound` | launch on login | Write `~/.config/autostart/fxsound.desktop` (XDG Autostart). Under a sandbox, use `org.freedesktop.portal.Background.RequestBackground` with `autostart: true` (`ashpd::desktop::background`). Read the toggle state from the file's existence + `Hidden=false`. |
| `updater.exe /silent`, `/checknow` | self-update | No equivalent. Remove the "Automatic updates" toggle and the menu item, or repoint them at the packaging system (Flatpak/`.deb`/AUR) and just link to the release page. Keep `automatic_updates` in the settings schema so old configs migrate cleanly. |
| `SHFileOperation(FO_DELETE)` in `resetPresets` | delete user preset files | `std::fs::remove_file`, or better `trash` crate → XDG Trash, so the destructive reset is recoverable. |
| `SHQueryUserNotificationState` | respect DND / presentation mode | Delegated to the notification daemon by `notify-rust`. |
| `Shell_NotifyIconGetRect` | locate the tray icon | Not available; drop the corner-snapping code. |
| JUCE `PropertiesFile` XML at `%APPDATA%\FxSound\FxSound.settings` | settings store | `$XDG_CONFIG_HOME/fxsound/settings.toml` (default `~/.config/fxsound/`). Keep the key names verbatim (§8) so the schema is greppable against the C++. `device_configs` stays a JSON/TOML array of the same four fields. |
| `%APPDATA%\FxSound\Presets` | imported presets | `$XDG_DATA_HOME/fxsound/presets` (default `~/.local/share/fxsound/presets`). |
| `%USERPROFILE%\Documents\FxSound\Presets\Export` | exported presets | `$XDG_DOCUMENTS_DIR/FxSound/Presets/Export` via the `dirs` crate, honouring `user-dirs.dirs`. |
| `DfxDsp::getPresetInfo` / `exportPreset` | read/write `.fac` | Must be reimplemented in the DSP crate; out of scope here. The dialog only needs `fn preset_name_of(path) -> Result<String>` and `fn export(preset, dir) -> Result<()>`. |

### 9.7 Fonts

The UI is Gilroy (Regular/Semibold/Bold), embedded as three TTFs (`GUI/FxTheme.cpp:95-97`).
Gilroy is a commercial typeface; if the Linux build cannot ship it, pick one metric-compatible
substitute and set it once in `egui::FontDefinitions`, then define exactly three text styles to
mirror the three accessors:

```rust
// FxTheme::getNormalFont / getSmallFont / getTitleFont  (GUI/FxTheme.cpp:466-479)
style.text_styles.insert(TextStyle::Name("normal".into()), FontId::new(17.0, ui_semibold));
style.text_styles.insert(TextStyle::Name("small".into()),  FontId::new(14.0, ui_regular));
style.text_styles.insert(TextStyle::Name("title".into()),  FontId::new(17.0, ui_bold));
```

Note that `FxTheme::loadFont(language)` swaps the typeface per language
(`GUI/FxController.cpp:2462`) — CJK/Arabic/Thai need fallback faces. In egui, push the fallback
families into the same `FontFamily` list rather than swapping, so mixed-script strings render.

### 9.8 Theme switching

Define a single `Palette` struct with the 27 fields of §0.4, two `const` instances (Dark, Light),
and rebuild `egui::Visuals` from it when `theme_mode` changes. The C++ reads the palette through
the `FXCOLOR(x)` macro on every paint (`GUI/FxTheme.h:118`), which is why theme changes are
instant there; in egui, just call `ctx.set_visuals()` and request a repaint.

---

## Open questions / risks for the Rust port

1. **Toggle-button (checkbox) geometry is unspecified in this tree.** `FxTheme` does not override
   `drawToggleButton`/`drawTickBox`, and the JUCE 6.1.6 modules are not vendored under
   `/home/blackixxce/Загрузки/fxsound-app-main` (no `juce_LookAndFeel_V4.cpp` anywhere). The tick
   size, tick inset and label offset for all six checkboxes in this document therefore have **no
   citable value here**. Either pull JUCE 6.1.6 and read `LookAndFeel_V4::drawToggleButton`, or
   pick an egui-native checkbox and accept the difference. Same caveat for `TextButton` background
   rendering (corner radius, hover/press states) and for `FileBrowserComponent`'s internals.
2. **`revealToUser()` on the export folder is a hard Windows Explorer dependency**
   (`GUI/FxPresetExportDialog.cpp:196`, note the `L"FxSound\\Presets\\Export\\"` backslash literal).
   The portal `OpenDirectory` call may be refused or silently no-op under some sandboxes; decide
   whether a failed reveal should surface an error or be silent.
3. **Modal-return refactor is the single biggest structural change.**
   `FxConfirmationMessage::showMessage()` returns a `bool` from inside a nested event loop and is
   called from *inside* `FxController::exportPresets()` (`GUI/FxController.cpp:1403`), i.e. from
   the audio/controller layer, once per colliding file. In Rust that inversion has to be lifted:
   either pre-compute the collision list and ask once ("N files exist — overwrite all / skip all /
   ask per file"), or make `export_presets` an async state machine driven by the UI.
   Pre-computing and asking once is strongly recommended.
4. **The destructive "Reset presets to factory defaults" has no confirmation.**
   `GUI/FxSettingsDialog.cpp:222-227` calls `FxController::resetPresets()` directly, which
   permanently deletes every user preset file (`GUI/FxController.cpp:1353-1366`). Add a
   `FxConfirmationMessage`-style Yes/No and route deletions through the XDG trash.
5. **Global hotkeys may simply not exist on the target desktop.** If the GlobalShortcuts portal is
   absent, the five hotkey rows in the General pane become dead UI. Decide up front whether to
   hide the whole hotkey block, show it read-only with an explanatory line, or keep it as a
   "bind these in your compositor" reference. The `hotkeys` setting key and the five
   `cmd_*` int values should still be persisted so behaviour is stable across desktops.
6. **Window placement and always-on-top cannot be honoured.** `centreWithSize`,
   `setTopLeftPosition(x, y)` and `setAlwaysOnTop(true)` all become no-ops. If any product
   requirement depends on the confirmation box appearing exactly over the export dialog, it needs a
   different design (in-window `egui::Modal`).
7. **Two geometry bugs and one paint bug are baked into the current look.** The 2 px pane overhang
   (`GUI/FxSettingsDialog.cpp:125`), the 5 px separator offset (`GUI/FxSettingsDialog.cpp:43` vs
   `:125`), the 4 px remove-button offset (`GUI/FxOutputPreference.cpp:129`) and the sloped
   selected-row underline (`GUI/FxOutputPreference.cpp:188`). Decide explicitly whether the port is
   pixel-faithful or corrected; do not leave it to chance during implementation.
8. **Three Help-pane controls are declared but never shown** (`quicktour_link_`, `submitlogs_link_`,
   `feedback_link_`) and `debug_log_toggle_` is declared and never constructed
   (`GUI/FxSettingsDialog.h:184`). `FxModel::getDebugLogging()/setDebugLogging()` exist
   (`GUI/FxModel.h:160-168`) with no UI. Is a debug-log toggle wanted in the Linux build? If so it
   is a new feature, not a port.
9. **`FxMessage` is dead code** (no call sites). Confirm it should be dropped rather than being an
   unfinished feature someone intends to wire up.
10. **`launch_toggle_` is gated on `OperatingSystemType::Windows7`** (`GUI/FxSettingsDialog.cpp:402`)
    so "Launch on system startup" is invisible on every modern Windows. Verify with the product
    owner that showing it unconditionally on Linux is the intent (it almost certainly is — the gate
    looks like a leftover from a Windows Store packaging constraint).
11. **Translation catalogue is not in this tree.** Only the `BinaryData::FxSound_xx_txt` symbol
    names are visible (`GUI/FxController.cpp:2340-2457`). The 30-language `.txt` files must be
    sourced from the build artefacts before the string ids in §7 can be reused; the `%s` format
    placeholders in `"Changes to preset %s are saved."` etc. are substituted by
    `FxController::FormatString`, not by `printf`, so the Rust side needs a matching
    single-positional-argument substitution that tolerates translators moving the `%s`.
12. **`FxLanguage`'s start-up index lookup is prefix-based and takes the last match**
    (`GUI/FxLanguage.cpp:63-71`), leaving `-1` for unknown codes. Replace with an exact-match table;
    also decide whether to auto-detect from `$LANG`/`$LC_ALL` on first run, which the original does
    not do (it defaults to whatever `settings["language"]` holds, falling back to `"en"` at
    `GUI/FxController.cpp:2332-2335`).
