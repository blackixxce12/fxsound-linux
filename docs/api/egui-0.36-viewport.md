# egui 0.36 — Viewport / Window Control (VERIFIED)

**Every signature below was read verbatim out of vendored crate source.** Nothing here comes from
memory or from docs.rs. Citations are `<file>:<line>`, relative to
`/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.

Crates read for this document:

| Crate | Version | Path prefix used in citations |
|---|---|---|
| egui | 0.36.0 | `egui-0.36.0/` |
| egui-winit | 0.36.0 | `egui-winit-0.36.0/` |
| eframe | 0.36.0 | `eframe-0.36.0/` |
| winit | 0.30.13 | `winit-0.30.13/` (the backend that actually executes every command) |

Primary file: `egui-0.36.0/src/viewport.rs` (1311 lines). Everything viewport-related lives there,
except `ViewportInfo` (`egui-0.36.0/src/data/input/viewport_info.rs`) and the `Context` methods
(`egui-0.36.0/src/context.rs`).

---

## 0. Read this first — things that are NOT what you remember

Verified facts that break code written against older egui:

1. **`eframe::App`'s required method is `ui`, not `update`.**
   `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);` — `eframe-0.36.0/src/epi.rs:182`.
   There is also an optional `fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame)`
   (`eframe-0.36.0/src/epi.rs:167`), called even while the window is hidden.
2. **`eframe::run_simple_native` does not exist.** `grep -rn "run_simple_native" eframe-0.36.0/`
   returns nothing. Only `run_native` (`eframe-0.36.0/src/lib.rs:288`) and `run_native_ext`
   (`eframe-0.36.0/src/lib.rs:306`).
3. **`TopBottomPanel` / `SidePanel` are gone.** Use `Panel::top(id)` / `Panel::bottom(id)` /
   `Panel::left(id)` / `Panel::right(id)` (`egui-0.36.0/src/containers/panel.rs:265,274,249,256`),
   and `.show(ui, …)` takes a `&mut Ui`, not a `&Context`
   (`egui-0.36.0/src/containers/panel.rs:422`). `CentralPanel` survives and also takes `&mut Ui`
   (`egui-0.36.0/src/containers/panel.rs:1212`).
4. **`Context::screen_rect()` is gone.** It is now `Context::viewport_rect() -> Rect`
   (`egui-0.36.0/src/context.rs:2918`), with `InputState::viewport_rect()`
   (`egui-0.36.0/src/input_state/mod.rs:526`) and `InputState::content_rect()` (safe-area aware).
5. **`Rounding` is now `CornerRadius`** (`epaint-0.36.2/src/corner_radius.rs:13`), with
   `CornerRadiusF32` (`epaint-0.36.2/src/corner_radius_f32.rs:8`).
6. **The deferred viewport callback takes a `&mut Ui`, not a `&Context`**:
   `pub type DeferredViewportUiCallback = dyn Fn(&mut Ui) + Sync + Send;`
   — `egui-0.36.0/src/viewport.rs:265`.
7. **`ViewportClass` does not implement `Debug`.** Its derive is
   `#[derive(Clone, Copy, Default, Hash, PartialEq, Eq)]` (`egui-0.36.0/src/viewport.rs:80-81`) and
   there is no manual impl in the file. `println!("{:?}", class)` will not compile — match on it.
8. **`ViewportBuilder::with_movable_by_background` writes the field
   `movable_by_window_background`** (`egui-0.36.0/src/viewport.rs:478-481`). Method name and field
   name differ; if you construct the struct literally, use the field name.
9. `ViewportBuilder` is `#[derive(Clone, Debug, Default, Eq, PartialEq)]`
   (`egui-0.36.0/src/viewport.rs:283`) — **no serde**, even with the `serde` feature.
   `ViewportCommand` *does* get serde under the feature (`egui-0.36.0/src/viewport.rs:1078`).
10. `ViewportBuilder::patch` is `#[must_use]` and returns
    `(Vec<ViewportCommand>, bool)` — `egui-0.36.0/src/viewport.rs:711-712`.

---

## 1. Feature flags that gate anything here

Nothing in `egui`'s own viewport module is feature-gated except serde derives.
The gating that matters is downstream, in `egui-winit` / `eframe`:

| Item | Gate | Cite |
|---|---|---|
| serde on `ViewportId`, `ViewportClass`, `IconData`, `ViewportIdPair`, `ViewportCommand`, `WindowLevel`, `X11WindowType`, `IMEPurpose`, `SystemTheme`, `CursorGrab`, `ResizeDirection`, `ViewportInfo`, `ViewportEvent` | `egui/serde` | `egui-0.36.0/src/viewport.rs:81,117,182,238,962,971,1026,1035,1044,1053,1078`; `egui-0.36.0/Cargo.toml [features] serde` |
| `ViewportBuilder::app_id` actually applied | `egui-winit/wayland` **and** `target_os = "linux"` | `egui-winit-0.36.0/src/lib.rs:2132-2136` |
| `ViewportBuilder::window_type` / `override_redirect` applied | `egui-winit/x11` **and** `target_os = "linux"` | `egui-winit-0.36.0/src/lib.rs:2138-2166` |
| `drag_and_drop`, `taskbar`, undecorated shadow | `target_os = "windows"` | `egui-winit-0.36.0/src/lib.rs:2168-2178` |
| `fullsize_content_view`, `movable_by_window_background`, `title_shown`, `titlebar_buttons_shown`, `titlebar_shown`, `has_shadow` | `target_os = "macos"` | `egui-winit-0.36.0/src/lib.rs:2180-2190` |
| `eframe` wayland / x11 support | `eframe/wayland`, `eframe/x11` (both in `default`) | `eframe-0.36.0/Cargo.toml [features]` |
| `egui-winit` wayland / x11 | `egui-winit/wayland`, `egui-winit/x11` (both in `default`) | `egui-winit-0.36.0/Cargo.toml:67-86` |
| `App::on_exit(&mut self, gl: Option<&glow::Context>)` vs `on_exit(&mut self)` | `eframe/glow` | `eframe-0.36.0/src/epi.rs:222,228` |

`egui-winit` default = `["clipboard", "links", "wayland", "winit/default", "x11"]`
(`egui-winit-0.36.0/Cargo.toml:67-73`). **If you `default-features = false` on eframe to shrink the
build, you silently lose `app_id` on Wayland** — it is compiled out, not runtime-detected.

---

## 2. `ViewportId`, `ViewportIdPair`, aliases

```rust
// egui-0.36.0/src/viewport.rs:118
pub struct ViewportId(pub Id);

// egui-0.36.0/src/viewport.rs:149
    pub const ROOT: Self = Self(Id::NULL);

// egui-0.36.0/src/viewport.rs:152
    pub fn from_hash_of(source: impl AsId) -> Self {

// egui-0.36.0/src/viewport.rs:159
    fn from(id: ViewportId) -> Self {   // impl From<ViewportId> for Id
```

`Default for ViewportId` returns `Self::ROOT` (`egui-0.36.0/src/viewport.rs:134-138`).
`Ord`/`PartialOrd` are hand-written so it can key a `BTreeMap` with stable iteration order
(`egui-0.36.0/src/viewport.rs:122-132`).
`Debug` is hand-written and prints `Id::short_debug_format` (`egui-0.36.0/src/viewport.rs:141-145`).

```rust
// egui-0.36.0/src/viewport.rs:167
pub type ViewportIdSet = nohash_hasher::IntSet<ViewportId>;
// egui-0.36.0/src/viewport.rs:170
pub type ViewportIdMap<T> = nohash_hasher::IntMap<ViewportId, T>;
// egui-0.36.0/src/viewport.rs:173
pub type OrderedViewportIdMap<T> = std::collections::BTreeMap<ViewportId, T>;
```

```rust
// egui-0.36.0/src/viewport.rs:239
pub struct ViewportIdPair {
    pub this: ViewportId,
    pub parent: ViewportId,
}
// egui-0.36.0/src/viewport.rs:253
    pub const ROOT: Self = Self {
        this: ViewportId::ROOT,
        parent: ViewportId::ROOT,
    };
// egui-0.36.0/src/viewport.rs:259
    pub fn from_self_and_parent(this: ViewportId, parent: ViewportId) -> Self {
```

Callback type aliases:

```rust
// egui-0.36.0/src/viewport.rs:265
pub type DeferredViewportUiCallback = dyn Fn(&mut Ui) + Sync + Send;
// egui-0.36.0/src/viewport.rs:268
pub type ImmediateViewportRendererCallback = dyn for<'a> Fn(&Context, ImmediateViewport<'a>);
```

Copy-pasteable id creation:

```rust
use egui::ViewportId;

let settings_vp = ViewportId::from_hash_of("fxsound_settings_window");
let root = ViewportId::ROOT;
```

---

## 3. `ViewportClass`

```rust
// egui-0.36.0/src/viewport.rs:82
pub enum ViewportClass {
    /// The root viewport; i.e. the original window.
    #[default]
    Root,                 // :85
    Deferred,             // :92
    Immediate,            // :101
    EmbeddedWindow,       // :108
}
```

`EmbeddedWindow` is the fallback when the integration has no real multi-window support, or when
`Context::embed_viewports` is `true` (`egui-0.36.0/src/viewport.rs:103-107`). In that case your
callback runs inside a normal `egui::Window` in the parent viewport.

```rust
// The callback you pass to show_viewport_* receives the class; branch on it:
match class {
    egui::ViewportClass::EmbeddedWindow => { /* no real OS window: don't send window commands */ }
    _ => { /* real native window */ }
}
```

`ViewportOutput::class` "will never be `ViewportClass::EmbeddedWindow`, since those don't result in
real viewports" (`egui-0.36.0/src/viewport.rs:1251-1255`).

---

## 4. `IconData`

```rust
// egui-0.36.0/src/viewport.rs:183
pub struct IconData {
    /// RGBA pixels, with separate/unmultiplied alpha.
    pub rgba: Vec<u8>,
    /// Image width. This should be a multiple of 4.
    pub width: u32,
    /// Image height. This should be a multiple of 4.
    pub height: u32,
}

// egui-0.36.0/src/viewport.rs:196
    pub fn is_empty(&self) -> bool {
```

Conversions: `impl From<IconData> for epaint::ColorImage` (`egui-0.36.0/src/viewport.rs:210`) and
`impl From<&IconData> for epaint::ColorImage` (`egui-0.36.0/src/viewport.rs:222`). Both call
`ColorImage::from_rgba_premultiplied` despite the field doc saying unmultiplied — that is what the
source does (`egui-0.36.0/src/viewport.rs:218, 230`).

The winit side rejects an empty icon and logs on malformed data:

```rust
// egui-winit-0.36.0/src/lib.rs:2193
fn to_winit_icon(icon: &egui::IconData) -> Option<winit::window::Icon> {
    if icon.is_empty() {
        None
    } else {
        match winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height) {
```

Copy-pasteable icon load (no image crate, raw RGBA):

```rust
use std::sync::Arc;
use egui::IconData;

fn icon_from_rgba(rgba: Vec<u8>, width: u32, height: u32) -> Arc<IconData> {
    debug_assert_eq!(rgba.len(), (width * height * 4) as usize);
    Arc::new(IconData { rgba, width, height })
}

// eframe:
let mut native_options = eframe::NativeOptions::default();
native_options.viewport = native_options
    .viewport
    .with_icon(icon_from_rgba(my_rgba, 256, 256));
```

> To get the **OS default** icon instead of egui's built-in white "e", set
> `IconData::default()` — `eframe-0.36.0/src/epi.rs:300-302`.

---

## 5. `ViewportBuilder` — the struct

```rust
// egui-0.36.0/src/viewport.rs:283-284
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ViewportBuilder {
```

All fields are `pub` and all are `Option<_>`; **`None` means "keep current / use default"** — the
builder is accumulative because egui is immediate-mode (`egui-0.36.0/src/viewport.rs:277-282`).

| Field | Type | Line |
|---|---|---|
| `title` | `Option<String>` | 287 |
| `app_id` | `Option<String>` | 290 |
| `position` | `Option<Pos2>` | 293 |
| `inner_size` | `Option<Vec2>` | 294 |
| `min_inner_size` | `Option<Vec2>` | 295 |
| `max_inner_size` | `Option<Vec2>` | 296 |
| `clamp_size_to_monitor_size` | `Option<bool>` | 301 |
| `fullscreen` | `Option<bool>` | 303 |
| `maximized` | `Option<bool>` | 304 |
| `resizable` | `Option<bool>` | 305 |
| `transparent` | `Option<bool>` | 306 |
| `decorations` | `Option<bool>` | 307 |
| `icon` | `Option<Arc<IconData>>` | 308 |
| `active` | `Option<bool>` | 309 |
| `visible` | `Option<bool>` | 310 |
| `fullsize_content_view` (macOS) | `Option<bool>` | 313 |
| `movable_by_window_background` (macOS) | `Option<bool>` | 314 |
| `title_shown` (macOS) | `Option<bool>` | 315 |
| `titlebar_buttons_shown` (macOS) | `Option<bool>` | 316 |
| `titlebar_shown` (macOS) | `Option<bool>` | 317 |
| `has_shadow` (macOS) | `Option<bool>` | 318 |
| `drag_and_drop` (Windows) | `Option<bool>` | 321 |
| `taskbar` (Windows) | `Option<bool>` | 322 |
| `close_button` | `Option<bool>` | 324 |
| `minimize_button` | `Option<bool>` | 325 |
| `maximize_button` | `Option<bool>` | 326 |
| `window_level` | `Option<WindowLevel>` | 328 |
| `mouse_passthrough` | `Option<bool>` | 330 |
| `window_type` (X11) | `Option<X11WindowType>` | 333 |
| `override_redirect` (X11) | `Option<bool>` | 334 |
| `monitor` | `Option<usize>` | 347 |

---

## 6. `ViewportBuilder` — EVERY method, verbatim

All are `#[inline]` and consume/return `Self` (except `patch`). Defaults quoted below come from the
doc comments in the same file and from `create_winit_window_attributes`
(`egui-winit-0.36.0/src/lib.rs:2045-2076`).

### Identity & chrome

```rust
// egui-0.36.0/src/viewport.rs:355
    pub fn with_title(mut self, title: impl Into<String>) -> Self {

// egui-0.36.0/src/viewport.rs:366   (default: true)
    pub fn with_decorations(mut self, decorations: bool) -> Self {

// egui-0.36.0/src/viewport.rs:434
    pub fn with_icon(mut self, icon: impl Into<Arc<IconData>>) -> Self {

// egui-0.36.0/src/viewport.rs:645   (Wayland app id; also eframe persistence key)
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
```

`with_title` falls back to `"egui window"` in the winit layer if unset
(`egui-winit-0.36.0/src/lib.rs:2046`).

### Size & position

```rust
// egui-0.36.0/src/viewport.rs:531   (should be > 0)
    pub fn with_inner_size(mut self, size: impl Into<Vec2>) -> Self {

// egui-0.36.0/src/viewport.rs:544
    pub fn with_min_inner_size(mut self, size: impl Into<Vec2>) -> Self {

// egui-0.36.0/src/viewport.rs:557
    pub fn with_max_inner_size(mut self, size: impl Into<Vec2>) -> Self {

// egui-0.36.0/src/viewport.rs:617   ("outer" position = top-left of frame/chrome)
    pub fn with_position(mut self, pos: impl Into<Pos2>) -> Self {

// egui-0.36.0/src/viewport.rs:566   (default: true on linux, false elsewhere)
    pub fn with_clamp_size_to_monitor_size(mut self, value: bool) -> Self {

// egui-0.36.0/src/viewport.rs:400   (default: true)
    pub fn with_resizable(mut self, resizable: bool) -> Self {

// egui-0.36.0/src/viewport.rs:389   (default: false)
    pub fn with_maximized(mut self, maximized: bool) -> Self {

// egui-0.36.0/src/viewport.rs:378   (default: None; borderless fullscreen)
    pub fn with_fullscreen(mut self, fullscreen: bool) -> Self {

// egui-0.36.0/src/viewport.rs:704   (index into winit available_monitors())
    pub fn with_monitor(mut self, index: usize) -> Self {
```

`with_position` doc verbatim: *"**Android / Wayland:** Unsupported."*
(`egui-0.36.0/src/viewport.rs:613`).

`with_monitor` doc verbatim: *"On Wayland this is the only reliable way to target a specific output,
since absolute window positions are not exposed."* (`egui-0.36.0/src/viewport.rs:698-700`). It puts
the window in **borderless fullscreen** on that monitor — it is not a plain "place here" hint
(`egui-winit-0.36.0/src/lib.rs:1981-1990`). It "takes precedence over `with_position` /
`with_fullscreen` for monitor selection" (`egui-0.36.0/src/viewport.rs:344-346`).

`clamp_size_to_monitor_size` is not handled in `create_winit_window_attributes` — it is applied in
eframe (`eframe-0.36.0/src/native/epi_integration.rs:28,36,55`), and note the eframe default there is
`unwrap_or(true)` on **all** platforms, not just Linux
(`eframe-0.36.0/src/native/epi_integration.rs:28`).

### Visibility, focus, layering, input

```rust
// egui-0.36.0/src/viewport.rs:449   (Android/iOS/X11/Wayland/Orbital: Unsupported)
    pub fn with_active(mut self, active: bool) -> Self {

// egui-0.36.0/src/viewport.rs:460   (default: show)
    pub fn with_visible(mut self, visible: bool) -> Self {

// egui-0.36.0/src/viewport.rs:424   (default: false)
    pub fn with_transparent(mut self, transparent: bool) -> Self {

// egui-0.36.0/src/viewport.rs:654
    pub fn with_window_level(mut self, level: WindowLevel) -> Self {

// egui-0.36.0/src/viewport.rs:663   (convenience for WindowLevel::AlwaysOnTop)
    pub fn with_always_on_top(self) -> Self {

// egui-0.36.0/src/viewport.rs:672   (clicks pass through; pair with transparent+always_on_top)
    pub fn with_mouse_passthrough(mut self, value: bool) -> Self {
```

`with_always_on_top` body verbatim (`egui-0.36.0/src/viewport.rs:663-665`):

```rust
    pub fn with_always_on_top(self) -> Self {
        self.with_window_level(WindowLevel::AlwaysOnTop)
    }
```

### Title-bar buttons

```rust
// egui-0.36.0/src/viewport.rs:573   ("Does not work on X11.")
    pub fn with_close_button(mut self, value: bool) -> Self {
// egui-0.36.0/src/viewport.rs:580   ("Does not work on X11.")
    pub fn with_minimize_button(mut self, value: bool) -> Self {
// egui-0.36.0/src/viewport.rs:587   ("Does not work on X11.")
    pub fn with_maximize_button(mut self, value: bool) -> Self {
```

All three map onto one winit `WindowButtons` bitflag set, each defaulting to `true`
(`egui-winit-0.36.0/src/lib.rs:2064-2075`). winit says
*"**Wayland / X11 / Orbital:** Not implemented."* (`winit-0.30.13/src/window.rs:1026-1031`), so on
Linux these are inert either way.

### macOS-only

```rust
// egui-0.36.0/src/viewport.rs:470
    pub fn with_fullsize_content_view(mut self, value: bool) -> Self {
// egui-0.36.0/src/viewport.rs:478   (writes field `movable_by_window_background`)
    pub fn with_movable_by_background(mut self, value: bool) -> Self {
// egui-0.36.0/src/viewport.rs:485
    pub fn with_title_shown(mut self, title_shown: bool) -> Self {
// egui-0.36.0/src/viewport.rs:492
    pub fn with_titlebar_buttons_shown(mut self, titlebar_buttons_shown: bool) -> Self {
// egui-0.36.0/src/viewport.rs:499
    pub fn with_titlebar_shown(mut self, shown: bool) -> Self {
// egui-0.36.0/src/viewport.rs:512   (default true; set false to kill ghosting with transparency)
    pub fn with_has_shadow(mut self, has_shadow: bool) -> Self {
```

### Windows-only

```rust
// egui-0.36.0/src/viewport.rs:600
    pub fn with_drag_and_drop(mut self, value: bool) -> Self {
// egui-0.36.0/src/viewport.rs:519
    pub fn with_taskbar(mut self, show: bool) -> Self {
```

Note the inversion in the backend: `taskbar(show)` becomes `with_skip_taskbar(!show)`
(`egui-winit-0.36.0/src/lib.rs:2175`).

### X11-only

```rust
// egui-0.36.0/src/viewport.rs:681   (_NET_WM_WINDOW_TYPE)
    pub fn with_window_type(mut self, value: X11WindowType) -> Self {
// egui-0.36.0/src/viewport.rs:690   (set window_type too when using this)
    pub fn with_override_redirect(mut self, value: bool) -> Self {
```

### `patch` — the diffing engine

```rust
// egui-0.36.0/src/viewport.rs:711-712
    #[must_use]
    pub fn patch(&mut self, new_vp_builder: Self) -> (Vec<ViewportCommand>, bool) {
```

Returns `(commands_to_send, needs_window_recreation)`. **This is the authoritative list of which
properties can change live and which force the integration to destroy and rebuild the OS window**
(comment at `egui-0.36.0/src/viewport.rs:854-857`: *"Things we don't have commands for require a full
window recreation. The reason we don't have commands for them is that `winit` doesn't support
changing them without recreating the window."*).

| Changed field | Result | Cite |
|---|---|---|
| `title` | `ViewportCommand::Title` | 751-756 |
| `position` | `ViewportCommand::OuterPosition` | 758-763 |
| `inner_size` | `ViewportCommand::InnerSize` | 765-770 |
| `min_inner_size` | `ViewportCommand::MinInnerSize` | 772-777 |
| `max_inner_size` | `ViewportCommand::MaxInnerSize` | 779-784 |
| `fullscreen` | `ViewportCommand::Fullscreen` | 786-791 |
| `maximized` | `ViewportCommand::Maximized` | 793-798 |
| `resizable` | `ViewportCommand::Resizable` | 800-805 |
| `transparent` | `ViewportCommand::Transparent` | 807-812 |
| `decorations` | `ViewportCommand::Decorations` | 814-819 |
| `icon` | `ViewportCommand::Icon` (compared by `Arc::ptr_eq`!) | 821-831 (ptr_eq at 823) |
| `visible` | `ViewportCommand::Visible` | 833-838 |
| `mouse_passthrough` | `ViewportCommand::MousePassthrough` | 840-845 |
| `window_level` | `ViewportCommand::WindowLevel` | 847-852 |
| `monitor` | `ViewportCommand::SetMonitor` | 949-954 |
| `clamp_size_to_monitor_size` | **recreate window** | 861-866 |
| `active` | **recreate window** | 868-871 |
| `app_id` | **recreate window** | 873-876 |
| `close_button` | **recreate window** | 878-881 |
| `minimize_button` | **recreate window** | 883-886 |
| `maximize_button` | **recreate window** | 888-891 |
| `title_shown` | **recreate window** | 893-896 |
| `titlebar_buttons_shown` | **recreate window** | 898-903 |
| `titlebar_shown` | **recreate window** | 905-908 |
| `has_shadow` | **recreate window** | 910-913 |
| `taskbar` | **recreate window** | 915-918 |
| `fullsize_content_view` | **recreate window** | 920-925 |
| `movable_by_window_background` | **recreate window** | 927-932 |
| `drag_and_drop` | **recreate window** | 934-937 |
| `window_type` | **recreate window** | 939-942 |
| `override_redirect` | **recreate window** | 944-947 |

> **Gotcha — icon identity:** the icon diff uses `Arc::ptr_eq`, not value equality
> (`egui-0.36.0/src/viewport.rs:821-825`, `Arc::ptr_eq` on line 823). Rebuilding an identical `Arc<IconData>` every frame will
> resend `ViewportCommand::Icon` every frame. Build it **once** and clone the `Arc`.

---

## 7. Supporting enums

```rust
// egui-0.36.0/src/viewport.rs:963
pub enum WindowLevel {
    #[default]
    Normal,             // :965
    AlwaysOnBottom,     // :966
    AlwaysOnTop,        // :967
}
```

```rust
// egui-0.36.0/src/viewport.rs:972
pub enum X11WindowType {
    #[default]
    Normal,        // :975
    Desktop,       // :980
    Dock,          // :983
    Toolbar,       // :986
    Menu,          // :989
    Utility,       // :992
    Splash,        // :995
    Dialog,        // :998
    DropdownMenu,  // :1002
    PopupMenu,     // :1006
    Tooltip,       // :1010
    Notification,  // :1014
    Combo,         // :1018
    Dnd,           // :1022
}
```

```rust
// egui-0.36.0/src/viewport.rs:1027
pub enum IMEPurpose { #[default] Normal, Password, Terminal }

// egui-0.36.0/src/viewport.rs:1036
pub enum SystemTheme { #[default] SystemDefault, Light, Dark }

// egui-0.36.0/src/viewport.rs:1045
pub enum CursorGrab { #[default] None, Confined, Locked }

// egui-0.36.0/src/viewport.rs:1054   (NOTE: no Default derive)
pub enum ResizeDirection {
    North, South, East, West, NorthEast, SouthEast, NorthWest, SouthWest,
}
```

```rust
// egui-0.36.0/src/data/output.rs:296
pub enum UserAttentionType { Critical, Informational, Reset }

// egui-0.36.0/src/data/user_data.rs:6
pub struct UserData {
    pub data: Option<Arc<dyn Any + Send + Sync>>,
}
// egui-0.36.0/src/data/user_data.rs:14
    pub fn new(user_info: impl Any + Send + Sync) -> Self {
```

---

## 8. `ViewportCommand` — EVERY variant, verbatim

```rust
// egui-0.36.0/src/viewport.rs:1077-1079
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum ViewportCommand {
```

Sent with `Context::send_viewport_cmd` / `send_viewport_cmd_to`. **All coordinates are in logical
points** (`egui-0.36.0/src/viewport.rs:1071`). The "backend does" column is the actual winit call in
`egui-winit-0.36.0/src/lib.rs` `process_viewport_command`.

| Variant (verbatim) | egui line | Backend does | egui-winit line |
|---|---|---|---|
| `Close` | 1084 | pushes `ViewportEvent::Close` into `ViewportInfo::events` — **does not close anything itself** | 1744-1746 |
| `CancelClose` | 1087 | nothing here ("Need to be handled elsewhere") | 1747-1749 |
| `Title(String)` | 1090 | `window.set_title(&title)` | 1798-1800 |
| `Transparent(bool)` | 1093 | `window.set_transparent(v)` | 1801 |
| `Visible(bool)` | 1096 | `window.set_visible(v)` | 1802 |
| `StartDrag` | 1102 | `if window.has_focus() { window.drag_window() }` | 1750-1757 |
| `OuterPosition(Pos2)` | 1105 | `window.set_outer_position(PhysicalPosition::new(ppp*x, ppp*y))` | 1803-1808 |
| `InnerSize(Vec2)` | 1108 | `window.request_inner_size(PhysicalSize)`, clamped `max(1.0)` | 1758-1783 |
| `MinInnerSize(Vec2)` | 1111 | `set_min_inner_size(Some(..))`, `None` if non-finite or `Vec2::ZERO` | 1809-1813 |
| `MaxInnerSize(Vec2)` | 1114 | `set_max_inner_size(Some(..))`, `None` if non-finite or `Vec2::INFINITY` | 1814-1818 |
| `ResizeIncrements(Option<Vec2>)` | 1117 | `window.set_resize_increments(..)` | 1819-1823 |
| `BeginResize(ResizeDirection)` | 1123 | `window.drag_resize_window(dir)` | 1784-1797 |
| `Resizable(bool)` | 1126 | `window.set_resizable(v)` | 1824 |
| `EnableButtons { close: bool, minimized: bool, maximize: bool }` | 1129-1133 | `window.set_enabled_buttons(bitflags)` | 1825-1843 |
| `Minimized(bool)` | 1134 | `set_minimized(v)` **and writes `info.minimized`** | 1844-1847 |
| `Maximized(bool)` | 1137 | `set_maximized(v)` **and writes `info.maximized`** | 1848-1851 |
| `Fullscreen(bool)` | 1140 | `set_fullscreen(v.then_some(Fullscreen::Borderless(None)))` | 1852-1854 |
| `SetMonitor(usize)` | 1146 | `set_fullscreen(Some(Borderless(Some(monitor))))`; out-of-range → `log::warn!`, ignored | 1855-1864 |
| `Decorations(bool)` | 1150 | `set_decorations(v)`; on Windows also `set_undecorated_shadow(!v)` | 1865-1872 |
| `WindowLevel(WindowLevel)` | 1153 | `window.set_window_level(..)` | 1873-1877 |
| `Icon(Option<Arc<IconData>>)` | 1156 | `window.set_window_icon(to_winit_icon(..))` | 1878-1881 |
| `IMERect(crate::Rect)` | 1159 | `window.set_ime_cursor_area(pos, size)` | 1882-1890 |
| `IMEAllowed(bool)` | 1160 | `window.set_ime_allowed(v)` | 1891 |
| `IMEPurpose(IMEPurpose)` | 1161 | `window.set_ime_purpose(..)` | 1892 |
| `Focus` | 1169 | `if !window.has_focus() { window.focus_window() }` | 1893-1897 |
| `RequestUserAttention(crate::UserAttentionType)` | 1180 | `window.request_user_attention(..)`; `Reset` → `None` | 1898-1908 |
| `SetTheme(SystemTheme)` | 1182 | `window.set_theme(..)`; `SystemDefault` → `None` | 1909-1913 |
| `ContentProtected(bool)` | 1184 | `window.set_content_protected(v)` | 1914 |
| `CursorPosition(Pos2)` | 1187 | `window.set_cursor_position(..)` ("Will probably not work as expected!") | 1915-1922 |
| `CursorGrab(CursorGrab)` | 1189 | `window.set_cursor_grab(..)` | 1923-1931 |
| `CursorVisible(bool)` | 1191 | `window.set_cursor_visible(v)` | 1932 |
| `MousePassthrough(bool)` | 1194 | `window.set_cursor_hittest(!passthrough)` | 1933-1937 |
| `Screenshot(crate::UserData)` | 1199 | queues `ActionRequested::Screenshot(user_data)` | 1938-1940 |
| `RequestCut` | 1204 | queues `ActionRequested::Cut` | 1941-1943 |
| `RequestCopy` | 1209 | queues `ActionRequested::Copy` | 1944-1946 |
| `RequestPaste` | 1214 | queues `ActionRequested::Paste` | 1947-1949 |

That is **36 variants**. Verbatim doc text worth knowing:

- `Close` (`egui-0.36.0/src/viewport.rs:1080-1084`): *"For the root viewport, this usually results in
  the application shutting down. For other viewports, the `ViewportInfo::close_requested` flag will
  be set."*
- `StartDrag` (`egui-0.36.0/src/viewport.rs:1098-1102`): *"Moves the window with the left mouse
  button until the button is released. There's no guarantee that this will work unless the left
  mouse button was pressed immediately before this function is called."*
- `BeginResize` (`egui-0.36.0/src/viewport.rs:1119-1123`): same caveat.
- `Focus` (`egui-0.36.0/src/viewport.rs:1163-1169`): *"Has no effect on Wayland, or if the window is
  minimized or invisible."*

Backend entry points:

```rust
// egui-winit-0.36.0/src/lib.rs:1716
pub fn process_viewport_commands(
    egui_ctx: &egui::Context,
    info: &mut ViewportInfo,
    commands: impl IntoIterator<Item = ViewportCommand>,
    window: &Window,
    actions_requested: &mut Vec<ActionRequested>,
) {

// egui-winit-0.36.0/src/lib.rs:1708
pub enum ActionRequested {
    Screenshot(egui::UserData),
    Cut,
    Copy,
    Paste,
}
```

### `ViewportCommand` helpers

```rust
// egui-0.36.0/src/viewport.rs:1219
    pub fn center_on_screen(ctx: &crate::Context) -> Option<Self> {

// egui-0.36.0/src/viewport.rs:1235
    pub fn requires_parent_repaint(&self) -> bool {
```

`center_on_screen` body (`egui-0.36.0/src/viewport.rs:1219-1233`) reads
`i.viewport().outer_rect` and `i.viewport().monitor_size`; **both are `None` on Wayland**, so it
returns `None` there. `requires_parent_repaint` is literally `self == &Self::Close`
(`egui-0.36.0/src/viewport.rs:1236`).

---

## 9. `ViewportInfo` — what the OS tells you

```rust
// egui-0.36.0/src/data/input/viewport_info.rs:26-28
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewportInfo {
    pub parent: Option<crate::ViewportId>,        // :30
    pub title: Option<String>,                    // :33
    pub events: Vec<ViewportEvent>,               // :35
    pub native_pixels_per_point: Option<f32>,     // :43
    pub monitor_size: Option<Vec2>,               // :46
    pub inner_rect: Option<Rect>,                 // :56
    pub outer_rect: Option<Rect>,                 // :66
    pub minimized: Option<bool>,                  // :69
    pub maximized: Option<bool>,                  // :72
    pub fullscreen: Option<bool>,                 // :75
    pub focused: Option<bool>,                    // :80
    pub occluded: Option<bool>,                   // :86
}
```

`None` means "unknown" (`egui-0.36.0/src/data/input/viewport_info.rs:22`). Both `inner_rect` and
`outer_rect` carry the verbatim note: *"On Android / Wayland, this will always be `None` since
getting the position of the window is not possible."*
(`egui-0.36.0/src/data/input/viewport_info.rs:52-55, 62-65`).

```rust
// egui-0.36.0/src/data/input/viewport_info.rs:95
    pub fn visible(&self) -> Option<bool> {
// egui-0.36.0/src/data/input/viewport_info.rs:111
    pub fn close_requested(&self) -> bool {
// egui-0.36.0/src/data/input/viewport_info.rs:116
    pub fn take(&mut self) -> Self {
// egui-0.36.0/src/data/input/viewport_info.rs:133
    pub fn ui(&self, ui: &mut crate::Ui) {
```

`close_requested` is `self.events.contains(&ViewportEvent::Close)`
(`egui-0.36.0/src/data/input/viewport_info.rs:112`).
`ViewportInfo::ui` is a ready-made debug grid — drop it in a panel to see all of this live.

```rust
// egui-0.36.0/src/data/input/viewport_info.rs:4-6
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewportEvent {
    Close,     // :17
}
```

Reading it:

```rust
// egui-0.36.0/src/input_state/mod.rs:500
    pub fn viewport(&self) -> &ViewportInfo {

// egui-0.36.0/src/data/input/raw_input.rs:116
    pub fn viewport(&self) -> &ViewportInfo {

// egui-0.36.0/src/data/input/raw_input.rs:20
    pub viewport_id: ViewportId,
// egui-0.36.0/src/data/input/raw_input.rs:23
    pub viewports: ViewportIdMap<ViewportInfo>,
```

```rust
// Current viewport info + all viewports:
ui.ctx().input(|i| {
    let vp: &egui::ViewportInfo = i.viewport();
    let _maximized = vp.maximized.unwrap_or(false);
    let _all = &i.raw.viewports; // ViewportIdMap<ViewportInfo>
});
```

`RawInput::viewport()` **panics** if the backend didn't register the current viewport
(`egui-0.36.0/src/data/input/raw_input.rs:118`).

How the backend fills it in:

```rust
// egui-winit-0.36.0/src/lib.rs:1339
pub fn update_viewport_info(
    viewport_info: &mut ViewportInfo,
    egui_ctx: &egui::Context,
    window: &Window,
    is_init: bool,
) {

// egui-winit-0.36.0/src/lib.rs:1310
pub fn inner_rect_in_points(window: &Window, pixels_per_point: f32) -> Option<Rect> {
// egui-winit-0.36.0/src/lib.rs:1322
pub fn outer_rect_in_points(window: &Window, pixels_per_point: f32) -> Option<Rect> {
```

Note: when the window is minimized, `inner_rect`/`outer_rect` are forced to `None`
(`egui-winit-0.36.0/src/lib.rs:1347-1362`), and on macOS `maximized`/`minimized` are only read on
init to avoid a deadlock (`egui-winit-0.36.0/src/lib.rs:1383-1390`). `occluded` is never written by
`update_viewport_info` — it is event-driven.

---

## 10. `ViewportOutput`, `ImmediateViewport`, `ViewportState`

```rust
// egui-0.36.0/src/viewport.rs:1246-1247
#[derive(Clone)]
pub struct ViewportOutput {
    pub parent: ViewportId,                                       // :1249
    pub class: ViewportClass,                                     // :1255
    pub builder: ViewportBuilder,                                 // :1262
    pub viewport_ui_cb: Option<Arc<DeferredViewportUiCallback>>,  // :1267
    pub commands: Vec<ViewportCommand>,                           // :1270
    pub repaint_delay: std::time::Duration,                       // :1278
}
// egui-0.36.0/src/viewport.rs:1283
    pub fn append(&mut self, newer: Self) {
```

`viewport_ui_cb` is `None` for immediate viewports and for ROOT
(`egui-0.36.0/src/viewport.rs:1264-1266`).

```rust
// egui-0.36.0/src/viewport.rs:1303
pub struct ImmediateViewport<'a> {
    pub ids: ViewportIdPair,                          // :1305
    pub builder: ViewportBuilder,                     // :1307
    pub viewport_ui_cb: Box<dyn FnMut(&mut Ui) + 'a>, // :1310
}
```

```rust
// egui-0.36.0/src/context.rs:192-193
#[derive(Default)]
pub struct ViewportState {
    pub class: ViewportClass,                                     // :198
    pub builder: ViewportBuilder,                                 // :201
    pub viewport_ui_cb: Option<Arc<DeferredViewportUiCallback>>,  // :206
    pub input: InputState,                                        // :208
    pub this_pass: PassState,                                     // :211
    pub prev_pass: PassState,                                     // :216
    pub used: bool,                                               // :219
    pub hits: WidgetHits,                                         // :228
    pub interact_widgets: InteractionSnapshot,                    // :233
    pub graphics: GraphicLayers,                                  // :238
    pub output: PlatformOutput,                                   // :240
    pub commands: Vec<ViewportCommand>,                           // :241
    pub num_multipass_in_row: usize,                              // :245
}
```

Doc verbatim: *"Mostly for internal use. Things here may move and change without warning."*
(`egui-0.36.0/src/context.rs:189-190`).

And where the output lands:

```rust
// egui-0.36.0/src/data/output.rs:39
    pub viewport_output: OrderedViewportIdMap<ViewportOutput>,
```

---

## 11. `Context` — the viewport API

All in `impl Context` under `/// ## Viewports` (`egui-0.36.0/src/context.rs:3939-3940`).

```rust
// egui-0.36.0/src/context.rs:3946
    pub fn viewport_id(&self) -> ViewportId {

// egui-0.36.0/src/context.rs:3955
    pub fn parent_viewport_id(&self) -> ViewportId {

// egui-0.36.0/src/context.rs:3960
    pub fn viewport<R>(&self, reader: impl FnOnce(&ViewportState) -> R) -> R {

// egui-0.36.0/src/context.rs:3965
    pub fn viewport_for<R>(
        &self,
        viewport_id: ViewportId,
        reader: impl FnOnce(&ViewportState) -> R,
    ) -> R {

// egui-0.36.0/src/context.rs:3985   (NOTE: associated fn, no &self)
    pub fn set_immediate_viewport_renderer(
        callback: impl for<'a> Fn(&Self, ImmediateViewport<'a>) + 'static,
    ) {

// egui-0.36.0/src/context.rs:3998
    pub fn embed_viewports(&self) -> bool {

// egui-0.36.0/src/context.rs:4006
    pub fn set_embed_viewports(&self, value: bool) {

// egui-0.36.0/src/context.rs:4013
    pub fn send_viewport_cmd(&self, command: ViewportCommand) {

// egui-0.36.0/src/context.rs:4020
    pub fn send_viewport_cmd_to(&self, id: ViewportId, command: ViewportCommand) {
```

`send_viewport_cmd` is exactly `self.send_viewport_cmd_to(self.viewport_id(), command)`
(`egui-0.36.0/src/context.rs:4014`), and `send_viewport_cmd_to` requests a repaint of the target and
— for `Close` only — of the parent (`egui-0.36.0/src/context.rs:4021-4027`).

> `set_immediate_viewport_renderer` takes **no `self`** and sets a **thread-local**
> (`egui-0.36.0/src/context.rs:3989-3991`, doc at `:3975-3976`: *"This will only set the callback for
> the current thread"*). Integration-only.

Related, outside the viewport impl block:

```rust
// egui-0.36.0/src/context.rs:2918
    pub fn viewport_rect(&self) -> Rect {
// egui-0.36.0/src/context.rs:1835
    pub fn request_repaint_of(&self, id: ViewportId) {
// egui-0.36.0/src/context.rs:1925
    pub fn requested_repaint_last_pass_for(&self, viewport_id: &ViewportId) -> bool {
// egui-0.36.0/src/context.rs:1937
    pub fn has_requested_repaint_for(&self, viewport_id: &ViewportId) -> bool {
// egui-0.36.0/src/context.rs:1958
    pub fn set_request_repaint_callback(
// egui-0.36.0/src/context.rs:793
    pub fn run_ui(&self, new_input: RawInput, mut run_ui: impl FnMut(&mut Ui)) -> FullOutput {
// egui-0.36.0/src/context.rs:912
    pub fn run_logic(&self, new_input: &RawInput, logic: impl FnOnce(&Self)) -> LogicOutput {
```

---

## 12. Deferred vs immediate viewports

### `show_viewport_deferred` — verbatim

```rust
// egui-0.36.0/src/context.rs:4059
    pub fn show_viewport_deferred(
        &self,
        new_viewport_id: ViewportId,
        viewport_builder: ViewportBuilder,
        viewport_ui_cb: impl Fn(&mut Ui, ViewportClass) + Send + Sync + 'static,
    ) {
```

Note the bounds: `Fn` (not `FnMut`/`FnOnce`), `+ Send + Sync + 'static`. The callback is stored as
`Arc<DeferredViewportUiCallback>` (`egui-0.36.0/src/context.rs:4080-4082`), so **all state it touches
must be `Arc<Mutex<…>>` / `Arc<RwLock<…>>` / channels** — the docs say so explicitly
(`egui-0.36.0/src/context.rs:4044-4045`). Returns `()`.

If `embed_viewports()` is `true`, it instead immediately runs the callback inside
`crate::Window::from_viewport(new_viewport_id, viewport_builder).show(self, …)` with
`ViewportClass::EmbeddedWindow` (`egui-0.36.0/src/context.rs:4067-4071`).

```rust
use std::sync::{Arc, Mutex};
use egui::{ViewportBuilder, ViewportClass, ViewportId};

#[derive(Default)]
struct EqState { gain_db: f32 }

struct App {
    show_eq: bool,
    eq: Arc<Mutex<EqState>>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.checkbox(&mut self.show_eq, "Show EQ window");
        });

        if self.show_eq {
            let eq = Arc::clone(&self.eq);
            ui.ctx().show_viewport_deferred(
                ViewportId::from_hash_of("fxsound_eq"),
                ViewportBuilder::default()
                    .with_title("FxSound EQ")
                    .with_inner_size([420.0, 300.0])
                    .with_min_inner_size([320.0, 200.0]),
                move |ui, class| {
                    if class == ViewportClass::EmbeddedWindow {
                        ui.label("(embedded fallback: no multi-window support)");
                    }
                    egui::CentralPanel::default().show(ui, |ui| {
                        let mut st = eq.lock().unwrap();
                        ui.add(egui::Slider::new(&mut st.gain_db, -24.0..=24.0).text("Gain dB"));
                    });

                    // The user hit the window's X:
                    if ui.ctx().input(|i| i.viewport().close_requested()) {
                        // Deferred viewports run on their own; signal the parent however you like.
                        // Simplest: an AtomicBool the parent reads next frame.
                    }
                },
            );
        }
    }
}
```

> **Closing a deferred viewport**: the child's `close_requested()` does **not** destroy the window.
> You stop calling `show_viewport_deferred` for that id. Doc verbatim
> (`egui-0.36.0/src/data/input/viewport_info.rs:13-14`): *"If this is not the root viewport, it is up
> to the user to hide this viewport the next frame."* Because the deferred closure cannot touch your
> app struct, route the signal through the `Arc` (e.g. an `AtomicBool`).

### `show_viewport_immediate` — verbatim

```rust
// egui-0.36.0/src/context.rs:4113
    pub fn show_viewport_immediate<T>(
        &self,
        new_viewport_id: ViewportId,
        builder: ViewportBuilder,
        mut viewport_ui_cb: impl FnMut(&mut Ui, ViewportClass) -> T,
    ) -> T {
```

`FnMut`, **no** `Send`/`Sync`/`'static`, and it **returns `T`** from your closure. Main thread only
(`egui-0.36.0/src/context.rs:4102`). It panics with *"egui backend is implemented incorrectly - the
user callback was never called"* if the integration's renderer never invokes the callback
(`egui-0.36.0/src/context.rs:4165-4167`).

```rust
use egui::{ViewportBuilder, ViewportClass, ViewportId};

// Inside `fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame)`:
if self.show_about {
    let ctx = ui.ctx().clone();
    ctx.show_viewport_immediate(
        ViewportId::from_hash_of("fxsound_about"),
        ViewportBuilder::default()
            .with_title("About FxSound")
            .with_inner_size([360.0, 200.0])
            .with_resizable(false),
        |ui, _class: ViewportClass| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("FxSound");
                ui.label("Linux port");
            });
            // Direct access to `self` works here — that's the whole point of immediate.
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                self.show_about = false;
            }
        },
    );
}
```

### Which to use

| | Deferred | Immediate |
|---|---|---|
| Constructor | `show_viewport_deferred` (`context.rs:4059`) | `show_viewport_immediate` (`context.rs:4113`) |
| Closure bound | `Fn(&mut Ui, ViewportClass) + Send + Sync + 'static` | `FnMut(&mut Ui, ViewportClass) -> T` |
| Returns | `()` | `T` |
| State sharing | `Arc<Mutex<_>>` / channels (`context.rs:4044`) | borrow `self` directly |
| Repaint coupling | independent (`viewport.rs:22`) | parent and child repaint together — "N viewports ⇒ N× CPU work" (`viewport.rs:31`) |
| Thread | integration-scheduled | main thread only (`context.rs:4102`) |

Both: *"You need to call this each pass when the child viewport should exist."*
(`egui-0.36.0/src/context.rs:4034, 4096`).

### Embedded fallback

```rust
// egui-0.36.0/src/context.rs:4171
    fn show_embedded_viewport<T>(
        &self,
        new_viewport_id: ViewportId,
        builder: ViewportBuilder,
        viewport_ui_cb: impl FnOnce(&mut Ui) -> T,
    ) -> T {
```

(private; it `.unwrap_or_else(|| panic!("Window did not show"))` and
`panic!("Window was collapsed")` — `egui-0.36.0/src/context.rs:4180-4182`.)

`Window::from_viewport` maps only a subset of the builder — `title`, `app_id`, `inner_size`,
`min_inner_size`, `max_inner_size`, `resizable`, `decorations`, `title_shown`, `minimize_button`;
the source comment says *"A lot of things not implemented yet"*
(`egui-0.36.0/src/containers/window.rs:126-138`):

```rust
// egui-0.36.0/src/containers/window.rs:126
    pub fn from_viewport(id: ViewportId, viewport: ViewportBuilder) -> Self {
```

`eframe` native sets `embed_viewports` to `false`; the egui default is `true`
(`egui-0.36.0/src/context.rs:3994-3997`).

---

## 13. Recipe: frameless window with a custom drag region

The pieces, each verified:

- `ViewportBuilder::with_decorations(false)` — `egui-0.36.0/src/viewport.rs:366`
- `ViewportBuilder::with_transparent(true)` — `egui-0.36.0/src/viewport.rs:424` (needed for rounded
  corners / shadow)
- `eframe::App::clear_color` — `eframe-0.36.0/src/epi.rs:248`, must return transparent
- `Ui::interact(rect, id, sense)` — `egui-0.36.0/src/ui.rs:906`
- `Sense::click_and_drag()` — `egui-0.36.0/src/sense.rs:81`
- `Response::drag_started()` — `egui-0.36.0/src/response.rs:392`
- `Response::double_clicked()` — `egui-0.36.0/src/response.rs:236`
- `ViewportCommand::StartDrag` — `egui-0.36.0/src/viewport.rs:1102`
- `ViewportCommand::BeginResize(ResizeDirection)` — `egui-0.36.0/src/viewport.rs:1123`
- `Panel::top(id)` — `egui-0.36.0/src/containers/panel.rs:265`
- `Panel::exact_size(f32)` — `egui-0.36.0/src/containers/panel.rs:405`
- `CentralPanel::no_frame()` — `egui-0.36.0/src/containers/panel.rs:1193`
- `epaint::CornerRadius` — `epaint-0.36.2/src/corner_radius.rs:13`

```rust
use eframe::egui;
use egui::{
    CentralPanel, Color32, CornerRadius, Id, Panel, Rect, Sense, Stroke, Vec2,
    ViewportBuilder, ViewportCommand,
};
use egui::viewport::ResizeDirection;

const TITLE_BAR_H: f32 = 32.0;
const RESIZE_GRAB: f32 = 6.0; // points

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("FxSound")
            .with_inner_size([900.0, 560.0])
            .with_min_inner_size([640.0, 400.0])
            .with_decorations(false)   // frameless
            .with_transparent(true)    // so rounded corners aren't black
            .with_resizable(true)
            .with_app_id("fxsound"),   // Wayland: must match fxsound.desktop
        ..Default::default()
    };
    eframe::run_native("FxSound", native_options, Box::new(|_cc| Ok(Box::new(App::default()))))
}

#[derive(Default)]
struct App {}

impl eframe::App for App {
    // Transparent so the frameless window's rounded corners show through.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let app_rect = ui.max_rect();

        // Paint our own window background with rounded corners.
        ui.painter().rect(
            app_rect,
            CornerRadius::same(10),
            ui.visuals().window_fill,
            Stroke::new(1.0, ui.visuals().window_stroke.color),
            egui::StrokeKind::Inside,
        );

        resize_edges(ui, app_rect);

        Panel::top(Id::new("fx_title_bar"))
            .exact_size(TITLE_BAR_H)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| title_bar(ui));

        CentralPanel::no_frame().show(ui, |ui| {
            ui.label("content");
        });
    }
}

fn title_bar(ui: &mut egui::Ui) {
    let bar_rect = ui.max_rect();

    // One interaction region covering the whole bar.
    let resp = ui.interact(bar_rect, Id::new("fx_title_bar_drag"), Sense::click_and_drag());

    if resp.drag_started() {
        // StartDrag only works if the LMB was pressed immediately before — drag_started()
        // is exactly that moment. (egui-0.36.0/src/viewport.rs:1098-1102)
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }

    // Draw the bar contents on top (buttons get their own interaction and win the hit test
    // because they are added after / are children of this Ui).
    ui.horizontal_centered(|ui| {
        ui.add_space(8.0);
        ui.label("FxSound");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(6.0);
            if ui.button("✕").clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
            let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            if ui.button(if maximized { "🗗" } else { "🗖" }).clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            if ui.button("🗕").clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
            }
        });
    });
}

/// Eight invisible grab strips around the window edge that start a native resize.
fn resize_edges(ui: &mut egui::Ui, rect: Rect) {
    let g = RESIZE_GRAB;
    let edges: [(&str, Rect, ResizeDirection, egui::CursorIcon); 8] = [
        ("n",  Rect::from_min_size(rect.left_top(),  Vec2::new(rect.width(), g)),
               ResizeDirection::North, egui::CursorIcon::ResizeNorth),
        ("s",  Rect::from_min_size(rect.left_bottom() - Vec2::new(0.0, g), Vec2::new(rect.width(), g)),
               ResizeDirection::South, egui::CursorIcon::ResizeSouth),
        ("w",  Rect::from_min_size(rect.left_top(),  Vec2::new(g, rect.height())),
               ResizeDirection::West,  egui::CursorIcon::ResizeWest),
        ("e",  Rect::from_min_size(rect.right_top() - Vec2::new(g, 0.0), Vec2::new(g, rect.height())),
               ResizeDirection::East,  egui::CursorIcon::ResizeEast),
        ("nw", Rect::from_min_size(rect.left_top(),  Vec2::splat(g)),
               ResizeDirection::NorthWest, egui::CursorIcon::ResizeNorthWest),
        ("ne", Rect::from_min_size(rect.right_top() - Vec2::new(g, 0.0), Vec2::splat(g)),
               ResizeDirection::NorthEast, egui::CursorIcon::ResizeNorthEast),
        ("sw", Rect::from_min_size(rect.left_bottom() - Vec2::new(0.0, g), Vec2::splat(g)),
               ResizeDirection::SouthWest, egui::CursorIcon::ResizeSouthWest),
        ("se", Rect::from_min_size(rect.right_bottom() - Vec2::splat(g), Vec2::splat(g)),
               ResizeDirection::SouthEast, egui::CursorIcon::ResizeSouthEast),
    ];

    for (name, edge_rect, dir, cursor) in edges {
        let resp = ui.interact(edge_rect, Id::new(("fx_resize", name)), Sense::drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(cursor);
        }
        if resp.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}
```

Why `drag_started()` and not `dragged()`: `StartDrag` / `BeginResize` map straight onto
`window.drag_window()` / `window.drag_resize_window()` (`egui-winit-0.36.0/src/lib.rs:1750-1797`),
and winit's own doc is *"There's no guarantee that this will work unless the left mouse button was
pressed immediately before this function is called."* (`winit-0.30.13/src/window.rs:1512-1513`).
Sending it every frame while dragging re-arms the compositor grab and produces jitter.

Why `has_focus()` matters: egui-winit guards `StartDrag` with
`if window.has_focus()` and the comment *"If `.has_focus()` is not checked on x11 the input will be
permanently taken until the app is killed!"* (`egui-winit-0.36.0/src/lib.rs:1750-1752`). So a
`StartDrag` sent from an unfocused window is silently dropped — that's intentional.

`BeginResize` is **not** guarded and **not** supported on macOS (winit returns
`ExternalError::NotSupported`, `winit-0.30.13/src/window.rs:1534`); failures are `log::warn!`-ed
(`egui-winit-0.36.0/src/lib.rs:1795`).

---

## 14. Recipe: other common window operations

```rust
use egui::{ViewportCommand, WindowLevel, SystemTheme, IconData};
use std::sync::Arc;

// --- Close / cancel close ------------------------------------------------
ctx.send_viewport_cmd(ViewportCommand::Close);

// Veto the user's close (root viewport only exits if you don't):
if ctx.input(|i| i.viewport().close_requested()) {
    if self.has_unsaved_changes {
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        self.show_confirm_quit = true;
    }
}

// --- Always on top -------------------------------------------------------
ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnBottom));

// --- Size / position (logical points) ------------------------------------
ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(1024.0, 640.0)));
ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(egui::vec2(640.0, 400.0)));
// Clear the max constraint: MaxInnerSize is turned into `None` when it is INFINITY
// (egui-winit-0.36.0/src/lib.rs:1815)
ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(egui::Vec2::INFINITY));
// Clear the min constraint: MinInnerSize -> None when ZERO (egui-winit-0.36.0/src/lib.rs:1810)
ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(egui::Vec2::ZERO));
ctx.send_viewport_cmd(ViewportCommand::OuterPosition(egui::pos2(100.0, 100.0)));

// Center (returns None when outer_rect/monitor_size are unknown, e.g. Wayland):
if let Some(cmd) = ViewportCommand::center_on_screen(&ctx) {
    ctx.send_viewport_cmd(cmd);
}

// --- Window state --------------------------------------------------------
ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));   // borderless
ctx.send_viewport_cmd(ViewportCommand::SetMonitor(1));      // borderless fullscreen on monitor #1
ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
ctx.send_viewport_cmd(ViewportCommand::Decorations(false));

// --- Icon (build the Arc ONCE; patch() compares by Arc::ptr_eq) -----------
ctx.send_viewport_cmd(ViewportCommand::Icon(Some(Arc::clone(&self.icon))));
ctx.send_viewport_cmd(ViewportCommand::Icon(None));          // clear it

// --- Transparency / overlay ----------------------------------------------
ctx.send_viewport_cmd(ViewportCommand::Transparent(true));
ctx.send_viewport_cmd(ViewportCommand::MousePassthrough(true)); // clicks go behind

// --- Title / theme / attention -------------------------------------------
ctx.send_viewport_cmd(ViewportCommand::Title("FxSound — muted".to_owned()));
ctx.send_viewport_cmd(ViewportCommand::SetTheme(SystemTheme::Dark));
ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
    egui::UserAttentionType::Informational,
));
ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
    egui::UserAttentionType::Reset,
));

// --- Focus a specific viewport -------------------------------------------
let settings = egui::ViewportId::from_hash_of("fxsound_settings");
ctx.send_viewport_cmd_to(settings, ViewportCommand::Focus);

// --- Screenshot -----------------------------------------------------------
ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::new("preset_thumb")));
// ...then, a later frame:
ctx.input(|i| {
    for ev in &i.raw.events {
        if let egui::Event::Screenshot { viewport_id, user_data, image } = ev {
            let _ = (viewport_id, user_data, image); // image: Arc<ColorImage>
        }
    }
});
```

`Event::Screenshot` verbatim (`egui-0.36.0/src/data/input/event.rs:181-188`):

```rust
    Screenshot {
        viewport_id: crate::ViewportId,
        user_data: crate::UserData,
        image: std::sync::Arc<ColorImage>,
    },
```

### `app_id` (Wayland) — get this right or you get a generic icon

```rust
// egui-0.36.0/src/viewport.rs:645
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
```

Doc verbatim (`egui-0.36.0/src/viewport.rs:622-643`): the id is used *"for grouping windows of the
same application"* and *"for connecting the configuration of a `.desktop` file with the window, by
using the application ID as file name. This allows e.g. a proper icon handling under Wayland."* and
*"The `app_id` should match the `.desktop` file distributed with your program."*

Backend: `window_attributes.with_name(app_id, "")` under
`#[cfg(all(feature = "wayland", target_os = "linux"))]` (`egui-winit-0.36.0/src/lib.rs:2132-2136`).

Second effect: *"On eframe, the `app_id` of the root window is also used to determine the storage
location of persistence files."* (`egui-0.36.0/src/viewport.rs:641-643`), and
`NativeOptions` doc confirms *"If you don't set an app id, the title argument to `crate::run_native`
will be used as app id instead."* (`eframe-0.36.0/src/epi.rs:292-293`).

So: install `/usr/share/applications/fxsound.desktop` with `Icon=fxsound`, and call
`.with_app_id("fxsound")`. On Wayland, `ViewportCommand::Icon` / `with_icon` do nothing (see §15) —
the `.desktop` file is the only icon mechanism.

---

## 15. Wayland: what is a no-op or unsupported

Sources are the platform notes in the vendored source, not folklore. `E` = egui doc,
`W` = winit doc, `WI` = egui-winit code path.

| API | Wayland status | Evidence |
|---|---|---|
| `ViewportBuilder::with_app_id` / `ViewportCommand` equivalent | **Wayland-only feature.** Requires `egui-winit/wayland` + linux; there is no `ViewportCommand` for it, so changing it **recreates the window** | WI `egui-winit-0.36.0/src/lib.rs:2132-2136`; E `viewport.rs:870-873` |
| `with_position` / `ViewportCommand::OuterPosition` | **Unsupported — no-op.** | E `viewport.rs:613`; W `winit-0.30.13/src/window.rs:730` ("Android / Wayland: Unsupported.") |
| `ViewportInfo::inner_rect` / `outer_rect` | **Always `None`.** | E `data/input/viewport_info.rs:52-55, 62-65`; W `winit-0.30.13/src/window.rs:674, 699` (`NotSupportedError`) |
| `ViewportCommand::center_on_screen` | **Returns `None`** (needs `outer_rect` + `monitor_size`) | E `viewport.rs:1219-1233` |
| `NativeOptions::centered` | **"Wayland desktop currently not supported."** | `eframe-0.36.0/src/epi.rs:362-367` |
| `with_window_level` / `ViewportCommand::WindowLevel` / `with_always_on_top` | **Unsupported.** | W `winit-0.30.13/src/window.rs:1801-1803` ("iOS / Android / Web / Wayland: Unsupported.") |
| `with_icon` / `ViewportCommand::Icon` | **Unsupported — no-op.** Use `app_id` + `.desktop` file. | W `winit-0.30.13/src/window.rs:1199-1200` ("iOS / Android / Web / Wayland / macOS / Orbital: Unsupported.") |
| `with_visible` / `ViewportCommand::Visible` | **Unsupported.** | W `winit-0.30.13/src/window.rs:966-970` ("Android / Wayland / Web: Unsupported.") |
| `with_active` | **Unsupported.** | E `viewport.rs:445` ("Android / iOS / X11 / Wayland / Orbital: Unsupported.") |
| `ViewportCommand::Focus` | **No effect.** | E `viewport.rs:1168` ("Has no effect on Wayland…"); W `winit-0.30.13/src/window.rs:1312` |
| `ViewportCommand::Minimized(false)` (un-minimize) | **Unsupported.** Minimize works, restore does not. | W `winit-0.30.13/src/window.rs:1054-1058` ("Wayland: Un-minimize is unsupported.") |
| `with_close_button` / `with_minimize_button` / `with_maximize_button` / `ViewportCommand::EnableButtons` | **Not implemented** (Wayland *and* X11). | W `winit-0.30.13/src/window.rs:1026-1031` |
| `ViewportCommand::ContentProtected` | **Unsupported.** | W `winit-0.30.13/src/window.rs:1390-1394` ("iOS / Android / x11 / Wayland / Web / Orbital: Unsupported.") |
| `ViewportCommand::RequestUserAttention` | **Conditional**: needs the `xdg_activation_v1` protocol; `Reset`/`None` has no effect. | W `winit-0.30.13/src/window.rs:1341-1342` |
| `ViewportCommand::CursorPosition` | **Only works while `CursorGrab::Locked`.** | W `winit-0.30.13/src/window.rs:1453-1454` ("Wayland: Cursor must be in `CursorGrabMode::Locked`.") |
| `ViewportCommand::SetTheme` | **Works, but only for client-side decorations**; `SystemDefault` goes through dbus. | W `winit-0.30.13/src/window.rs:1358-1360` |
| `ViewportCommand::StartDrag` | **Works, with a condition**: the cursor must be inside the window. | W `winit-0.30.13/src/window.rs:1518` |
| `ViewportCommand::BeginResize` | **Works** (only macOS/iOS/Android/Web are listed unsupported). | W `winit-0.30.13/src/window.rs:1532-1535` |
| `ViewportCommand::MousePassthrough` | **Works** (only iOS/Android/Web/Orbital unsupported). | W `winit-0.30.13/src/window.rs:1565-1573` |
| `with_transparent` / `ViewportCommand::Transparent` | **Works.** (It is **X11** that can only set it at build time.) | W `winit-0.30.13/src/window.rs:940-944` |
| `with_decorations` / `ViewportCommand::Decorations` | **Works** (CSD). Only iOS/Android/Web are "No effect". | W `winit-0.30.13/src/window.rs:1150-1158` |
| `ViewportCommand::ResizeIncrements` | **Works** (iOS/Android/Web/Orbital unsupported). | W `winit-0.30.13/src/window.rs:895-903` |
| `with_monitor` / `ViewportCommand::SetMonitor` | **Works, and is the only reliable multi-monitor targeting on Wayland.** Puts the window in borderless fullscreen. | E `viewport.rs:698-700`; WI `egui-winit-0.36.0/src/lib.rs:1855-1864, 1976-1990` |
| `with_window_type` / `with_override_redirect` | **X11 only** — compiled out unless `egui-winit/x11` + linux. | WI `egui-winit-0.36.0/src/lib.rs:2138-2166` |
| `with_drag_and_drop` / `with_taskbar` | **Windows only.** | WI `egui-winit-0.36.0/src/lib.rs:2168-2178` |
| `with_fullsize_content_view` / `with_movable_by_background` / `with_title_shown` / `with_titlebar_buttons_shown` / `with_titlebar_shown` / `with_has_shadow` | **macOS only.** | WI `egui-winit-0.36.0/src/lib.rs:2180-2190` |
| `ViewportCommand::InnerSize` | Works, but the **resize is applied immediately and no `Resized` event may follow** on Linux; egui-winit therefore assumes it worked and refreshes the rects itself. | WI `egui-winit-0.36.0/src/lib.rs:1763-1777` |

### Practical Wayland consequences for a frameless FxSound window

1. You cannot position or center the window. Drop any "restore last window position" feature on
   Wayland; `outer_rect` is `None` so there is nothing to save.
2. "Always on top" is impossible. Hide the toggle or grey it out when
   `ViewportInfo::outer_rect.is_none()` is your Wayland proxy (there is no direct
   "am I on Wayland" query in egui).
3. Window icon must come from `.desktop` + `app_id`. `with_icon` is dead code on Wayland.
4. `Minimized(true)` works; you cannot programmatically restore. Don't build a
   "minimize to tray, restore from tray" flow around `ViewportCommand::Minimized`.
5. `StartDrag` needs the cursor inside the window (it will be, if you fire it on `drag_started()`)
   and the window focused (egui-winit's own guard).
6. `Visible(false)` won't hide the window. To hide a secondary window, stop calling
   `show_viewport_deferred` for it.
7. Multi-monitor: use `with_monitor(idx)` / `SetMonitor(idx)` — accepting that it forces borderless
   fullscreen.

---

## 16. eframe glue (the bits you need to make any of this run)

```rust
// eframe-0.36.0/src/lib.rs:288
pub fn run_native(
    app_name: &str,
    native_options: NativeOptions,
    app_creator: AppCreator<'_>,
) -> Result {

// eframe-0.36.0/src/lib.rs:306
pub fn run_native_ext(
    app_name: &str,
    mut native_options: NativeOptions,
    egui_ctx: Option<egui::Context>,
    app_creator: AppCreator<'_>,
) -> Result {

// eframe-0.36.0/src/epi.rs:49
pub type AppCreator<'app> =
    Box<dyn 'app + FnOnce(&CreationContext<'_>) -> Result<Box<dyn 'app + App>, DynError>>;

// eframe-0.36.0/src/epi.rs:42
pub type WindowBuilderHook = Box<dyn FnOnce(egui::ViewportBuilder) -> egui::ViewportBuilder>;

// eframe-0.36.0/src/lib.rs:617
pub type Result<T = (), E = Error> = std::result::Result<T, E>;
```

`App` trait, required + relevant optional methods:

```rust
// eframe-0.36.0/src/epi.rs:167
    fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame) {
// eframe-0.36.0/src/epi.rs:182   <-- the only REQUIRED method
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);
// eframe-0.36.0/src/epi.rs:212
    fn save(&mut self, _storage: &mut dyn Storage) {}
// eframe-0.36.0/src/epi.rs:222   (feature = "glow")
    fn on_exit(&mut self, _gl: Option<&glow::Context>) {}
// eframe-0.36.0/src/epi.rs:228   (not feature = "glow")
    fn on_exit(&mut self) {}
// eframe-0.36.0/src/epi.rs:236
    fn auto_save_interval(&self) -> std::time::Duration {
// eframe-0.36.0/src/epi.rs:248
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
// eframe-0.36.0/src/epi.rs:254
    fn persist_egui_memory(&self) -> bool {
```

`App::ui` doc verbatim (`eframe-0.36.0/src/epi.rs:172-181`): *"The given `egui::Ui` has no margin or
background color. You can wrap your UI code in `egui::CentralPanel` or a `egui::Frame::central_panel`
to remedy this. […] This is called for the root viewport (`egui::ViewportId::ROOT`). Use
`egui::Context::show_viewport_deferred` to spawn additional viewports (windows)."*

`App::on_exit` doc verbatim (`eframe-0.36.0/src/epi.rs:216-218`): *"If you need to abort an exit
check `ctx.input(|i| i.viewport().close_requested())` and respond with
`egui::ViewportCommand::CancelClose`."*

Viewport-relevant `NativeOptions` fields:

```rust
// eframe-0.36.0/src/epi.rs:303
    pub viewport: egui::ViewportBuilder,
// eframe-0.36.0/src/epi.rs:360   (Option<WindowBuilderHook>; NOT preserved by Clone)
    pub window_builder: Option<WindowBuilderHook>,
// eframe-0.36.0/src/epi.rs:367   ("Wayland desktop currently not supported.")
    pub centered: bool,
// eframe-0.36.0/src/epi.rs:379
    pub persist_window: bool,
// eframe-0.36.0/src/epi.rs:383
    pub persistence_path: Option<std::path::PathBuf>,
// eframe-0.36.0/src/epi.rs:342
    pub run_and_return: bool,
```

eframe builds the final `ViewportBuilder` here:

```rust
// eframe-0.36.0/src/native/epi_integration.rs:16
pub fn viewport_builder(
    egui_zoom_factor: f32,
    event_loop: &ActiveEventLoop,
    native_options: &mut epi::NativeOptions,
    window_settings: Option<WindowSettings>,
) -> ViewportBuilder {
```

Order of operations there: restore persisted geometry → clamp to monitor → apply `centered` →
run `window_builder` hook last (`eframe-0.36.0/src/native/epi_integration.rs:33-88`). **The hook
wins over everything**, including persisted window settings.

Backend helpers you may need if you drive winit yourself:

```rust
// egui-winit-0.36.0/src/lib.rs:1967
pub fn create_window(
    egui_ctx: &egui::Context,
    event_loop: &ActiveEventLoop,
    viewport_builder: &ViewportBuilder,
) -> Result<Window, winit::error::OsError> {

// egui-winit-0.36.0/src/lib.rs:1997
pub fn create_winit_window_attributes(
    egui_ctx: &egui::Context,
    viewport_builder: ViewportBuilder,
) -> winit::window::WindowAttributes {

// egui-winit-0.36.0/src/lib.rs:2208
pub fn apply_viewport_builder_to_window(
    egui_ctx: &egui::Context,
    window: &Window,
    builder: &ViewportBuilder,
) {

// egui-winit-0.36.0/src/lib.rs:54
pub fn pixels_per_point(egui_ctx: &egui::Context, window: &Window) -> f32 {
```

Note `create_winit_window_attributes` applies sizes/positions as **`LogicalSize`/`LogicalPosition`
scaled by `egui_ctx.zoom_factor()`**, with a comment pointing at a Wayland bug that motivated it
(`egui-winit-0.36.0/src/lib.rs:2078-2113`), while `apply_viewport_builder_to_window` re-applies them
as `PhysicalSize`/`PhysicalPosition` once the monitor is known
(`egui-winit-0.36.0/src/lib.rs:2219-2255`). `ViewportCommand::InnerSize` etc. are always physical
(`egui-winit-0.36.0/src/lib.rs:1759-1761`).

---

## 17. Gotcha list (all verified)

1. **`ViewportCommand::Close` does not close.** In the winit backend it only pushes
   `ViewportEvent::Close` into `ViewportInfo::events` (`egui-winit-0.36.0/src/lib.rs:1744-1746`).
   The root viewport then exits because eframe acts on the event; a child viewport disappears only
   when you stop calling `show_viewport_*` for it.
2. **`CancelClose` is a no-op at the winit layer** — *"Need to be handled elsewhere"*
   (`egui-winit-0.36.0/src/lib.rs:1747-1749`). It works because eframe checks it between passes.
   It must be sent **in the same frame** you observe `close_requested()`.
3. **`ViewportClass` has no `Debug`** — see §0.7.
4. **`ResizeDirection` has no `Default`** (`egui-0.36.0/src/viewport.rs:1052-1054`).
5. **`ViewportBuilder` is not serde-serializable** even with `egui/serde`
   (`egui-0.36.0/src/viewport.rs:283`). Persist your own struct and rebuild the builder.
6. **Icon diffing uses `Arc::ptr_eq`** (`egui-0.36.0/src/viewport.rs:821-825`, `Arc::ptr_eq` on line 823) — cache the `Arc`.
7. **`with_monitor` means borderless fullscreen**, not "place on this monitor"
   (`egui-winit-0.36.0/src/lib.rs:1981-1985`), and it silently wins over `with_position` /
   `with_fullscreen` (`egui-0.36.0/src/viewport.rs:344-346`). Out-of-range index → `log::warn!` and
   ignored (`egui-winit-0.36.0/src/lib.rs:1858-1863, 1986-1989`).
8. **`MinInnerSize(Vec2::ZERO)` / `MaxInnerSize(Vec2::INFINITY)` are the "clear the constraint"
   encodings**, not errors (`egui-winit-0.36.0/src/lib.rs:1810, 1815`). Non-finite values also clear.
9. **`Minimized`/`Maximized` write back into `ViewportInfo` immediately**
   (`egui-winit-0.36.0/src/lib.rs:1845-1850`), so reading `i.viewport().maximized` right after
   sending the command reflects the request, not necessarily the compositor's answer.
10. **`StartDrag` is dropped when the window isn't focused** (`egui-winit-0.36.0/src/lib.rs:1751`).
11. **Changing `app_id` at runtime recreates the window**
    (`egui-0.36.0/src/viewport.rs:870-873`). Set it once, at startup.
12. **Changing any titlebar-button / macOS-titlebar / X11 / Windows-only property at runtime
    recreates the window** — see the `patch` table in §6.
13. **`set_immediate_viewport_renderer` is thread-local and takes no `self`**
    (`egui-0.36.0/src/context.rs:3985-3992`).
14. **Embedded fallback loses most of your builder** (`egui-0.36.0/src/containers/window.rs:126-138`)
    and `show_embedded_viewport` panics if the embedded `Window` is collapsed
    (`egui-0.36.0/src/context.rs:4180-4182`). Setting `.collapsible(false)` is done for you in the
    immediate path (`egui-0.36.0/src/context.rs:4178`) but **not** in the deferred path
    (`egui-0.36.0/src/context.rs:4068-4070`).
15. **`show_viewport_immediate` panics** if the backend never calls your closure
    (`egui-0.36.0/src/context.rs:4165-4167`) — you'll see this if you forget
    `set_immediate_viewport_renderer` in a custom integration.
16. **`RawInput::viewport()` panics** if the current viewport id isn't in the map
    (`egui-0.36.0/src/data/input/raw_input.rs:118`).
17. **On macOS, `ViewportInfo::maximized`/`minimized` are only refreshed at init**
    (`egui-winit-0.36.0/src/lib.rs:1383-1387`) — querying at runtime deadlocked
    (egui issue 3494, cited in the source).
18. **`clamp_size_to_monitor_size` defaults to `true` in eframe on every platform**
    (`eframe-0.36.0/src/native/epi_integration.rs:28`), even though the egui doc says "true on linux,
    otherwise false" (`egui-0.36.0/src/viewport.rs:298`). If a large requested size mysteriously
    shrinks, this is why — set `.with_clamp_size_to_monitor_size(false)`.
19. **`NativeOptions::window_builder` is not preserved by `Clone`**
    (`eframe-0.36.0/src/epi.rs:358`), and it is `std::mem::take`n when used
    (`eframe-0.36.0/src/native/epi_integration.rs:85-88`) — it runs once.
20. **Transparency needs two things**: `with_transparent(true)` *and* an `App::clear_color` that is
    actually transparent (`eframe-0.36.0/src/epi.rs:238-248`; the default is
    `Color32::from_rgba_unmultiplied(12, 12, 12, 180)`, i.e. already slightly transparent). On macOS
    also pair it with `with_has_shadow(false)` (`egui-0.36.0/src/viewport.rs:508`). On X11,
    winit can only set transparency **at window creation**
    (`winit-0.30.13/src/window.rs:942-944`), so `ViewportCommand::Transparent` is an X11 no-op.
