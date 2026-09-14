# 01 — Top-level window, frameless chrome, and the three view modes

Reverse-engineering spec for the Rust/egui/Wayland port of FxSound.

**Source of truth** — every number below is cited as `path:line` relative to the repo root
`/home/blackixxce/Загрузки/fxsound-app-main`. Files read in full for this document:

| File | Lines | Role |
|---|---|---|
| `fxsound/Source/MainComponent.h` | 73 | **DEAD CODE** — see §0 |
| `fxsound/Source/MainComponent.cpp` | 219 | **DEAD CODE** — see §0 |
| `fxsound/Source/GUI/FxMainWindow.h` | 84 | the concrete app window |
| `fxsound/Source/GUI/FxMainWindow.cpp` | 635 | title-bar buttons, menu, view switching |
| `fxsound/Source/GUI/FxWindow.h` | 103 | the reusable frameless-chrome base class |
| `fxsound/Source/GUI/FxWindow.cpp` | 394 | title bar, shadow, corner radius, drag |
| `fxsound/Source/GUI/FxView.h` | 56 | shared base of Pro/Lite: preset + output combos |
| `fxsound/Source/GUI/FxView.cpp` | 220 | combo population, error notification |
| `fxsound/Source/GUI/FxProView.h` | 68 | Pro geometry constants |
| `fxsound/Source/GUI/FxProView.cpp` | 147 | Pro `resized()` / `paint()` |
| `fxsound/Source/GUI/FxLiteView.h` | 45 | Lite geometry constants |
| `fxsound/Source/GUI/FxLiteView.cpp` | 58 | Lite `resized()` / `paint()` |

Supporting files consulted for constants that the above reference:
`fxsound/Source/GUI/FxTheme.h`, `fxsound/Source/GUI/FxTheme.cpp`,
`fxsound/Source/GUI/FxController.h`, `fxsound/Source/GUI/FxController.cpp`,
`fxsound/Source/GUI/FxSystemTrayView.h`, `fxsound/Source/GUI/FxSystemTrayView.cpp`,
`fxsound/Source/GUI/FxPowerButton.cpp`, `fxsound/Source/GUI/FxComboBox.cpp`,
`fxsound/Source/GUI/FxVisualizer.h`, `fxsound/Source/GUI/FxAudioControls.h`,
`fxsound/Source/GUI/FxEqualizer.h`, `fxsound/Source/GUI/FxNotification.h`,
`fxsound/Source/Main.cpp`, `fxsound/Source/Utils/Settings/Settings.cpp`,
`fxsound/Project/FxSound_App.vcxproj`.

---

## 0. First, a trap: `MainComponent.*` is dead code — do NOT port it

`fxsound/Source/MainComponent.h` and `.cpp` describe a *completely different* window design:
a `MinView`/`ExpandedView` pair (`MainComponent.h:69`), a `ComponentAnimator` that morphs the
window over **2000 ms** (`MainComponent.cpp:190`), a rounded-rect drop shadow with
`offset = (10,10)` and corner radius **20** (`MainComponent.cpp:57-58`), a colour-scheme cycler
bound to double-click that walks Dark → Midnight → Grey → Light (`MainComponent.cpp:101-126`),
and a web "announcement" popup fetched from
`https://www.fxsound.com/tmp/check_announcement.json` (`MainComponent.h:23`).

**None of this ships.** Evidence:

1. `MainComponent.h:15-16` includes `GUI/FxMinView.h` and `GUI/FxExpandedView.h`;
   `GUI/UITheme.h`, `GUI/AnnouncementComponent.h`, `GUI/AnimationDemoComponent.h`
   (`MainComponent.h:12-14`). **None of those files exist anywhere in the tree.**
2. `fxsound/Project/FxSound_App.vcxproj` contains `<ClCompile Include="..\Source\Main.cpp" />`
   (line 306), `FxMainWindow.cpp` (299), `FxWindow.cpp` (300), `FxLiteView.cpp` (292),
   `FxProView.cpp` (293) — **but no entry for `Source\MainComponent.cpp`**.
3. `Main.cpp` never mentions `MainComponent`; it constructs `FxMainWindow` directly
   (`fxsound/Source/Main.cpp:70`).

Treat `MainComponent.*` as an abandoned JUCE-wizard leftover. The *only* value it has for the
port is historical: the "two window sizes that swap with a centring animation" idea survives in
the shipping code as Pro/Lite, but without the animation.

---

## 1. Process / window topology

```
FxSoundApplication (JUCEApplication)          Main.cpp:38
 ├── FxTheme theme_                           Main.cpp:142  (set as default LookAndFeel, Main.cpp:63)
 ├── AudioPassthru        audio_passthru_     Main.cpp:69
 ├── FxMainWindow         main_window_        Main.cpp:70   <-- THE window (this document)
 └── FxSystemTrayView     system_tray_view_   Main.cpp:71   <-- hidden HWND + Shell_NotifyIcon
```

`moreThanOneInstanceAllowed()` returns **false** (`Main.cpp:46`); a second launch is routed into
`FxController::applyConfig(commandline)` (`Main.cpp:138`).

`FxMainWindow` is **not** a `DocumentWindow` / `ResizableWindow`. It is a bare `juce::Component`
subclass (`FxMainWindow.h:32` → `FxWindow` → `Component`, `FxWindow.h:27`) that is pushed onto the
desktop by hand:

```cpp
addToDesktop(ComponentPeer::windowAppearsOnTaskbar);      // FxMainWindow.cpp:250
```

The peer flags are **only** `windowAppearsOnTaskbar`. There is no `windowHasTitleBar`, no
`windowIsResizable`, no `windowHasDropShadow`. Consequences that the port must reproduce:

* No OS title bar, no OS border, no OS resize grips.
* **The window is not user-resizable at all.** There is no `ResizableCornerComponent`, no
  `ResizableBorderComponent`, no `setResizable()` call anywhere. Each view mode has exactly one
  hard-coded size.
* It still gets a taskbar button.
* `setOpaque(false)` (`FxMainWindow.cpp:182`) → the window surface is alpha-blended, which is what
  makes the 21 px rounded corners actually look rounded rather than showing black.

Other `FxWindow` subclasses (same chrome, different content), for context:
`FxSettingsDialog` (`FxSettingsDialog.h:37`), `FxPresetExportDialog` (`FxPresetExportDialog.h:31`),
`FxPresetImportDialog` (`FxPresetImportDialog.h:31`), `FxMessage` (`FxMessage.h:33`),
`FxConfirmationMessage` (`FxMessage.h:72`), `FxDeviceErrorMessage` (`FxController.cpp:30`),
`FxImportCompleteMessage` (`FxPresetImportDialog.cpp:21`).
Those keep the drop shadow (see §3.3); the main window does not.

---

## 2. The master constants

### 2.1 Chrome constants

| Constant | Value | Source |
|---|---:|---|
| `FxTheme::WINDOW_CORNER_RADIUS` | **21** | `FxTheme.h:42` |
| `FxTheme::TITLE_BAR_HEIGHT` | **57** | `FxTheme.h:43` |
| `FxWindow::SHADOW_WIDTH` | **5** | `FxWindow.h:44` |
| `FxWindow::CLOSE_BUTTON_WIDTH` | **15** | `FxWindow.h:45` |
| `FxWindow::TitleBar::ICON_WIDTH` | **106** | `FxWindow.h:71` |
| `FxWindow::TitleBar::ICON_HEIGHT` | **15** | `FxWindow.h:72` |
| `FxMainWindow::BUTTON_WIDTH` | **24** | `FxMainWindow.h:54` |
| title-bar *component* height | **56** = `TITLE_BAR_HEIGHT - 1` | `FxWindow.cpp:29` |
| `FxWindow` initial size before content | 64 × 64 | `FxWindow.cpp:27` |
| title bar initial size before layout | 64 × 56 | `FxWindow.cpp:29` |
| `shadow_width_` for **main window** | **0** (`enableShadow(false)`) | `FxMainWindow.cpp:183`, `FxWindow.cpp:100-111` |
| `shadow_width_` for dialogs | **5** (default `draw_shadow_ = true`) | `FxWindow.cpp:32-33` |

> ⚠️ Note the off-by-one: the title-bar *component* is 56 px tall, but the logo's vertical
> centring maths uses the 57 px `TITLE_BAR_HEIGHT` (`FxWindow.cpp:365, 369, 376`). Both numbers
> are live in the shipping build. Reproduce both or you will be 0.5 px off on the logo.

### 2.2 Colour palette (used by this subsystem)

`FxTheme.h:29-31` declares the `FxColor` enum; `FxTheme.cpp:22-29` is the two-row table.
Index order: `WindowBackground, WidgetBackground, MenuBackground, Outline, DefaultText,
DefaultFill, HighlightedText, HighlightedFill, MenuText, ComboBoxBackground,
TextButtonBackground, ImageButton, HintText, ValidTextBorder, InvalidTextBorder,
ControlBackground, SliderTrack, SliderHighlight, GraphHigh, GraphLow, EqStart, EqEnd,
VerticalSliderLow, MenuHighlightBackground, PanelBackground, RowOutline, SelectedRowOutline`.

Only the colours this subsystem paints:

| `FxColor` | Dark (`FxTheme.cpp:23-25`) | Light (`FxTheme.cpp:27-29`) | Where used |
|---|---|---|---|
| `WindowBackground` (idx 0) | `#181818` | `#f5f5f5` | window fill + title-bar fill (`FxWindow.cpp:131,239`), Pro/Lite `fillAll` (`FxProView.cpp:111-112`, `FxLiteView.cpp:51-52`) |
| `DefaultFill` (idx 5) | `#000000` | `#ffffff` | Lite panel @ α 0.2 (`FxLiteView.cpp:54`); help-bubble bg @ α 1.0 (`FxMainWindow.cpp:341`) |
| `ImageButton` (idx 11) | `#e63462` | `#23b6eb` | the ✕ glyph (`FxWindow.cpp:167`) |
| `ControlBackground` (idx 15) | `#0f0f0f` | `#e0e0e0` | the 1 px line under the title bar (`FxWindow.cpp:141`) |
| `PanelBackground` (idx 24) | `#000000` | `#c0c0c0` | Pro panel @ α 0.2 (`FxProView.cpp:114`) |
| `HighlightedText` (idx 6) | `#ffffff` | `#000000` | title text when the title bar is named (`FxWindow.cpp:383`) |
| `DefaultText` (idx 4) | `#b1b1b1` | `#4e4e4e` | combo text, tooltip text |

Theme mode default is **Dark** (`FxTheme.cpp:61`: `FxThemeMode::theme_mode_ = FxThemeMode::Dark`);
enum is `{Dark=0, Light=1, NumModes=2}` (`FxTheme.h:28`).

### 2.3 Fonts

| Accessor | Typeface | Height | Source |
|---|---|---:|---|
| `getNormalFont()` | Gilroy **Semibold** (`font_600_`) | **17.0 px** | `FxTheme.cpp:466-469` |
| `getSmallFont()` | Gilroy **Regular** (`font_400_`) | **14.0 px** | `FxTheme.cpp:471-474` |
| `getTitleFont()` | Gilroy **Bold** (`font_700_`) | **17.0 px** | `FxTheme.cpp:476-479` |

Typefaces are embedded TTFs: `GilroyRegular_ttf`, `GilroySemibold_ttf`, `GilroyBold_ttf`
(`FxTheme.cpp:95-97`), sourced from `fxsound/Fonts/Gilroy-{Regular,Semibold,Bold}.ttf`
(`fxsound/FxSound.jucer:10-14`).

> `juce::Font::withHeight(h)` sets **ascent + descent = h px**. `egui::FontId::size` is the em
> size. For a typical face `ascent+descent ≈ 1.2 em`, so egui sizes ≈ **14.2 / 11.7 / 14.2**.
> Calibrate against a screenshot; do not blindly copy 17.0/14.0 into `FontId::new`.

---

## 3. `FxWindow` — the frameless chrome

### 3.1 Component tree

```
FxWindow                    (juce::Component, on desktop, non-opaque)
 ├── TitleBar title_bar_    (FxWindow.h:97)
 │    ├── Drawable  icon_             (wordmark logo, 106×15)
 │    ├── Drawable  animation_icon_   (highlight logo, alpha 0 at rest)
 │    ├── Label     title_            (visible ONLY when the window was constructed with a name)
 │    ├── CloseButton close_button_   (15×15, always right-most)
 │    └── std::vector<std::pair<Button*, bool>> toolbar_buttons_   (button, right_aligned)
 └── Component* content_    (FxWindow.h:96) — swapped between FxProView and FxLiteView
```

`TitleBar` is a keyboard-focus container: `setFocusContainer(true)` and
`setFocusContainerType(FocusContainerType::keyboardFocusContainer)` (`FxWindow.cpp:184-185`).

### 3.2 `setContent()` — the size/position algebra

`FxWindow.cpp:41-83`. Reproduced as pseudo-code:

```
fn set_content(new_content):
    sw = shadow_width_                      # 0 for main window, 5 for dialogs
    x = -1; y = -1
    if content_ != null and content_ != new_content:
        # keep the window visually centred while the content changes size
        if new_content.w > content_.w: x = self.x - (new_content.w - content_.w) / 2
        else:                          x = self.x + (content_.w - new_content.w) / 2
        if new_content.h > content_.h: y = self.y - (new_content.h - content_.h) / 2
        else:                          y = self.y + (content_.h - new_content.h) / 2

    if new_content != content_:
        remove_child(content_); content_ = new_content; add_and_make_visible(content_)

    content_.set_bounds(sw, title_bar.bottom + 1, content_.w, content_.h)

    if x >= 0 and y >= 0:
        self.set_bounds(x - sw, y - sw,
                        content_.w + 2*sw,
                        content_.h + title_bar.h + WINDOW_CORNER_RADIUS + 2*sw)
    else:
        self.set_size(content_.w + 2*sw,
                      content_.h + title_bar.h + WINDOW_CORNER_RADIUS + 2*sw)
```

**Window size formula** (the single most important equation in this file):

```
window_w = content_w + 2 * shadow_width
window_h = content_h + 56 + 21 + 2 * shadow_width        # title bar + WINDOW_CORNER_RADIUS
```

For the main window (`shadow_width = 0`): `window_h = content_h + 77`.

The `+ WINDOW_CORNER_RADIUS` term is **not** a corner radius here — it is reused as a bare
21 px of dead vertical padding below the content so the rounded bottom corners do not clip the
content. Since the content is placed at `title_bar.bottom + 1 = 57`, the actual empty strip at
the bottom is **20 px** (`57 + content_h` … `content_h + 77`).

> 🐛 Quirk worth porting deliberately or fixing: if the recentring maths produces a negative
> `x` **or** `y` (window near the top/left screen edge), the `x >= 0 && y >= 0` guard at
> `FxWindow.cpp:75` fails and the code falls through to `setSize()`, which keeps the old top-left
> instead of recentring. In practice `FxMainWindow::showLiteView()`/`showProView()` overwrite the
> position right afterwards anyway (§6), so this is mostly invisible.

### 3.3 `paint()` — `FxWindow.cpp:118-143`

```
fn paint(g):
    sw = shadow_width_
    if draw_shadow_ and sw != 0:                       # NOT the main window
        path = rounded_rect(sw, sw, W - 2*sw, H - 2*sw, r = 21)
        DropShadow{ radius: sw }.draw_for_path(g, path)      # juce default: black, alpha 0.5, offset (0,0)

    g.fill = colour_scheme.windowBackground            # Dark #181818 / Light #f5f5f5
    if is_opaque():                                    # dialogs
        g.fill_rect(sw, sw, W - 2*sw, H - 2*sw)
    else:                                              # MAIN WINDOW (setOpaque(false))
        g.fill_rounded_rect(sw, sw, W - 2*sw, H - 2*sw, r = 21)

    g.colour = FXCOLOR(ControlBackground) @ alpha 1.0  # Dark #0f0f0f / Light #e0e0e0
    g.draw_line(sw, title_bar.bottom, W - 2*sw, title_bar.bottom)   # y = 56, 1 px
```

For the main window this reduces to: **a rounded rect `(0,0,W,H)` with radius 21 filled with
`#181818`, and a 1 px horizontal divider at `y = 56` in `#0f0f0f`.**

> 🐛 The divider's end-x is `getWidth() - shadow_width_*2`, not `getWidth() - shadow_width_`.
> For the main window (`sw = 0`) they coincide. For dialogs (`sw = 5`) the line runs 5 px past
> where it should. Port the *correct* version.

### 3.4 `resized()` — `FxWindow.cpp:145-152`

```
fn resized():
    sw = shadow_width_
    title_bar.set_bounds(21 + sw,  sw,  W - 42 - 2*sw,  56)
    if content_ != null:
        content_.set_bounds(sw, title_bar.bottom + 1, content_.w, content_.h)
```

So for the main window: **title bar occupies `x ∈ [21, W-21)`, `y ∈ [0, 56)`**, and the content
sits at **`(0, 57)`** at its own natural size. The 21 px horizontal inset is where the rounded
corners live.

### 3.5 The close button — `FxWindow::CloseButton`

`FxWindow.cpp:154-169`. It is not an image; it is drawn procedurally:

* fills its whole rect with `windowBackground` first (`FxWindow.cpp:157`);
* builds a `Path` of two line segments — `(0,0)→(1,1)` and `(1,0)→(0,1)` — each with **thickness
  0.08** in that unit square (`FxWindow.cpp:164-165`);
* scales that path to fit a **square of side = component height**, centred in the component
  (`FxWindow.cpp:159-161`);
* fills it with `FXCOLOR(ImageButton)` @ α 1.0 — **Dark `#e63462`, Light `#23b6eb`**
  (`FxWindow.cpp:167`).

Size 15 × 15 (`FxWindow.cpp:188`), `MouseCursor::PointingHandCursor` (`FxWindow.cpp:187`).
So: an **✕ made of two 1.2 px-thick diagonals inside a 15 px box**, magenta-pink in dark mode,
sky-blue in light mode. There is **no hover state** on the close button (the `paintButton`
signature ignores both `shouldDrawButtonAsHighlighted` and `...AsDown`).

### 3.6 Title-bar layout — `FxWindow::TitleBar::resized()`

`FxWindow.cpp:261-310`. Full pseudo-code, `tb_w = W - 42`, `tb_h = 56`:

```
right_placement = xRight | yMid | doNotResize
left_placement  = xLeft  | yMid | doNotResize

# 1. close button, always flush right
close_button.bounds = place(close_button.local_bounds(15×15), (0,0,tb_w,tb_h), right_placement)
#   => x = tb_w - 15, y = floor((56-15)/2) = 20

# 2. logo
if not title_.visible:                       # MAIN WINDOW takes this branch
    icon_.bounds = place((0,0,106,15), (0,0,tb_w,tb_h), xLeft|yTop|doNotResize)   # => (0,0,106,15)
    animation_icon_.bounds = same
else:                                        # named windows (e.g. Settings dialog)
    w = (15 - 1) * icon_.natural_w / icon_.natural_h
    icon_.bounds = (0, 0, w, 14)
    title_.bounds = place(title_.local_bounds, (0,0,tb_w,tb_h), left_placement).with_x(w + 2)

# 3. toolbar buttons
x_right = close_button.w + 20            # = 35
x_left  = icon_.w + 15                   # = 106 + 15 = 121  (main window)
for (button, right_aligned) in toolbar_buttons_:        # INSERTION ORDER
    if right_aligned:
        dest = (0,0,tb_w,tb_h).reduced(x_right, 0)      # => (x_right, 0, tb_w - 2*x_right, tb_h)
        button.bounds = place(button.local_bounds, dest, right_placement)
        #   => x = tb_w - x_right - button.w ,  y = floor((56 - button.h)/2)
        x_right += button.w + 20
    else:
        dest = (0,0,tb_w,tb_h).reduced(x_left, 0)
        button.bounds = place(button.local_bounds, dest, left_placement)
        #   => x = x_left ,  y = floor((56 - button.h)/2)
        x_left += button.w + 20
```

`RectanglePlacement::doNotResize` = `onlyReduceInSize | onlyIncreaseInSize`, which clamps the
scale factor to exactly 1.0 — i.e. buttons keep their set size and are only translated.
`appliedTo(Rectangle<int>, Rectangle<int>)` computes in floats and returns the *smallest integer
container*, so a half-pixel `yMid` (the 15 px close button in a 56 px bar) floors the y and may
round the height up by 1.

**Gap between adjacent toolbar buttons is a fixed 20 px.** The first right-aligned button sits
`15 + 20 = 35` px in from the title bar's right edge.

### 3.7 Logo and its animation

`updateLogo()` — `FxWindow.cpp:360-395`.

Unnamed window (`name_.isEmpty()`, **which is the main window**):

* `icon_` = `FXIMAGE(DefaultLogo)` → Dark `logowhite_svg`, Light `logoblack_svg`
  (`FxTheme.cpp:32, 39`; files `fxsound/Images/logo-white.svg`, `logo-black.svg`,
  `viewBox="0 0 526.19 75.15"` → aspect 7.002, hence 15 px tall ⇒ 105.03 ≈ **106 px wide**).
* `animation_icon_` = `FXIMAGE(HighlightedLogo)` → Dark `logored_svg` (`#e63462`),
  Light `logoblue_svg` (`#23B6EB`).
* Both get `setTransformToFit(Rect(0, (57 - 15)/2 = 21.0, 106, 15), xLeft|yMid)`
  (`FxWindow.cpp:365-370`).
* `animation_icon_->setAlpha(0.0f)` at construction (`FxWindow.cpp:181`).

Named window (dialogs): `icon_` = `FXIMAGE(IconLogo)` → `FxSound_White_Bars_svg` /
`FxSound_Black_Bars_svg`, scaled to height `15 - 1 = 14`, transform rect
`(0, (57 - 15 - 1)/2 = 20.5, w, 14)` (`FxWindow.cpp:374-377`); the `title_` label gets
`getNormalFont()` (Gilroy Semibold 17 px), colour `highlightedText`, `centredLeft`, and is sized
to `font.getStringWidth(name) * 2` wide (`FxWindow.cpp:379-387`).

Animation (`FxWindow.cpp:193-208`) — driven by `FxController` when DSP processing starts/stops:

```
start_logo_animation():  fade_out(icon_, 600 ms);            fade_in(animation_icon_, 600 ms)
stop_logo_animation():   fade_out(animation_icon_, 600 ms);  fade_in(icon_, 600 ms)
```

i.e. the wordmark **cross-fades white↔red (dark) / black↔blue (light) over 600 ms** to indicate
that audio is being processed.

`lookAndFeelChanged()` (`FxWindow.cpp:312-328`) re-creates both drawables for the new theme and
**restores their previous alpha values**, so a theme switch mid-animation does not pop.

### 3.8 Window dragging

`FxWindow::TitleBar::mouseDown/mouseDrag/mouseUp` — `FxWindow.cpp:339-358`:

```
mouseDown(e): dragging_ = true; dragger_.startDraggingComponent(parent_FxWindow, e)
mouseDrag(e): if dragging_: dragger_.dragComponent(parent_FxWindow, e, nullptr)   // nullptr = no constrainer
mouseUp(_):   dragging_ = false
```

Because the `FxWindow` is the top-level desktop component, `ComponentDragger::dragComponent`
moves the **native window**. The third argument is `nullptr`, so there is **no
`ComponentBoundsConstrainer`** — the window can be dragged fully off-screen.

Drag is only initiated from the **title bar**, and only from parts of it not covered by a child
(the toolbar buttons and, note, the logo `Drawable` components, which by JUCE default intercept
mouse clicks). So the draggable strip is: `y ∈ [0,56)`, `x ∈ [21+106, …)` minus the button rects,
plus the 21 px corners are *not* part of the title bar at all and therefore **not draggable**.

There is **no** `mouseDoubleClick` handler on `FxWindow` or `FxMainWindow` — double-clicking the
title bar does nothing (unlike the dead `MainComponent`, which cycled colour schemes).

---

## 4. `FxMainWindow` — the concrete app window

### 4.1 Construction — `FxMainWindow.cpp:179-235`

```cpp
FxMainWindow::FxMainWindow()
  : power_button_   (L"powerButton"),
    menu_button_    (L"menuButton",     DrawableButton::ImageFitted),
    resize_button_  (L"resizeButton",   DrawableButton::ImageFitted),
    donate_button_  (L"donateButton",   DrawableButton::ImageFitted),
    minimize_button_(L"minimizeButton", DrawableButton::ImageFitted)
```

(`FxMainWindow.cpp:179`). Note the base `FxWindow()` default-constructs with `name = ""`, so the
title bar takes the **wordmark-logo, no-text** branch. `setName("FxSound")` at
`FxMainWindow.cpp:181` names the *Component* (used for the taskbar/window title), not the title
bar label.

| Line | Call | Effect |
|---|---|---|
| `:181` | `setName("FxSound")` | window title string |
| `:182` | `setOpaque(false)` | alpha surface → rounded corners render |
| `:183` | `enableShadow(false)` | `shadow_width_ = 0`, no `DropShadow` |
| `:185` | `setWantsKeyboardFocus(true)` | window itself is focusable |
| `:189` | `power_button_.setPowerState(model.getPowerState())` | initial on/off glyph |
| `:192` | `power_button_.setSize(24, 24)` | |
| `:193` | `power_button_.setImageWidth(24)` | inner SVG box (see §4.3) |
| `:194` | `setHelpText(TRANS("Power Button"))` | accessibility |
| `:199` | `menu_button_.setSize(24, 24)` | |
| `:200` | `setHelpText(TRANS("Menu Button"))` | |
| `:205` | `resize_button_.setHelpText(TRANS("Resize Button"))` | size set later by `setResizeImage()` |
| `:210` | `donate_button_.setSize(26, 30)` | `BUTTON_WIDTH+2`, `BUTTON_WIDTH+6` |
| `:211-212` | `setHelpText/setTooltip(TRANS("Donate"))` | |
| `:214-217` | `donate_button_.onClick` → `https://www.paypal.com/donate/?hosted_button_id=JVNQGYXCQ2GPG` | |
| `:220` | `minimize_button_.setSize(26, 30)` | |
| `:221` | `setHelpText(TRANS("Minimize Button"))` | |
| `:225-226` | `help_bubble_.addToDesktop(0); setAlwaysOnTop(true)` | separate always-on-top popup |
| `:230` | `addToolbarButton(&menu_button_, false)` | **left**-aligned |
| `:231` | `addToolbarButton(&minimize_button_)` | right, 1st |
| `:232` | `addToolbarButton(&resize_button_)` | right, 2nd |
| `:233` | `addToolbarButton(&power_button_)` | right, 3rd |
| `:234` | `addToolbarButton(&donate_button_)` | right, 4th |

Every button gets `MouseCursor::PointingHandCursor` and `setWantsKeyboardFocus(true)`.

### 4.2 Title-bar button order, sizes and icons

Visual order, left → right:

```
[ FxSound wordmark ]   [☰ menu]  …………  [♥ donate] [⏻ power] [⤢ resize] [— minimize] [✕ close]
```

| Button | W × H | Source | Normal image | Hover image | Image asset size |
|---|---:|---|---|---|---|
| `menu_button_` | 24 × 24 | `FxMainWindow.cpp:199` | `MenuButton` (`menu.svg` / `menu_black.svg`) | `MenuButtonHover` (`menu_hover.svg` / `menu_hover_blue.svg`) | 14 × 10 px viewBox |
| `donate_button_` | 26 × 30 | `FxMainWindow.cpp:210` | `DonateButton` (`donate.svg` / `donate_blue.svg`) | `DonateButtonHover` | 30 × 31 px viewBox |
| `power_button_` | 24 × 24 | `FxMainWindow.cpp:192` | `PowerOnButton`/`PowerOffButton` (`power_on.svg`/`power_off.svg`) | — (state, not hover) | 30 × 31 px viewBox |
| `resize_button_` **in Pro** | 26 × 26 | `FxMainWindow.cpp:355` | `MinimizeButton` (`minimize.svg` / `minimize_black.svg`) | `MinimizeButtonHover` | 18 × 18 px viewBox |
| `resize_button_` **in Lite** | 24 × 24 | `FxMainWindow.cpp:361` | `MaximizeButton` (`maximize.svg` / `maximize_black.svg`) | `MaximizeButtonHover` | 16 × 16 px viewBox |
| `minimize_button_` | 26 × 30 | `FxMainWindow.cpp:220` | `MinimizeWindowButton` (`min_window.svg` / `min_window_black.svg`) | `MinimizeWindowButtonHover` | 14 × 10 px viewBox |
| `close_button_` | 15 × 15 | `FxWindow.cpp:188` | procedural ✕ | none | — |

Theme → asset mapping is the two-row table at `FxTheme.cpp:32-44`; sizes at `FxTheme.cpp:46-59`.
All are `DrawableButton::ButtonStyle::ImageFitted`, which scales the SVG to fit the button rect
preserving aspect, centred.

**Computed absolute positions.** `tb_x = 21`, `tb_w = W - 42`, `tb_h = 56`; button y is
`floor((56 - h)/2)`.

#### Pro view — window 1040 × 588, `tb_w = 998`

| Button | local x | **window x** | y | w | h |
|---|---:|---:|---:|---:|---:|
| logo (wordmark) | 0 | **21** | 21 (via transform) | 106 | 15 |
| `menu_button_` | 121 | **142** | 16 | 24 | 24 |
| `donate_button_` | 801 | **822** | 13 | 26 | 30 |
| `power_button_` | 847 | **868** | 16 | 24 | 24 |
| `resize_button_` | 891 | **912** | 15 | 26 | 26 |
| `minimize_button_` | 937 | **958** | 13 | 26 | 30 |
| `close_button_` | 983 | **1004** | 20 | 15 | 15 |

Running `x_right`: 35 → 81 → 127 → 171 → 217.
Running `x_left`: 121 → 165.

#### Lite view — window 550 × 189, `tb_w = 508`

| Button | local x | **window x** | y | w | h |
|---|---:|---:|---:|---:|---:|
| logo (wordmark) | 0 | **21** | 21 | 106 | 15 |
| `menu_button_` | 121 | **142** | 16 | 24 | 24 |
| `donate_button_` | 313 | **334** | 13 | 26 | 30 |
| `power_button_` | 359 | **380** | 16 | 24 | 24 |
| `resize_button_` | 403 | **424** | 16 | 24 | 24 |
| `minimize_button_` | 447 | **468** | 13 | 26 | 30 |
| `close_button_` | 493 | **514** | 20 | 15 | 15 |

Running `x_right`: 35 → 81 → 125 → 169 → 215.

**Hit areas are exactly the button rects** — JUCE `Button` uses the default
`Component::hitTest` (whole bounds). There is no enlarged touch target. Note the ✕ glyph is only
15 px, which is a small target; the port may legitimately widen the *hit* rect while keeping the
*drawn* glyph at 15 px.

### 4.3 Power button rendering — `FxPowerButton`

`FxPowerButton.cpp:30-49`:

```
paint(g):
    image_area = Rect(0, 0, image_width_, image_width_)          # 24 × 24 (setImageWidth(24))
    dest = RectanglePlacement(xMid|yMid|doNotResize).appliedTo(image_area, local_bounds)
    img  = power_state_ ? power_on_image_ : power_off_image_
    img.drawWithin(g, dest, stretchToFit|centred, alpha = is_enabled() ? 1.0 : 0.5)
```

`power_on.svg` / `power_off.svg` both have `viewBox="0 0 30 31"`. Dark mode uses
`power_on_svg` / `power_off_svg`; light uses `power_on_blue_svg` / `power_off_black_svg`
(`FxTheme.cpp:33, 40`). **Disabled state = 50 % alpha.**

`keyPressed`: **Space** triggers the click (`FxPowerButton.cpp:51-59`); every other key is
swallowed (returns `false` only after the space check — see `FxPowerButton.cpp:58`, it returns
`false` so the key propagates).

`enablePowerButton(bool)` (`FxMainWindow.cpp:405-420`): when disabled the tooltip becomes
`TRANS("Audio enhancements are not available over Remote Desktop")`; when enabled the tooltip is
cleared.

### 4.4 The hamburger menu

Opened by clicking `menu_button_` (`FxMainWindow.cpp:569-573`) **or by pressing Alt+Enter**
(`FxMainWindow.cpp:422-432`). Both first call `FxController::setMenuClicked(true)`, which
persists `menu_clicked = true` (`FxController.cpp:975-979`) so the first-run hint bubble stops
appearing. `popup_menu.showAt(&menu_button_)` anchors it under the button
(`FxMainWindow.cpp:554`).

Menu structure (`FxMainWindow.cpp:507-553`), in order:

```
Settings                                     -> FxSettingsDialog (modal)
-----
Save New Preset          >  [inline TextEditor custom item, 200×30]
Overwrite Existing Preset[ - <preset name> ]
Undo Preset Changes
Rename Preset            >  [inline TextEditor custom item, 200×30]
Delete Preset
-----
Export Presets
Import Presets
-----
Download Bonus Presets                       -> https://www.fxsound.com/presets
-----
Check for updates                            -> ChildProcess "updater.exe /checknow"
-----
Theme                    >  Dark (ticked if current) / Light (ticked if current)
Always On Top            [ticked = isAlwaysOnTop()]
-----
Donate                                       -> PayPal URL
```

Enablement predicates (`FxMainWindow.cpp:536-543`) all depend on
`model.isPresetModified()`, `preset.type == UserPreset`, `model.getPowerState()`, and
`model.getUserPresetCount() < FxController::getMaxUserPresets()`.

`FxPresetMenuItem` (`FxMainWindow.cpp:27-176`) is the inline preset-name editor:
`WIDTH = 200`, `HEIGHT = 30` (`FxMainWindow.cpp:96-97`), inner bounds `reduced(2, 2)`
(`FxMainWindow.cpp:101`), a 2 px outline (`FxMainWindow.cpp:127`) coloured
`ValidTextBorder` `#009cdd` (both themes) when the typed name is unique, else `InvalidTextBorder`
`#d51535` (both themes) (`FxMainWindow.cpp:114-121`, table `FxTheme.cpp:24, 28`).
Input restricted to 64 characters (`FxMainWindow.cpp:58`). Escape cancels, Enter commits
(`FxMainWindow.cpp:63-82`).

### 4.5 First-run help bubble

`FxMainWindow::mouseEnter` (`FxMainWindow.cpp:588-601`): when the pointer is over
`menu_button_` **and** `!FxModel::isMenuClicked()`, a `BubbleMessageComponent` pops up anchored at
the menu button with the text
`TRANS("Click here to save new presets, overwrite old ones, or reset your settings.")`,
centred, `getSmallFont()` (Gilroy Regular 14 px), colours: background `DefaultFill` @ α 1.0,
outline = `TextEditor::textColourId` (`FxMainWindow.cpp:341-342`).
`showAt(&menu_button_, text, 0, true, false)` → timeout 0 (never auto-dismiss), dismiss on mouse
click = true, dismiss on mouse-out = false.

---

## 5. The two shipping views

### 5.1 `FxView` — shared base (`FxView.h`, `FxView.cpp`)

Owns the two combo boxes and the error notification:

| Member | Type | Default size | Source |
|---|---|---|---|
| `preset_list_` | `FxComboBox` | `LIST_WIDTH × LIST_HEIGHT` = **225 × 50** | `FxView.h:39-40`, `FxView.cpp:31` |
| `endpoint_list_` | `FxComboBox` | **225 × 50** | `FxView.cpp:40` |
| `error_notification_` | `FxNotification` | child component, hidden by default | `FxView.cpp:50` |

Both combos: `Justification::centredLeft` (`FxView.cpp:29, 38`), empty
`setTextWhenNoChoicesAvailable` (`:30, :39`), `setWantsKeyboardFocus(true)` (`:33, :42`),
accessibility descriptions `TRANS("Preset List")` (`:32`) and
`TRANS("Playback Device List")` (`:41`).
`endpoint_list_.onShowPopup = FxController::checkDeviceChanges` (`FxView.cpp:46-48`) — the device
list is refreshed lazily, only when the dropdown is about to open.

> **Shadowing alert.** `FxProView` re-declares `LIST_WIDTH = 470` and `LIST_HEIGHT = 40`
> (`FxProView.h:51-52`), which **shadow** the base's 225/50. `FxLiteView` does **not** shadow
> them, so `FxLiteView::BACKGROUND_WIDTH`/`HEIGHT` are computed from the base 225/50
> (`FxLiteView.h:40-41`). The combos' *component* size stays 225 × 50 in Lite (never re-set) but
> is explicitly overwritten to 470 × 40 in Pro's `resized()`.

Combo box chrome (`FxTheme::drawComboBox`, `FxTheme.cpp:135-164`):

* `cornerSize = height / 5` → **8.0 px** in Pro (h=40), **10.0 px** in Lite (h=50).
* Fill `ComboBox::backgroundColourId` = `ComboBoxBackground` (Dark `#000000`, Light `#d7d7d7`).
* 1.0 px outline, inset 0.5 px: `focusedOutlineColourId` = `SliderHighlight` @ α 0.2
  (Dark `#f7546f`, Light `#53ccff`) when focused, else `outlineColourId` = `ComboBoxBackground`.
* Drop-down arrow drawn in a `12 × height` box at `x = width - margin`, centred, where
  **`margin = 32`, or 24 if `width <= 150`** (`FxTheme.cpp:155-157`). Both Pro (470) and Lite
  (225) use **32**. Disabled → grey arrow variant, arrow colour α 0.2.

`FxComboBox::setError(true)` recolours `outlineColourId` to `SliderTrack` @ α 1.0
(Dark `#e33250`, Light `#0a4d66`) (`FxComboBox.cpp:61-72`).
`highlightText(bool)` swaps the text colour between `defaultText` and `highlightedText`
(`FxComboBox.cpp:36-54`); it is driven from `FxView::mouseEnter/mouseExit`
(`FxView.cpp:202-221`).

Error notification placement — `FxView::showErrorNotification(true)` (`FxView.cpp:58-72`):

```
b = endpoint_list_.bounds
b.x = b.x - (FxNotification::MAX_WIDTH - b.w)     # MAX_WIDTH = 560  (FxNotification.h:35)
b.y = b.bottom + 5                                # bottom computed BEFORE width/height change
b.w = 560 ; b.h = 120                             # MAX_HEIGHT = 120 (FxNotification.h:36)
error_notification_.set_bounds(b); error_notification_.show_message(false)
```

* **Pro:** `(530 - 90, 72 + 5, 560, 120)` = **`(440, 77, 560, 120)`** in content coords.
* **Lite:** `(285 - 335, 92 + 5, 560, 120)` = **`(-50, 97, 560, 120)`** — 🐛 it hangs 50 px off
  the left edge of a 550 px-wide view and is clipped. Fix in the port (right-align it to the
  combo instead).

Message text (`FxView.cpp:62-63`):
`"FxSound is unable to play processed audio through the selected output device.\nAnother
application could be using it in exclusive mode or the device could be\ndisconnected. To disable
exclusive mode follow these "` + hyperlink `"steps."` →
`https://www.fxsound.com/learning-center/no-sound-with-fxsound-realtek`.

### 5.2 `FxProView` — the big view

Constants (`FxProView.h:44-52`):

| Name | Value |
|---|---:|
| `WIDTH` | **1040** |
| `HEIGHT` | **491** |
| `PRESET_LIST_X` | **40** |
| `OUTPUT_LIST_X` | **530** |
| `LIST_Y` | **32** |
| `AUDIO_X` | **40** |
| `AUDIO_Y` | **88** |
| `LIST_WIDTH` | **470** |
| `LIST_HEIGHT` | **40** |

Constructor (`FxProView.cpp:28-44`): adds `audio_controls_` and `equalizer_` visible,
`visualizer_` as a **hidden** child (`addChildComponent`, `:32`), attaches a `TooltipWindow`
scoped to itself (`:28`), `setOpaque(false)` (`:42`), `setSize(1040, 491)` (`:43`).

`update()` (`FxProView.cpp:56-73`) — **always called before the view is shown**
(`FxMainWindow.cpp:280` and `:312`):

```
audio_controls_.update(); equalizer_.update(); visualizer_.calcGradient()
if FxController::isAudioProcessing(): visualizer_.reset()
visualizer_.setVisible(true)
setSize(WIDTH, HEIGHT + 20)           # -> 1040 × 511    (FxProView.cpp:69)
equalizer_.showValues(true)           # since v2.0 values are always visible (touch screens)
audio_controls_.showValues(true)
```

So **the effective Pro content size is 1040 × 511**, and the visualizer is always visible in
practice. `HEIGHT = 491` is the never-used "no visualizer" size.

Child natural sizes:

| Child | W × H | Source |
|---|---:|---|
| `FxVisualizer` | **960 × 120** | `FxVisualizer.h:51-52`, `setSize` at `FxVisualizer.cpp:40`; `setOpaque(false)` at `:39`; `NUM_BARS = 10` (`FxVisualizer.h:53`) |
| `FxAudioControls` | **168 × 257** | `FxAudioControls.h:147-148`, `setSize` at `FxAudioControls.cpp:50` |
| `FxEqualizer` | **776 × 257** | `FxEqualizer.h:96-97`, `setSize` at `FxEqualizer.cpp:64` |

`resized()` (`FxProView.cpp:80-99`) as pseudo-code:

```
preset_list_.bounds   = (40,  32, 470, 40)
endpoint_list_.bounds = (530, 32, 470, 40)

visualizer_offset = 0
if visualizer_.visible:
    visualizer_.bounds = (40, preset_list_.bottom + 20, visualizer_.w, visualizer_.h)
    #                  = (40, 32 + 40 + 20 = 92, 960, 120)
    visualizer_offset  = visualizer_.h + 20 = 140

audio_controls_.bounds = audio_controls_.bounds.with_x(40).with_y(88 + visualizer_offset)
#                      = (40, 228, 168, 257)
equalizer_.bounds      = equalizer_.bounds.with_x(audio_controls_.right + 16).with_y(88 + offset)
#                      = (40 + 168 + 16 = 224, 228, 776, 257)
```

`paint()` (`FxProView.cpp:101-123`):

```
visualizer_offset = visualizer_.visible ? visualizer_.h + 20 : 0     # = 140
fill_all(windowBackground)                                            # #181818 / #f5f5f5
fill_rounded_rect(20, 16, 1000, 347 + visualizer_offset, r = 8)       # = (20,16,1000,487)
    with FXCOLOR(PanelBackground) @ alpha 0.2                         # #000000 / #c0c0c0

enable = FxModel::getPowerState()
preset_list_.enabled   = enable
audio_controls_.enabled= enable
equalizer_.enabled     = enable
visualizer_.enabled    = enable
# NOTE: endpoint_list_ is deliberately NOT disabled
```

> 🐛 `paint()` mutates enablement — a side effect inside a paint routine. In an immediate-mode
> port this is naturally correct (compute `enabled` each frame), but do not copy the pattern of
> "layout constants baked into the paint function" — derive the panel height from the same
> `visualizer_offset` you use for layout.

Geometry cross-check (content-local):

* Panel `x ∈ [20, 1020)`, content width 1040 ⇒ **20 px margin each side.**
* Combos `x ∈ [40, 510)` and `[530, 1000)` ⇒ **20 px gutter between them**, 20 px inside the panel
  on each side.
* Visualizer `x ∈ [40, 1000)` — 960 wide, aligned with the combos.
* Equalizer right edge `224 + 776 = 1000` — aligned with the output combo.
* Panel `y ∈ [16, 503)`; equalizer bottom `228 + 257 = 485`; content height 511 ⇒ 8 px below the
  panel, 26 px between equalizer bottom and panel bottom.

### 5.3 `FxLiteView` — the compact view

Constants (`FxLiteView.h:37-43`):

| Name | Expression | Value |
|---|---|---:|
| `PRESET_LIST_X` | — | **40** |
| `OUTPUT_LIST_X` | — | **285** |
| `LIST_Y` | — | **42** |
| `BACKGROUND_WIDTH` | `LIST_WIDTH*2 + 20*3` = `225*2 + 60` | **510** |
| `BACKGROUND_HEIGHT` | `LIST_HEIGHT + 40` = `50 + 40` | **90** |
| `WIDTH` | `BACKGROUND_WIDTH + 40` | **550** |
| `HEIGHT` | `BACKGROUND_HEIGHT + 22` | **112** |

Constructor: `setOpaque(false)`, `setSize(550, 112)` (`FxLiteView.cpp:26-27`).

`resized()` (`FxLiteView.cpp:30-45`) — note it uses each combo's **own** `getLocalBounds()`
(225 × 50, never overridden in Lite) and only moves the origin:

```
preset_list_.bounds   = (40,  42, 225, 50)
endpoint_list_.bounds = (285, 42, 225, 50)
```

(The `RectanglePlacement placement(xMid|doNotResize)` declared at `FxLiteView.cpp:32` and the
`auto bounds = getLocalBounds()` at `:34` are **unused** — dead locals.)

`paint()` (`FxLiteView.cpp:47-59`):

```
fill_all(windowBackground)                                   # #181818 / #f5f5f5
fill_rounded_rect(20, 22, 510, 90, r = 10)                   # BACKGROUND_WIDTH × BACKGROUND_HEIGHT
    with FXCOLOR(DefaultFill) @ alpha 0.2                    # #000000 / #ffffff
preset_list_.enabled = FxModel::getPowerState()
# NOTE: endpoint_list_ is NOT gated on power state here either
```

Geometry cross-check: panel `x ∈ [20, 530)` inside a 550 px view ⇒ 20 px margins; combos at
`[40,265)` and `[285,510)` ⇒ 20 px inside the panel and a 20 px gutter; panel `y ∈ [22, 112)`
— i.e. the panel's bottom edge **is** the content's bottom edge; combos `y ∈ [42, 92)` ⇒ 20 px
above and below inside the panel.

Differences Pro vs Lite, at a glance:

| | **Pro** | **Lite** |
|---|---|---|
| content size | 1040 × 511 | 550 × 112 |
| window size | **1040 × 588** | **550 × 189** |
| combo size | 470 × 40 (corner radius 8) | 225 × 50 (corner radius 10) |
| combo y (content) | 32 | 42 |
| panel | `(20,16,1000,487)` r=8, `PanelBackground` α .2 | `(20,22,510,90)` r=10, `DefaultFill` α .2 |
| visualizer | yes, 960 × 120 @ (40,92) | **no** |
| audio controls | yes, 168 × 257 @ (40,228) | no |
| equalizer | yes, 776 × 257 @ (224,228) | no |
| tooltip window | yes (`FxProView.cpp:28`) | no |
| resize-button glyph | `minimize.svg`, 26 × 26 | `maximize.svg`, 24 × 24 |
| window position | restored from `window_x`/`window_y`, saved on move | **snapped to the tray corner every time** |
| power-off disables | preset combo, audio controls, EQ, visualizer | preset combo only |

---

## 6. View switching, positioning, persistence

### 6.1 The switch

```
resize_button_ clicked                         FxMainWindow.cpp:574-578
  -> FxController::switchView()                FxController.cpp:890-903
       if view_ == Pro:  view_ = Lite; main_window_->showLiteView(); settings["view"] = 1
       else:             view_ = Pro;  main_window_->showProView();  settings["view"] = 2
  -> FxMainWindow::setResizeImage()            FxMainWindow.cpp:577 (also called inside show*View)
```

`enum ViewType { Lite = 1, Pro = 2 }` (`FxController.h:40`).

`setResizeImage()` (`FxMainWindow.cpp:349-365`):

```
if FxController::getCurrentView() == Pro:
    images = (MinimizeButton, MinimizeButtonHover);  resize_button_.set_size(26, 26)
else:
    images = (MaximizeButton, MaximizeButtonHover);  resize_button_.set_size(24, 24)
```

i.e. **while in Pro the button shows a "shrink" glyph; while in Lite it shows an "expand" glyph.**

**There is no animation.** The content is swapped and the window is re-bounded in one step
(the 2000 ms `ComponentAnimator` morph lives only in the dead `MainComponent.cpp:190`).

### 6.2 `showLiteView()` — `FxMainWindow.cpp:266-276`

```
set_content(&lite_view_)                                  # -> window becomes 550 × 189
set_resize_image()
set_always_on_top(FxController::isAlwaysOnTop())
bounds = get_bounds()
pos    = FxController::getSystemTrayWindowPosition(bounds.w, bounds.h)
bounds.set_position(pos)
set_bounds(bounds)
```

**Lite always teleports to the tray corner.** It never remembers where the user left it.

`FxSystemTrayView::getSystemTrayWindowPosition(w, h)` — `FxSystemTrayView.cpp:123-170`:

```
Shell_NotifyIconGetRect(&icon_id /* by GUID */, &rect)   # physical pixels of the tray icon
if it fails: return (0, 0)
lrect = Desktop::physicalToLogical(rect)
area  = primary_display.userArea                          # excludes the taskbar
pos.x = (lrect.x < area.centre_x) ? area.x + 10 : area.right  - w - 10
pos.y = (lrect.y < area.centre_y) ? area.y + 10 : area.bottom - h - 10
```

**10 px inset from the work-area corner nearest the tray icon.**

### 6.3 `showProView()` — `FxMainWindow.cpp:278-308`

```
pro_view_.update()                       # -> content 1040 × 511
set_content(&pro_view_)                  # -> window 1040 × 588
set_resize_image()
set_always_on_top(FxController::isAlwaysOnTop())

bounds         = get_bounds()
desktop_bounds = Desktop::getDisplays().getTotalBounds(true)      # union of all monitors, work areas
FxController::getWindowPosition(x, y)                              # settings "window_x","window_y", default 0
if x == 0 and y == 0:
    centre_with_size(w, h)
else:
    bounds.set_position(x, y)
    if desktop_bounds.contains(bounds): set_bounds(bounds)
    else:                               centre_with_size(w, h)
```

**Pro remembers its position; Lite does not.** `(0,0)` is used as the sentinel for "never saved",
so a window genuinely dragged to `(0,0)` will re-centre on next launch — a real (minor) bug.

### 6.4 Saving the position — `FxMainWindow::moved()` — `FxMainWindow.cpp:618-630`

```
if FxController::getCurrentView() == Pro:
    bounds = get_bounds()
    if Desktop::getDisplays().getTotalBounds(true).contains(bounds):
        FxController::saveWindowPosition(bounds.x, bounds.y)
```

→ `settings["window_x"] = x; settings["window_y"] = y` (`FxController.cpp:2629-2633`);
read back by `getWindowPosition` with default 0 (`FxController.cpp:2635-2639`).
**Only fully on-screen positions are persisted**, and **only in Pro**.

### 6.5 Settings store

`FxSound::Settings` wraps `juce::ApplicationProperties` / `PropertiesFile`
(`Settings.h:53-57`), with `applicationName = L"FxSound"`, `folderName = L"FxSound"`,
`filenameSuffix = L"settings"` (user) and `L"secure"` (`Settings.h:29-32`,
`Settings.cpp:42-50`). On Windows that resolves to
`%APPDATA%\FxSound\FxSound.settings` (XML). Built-in defaults are an inline XML fallback set
(`Settings.cpp:29-40`) containing `power=1`, `hotkeys=1`, `preset=General`, and the five hotkey
codes.

Keys this subsystem owns:

| Key | Type | Meaning | Source |
|---|---|---|---|
| `view` | int | `1` = Lite, `2` = Pro; anything outside `(0,2]` ⇒ Pro | `FxController.cpp:173-180`, `:895, :901` |
| `window_x`, `window_y` | int | Pro window top-left | `FxController.cpp:2629-2639` |
| `always_on_top` | bool | | `FxController.cpp:190`, `:2777-2787` |
| `run_minimized` | bool | start hidden in the tray | `FxController.cpp:916`, `:931`, `:775-782` |
| `theme_mode` | int | `0` = Dark, `1` = Light; clamped | `FxController.cpp:755-771`, `:2765` |
| `menu_clicked` | bool | suppresses the first-run help bubble | `FxController.cpp:975-979` |

Command-line override: `--view 1|2` writes `settings["view"]` and sets `view_` immediately
(`FxController.cpp:259-267`); `--run_minimized` sets `run_minimized = true`
(`FxController.cpp:236-239`).

---

## 7. Window lifecycle: show, hide, minimise, close, tray

| Action | Implementation | Net effect |
|---|---|---|
| **Startup** | `FxController::init` → `showView()` (`FxController.cpp:753`) → `showProView()`/`showLiteView()`; then `if (!settings["run_minimized"]) showMainWindow() else hideMainWindow()` (`:775-782`) | window appears, or stays tray-only |
| **Show** | `FxMainWindow::show()` (`:243-264`): `setVisible(true)`; `addToDesktop(windowAppearsOnTaskbar)`; `toFront(true)`; then Win32 `IsIconic → ShowWindow(SW_RESTORE)`, `SetForegroundWindow`, `SetWindowPos(HWND_TOP, SWP_NOMOVE\|SWP_NOSIZE\|SWP_SHOWWINDOW)` | force-raise + focus |
| **Close ✕** | `TitleBar::buttonClicked` → `FxWindow::closeButtonPressed()` → `FxMainWindow::closeButtonPressed()` (`:613-616`) → `FxController::hideMainWindow()` | **hides to tray; does NOT quit** |
| **Alt+F4 / WM_CLOSE** | `FxMainWindow::userTriedToCloseWindow()` (`:608-611`) → `hideMainWindow()` | same |
| **Hide to tray** | `FxController::hideMainWindow()` (`:911-927`): `removeFromDesktop(); setVisible(false); settings["run_minimized"] = true`; first time only, after **2000 ms**, pushes `TRANS("FxSound in system tray\r\nClick FxSound icon to reopen")` | window peer destroyed entirely |
| **Minimise —** | `FxMainWindow::buttonClicked` (`:579-585`): `ShowWindow((HWND)getWindowHandle(), SW_MINIMIZE)` | normal taskbar minimise |
| **Minimise-box enable** | `visibilityChanged()` (`:434-446`): OR `WS_MINIMIZEBOX` into `GWL_STYLE` if missing | makes taskbar minimise/restore work on a frameless window |
| **Tray "Open"** | `FxSystemTrayView.cpp:253-255` → `FxController::showMainWindow()` (`:929-963`), which sets `run_minimized = false` and calls `show()` | |
| **Quit** | only from the tray menu **Exit** (`FxSystemTrayView.cpp:285-287`) → `FxController::exit()` (`FxController.cpp:993-1000`): auto-saves the modified preset, then `systemRequestedQuit()` | |

Tray context menu (`FxSystemTrayView.cpp:311-322`), in order:
`Open`, `Turn On`/`Turn Off`, [`Preset Select` ▸ only when powered], output devices (inline if
≤ 5, else a submenu — `FxSystemTrayView.cpp:337-345`), `Settings`, `Theme` ▸ (`Dark`/`Light`),
`Always On Top` (ticked), `Donate`, `Exit`.

Window icon (`FxMainWindow::setIcon(power, processing)` — `FxMainWindow.cpp:367-403`), via
`SendMessage(hWnd, WM_SETICON, ICON_SMALL, …)`; the old icon is `DestroyIcon`-ed:

| power | processing | theme | resource |
|---|---|---|---|
| true | true | Dark | `IDI_LOGO_RED` |
| true | true | Light | `IDI_LOGO_BLUE` |
| true | false | any | `IDI_LOGO_WHITE` |
| false | — | any | `IDI_LOGO_GRAY` |

Always-on-top: `FxController::setAlwaysOnTop` (`FxController.cpp:2782-2787`) persists and calls
`main_window_->setAlwaysOnTop(...)`; re-applied on every view switch
(`FxMainWindow.cpp:270, 283`).

Theme change: `FxController::setThemeMode` (`FxController.cpp:2760-2775`) → `FxTheme::setThemeMode`,
persists `theme_mode`, reloads the font for the language, then
`main_window_->sendLookAndFeelChange()`, which reaches `FxMainWindow::lookAndFeelChanged()`
(`FxMainWindow.cpp:632-636`) → `setLookAndFeel()` (re-creates every `Drawable` from the new theme
row, `FxMainWindow.cpp:325-347`) → `repaint()`.

---

## 8. ASCII wireframes with real pixel numbers

### 8.1 Pro view — window **1040 × 588** (Dark: bg `#181818`, corners r = 21)

```
 x=0                                                                                        x=1040
 ┌────────────────────────────────────────────────────────────────────────────────────────────┐ y=0
 │ ╭─ rounded corner r=21                                            corner r=21 ──╮          │
 │                                                                                            │
 │  ┌──── TITLE BAR component: x=21..1018 (w=998), y=0..55 (h=56) ──────────────────────────┐  │
 │  │                                                                                      │  │
 │  │ [FxSound wordmark]        [☰]                   [♥]   [⏻]   [⤡]   [—]        [✕]     │  │
 │  │  x=21 w=106 h=15           x=142                x=822 x=868 x=912 x=958      x=1004  │  │
 │  │  y=21 (transform)          y=16 24×24           y=13  y=16  y=15  y=13       y=20    │  │
 │  │                                                 26×30 24×24 26×26 26×30      15×15   │  │
 │  └──────────────────────────────────────────────────────────────────────────────────────┘  │
 ├────────────────────────────────────────────────────────────────────────────────────────────┤ y=56  1px #0f0f0f
 │  CONTENT = FxProView at (0,57), 1040 × 511                                                 │ y=57
 │                                                                                            │
 │    ┌────── panel: content-local (20,16) 1000×487, r=8, PanelBackground α0.2 ───────────┐   │ y=73
 │    │                                                                                   │   │
 │    │  ┌──── preset combo ─────┐          ┌──── output combo ─────┐                     │   │ y=89
 │    │  │  470 × 40, r=8        │  gutter  │  470 × 40, r=8        │                     │   │
 │    │  │  local (40,32)        │◄──20──►  │  local (530,32)       │                     │   │
 │    │  └───────────────────────┘          └───────────────────────┘                     │   │ y=129
 │    │        abs x=40..510                       abs x=530..1000                        │   │
 │    │                                                                                   │   │
 │    │  ┌──────────────── FxVisualizer 960 × 120 ─────────────────────────────────────┐  │   │ y=149
 │    │  │  local (40,92)   abs (40,149)   10 bars                                     │  │   │
 │    │  └─────────────────────────────────────────────────────────────────────────────┘  │   │ y=269
 │    │                                                                                   │   │
 │    │  ┌── FxAudio ──┐ ┌──────────────── FxEqualizer 776 × 257 ────────────────────┐    │   │ y=285
 │    │  │ Controls    │ │  local (224,228)   abs (224,285)                          │    │   │
 │    │  │ 168 × 257   │ │  ◄─16px gap between audio.right(208) and eq.left(224)     │    │   │
 │    │  │ local(40,228)│ │                                                          │    │   │
 │    │  └─────────────┘ └──────────────────────────────────────────────────────────┘    │   │ y=542
 │    │                                                                                   │   │
 │    └───────────────────────────────────────────────────────────────────────────────────┘   │ y=560
 │                                                                                            │
 │ ╰─ 21px bottom pad (effective 20px below content: 568..588) ─────────────────────────────╯ │
 └────────────────────────────────────────────────────────────────────────────────────────────┘ y=588
   x=20                                                                              x=1020
```

Absolute (window) child rects, Pro:

| Element | x | y | w | h |
|---|---:|---:|---:|---:|
| title bar | 21 | 0 | 998 | 56 |
| divider line | 0 | 56 | 1040 | 1 |
| content (`FxProView`) | 0 | 57 | 1040 | 511 |
| panel rounded rect | 20 | 73 | 1000 | 487 |
| preset combo | 40 | 89 | 470 | 40 |
| output combo | 530 | 89 | 470 | 40 |
| visualizer | 40 | 149 | 960 | 120 |
| audio controls | 40 | 285 | 168 | 257 |
| equalizer | 224 | 285 | 776 | 257 |
| error notification (when shown) | 440 | 134 | 560 | 120 |

### 8.2 Lite view — window **550 × 189**

```
 x=0                                                              x=550
 ┌──────────────────────────────────────────────────────────────────┐ y=0
 │ ╭─ r=21                                                   r=21 ─╮│
 │  ┌── TITLE BAR: x=21..528 (w=508), y=0..55 (h=56) ─────────────┐ │
 │  │ [FxSound wordmark]  [☰]        [♥]  [⏻]  [⤢] [—]     [✕]   │ │
 │  │  x=21 w=106 h=15    x=142      x=334 x=380 x=424 x=468 x=514│ │
 │  │  y=21               y=16       y=13  y=16  y=16  y=13  y=20 │ │
 │  │                     24×24      26×30 24×24 24×24 26×30 15×15│ │
 │  └─────────────────────────────────────────────────────────────┘ │
 ├──────────────────────────────────────────────────────────────────┤ y=56  1px #0f0f0f
 │  CONTENT = FxLiteView at (0,57), 550 × 112                       │ y=57
 │                                                                  │
 │   ┌─ panel: local (20,22) 510×90, r=10, DefaultFill α0.2 ──────┐  │ y=79
 │   │                                                            │  │
 │   │  ┌── preset combo ──┐        ┌── output combo ──┐          │  │ y=99
 │   │  │ 225 × 50, r=10   │◄─20──► │ 225 × 50, r=10   │          │  │
 │   │  │ local (40,42)    │        │ local (285,42)   │          │  │
 │   │  └──────────────────┘        └──────────────────┘          │  │ y=149
 │   │      abs x=40..265               abs x=285..510            │  │
 │   │                                                            │  │
 │   └────────────────────────────────────────────────────────────┘  │ y=169  (= content bottom)
 │ ╰─ 20px bottom pad ───────────────────────────────────────────────╯│
 └──────────────────────────────────────────────────────────────────┘ y=189
   x=20                                                      x=530
```

Absolute (window) child rects, Lite:

| Element | x | y | w | h |
|---|---:|---:|---:|---:|
| title bar | 21 | 0 | 508 | 56 |
| divider line | 0 | 56 | 550 | 1 |
| content (`FxLiteView`) | 0 | 57 | 550 | 112 |
| panel rounded rect | 20 | 79 | 510 | 90 |
| preset combo | 40 | 99 | 225 | 50 |
| output combo | 285 | 99 | 225 | 50 |
| error notification (when shown) 🐛 | −50 | 154 | 560 | 120 |

---

## 9. Full component tree with the layout maths as pseudo-code

```
FxMainWindow  (desktop, frameless, non-opaque, non-resizable, shadow_width_ = 0)
│   size = (content.w, content.h + 56 + 21)
│
├── TitleBar  @ (21, 0, W-42, 56)
│   ├── Drawable  icon_            @ (0, 0, 106, 15)  + transform to (0, 21, 106, 15)
│   ├── Drawable  animation_icon_  @ same, alpha 0 at rest, 600 ms cross-fade
│   ├── Label     title_           (INVISIBLE for the main window)
│   ├── DrawableButton menu_button_      @ (x_left = 121, floor((56-24)/2) = 16, 24, 24)   [left]
│   ├── DrawableButton donate_button_    @ (tb_w - 171, 13, 26, 30)                        [right #4]
│   ├── FxPowerButton  power_button_     @ (tb_w - 127, 16, 24, 24)                        [right #3]
│   ├── DrawableButton resize_button_    @ (tb_w -  81, 15|16, 26|24, 26|24)               [right #2]
│   ├── DrawableButton minimize_button_  @ (tb_w -  35, 13, 26, 30)                        [right #1]
│   └── CloseButton    close_button_     @ (tb_w -  15, 20, 15, 15)
│
└── content_  @ (0, 57, content.w, content.h)     ← FxProView | FxLiteView (never both)
    │
    ├─ FxProView (1040 × 511)
    │   ├── FxComboBox   preset_list_    @ (40,  32, 470, 40)
    │   ├── FxComboBox   endpoint_list_  @ (530, 32, 470, 40)
    │   ├── FxVisualizer visualizer_     @ (40,  92, 960, 120)
    │   ├── FxAudioControls audio_controls_ @ (40,  228, 168, 257)
    │   ├── FxEqualizer  equalizer_      @ (224, 228, 776, 257)
    │   ├── FxNotification error_notification_ @ (440, 77, 560, 120)  [hidden by default]
    │   └── TooltipWindow tool_tip_       (own desktop window)
    │
    └─ FxLiteView (550 × 112)
        ├── FxComboBox   preset_list_    @ (40,  42, 225, 50)
        ├── FxComboBox   endpoint_list_  @ (285, 42, 225, 50)
        └── FxNotification error_notification_ @ (-50, 97, 560, 120)  [hidden; 🐛 clipped]
```

Consolidated layout algorithm (what an egui frame must compute each tick):

```
fn layout(view: ViewType) -> Layout {
    let (cw, ch) = match view {
        Pro  => (1040, 511),        // FxProView::WIDTH, HEIGHT + 20
        Lite => ( 550, 112),        // FxLiteView::WIDTH, HEIGHT
    };
    let (ww, wh) = (cw, ch + 56 + 21);

    let tb = Rect::from_xywh(21, 0, ww - 42, 56);
    let divider_y = 56;
    let content = Rect::from_xywh(0, 57, cw, ch);

    // ---- title bar ----
    let logo = Rect::from_xywh(tb.x + 0, tb.y + 21, 106, 15);
    let mut x_right = 15 + 20;                       // 35
    let mut x_left  = 106 + 15;                      // 121
    let mut place_right = |w, h| { let r = Rect::from_xywh(tb.x + tb.w - x_right - w,
                                                           tb.y + ((56 - h) / 2), w, h);
                                   x_right += w + 20; r };
    let close    = Rect::from_xywh(tb.x + tb.w - 15, tb.y + 20, 15, 15);
    let minimize = place_right(26, 30);
    let resize   = if view == Pro { place_right(26, 26) } else { place_right(24, 24) };
    let power    = place_right(24, 24);
    let donate   = place_right(26, 30);
    let menu     = Rect::from_xywh(tb.x + x_left, tb.y + ((56 - 24) / 2), 24, 24);

    // ---- content ----
    match view {
        Pro => {
            let preset = content.offset(40, 32).size(470, 40);
            let output = content.offset(530, 32).size(470, 40);
            let vis    = content.offset(40, preset.local_bottom() + 20).size(960, 120);  // y=92
            let off    = 120 + 20;                                                       // 140
            let audio  = content.offset(40, 88 + off).size(168, 257);                    // y=228
            let eq     = content.offset(40 + 168 + 16, 88 + off).size(776, 257);         // x=224
            let panel  = content.offset(20, 16).size(1000, 347 + off);                   // 1000×487, r=8
        }
        Lite => {
            let preset = content.offset(40, 42).size(225, 50);
            let output = content.offset(285, 42).size(225, 50);
            let panel  = content.offset(20, 22).size(510, 90);                           // r=10
        }
    }
}
```

---

## 10. JUCE → egui / Rust idiom map

| JUCE idiom (this subsystem) | egui / eframe 0.36 / winit equivalent |
|---|---|
| `Component::resized()` writing absolute child bounds | Immediate mode: compute the same `egui::Rect`s each frame and draw with `ui.put(rect, widget)` / `ui.allocate_new_ui(UiBuilder::new().max_rect(rect), …)`. **Do not** try to express this fixed-pixel design with `Layout`/`with_layout`; it is a pixel-perfect absolute layout. |
| `Component::paint(Graphics&)` | `ui.painter()` / `egui::Painter::rect_filled`, `::line_segment`, `::add(Shape::…)` |
| `g.fillRoundedRectangle(x,y,w,h,r)` | `painter.rect_filled(rect, CornerRadius::same(r as u8), color)` |
| `juce::DropShadow{radius}` + `drawForPath` | `egui::epaint::Shadow { offset: [0,0], blur: radius, spread: 0, color }` on a `Frame` |
| `LookAndFeel_V4` / `FxTheme` | `egui::Style` + `egui::Visuals` for the coarse stuff, **plus** a hand-rolled `struct FxPalette { window_bg: Color32, panel_bg: Color32, … }` in `egui::Memory`/app state, because the FxSound palette (27 named colours × 2 modes) does not map onto egui's `Visuals` fields. |
| `FxTheme::setThemeMode` + `sendLookAndFeelChange()` | swap the `FxPalette`, call `ctx.set_style(...)`, `ctx.request_repaint()`; re-rasterise the SVGs (see below) |
| `Drawable::createFromImageData(svg)` | `resvg` + `tiny-skia` → `egui::ColorImage` → `TextureHandle`; or pre-bake PNGs. Cache per (asset, theme, size); re-bake on theme change. `egui_extras::image` + `image` crate also works but SVG needs `resvg`. |
| `DrawableButton(ImageFitted)` with normal/hover images | `ImageButton::new(tex)` + `response.hovered()` to pick the texture, or `Button::image` with a manual hover swap. Keep the exact 26×30 / 24×24 rects. |
| `Desktop::getAnimator().fadeIn/Out(600ms)` | store `t0: Instant`; `alpha = ((now - t0)/600ms).clamp(0,1)`; `ctx.request_repaint_after(…)`; tint the texture with `Color32::from_white_alpha(a)` |
| `ComponentDragger` on the title bar | `if title_bar_response.drag_started() { ctx.send_viewport_cmd(ViewportCommand::StartDrag) }`. **This is the only correct way on Wayland** — manual `OuterPosition` is ignored. |
| `setAlwaysOnTop(bool)` | `ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop \| Normal)` — see §11, this is an X11-only capability |
| `ShowWindow(SW_MINIMIZE)` | `ViewportCommand::Minimized(true)` |
| `removeFromDesktop()` / `addToDesktop()` (hide to tray) | `ViewportCommand::Visible(false)` / `(true)` + `ViewportCommand::Focus`. Keep the event loop alive (`eframe` with `run_and_return = false` and a tray thread). |
| `setOpaque(false)` + rounded fill | `ViewportBuilder::with_transparent(true)` + `with_decorations(false)`, then `CentralPanel::default().frame(Frame::NONE.fill(Color32::TRANSPARENT))` and paint the rounded rect yourself. |
| `addToDesktop(windowAppearsOnTaskbar)` non-resizable | `ViewportBuilder::with_inner_size([1040.0, 588.0]).with_resizable(false).with_decorations(false)` |
| `setSize()` on view switch | `ViewportCommand::InnerSize(Vec2::new(550.0, 189.0))` |
| `centreWithSize(w,h)` | `ViewportBuilder::with_position(...)` at startup only; at runtime on Wayland this is **not possible** — see §11 |
| `PopupMenu` (`showAt(&menu_button_)`) | `egui::menu::menu_button` / `ui.menu_button`, or a manual `egui::Area` + `Order::Foreground` anchored to the button rect. Nested submenus (`Theme ▸`) and tick marks need `ui.selectable_label` / a custom item. |
| `PopupMenu::addCustomItem` (`FxPresetMenuItem`, 200×30 `TextEditor`) | a custom `Ui` closure inside the menu with `TextEdit::singleline` + `char_limit(64)` and a manual 2 px outline |
| `BubbleMessageComponent` (help bubble) | `egui::Area` + `Frame::popup`, or `Tooltip`/`response.on_hover_ui`. Note JUCE's has `timeout = 0` (never auto-dismiss) and dismiss-on-click, which `on_hover_ui` does not replicate — hand-roll it. |
| `TooltipWindow` (Pro view) | `response.on_hover_text(...)`; style via `Style::visuals.window_fill` |
| `setHelpText(...)` (accessibility) | `response.widget_info(...)` / `AccessKit` via `egui`'s `accesskit` feature |
| `setWantsKeyboardFocus` / `FocusContainerType::keyboardFocusContainer` | `Response::request_focus()`, `ui.memory_mut(\|m\| m.request_focus(id))`; egui's tab order is insertion order — insert widgets in the JUCE order to match |
| `keyPressed(Alt+Return)` → menu | `ctx.input(\|i\| i.modifiers.alt && i.key_pressed(Key::Enter))` |
| `MouseCursor::PointingHandCursor` | `response.on_hover_cursor(CursorIcon::PointingHand)` |
| `Component::hitTest` default (whole rect, corners included) | egui: the whole viewport rect receives input; transparent corners stay clickable — identical behaviour, nothing to do |
| `juce::PropertiesFile` (`%APPDATA%\FxSound\FxSound.settings`) | `serde` + `toml`/`ron` at `$XDG_CONFIG_HOME/fxsound/config.toml` (fall back to `~/.config/fxsound/`), or eframe's `Storage` (`eframe::set_value`/`get_value`) |
| `RectanglePlacement(xRight\|yMid\|doNotResize)` | trivial arithmetic — `Rect::from_min_size(pos2(dest.right() - w, dest.center().y - h/2.0), vec2(w,h))`. Do **not** import a placement abstraction; just write the maths in §9. |
| `juce::Font(typeface).withHeight(17.0)` | `FontId::new(size_em, FontFamily::Name("GilroySemibold".into()))` after `FontDefinitions` registration. Recalibrate `size_em` (≈ 14.2 for 17 px) — see §2.3. |

---

## 11. Windows-specific machinery → Linux/Wayland/PipeWire substitutes

Ordered by how much it will hurt.

### 11.1 Absolute window positioning — **hard blocker**

**What Windows does:** `setBounds(x, y, …)`, `centreWithSize`, `getSystemTrayWindowPosition`
(10 px inset from the tray corner, `FxSystemTrayView.cpp:149-166`), `saveWindowPosition` /
`getWindowPosition` (`FxController.cpp:2629-2639`), `Desktop::getDisplays().getTotalBounds(true)`
containment checks.

**Wayland reality:** an `xdg_toplevel` client **cannot know or set its own position**. winit's
`Window::set_outer_position` is a documented no-op on Wayland; `outer_position()` returns
`Err(NotSupportedError)`. `ViewportCommand::OuterPosition` will be silently dropped.

**Recommended substitutes:**

* **Pro view:** drop position persistence entirely on Wayland. Let the compositor place the
  window. Keep `window_x`/`window_y` in the config file only for an X11/XWayland fallback
  (winit does support positioning there), and gate it on
  `matches!(raw_window_handle, RawWindowHandle::Xlib(_) | ::Xcb(_))`.
* **Lite view corner-snapping:** this is the one case where it genuinely matters, and there *is*
  a real answer — **`wlr-layer-shell-unstable-v1`** (`zwlr_layer_surface_v1`) on the `Overlay` or
  `Top` layer, `set_anchor(BOTTOM | RIGHT)`, `set_margin(0, 10, 10, 0)`, `set_size(550, 189)`.
  Supported by every wlroots compositor (Sway, Hyprland, river, Niri) and by KDE Plasma.
  GNOME/Mutter does **not** implement layer-shell — on GNOME, fall back to a plain toplevel and
  accept compositor placement. Crates: `smithay-client-toolkit` (`LayerSurface`), or
  `egui-wgpu` + raw `wayland-client` if you want to keep egui. Note that eframe/winit does not
  expose layer-shell; you would need a second windowing path. **Budget this as real work, or
  declare corner-snapping a Windows-only behaviour.**
* `Desktop::getDisplays().getTotalBounds(true)` → `winit::EventLoop::available_monitors()` gives
  you `MonitorHandle::position()`/`size()` on X11; on Wayland you get output geometry via
  `wl_output` but you still cannot use it to place yourself.

### 11.2 Always on top

**Windows:** `Component::setAlwaysOnTop` → `SetWindowPos(HWND_TOPMOST)`.

**Linux:** `ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop)` works on **X11**
(`_NET_WM_STATE_ABOVE`). On **Wayland there is no xdg-shell protocol for it** — the request is
ignored. Substitutes, in order of preference:

1. `wlr-layer-shell` `Top`/`Overlay` layer (same caveat as above — no GNOME).
2. Expose it as a compositor-side hint and document it: Sway `for_window [app_id="fxsound"] floating enable, sticky enable`,
   Hyprland `windowrulev2 = pin, class:^(fxsound)$`.
3. Keep the menu item, but grey it out with a tooltip on Wayland rather than silently doing
   nothing.

### 11.3 System tray

**Windows:** `Shell_NotifyIcon` with a GUID (`FxSystemTrayView.cpp:172-…`), a hidden message-only
`HWND`, `WM_APP+1` callback (`FxSystemTrayView.h:50`), `Shell_NotifyIconGetRect`, balloon
notifications (`NIF_INFO | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME`, `FxSystemTrayView.cpp:409-411`).

**Linux:** there is no Wayland tray protocol. Use the **StatusNotifierItem / KStatusNotifierItem
D-Bus spec** (`org.kde.StatusNotifierItem`) — registered with
`org.kde.StatusNotifierWatcher`, plus a `com.canonical.dbusmenu` export for the context menu.

* Crate: **`ksni`** (pure Rust, async, exports both SNI and dbusmenu). Alternative:
  `libappindicator` via `tray-icon` (used by Tauri) — `tray-icon` also handles the GTK/AppIndicator
  fallback that Ubuntu/GNOME needs (GNOME requires the *AppIndicator and KStatusNotifierItem
  Support* extension; there is no built-in tray).
* The four state icons (`IDI_LOGO_{RED,BLUE,WHITE,GRAY}`) become SNI `IconName` (a themed icon
  name) or `IconPixmap` (ARGB32). Ship them as `hicolor` icons named
  `fxsound-{processing,on,off}` and switch `IconName`, which is much cheaper than pushing pixmaps.
* Balloon tips → **`org.freedesktop.Notifications.Notify`** (crate `notify-rust`). Map
  `NIIF_NOSOUND` → hint `"suppress-sound": true`; `NIIF_RESPECT_QUIET_TIME` has no equivalent,
  drop it. The custom `FxNotification` toast (a 560×120 always-on-top borderless window,
  `FxNotification.h:35-36`) is better replaced by a real desktop notification on Linux — it will
  respect the user's DND and positioning preferences, which the hand-rolled window cannot.
* `Shell_NotifyIconGetRect` has **no Linux equivalent**. See §11.1 for what to do about Lite's
  corner-snapping.

### 11.4 Foreground/activation

**Windows:** `SetForegroundWindow` + `SetWindowPos(HWND_TOP, SWP_SHOWWINDOW)`
(`FxMainWindow.cpp:261-262`), `IsIconic`/`SW_RESTORE` (`:258-259`).

**Linux:** X11 honours `_NET_ACTIVE_WINDOW`. Wayland requires an **`xdg-activation-v1` token**,
which you can only obtain from an event the compositor attributes to the user — in practice, the
tray click. Flow: tray item's `Activate` handler → request an activation token from the
compositor → pass it to `wl_surface`/toplevel activation. winit surfaces this as
`Window::request_user_attention` (fallback: urgency hint) and, in recent versions,
activation-token plumbing. Practical advice: `ViewportCommand::Visible(true)` +
`ViewportCommand::Focus`, and accept that on some compositors the window will only get an
attention flag rather than focus.

### 11.5 Global hotkeys

**Windows:** `RegisterHotKey(message_window_, CMD_*, mod, vk)` for five commands
(`FxController.cpp:2820-2860`; IDs `CMD_ON_OFF=1001` … `CMD_NEXT_OUTPUT=1005`,
`FxController.h:219-223`; keys `cmd_on_off` / `cmd_open_close` / `cmd_next_preset` /
`cmd_previous_preset` / `cmd_change_output`, `FxController.h:54-58`; defaults `393297`, `393285`,
`393281`, `393306`, `393303`, `Settings.cpp:33-37`).

**Linux:** a Wayland client **cannot grab global keys**, full stop. Three-part answer:

1. **XDG desktop portal `org.freedesktop.portal.GlobalShortcuts`** — the sanctioned route.
   Supported by KDE ≥ 5.27 and (partially) by wlroots portals. Crate: `ashpd`
   (`ashpd::desktop::global_shortcuts`). The *compositor* owns the binding UI; you register
   shortcut *ids* (`on_off`, `open_close`, `next_preset`, `previous_preset`, `next_output`) and
   receive `Activated` signals. The FxSound "set your own hotkey" settings page must therefore
   become "open the system shortcut settings", not a key-capture widget.
2. **MPRIS2** (`org.mpris.MediaPlayer2.Player`) for media keys — gives you `PlayPause`, `Next`,
   `Previous` for free from every desktop. Map `Next`/`Previous` → next/previous preset. Crate:
   `mpris-server` or `zbus` directly.
3. **A CLI + D-Bus activation fallback**: keep the existing `--power`, `--preset`, `--view`,
   `--output` command-line interface (`FxController.cpp:225-267`) and document compositor
   bindings (`bindsym $mod+F9 exec fxsound --power 0`). This also replaces
   `anotherInstanceStarted` (see §11.7).

### 11.6 Per-window icon

**Windows:** `WM_SETICON` with `IDI_LOGO_{RED,BLUE,WHITE,GRAY}` (`FxMainWindow.cpp:367-403`).

**Linux/Wayland:** there is no per-window icon protocol in xdg-shell (the `xdg-toplevel-icon-v1`
protocol is new and thinly implemented). The taskbar icon comes from the **`.desktop` file matched
by `xdg_toplevel.set_app_id`**. So:

* Set `ViewportBuilder::with_app_id("com.fxsound.FxSound")` and ship
  `/usr/share/applications/com.fxsound.FxSound.desktop` + `hicolor` icons.
* The **dynamic power/processing state must be shown in the tray icon (§11.3)**, not the window
  icon. Optionally also tint the in-window wordmark (which the code already does via the 600 ms
  cross-fade, §3.7) — on Linux that is the primary state indicator.

### 11.7 Single-instance

**Windows:** `moreThanOneInstanceAllowed() == false` (`Main.cpp:46`) with
`anotherInstanceStarted(commandline)` → `FxController::applyConfig` (`Main.cpp:138`).

**Linux:** own a well-known D-Bus name `com.fxsound.FxSound` with
`RequestName(DO_NOT_QUEUE)`; if it is taken, call a method on the running instance
(`ApplyConfig(as args)` or the freedesktop `org.freedesktop.Application.ActivateAction`) and exit.
Crate: `zbus`. A `$XDG_RUNTIME_DIR/fxsound.lock` flock is the low-tech fallback.

### 11.8 Miscellany

| Windows thing | Where | Linux answer |
|---|---|---|
| `WS_MINIMIZEBOX` style patch | `FxMainWindow.cpp:434-446` | not needed; `ViewportCommand::Minimized` works on a decorationless toplevel |
| `updater.exe /checknow`, `/silent` | `FxMainWindow.cpp:486`, `FxController.cpp:2626` | delete. Ship via the distro / Flatpak; if you must, check a JSON endpoint and point the user at their package manager. Never self-update. |
| `RegDeleteTree(HKEY_CURRENT_USER, "Software\\DFX")` on version change | `FxController.cpp:717` | delete the legacy config dir, or just no-op |
| `HKCU\...\CurrentVersion\Run` for launch-on-startup | `FxController.cpp:2793-2810` | write `$XDG_CONFIG_HOME/autostart/com.fxsound.FxSound.desktop`, or use the `org.freedesktop.portal.Background` `RequestBackground(autostart: true)` portal (required under Flatpak) |
| `CoInitializeEx` / `CoInitializeSecurity` | `Main.cpp:56-61` | none; PipeWire needs a `pw_thread_loop` instead |
| `SetUnhandledExceptionFilter` + `MiniDumpWriteDump` | `Main.cpp:54, 164` | `std::panic::set_hook` + `backtrace`; optionally `minidumper`/`crashpad`. Write to `$XDG_STATE_HOME/fxsound/`. |
| `WTSRegisterSessionNotification`, `RegisterSuspendResumeNotification` | `FxController.cpp:205, 797` | `org.freedesktop.login1.Manager` D-Bus `PrepareForSleep` signal + `SessionRemoved`/`Lock`; crate `zbus` |
| `SysInfo::isRemoteSession()` gating the power button | `FxController.cpp:1013`, `FxMainWindow.cpp:413` | detect `$WAYLAND_DISPLAY` over `waypipe`/`$SSH_CONNECTION` if you care at all; realistically **drop this gate** — there is no RDP-equivalent constraint on a PipeWire sink |
| `NvOptimusEnablement = 0`, `AmdPowerXpressRequestHighPerformance = 0` | `Main.cpp:32-34` | pick the integrated GPU via `wgpu::PowerPreference::LowPower` in `eframe::egui_wgpu::WgpuConfiguration`, or use the `glow` backend |
| Windows DPI scaling (JUCE handles implicitly; only `physicalToLogical` appears, `FxSystemTrayView.cpp:145`) | | `wp_fractional_scale_v1`; winit reports `scale_factor`; egui handles it via `pixels_per_point`. **All numbers in this document are logical points at scale 1.0.** |

---

## 12. Concrete recommendations for the Rust implementation

1. **Model the window as one `eframe` viewport with two fixed sizes.** Keep a
   `enum ViewMode { Lite, Pro }` in app state; on switch, `ViewportCommand::InnerSize`.
   Do not try to animate it; the original does not.
2. **Put every constant in one `mod geom`** with the exact names from the C++ so the mapping is
   auditable: `WINDOW_CORNER_RADIUS: f32 = 21.0`, `TITLE_BAR_H: f32 = 56.0`,
   `PRO_CONTENT: Vec2 = vec2(1040.0, 511.0)`, `LITE_CONTENT: Vec2 = vec2(550.0, 112.0)`, etc.
3. **Write `fn title_bar(ui, &mut state, view) -> TitleBarResponse`** that reproduces §3.6's
   `x_right`/`x_left` accumulator *literally*. It is 20 lines and it guarantees pixel parity.
4. **Fix the three bugs** listed inline (Lite error-notification x = −50; the divider's
   `W - 2*sw`; the `x >= 0 && y >= 0` recentring guard), and note them in a porting changelog so a
   future diff against upstream does not "restore" them.
5. **Do not port** `MainComponent.*`, `showAnnouncement()`, the colour-scheme double-click cycler,
   or the 2000 ms morph animation.
6. **Audio side (out of scope here, but it constrains the window):** `FxController` drives the
   visualizer at frame rate (`FxProView::startVisualizer`/`pauseVisualizer`,
   `FxMainWindow.cpp:315-323`) and is paused when the window is hidden. Keep that: when the
   viewport is `Visible(false)`, stop requesting repaints and stop pulling spectrum data from the
   PipeWire filter node, or you will burn CPU while tray-only.

---

## Open questions / risks for the Rust port

1. **Lite view corner-snapping is not portable.** `Shell_NotifyIconGetRect` + absolute positioning
   has no Wayland equivalent. Decide early: (a) `wlr-layer-shell` second windowing path — costs a
   non-eframe render path and excludes GNOME; (b) drop the behaviour and let the compositor place
   Lite like any other window; (c) X11-only. My recommendation: **(b) by default, (a) behind a
   feature flag for wlroots/KDE.** This is the single largest design decision in this subsystem.
2. **Always-on-top may simply be unavailable.** On Wayland without layer-shell there is no way to
   honour the `Always On Top` item that appears in *both* the hamburger menu
   (`FxMainWindow.cpp:550`) and the tray menu (`FxSystemTrayView.cpp:320`). Grey it out with an
   explanatory tooltip rather than shipping a toggle that does nothing.
3. **Global hotkeys are a UX redesign, not a port.** Five commands exist with stored key codes
   (`Settings.cpp:33-37`). The `GlobalShortcuts` portal moves binding into the compositor's
   settings UI, so the FxSound settings page must change shape. Confirm which portal backends the
   target distros ship; on wlroots this is still patchy as of 2026.
4. **Font metric mismatch.** JUCE `withHeight(17.0)` ≠ `FontId::new(17.0, …)`. Every label,
   combo and menu item in the app is sized off `getNormalFont()`/`getSmallFont()`. If the em
   conversion is wrong by 15 %, the 470 × 40 combos will show clipped or floating text. **Measure
   against a reference screenshot before building anything else.** Also confirm Gilroy's licence
   permits redistribution in the Linux build; if not, pick a metric-compatible substitute and
   re-derive the sizes.
5. **The `(0,0)` sentinel for "no saved position"** (`FxMainWindow.cpp:292`) is ambiguous with a
   legitimately-placed window. Use `Option<(i32,i32)>` in the new config.
6. **The 21 px bottom pad is load-bearing but undocumented.** It is `WINDOW_CORNER_RADIUS` reused
   as vertical padding (`FxWindow.cpp:81`), giving an *effective* 20 px because content starts at
   `title_bar.bottom + 1`. If someone "cleans this up" the Pro window becomes 1040 × 567 and every
   downstream screenshot/spec breaks. Name the constant `BOTTOM_PAD` in the Rust port and keep it
   at 21 with the `+1` content offset.
7. **`FxProView::HEIGHT = 491` is never the real height.** `update()` always sets 511
   (`FxProView.cpp:69`) and is always called before the view is shown. Whether the `491` /
   `visualizer hidden` path should exist at all in the port is an open product question — it is
   currently unreachable. Confirm with a maintainer before deleting it.
8. **Combo corner radius is derived from height** (`height / 5`, `FxTheme.cpp:138`), so Pro gets 8
   and Lite gets 10. If the port normalises the combo height across views, the radius silently
   changes. Keep the formula, not the literal.
9. **`paint()` mutating enablement** (`FxProView.cpp:117-122`, `FxLiteView.cpp:57-58`) means the
   enabled/disabled state is only correct after a repaint. In egui this is naturally fixed, but
   note the asymmetry it hides: **the output-device combo is never disabled on power-off in either
   view**, while the preset combo always is. Verify that is intentional before "fixing" it.
10. **Shadow is off for the main window but on for every dialog.** The port must therefore keep
    `shadow_width` as a per-window parameter in the shared chrome, not a constant — even though
    this document only covers the main window. `DropShadow` defaults (JUCE: black, α 0.5,
    offset (0,0), radius = `shadow_width_` = 5) are not stated in the source; verify against a
    screenshot of the Settings dialog.
11. **Logo clipping.** `icon_->setBounds(0, 0, 106, 15)` (`FxWindow.cpp:272`) combined with a
    transform that translates the content to `y = 21` (`FxWindow.cpp:365`) relies on JUCE's
    component-transform semantics. Confirm on a real screenshot whether the logo baseline is at
    y = 21 or y = 0 within the title bar before hard-coding it.
12. **Accessibility.** The Windows build wires `setHelpText` on every title-bar button and calls
    `UiaDisconnectAllProviders` at shutdown (`Main.cpp:109-113`). egui's AccessKit support is
    usable but the frameless custom chrome will need explicit `widget_info` on every painted
    button, and AT-SPI coverage on Linux is weaker. Budget time, or accept a downgrade.
