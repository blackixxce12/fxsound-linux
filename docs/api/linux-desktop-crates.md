# Linux desktop crates — verified API cheatsheet

`ksni` 0.3.6 · `notify-rust` 4.18.0 · `rfd` 0.17.2 · `egui_extras` 0.36.0 · `resvg`/`usvg` 0.48.1 · `tiny-skia` 0.12.0

Every signature below was read verbatim out of the vendored source under
`/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.
Citations are `<crate-dir>/<file>:<line>` relative to that registry root.

Companion documents: `eframe-0.36.md`, `egui-0.36-painting.md`, `egui-0.36-style-fonts.md`,
`egui-0.36-viewport.md`, `egui-0.36-widgets-input.md`.

**Verification status.** Every code block in sections 2, 3, 4 and 8 was compiled with
`cargo check --offline` against these exact vendored versions before being pasted here.
Section 8's module was additionally *run* (`cargo run --example`) and its assertions pass.
Where a snippet is illustrative rather than compiled, it says so.

---

## 0. Read this first — the things that break a build

| You probably assume | Reality in these versions | Cite |
|---|---|---|
| `egui_extras`'s SVG support and your `resvg` are the same crate | **They are not.** `egui_extras` 0.36.0 pins `resvg = "0.45.1"`; this workspace pins `resvg = "0.48.1"`. Both end up in the tree and their types **do not unify**. | `egui_extras-0.36.0/Cargo.toml:150-153` |
| `egui_extras::image::load_svg_bytes(bytes, &opt)` takes *my* `usvg::Options` | It takes **resvg 0.45's** `usvg::Options`. Passing 0.48's is a hard type error (see §5.2 for the verbatim rustc output). | `egui_extras-0.36.0/src/image.rs:62` |
| `egui_extras` default features include the loaders | `default = ["dep:mime_guess2"]` only. This workspace additionally sets `default-features = false`, so **no loader is compiled at all**. | `egui_extras-0.36.0/Cargo.toml`, `fxsound-linux/Cargo.toml` |
| `ksni::Tray::spawn()` is inherent | It is on the **`TrayMethods` extension trait**, which you must `use`. Async by default; `ksni::blocking::TrayMethods` is a *different* trait behind the `blocking` feature. | `ksni-0.3.6/src/lib.rs:315,331`; `src/blocking.rs:12,18` |
| `ksni` is runtime-agnostic out of the box | `default = ["tokio"]`, and `tokio` + `async-io` **cannot both be on** — it's a `compile_error!`. | `ksni-0.3.6/Cargo.toml:55`; `src/compat.rs:1-5` |
| `ksni::Tray::menu(&self)` may block | The `activate` callbacks run on the D-Bus service task. Blocking there freezes the menu. Send a message instead. | `ksni-0.3.6/src/menu.rs:115-128` |
| `ksni::Icon` holds RGBA | **ARGB32, network byte order.** You must rotate the channels. | `ksni-0.3.6/src/tray.rs:150-156` |
| `TrayService::new(tray)` / `service.handle()` (ksni 0.1/0.2) | **Gone.** `TrayServiceBuilder` exists but is not constructible directly; go through `TrayMethods`. | `ksni-0.3.6/src/lib.rs:419-420` |
| `Notification::show()` is cheap and non-blocking | It is **`zbus::block_on`** over a whole connect+send. Calling it from a tokio task panics (`tokio` feature) or stalls a worker (`async-io`). | `notify-rust-4.18.0/src/xdg/mod.rs:8,413`; `zbus-5.19.0/src/utils.rs:39-53` |
| `handle.wait_for_action(|action: &str| ..)` is the API | Still there, but the typed replacement is `wait_for_response(impl ResponseHandler)` taking `&NotificationResponse`. | `notify-rust-4.18.0/src/xdg/mod.rs:99,132` |
| `notification.actions(vec![..])` | `#[deprecated(note = "please use .action() only")]`. Use `.action(id, label)`. | `notify-rust-4.18.0/src/notification.rs:442-443` |
| `rfd::FileDialog` builders take `&mut self` | They take **`self` by value** and return `Self`. Chain them; you cannot hold one in a variable and mutate it. | `rfd-0.17.2/src/file_dialog.rs:51-107` |
| `AsyncFileDialog::pick_file()` is `async fn` | It returns **`impl Future<Output = Option<FileHandle>>`** — not `async fn`, and not `Send`-bound at the signature (the underlying `DialogFutureType` is `+ Send` on native). | `rfd-0.17.2/src/file_dialog.rs:268`; `src/backend.rs:90` |
| rfd's XDG backend needs a D-Bus crate at build time | It **`dlopen`s `libdbus-1.so.3`** at runtime, and silently falls back to spawning `zenity` if that fails. Package both. | `rfd-0.17.2/src/backend/xdg_desktop_portal/portal/mod.rs:194-203`; `src/backend/xdg_desktop_portal.rs:105` |
| `usvg::Tree::from_data(bytes, &opt, &fontdb)` (usvg ≤ 0.42) | The `fontdb` argument is **gone**; the database lives on `Options`. | `usvg-0.48.1/src/parser/mod.rs:105` |
| `resvg::Tree::from_usvg(&tree)` then `.render(..)` (resvg ≤ 0.40) | **Gone.** It is the free function `resvg::render(&tree, transform, &mut pixmap.as_mut())`. | `resvg-0.48.1/src/lib.rs:34-38` |
| `Pixmap::data()` is straight RGBA | **Premultiplied** RGBA. Use `ColorImage::from_rgba_premultiplied`, not `..._unmultiplied`. | `tiny-skia-0.12.0/src/pixmap.rs:228-232` |
| `ColorImage::new([w,h], Color32::TRANSPARENT)` | `new` now takes **`pixels: Vec<Color32>`**. The fill constructor is `ColorImage::filled(size, color)`. | `epaint-0.36.2/src/image.rs:61,75` |

Also still true from the eframe/egui docs, repeated because these crates sit next to them:
`eframe::run_simple_native` does not exist, `App`'s required method is
`fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)` (`eframe-0.36.0/src/epi.rs:182`),
panels take `&mut Ui` (`egui-0.36.0/src/containers/panel.rs:422,1212`),
`TopBottomPanel`/`SidePanel` are replaced by `Panel::top/bottom/left/right(id)`
(`egui-0.36.0/src/containers/panel.rs:249,256,265,274`), and `Rounding` is `CornerRadius`
(`epaint-0.36.2/src/corner_radius.rs:13`).

One more that bit the author of this file: `Panel` has **`exact_size(f32)`**, not
`exact_height` / `exact_width` (`egui-0.36.0/src/containers/panel.rs:405`).

---

## 1. What this workspace pins

From `fxsound-linux/Cargo.toml`:

```toml
egui        = "=0.36.0"
eframe      = { version = "=0.36.0", default-features = false, features = ["default_fonts", "glow", "wayland", "x11"] }
egui_extras = { version = "=0.36.0", default-features = false }

resvg       = "0.48.1"
usvg        = "0.48.1"
tiny-skia   = "0.12.0"

ksni        = "0.3.6"
notify-rust = "4.18.0"
rfd         = "0.17.2"
```

Consequences, each verified:

* `egui_extras` with `default-features = false` compiles **no loaders and no `svg`**, so
  `install_image_loaders` is a no-op warning path (`egui_extras-0.36.0/src/loaders.rs:99-105`).
  You are rasterising SVGs yourself — §8 is the module for that.
* `ksni` uses its **default `tokio` feature** (`ksni-0.3.6/Cargo.toml:55`). There is no `tokio`
  entry in the workspace's `[workspace.dependencies]`, so adding `ksni` to a crate pulls tokio in
  transitively with only `features = ["rt", "macros"]` (`ksni-0.3.6/Cargo.toml:150-155`) — **not**
  `rt-multi-thread`. Add tokio explicitly with the runtime flavour you want.
* `notify-rust` uses its **default `z` feature** = `["zbus", "serde", "async"]`, and
  `async = ["zbus/async-io"]` (`notify-rust-4.18.0/Cargo.toml`). So notify-rust drives
  **async-io**, while ksni drives **tokio**. Both zbus feature sets can coexist in one binary,
  but see §3.6 before calling `.show()` from a tokio task.
* `rfd` uses its **default `["xdg-portal", "wayland"]`** (`rfd-0.17.2/Cargo.toml`), i.e. the
  portal backend, not GTK3. No `libgtk-3-dev` at build time.

---

## 2. ksni 0.3.6 — StatusNotifierItem tray

Crate dir: `ksni-0.3.6/`. `rust-version = "1.80"`, `edition = "2021"` (`Cargo.toml:13-14`).

### 2.1 Cargo.toml, verbatim

```toml
# ksni-0.3.6/Cargo.toml:44-59
[features]
async-io = [
    "dep:async-io",
    "dep:async-lock",
    "dep:async-executor",
    "dep:futures-lite",
    "dep:futures-channel",
    "dep:task-local",
    "zbus/async-io",
]
blocking = []
default = ["tokio"]
tokio = [
    "dep:tokio",
    "zbus/tokio",
]
```

```toml
# ksni-0.3.6/Cargo.toml:150-160
[dependencies.tokio]
version = "1"
features = [
    "rt",
    "macros",
]
optional = true

[dependencies.zbus]
version = "5"
default-features = false
```

The runtime rules are enforced at compile time:

```rust
// ksni-0.3.6/src/compat.rs:1-5
#[cfg(all(not(feature = "async-io"), not(feature = "tokio")))]
compile_error!(r#"Either "tokio" (default) or "async-io" must be enabled."#);

#[cfg(all(feature = "async-io", feature = "tokio"))]
compile_error!(r#"Features "tokio" and "async-io" cannot be enabled at the same time."#);
```

Which runtime to pick:

| Want | Dependency line |
|---|---|
| tokio (default) | `ksni = "0.3.6"` |
| smol / async-io | `ksni = { version = "0.3.6", default-features = false, features = ["async-io"] }` |
| no async at all in your code | `ksni = { version = "0.3.6", features = ["blocking"] }` — keeps tokio underneath |
| blocking API on async-io | `ksni = { version = "0.3.6", default-features = false, features = ["async-io", "blocking"] }` |

`spawn()` on the tokio path calls `tokio::spawn`, so **you must already be inside a tokio
runtime** when you `.await` it:

```rust
// ksni-0.3.6/src/compat.rs:16-22
    pub fn spawn<F>(future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(future);
    }
```

The `blocking` feature sidesteps that with a private current-thread runtime:

```rust
// ksni-0.3.6/src/compat.rs:24-35
    #[cfg(feature = "blocking")]
    pub fn block_on<T>(future: impl Future<Output = T>) -> T {
        use std::sync::LazyLock;
        use tokio::runtime::Runtime;
        static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        });
        RUNTIME.block_on(future)
    }
```

### 2.2 Crate re-exports

```rust
// ksni-0.3.6/src/lib.rs:39-41
#[doc(inline)]
pub use menu::{MenuItem, TextDirection};
pub use tray::{Category, Icon, Orientation, Status, ToolTip};
```

Note `StandardItem` / `CheckmarkItem` / `SubMenu` / `RadioGroup` / `RadioItem` / `Disposition`
are **not** re-exported at the crate root — reach them through `ksni::menu::*`.

### 2.3 The `Tray` trait — every method, verbatim

```rust
// ksni-0.3.6/src/lib.rs:49-50
/// A system tray, implement this to create your tray
pub trait Tray: Sized + Send + 'static {
```

| Item | Signature (verbatim) | Default | Cite |
|---|---|---|---|
| assoc const | `const MENU_ON_ACTIVATE: bool = false;` | `false` | `src/lib.rs:57` |
| **required** | `fn id(&self) -> String;` | — | `src/lib.rs:72` |
| | `fn activate(&mut self, _x: i32, _y: i32) {}` | no-op | `src/lib.rs:82` |
| | `fn secondary_activate(&mut self, _x: i32, _y: i32) {}` | no-op | `src/lib.rs:93` |
| | `fn scroll(&mut self, _delta: i32, _orientation: Orientation) {}` | no-op | `src/lib.rs:101` |
| | `fn category(&self) -> Category` | `Category::ApplicationStatus` | `src/lib.rs:104-106` |
| | `fn title(&self) -> String` | `Default::default()` | `src/lib.rs:110-112` |
| | `fn status(&self) -> Status` | `Status::Active` | `src/lib.rs:115-117` |
| | `fn window_id(&self) -> i32` | `0` | `src/lib.rs:125-127` |
| | `fn icon_theme_path(&self) -> String` | `Default::default()` | `src/lib.rs:130-132` |
| | `fn icon_name(&self) -> String` | `Default::default()` | `src/lib.rs:141-143` |
| | `fn icon_pixmap(&self) -> Vec<Icon>` | `Default::default()` | `src/lib.rs:146-148` |
| | `fn overlay_icon_name(&self) -> String` | `Default::default()` | `src/lib.rs:153-155` |
| | `fn overlay_icon_pixmap(&self) -> Vec<Icon>` | `Default::default()` | `src/lib.rs:159-161` |
| | `fn attention_icon_name(&self) -> String` | `Default::default()` | `src/lib.rs:165-167` |
| | `fn attention_icon_pixmap(&self) -> Vec<Icon>` | `Default::default()` | `src/lib.rs:171-173` |
| | `fn attention_movie_name(&self) -> String` | `Default::default()` | `src/lib.rs:180-182` |
| | `fn tool_tip(&self) -> ToolTip` | `Default::default()` | `src/lib.rs:187-189` |
| | `fn text_direction(&self) -> TextDirection` | `TextDirection::LeftToRight` | `src/lib.rs:193-195` |
| | `fn menu(&self) -> Vec<MenuItem<Self>>` | `Default::default()` | `src/lib.rs:200-202` |
| | `fn menu_about_to_show(&mut self)` | sets an internal flag | `src/lib.rs:211-213` |
| | `fn watcher_online(&self) {}` | no-op | `src/lib.rs:220` |
| | `fn watcher_offline(&self, reason: OfflineReason) -> bool` | `true` | `src/lib.rs:230-232` |

Notes that matter:

* `window_id` is **`i32`**, deliberately, not the spec's `u32` —
  see the comment at `src/lib.rs:119-124`.
* `menu()` takes `&self`; the callbacks inside take `&mut T`.
* `watcher_offline` returning `false` **shuts the tray service down**
  (`src/lib.rs:226`). Return `true` to keep waiting for the watcher to come back.
* There is no `item_is_menu` — it is commented out at `src/lib.rs:137`. Use
  `MENU_ON_ACTIVATE` instead.

```rust
// ksni-0.3.6/src/lib.rs:235-252
/// Why is the tray offline
#[derive(Debug)]
#[non_exhaustive]
pub enum OfflineReason {
    No,
    Error(Error),
}
```

```rust
// ksni-0.3.6/src/lib.rs:257-287
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Dbus(zbus::Error),
    Watcher(zbus::fdo::Error),
    WontShow,
}
```

`Error` implements `Display` (`src/lib.rs:289`) and `std::error::Error` (`src/lib.rs:300`).

### 2.4 Data types

```rust
// ksni-0.3.6/src/tray.rs:150-156
#[derive(Clone, Debug, Hash, Type, Value, Serialize)]
pub struct Icon {
    pub width: i32,
    pub height: i32,
    /// ARGB32 format, network byte order
    pub data: Vec<u8>,
}
```

`Icon` has **no `Default`** and no constructor — build it literally. `data.len()` must be
`width * height * 4`. See §2.8 for how to fill it from an SVG.

```rust
// ksni-0.3.6/src/tray.rs:106-117
#[derive(Clone, Debug, Default, Hash, Type, Value, Serialize)]
pub struct ToolTip {
    pub icon_name: String,
    pub icon_pixmap: Vec<Icon>,
    pub title: String,
    pub description: String,
}
```

```rust
// ksni-0.3.6/src/tray.rs:14-19
pub enum Orientation {
    Horizontal,
    Vertical,
}

// ksni-0.3.6/src/tray.rs:24-41
pub enum Category {
    ApplicationStatus,
    Communications,
    SystemServices,
    Hardware,
}

// ksni-0.3.6/src/tray.rs:60-73
pub enum Status {
    Passive,
    Active,
    NeedsAttention,
}

// ksni-0.3.6/src/menu.rs:28-33
pub enum TextDirection {
    LeftToRight,
    RightToLeft,
}
```

`Status::Passive` means *"visualizations will likely hide it"* (`src/tray.rs:61-63`) — that is
how you hide a tray icon without tearing the service down.

### 2.5 Menu types

```rust
// ksni-0.3.6/src/menu.rs:72-80
/// All types of menu item
///
/// Do not use directly (except [`MenuItem::Separator`]), see examples in top level documents
pub enum MenuItem<T> {
    Standard(StandardItem<T>),
    /// A separator
    Separator,
    Checkmark(CheckmarkItem<T>),
    SubMenu(SubMenu<T>),
    RadioGroup(RadioGroup<T>),
}
```

Build the struct and call `.into()`; every variant has a `From` impl
(`src/menu.rs:147,224,332,424`).

```rust
// ksni-0.3.6/src/menu.rs:83-129 (field list, doc comments elided)
pub struct StandardItem<T> {
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub icon_name: String,
    pub icon_data: Vec<u8>,
    pub shortcut: Vec<Vec<String>>,
    pub disposition: Disposition,
    pub activate: Box<dyn Fn(&mut T) + Send>,
}
```

```rust
// ksni-0.3.6/src/menu.rs:132-145
impl<T> Default for StandardItem<T> {
    fn default() -> Self {
        StandardItem {
            label: String::default(),
            enabled: true,
            visible: true,
            icon_name: String::default(),
            icon_data: Vec::default(),
            shortcut: Vec::default(),
            disposition: Disposition::Normal,
            activate: Box::new(|_this| {}),
        }
    }
}
```

```rust
// ksni-0.3.6/src/menu.rs:173-205
pub struct SubMenu<T> {
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub icon_name: String,
    pub icon_data: Vec<u8>,
    pub shortcut: Vec<Vec<String>>,
    pub disposition: Disposition,
    /// List of submenu items
    pub submenu: Vec<MenuItem<T>>,
}
```

`SubMenu` has **no `activate`** — the field is commented out at `src/menu.rs:203-204`, and its
`RawMenuItem` conversion installs a no-op click handler (`src/menu.rs:243`).

```rust
// ksni-0.3.6/src/menu.rs:247-313
pub struct CheckmarkItem<T> {
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub checked: bool,
    pub icon_name: String,
    pub icon_data: Vec<u8>,
    pub shortcut: Vec<Vec<String>>,
    pub disposition: Disposition,
    pub activate: Box<dyn Fn(&mut T) + Send>,
}
```

`CheckmarkItem` does **not** flip `checked` for you — the callback must
(`src/menu.rs:299-306`).

```rust
// ksni-0.3.6/src/menu.rs:364-412
pub struct RadioGroup<T> {
    /// Index of the current selected radio item
    pub selected: usize,
    pub select: Box<dyn Fn(&mut T, usize) + Send>,
    /// List of radio items
    pub options: Vec<RadioItem>,
}

// ksni-0.3.6/src/menu.rs:432-462
pub struct RadioItem {
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub icon_name: String,
    pub icon_data: Vec<u8>,
    pub shortcut: Vec<Vec<String>>,
    pub disposition: Disposition,
}
```

`RadioItem` is *not* a `MenuItem` — it only lives inside `RadioGroup::options`
(`src/menu.rs:429-431`).

```rust
// ksni-0.3.6/src/menu.rs:851-862
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Disposition {
    Normal,
    Informative,
    Warning,
    Alert,
}
```

`icon_data` is documented as **PNG data** (`src/menu.rs:99`), not raw pixels — unlike
`Tray::icon_pixmap`, which is ARGB32.

The shortcut encoding, verbatim from `src/menu.rs:101-109`: each inner `Vec<String>` is a list
of modifiers (`"Control"`, `"Alt"`, `"Shift"`, `"Super"`) followed by the key.
`Ctrl+S` is `vec![vec!["Control".into(), "S".into()]]`.

### 2.6 Spawning — `TrayMethods`

```rust
// ksni-0.3.6/src/lib.rs:311-315
/// Provides methods for [`Tray`]
#[allow(async_fn_in_trait)]
pub trait TrayMethods: Tray + private::Sealed {
```

```rust
// ksni-0.3.6/src/lib.rs:331
    async fn spawn(self) -> Result<Handle<Self>, Error>;

// ksni-0.3.6/src/lib.rs:348  (deprecated since 0.3.4)
    async fn spawn_without_dbus_name(self) -> Result<Handle<Self>, Error>;

// ksni-0.3.6/src/lib.rs:379
    fn disable_dbus_name(self, disable: bool) -> TrayServiceBuilder<Self>;

// ksni-0.3.6/src/lib.rs:397
    fn assume_sni_available(self, assume_available: bool) -> TrayServiceBuilder<Self>;
```

(The four above are written with their bodies in the source; the bodies just forward to
`TrayServiceBuilder`. `impl<T: Tray> TrayMethods for T {}` at `src/lib.rs:401`.)

```rust
// ksni-0.3.6/src/lib.rs:415-424
/// Builder to customize tray service
///
/// Should not be constructed directly, use [`TrayMethods`] instead.
pub struct TrayServiceBuilder<T: Tray> {
    tray: T,
    own_name: bool,
    assume_sni_available: bool,
}
```

```rust
// ksni-0.3.6/src/lib.rs:442
    pub async fn spawn(self) -> Result<Handle<T>, Error>;
// ksni-0.3.6/src/lib.rs:473
    pub fn disable_dbus_name(self, disable: bool) -> Self;
// ksni-0.3.6/src/lib.rs:494
    pub fn assume_sni_available(self, assume_available: bool) -> Self;
```

What the two knobs do, from the doc comments:

* **`disable_dbus_name(true)`** — do not own the well-known name `StatusNotifierItem-PID-ID`.
  Violates the spec, but **required inside a sandbox (flatpak)** (`src/lib.rs:446-451`).
* **`assume_sni_available(true)`** — route `Error::Watcher(ServiceUnknown(..))` and
  `Error::WontShow` to `Tray::watcher_offline` instead of failing `spawn()`. Useful when the app
  can start before the desktop finishes initialising; the cost is that a truly absent SNI
  implementation never reports an error (`src/lib.rs:480-493`).

### 2.7 The handle — updating the tray from another thread

```rust
// ksni-0.3.6/src/lib.rs:523-527
/// Handle to the tray
pub struct Handle<T> {
    service: Weak<Mutex<service::Service<T>>>,
    sender: mpsc::UnboundedSender<HandleReuest>,
}
```

```rust
// ksni-0.3.6/src/lib.rs:530-560
    /// Update the tray
    ///
    /// Returns the result of `f`, returns `None` if the tray service
    /// has been shutdown.
    pub async fn update<R, F: FnOnce(&mut T) -> R>(&self, f: F) -> Option<R>;

    /// Shutdown the tray service
    pub fn shutdown(&self) -> ShutdownAwaiter;

    /// Returns `true` if the tray service has been shutdown
    pub fn is_closed(&self) -> bool;
```

`Handle<T>: Clone` for any `T` (`src/lib.rs:607-614`) — clone it freely across threads. Because
`Tray: Send + 'static`, the handle is `Send + Sync`.

`shutdown()` is **sync and fire-and-forget**; `.await` the returned `ShutdownAwaiter` only if you
want to block until the service is actually gone:

```rust
// ksni-0.3.6/src/lib.rs:563-566
/// Returned by [`Handle::shutdown`]
///
/// Just `.await` if you want to wait the shutdown to complete
pub struct ShutdownAwaiter { .. }
```

`ShutdownAwaiter: Future<Output = ()>` (`src/lib.rs:586-605`).

The important ordering detail, from the body of `update`: the closure runs against the tray
state **and then** an `Update` message is pushed and awaited, which is what makes the D-Bus
property-changed signals fire. So `update` is how you make a change visible — mutating a
`Tray` you kept a copy of elsewhere does nothing.

### 2.8 Complete tray, compiled

This is the shape to use: the tray struct owns a channel back to the app, callbacks only
*send*, and the app drives changes back in through `Handle::update`.

```rust
use std::sync::mpsc::Sender;

use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Category, Handle, Icon, MenuItem, Status, ToolTip, Tray, TrayMethods};

pub enum UiMsg { Toggle(bool), Preset(usize), Open, Quit }

pub struct FxTray {
    enabled: bool,
    preset: usize,
    presets: Vec<String>,
    icon: Vec<Icon>,
    tx: Sender<UiMsg>,
}

impl Tray for FxTray {
    const MENU_ON_ACTIVATE: bool = false;

    fn id(&self) -> String { "fxsound".into() }

    fn title(&self) -> String { "FxSound".into() }

    fn category(&self) -> Category { Category::Hardware }

    fn status(&self) -> Status {
        if self.enabled { Status::Active } else { Status::Passive }
    }

    fn icon_name(&self) -> String { String::new() }

    fn icon_pixmap(&self) -> Vec<Icon> { self.icon.clone() }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: "FxSound".into(),
            description: if self.enabled { "Processing" } else { "Bypassed" }.into(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) { let _ = self.tx.send(UiMsg::Open); }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.enabled = !self.enabled;
        let _ = self.tx.send(UiMsg::Toggle(self.enabled));
    }

    fn scroll(&mut self, _delta: i32, _orientation: ksni::Orientation) {}

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            CheckmarkItem {
                label: "Power".into(),
                checked: self.enabled,
                activate: Box::new(|t: &mut Self| {
                    t.enabled = !t.enabled;
                    let _ = t.tx.send(UiMsg::Toggle(t.enabled));
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Presets".into(),
                submenu: vec![RadioGroup {
                    selected: self.preset,
                    select: Box::new(|t: &mut Self, i| {
                        t.preset = i;
                        let _ = t.tx.send(UiMsg::Preset(i));
                    }),
                    options: self
                        .presets
                        .iter()
                        .map(|p| RadioItem { label: p.clone(), ..Default::default() })
                        .collect(),
                }
                .into()],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| { let _ = t.tx.send(UiMsg::Quit); }),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::warn!("StatusNotifierWatcher offline: {reason:?}");
        true // keep trying; return false to shut the service down
    }
}

async fn spawn_tray(tx: Sender<UiMsg>) -> Result<Handle<FxTray>, ksni::Error> {
    FxTray {
        enabled: true,
        preset: 0,
        presets: vec!["Flat".into(), "Bass".into()],
        icon: Vec::new(),
        tx,
    }
    .assume_sni_available(true)
    .spawn()
    .await
}

async fn drive(handle: Handle<FxTray>) {
    // Returns Some(R) with the closure's value, or None if the service is gone.
    let _now_enabled: Option<bool> = handle
        .update(|t: &mut FxTray| { t.enabled = false; t.enabled })
        .await;

    if handle.is_closed() { return; }
    handle.shutdown().await; // `.await` is optional
}
```

### 2.9 Updating from a non-async thread

`Handle::update` is `async`. From a plain `std::thread` in a tokio app, hold a
`tokio::runtime::Handle` and hop:

```rust
// Illustrative: needs tokio with `rt`.
fn update_from_worker(rt: tokio::runtime::Handle, tray: ksni::Handle<FxTray>, enabled: bool) {
    rt.spawn(async move {
        tray.update(move |t: &mut FxTray| t.enabled = enabled).await;
    });
}
```

Or take the blocking API, whose `Handle::update` is synchronous:

```rust
// ksni-0.3.6/src/blocking.rs:198-219
/// Handle to the tray
pub struct Handle<T>(crate::Handle<T>);

impl<T> Handle<T> {
    pub fn update<R, F: FnOnce(&mut T) -> R>(&self, f: F) -> Option<R>;
    pub fn shutdown(&self) -> ShutdownAwaiter;
    pub fn is_closed(&self) -> bool;
}

// ksni-0.3.6/src/blocking.rs:221-229
pub struct ShutdownAwaiter(crate::ShutdownAwaiter);

impl ShutdownAwaiter {
    /// Wait the shutdown to complete
    pub fn wait(self);
}
```

```rust
// ksni-0.3.6/src/blocking.rs:12-18, 65, 83
pub trait TrayMethods: Tray + private::Sealed {
    fn spawn(self) -> Result<Handle<Self>, Error>;
    fn disable_dbus_name(self, disable: bool) -> TrayServiceBuilder<Self>;
    fn assume_sni_available(self, assume_available: bool) -> TrayServiceBuilder<Self>;
}
// ksni-0.3.6/src/blocking.rs:115
impl<T: Tray> TrayServiceBuilder<T> { pub fn spawn(self) -> Result<Handle<T>, Error>; }
```

`ksni::blocking::TrayMethods` and `ksni::TrayMethods` have colliding method names — **import
exactly one** in a given module.

### 2.10 Supplying pixmaps

The crate's own example, verbatim, is the reference for the channel order:

```rust
// ksni-0.3.6/examples/custom_icon.rs:13-36
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static ICON: LazyLock<ksni::Icon> = LazyLock::new(|| {
            let img = image::load_from_memory_with_format(
                include_bytes!("custom_icon.png"),
                image::ImageFormat::Png,
            )
            .expect("valid image");
            let (width, height) = img.dimensions();
            let mut data = img.into_rgba8().into_vec();
            assert_eq!(data.len() % 4, 0);
            for pixel in data.chunks_exact_mut(4) {
                pixel.rotate_right(1) // rgba to argb
            }
            ksni::Icon {
                width: width as i32,
                height: height as i32,
                data,
            }
        });

        vec![ICON.clone()]
    }
```

`pixel.rotate_right(1)` on a 4-byte chunk turns `RGBA` into `ARGB`. That is the whole
conversion. To feed it from an SVG, use §8's `svg_to_rgba_premultiplied` and apply the same
rotate:

```rust
// Uses `svg_to_rgba_premultiplied` from §8.
fn svg_to_ksni_icon(svg_bytes: &[u8], size: u32) -> Result<ksni::Icon, SvgError> {
    let mut data = svg_to_rgba_premultiplied(svg_bytes, size)?;
    for px in data.chunks_exact_mut(4) {
        px.rotate_right(1); // RGBA -> ARGB
    }
    Ok(ksni::Icon { width: size as i32, height: size as i32, data })
}
```

Return **several sizes** from `icon_pixmap` (16, 22, 24, 32, 48) and let the host pick; the
return type is `Vec<Icon>` for exactly that reason.

---

## 3. notify-rust 4.18.0 — desktop notifications

Crate dir: `notify-rust-4.18.0/`.

### 3.1 Features, verbatim

```toml
# notify-rust-4.18.0/Cargo.toml
[features]
async = ["zbus/async-io"]
d = ["dbus"]
d_vendored = ["dbus/vendored"]
debug_namespace = []
default = ["z"]
images = ["images_no_default_features", "image/rayon", "image/default-formats"]
images_no_default_features = ["image", "lazy_static"]
preview-macos-un = ["dep:mac-usernotifications"]
tokio = ["zbus/tokio"]
z = ["zbus", "serde", "async"]
z-with-tokio = ["zbus", "serde", "tokio"]
```

| Want | Dependency line |
|---|---|
| default (zbus + async-io) | `notify-rust = "4.18.0"` |
| zbus driven by tokio | `notify-rust = { version = "4.18.0", default-features = false, features = ["z-with-tokio"] }` |
| `Hint::ImageData` / `Image` | add `features = ["images"]` |
| the old `dbus-rs` C backend | `default-features = false, features = ["d"]` — needs `libdbus-1-dev` at build time |

`zbus` itself is a **target-gated** dependency (`unix, not(macos)`) at version 5, no default
features (`Cargo.toml:140-143`).

### 3.2 `Notification`

```rust
// notify-rust-4.18.0/src/notification.rs:49-52
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Notification {
```

Public fields on Linux (`src/notification.rs:52-113`):
`appname: String`, `summary: String`, `subtitle: Option<String>`, `body: String`,
`icon: String`, `hints: HashSet<Hint>`, `actions: Vec<String>`, `timeout: Timeout`.
The struct is `#[non_exhaustive]` — build it with `new()`, never a struct literal.

> `hints` carries a warning in the source: *"This does not hold all hints. `Hint::Custom` and
> `Hint::CustomInt` are held elsewhere"* (`src/notification.rs:69-72`). Read hints back through
> the API, not the field.

### 3.3 Builder — every method, verbatim

All builders take `&mut self` and return `&mut Notification`, so the chain ends in a reference;
`.show()` takes `&self` so the chain works directly, and `.finalize()` gives you an owned copy.

```rust
// notify-rust-4.18.0/src/notification.rs
:123   pub fn new() -> Notification
:131   pub fn at_bus(sub_bus: &str) -> Notification            // #[deprecated] test-only, #[doc(hidden)]
:145   pub fn appname(&mut self, appname: &str) -> &mut Notification
:153   pub fn summary(&mut self, summary: &str) -> &mut Notification
:161   pub fn subtitle(&mut self, subtitle: &str) -> &mut Notification     // macOS only in effect
:168   pub fn image_data(&mut self, image: Image) -> &mut Notification     // feature "images"
:183   pub fn image_path(&mut self, path: &str) -> &mut Notification
:204   pub fn image<T: AsRef<std::path::Path> + Sized>(...)                // feature "images"
:246   pub fn body(&mut self, body: &str) -> &mut Notification
:259   pub fn icon(&mut self, icon: &str) -> &mut Notification
:270   pub fn auto_icon(&mut self) -> &mut Notification
:303   pub fn hint(&mut self, hint: Hint) -> &mut Notification
:361   pub fn timeout<T: Into<Timeout>>(&mut self, timeout: T) -> &mut Notification
:387   pub fn urgency(&mut self, urgency: Urgency) -> &mut Notification
:443   pub fn actions(&mut self, actions: Vec<String>) -> &mut Notification   // #[deprecated]
:451   pub fn action(&mut self, identifier: &str, label: &str) -> &mut Notification
:465   pub fn id(&mut self, id: u32) -> &mut Notification
:485   pub fn finalize(&self) -> Notification
:511   pub fn show(&self) -> Result<xdg::NotificationHandle>
:520   pub async fn show_async(&self) -> Result<xdg::NotificationHandle>
:530   pub async fn show_async_at_bus(&self, sub_bus: &str) -> Result<xdg::NotificationHandle>
```

`Result<T>` is `notify_rust::error::Result<T>` = `std::result::Result<T, Error>`
(`src/error.rs:7`). `Error` is an opaque struct wrapping a `#[non_exhaustive] ErrorKind`
(`src/error.rs:16-24`), so match on `ErrorKind`, not on `Error`.

`.icon()` takes either a **freedesktop icon name** or a `file://` URI
(`src/notification.rs:63-64`).

`.id(u32)` pre-assigns the replacement id, which is how you update a notification you have not
shown yet; after `show()`, prefer `NotificationHandle::update` (`src/notification.rs:457-464`).

### 3.4 Hints

```rust
// notify-rust-4.18.0/src/hints.rs:39-95
#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub enum Hint {
    /// If true, server may interpret action identifiers as named icons and display those.
    ActionIcons(bool),
    Category(String),
    /// Name of the `DesktopEntry` representing the calling application.
    DesktopEntry(String),
    #[cfg(all(feature = "images_no_default_features", unix, not(target_os = "macos")))]
    ImageData(Image),
    /// Display the image at this path.
    ImagePath(String),
    /// This does not work on all servers, however timeout=0 will do the job
    Resident(bool),
    /// Play the sound at this path.
    SoundFile(String),
    SoundName(String),
    SuppressSound(bool),
    Transient(bool),
    /// Lets the notification point to a certain 'x' position on the screen. Requires `Y`.
    X(i32),
    Y(i32),
    Urgency(Urgency),
    /// If you want to pass something entirely different.
    Custom(String, String),
    /// A custom numerical (integer) hint
    CustomInt(String, i32),
    /// Only used by this `NotificationServer` implementation.
    Invalid
}
```

`Hint::ImageData` is gated on `images_no_default_features` (so on either `images` or
`images_no_default_features`). Everything else is always available on Linux.

Accessors: `Hint::as_bool() -> Option<bool>` (`src/hints.rs:99`),
`Hint::as_i32() -> Option<i32>` (`src/hints.rs:110`).

`Hint::Custom` / `Hint::CustomInt` are stored **keyed by name**, so re-setting the same key
replaces it; every other variant goes into a `HashSet` (`src/notification.rs:303-318`). That's
how `x-dunst-stack-tag` style per-server hints behave sanely.

`.urgency(u)` is a thin wrapper over `.hint(Hint::Urgency(u))` (`src/notification.rs:387-390`).

```rust
// notify-rust-4.18.0/src/urgency.rs:28-35
pub enum Urgency {
    Low = 0,
    Normal = 1,
    /// A critical notification will not time out.
    Critical = 2,
}
```

`Urgency: TryFrom<&str>` (`src/urgency.rs:37`).

### 3.5 Timeout

```rust
// notify-rust-4.18.0/src/timeout.rs:14-26
pub enum Timeout {
    /// Expires according to server default.
    #[default]
    Default,
    /// Do not expire, user will have to close this manually.
    Never,
    /// Expire after n milliseconds.
    Milliseconds(u32),
}
```

The `Into<Timeout>` conversions are surprising — memorise them:

```rust
// notify-rust-4.18.0/src/timeout.rs:35-44
impl From<i32> for Timeout {
    fn from(int: i32) -> Timeout {
        use std::cmp::Ordering::*;
        match int.cmp(&0) {
            Greater => Timeout::Milliseconds(int as u32),
            Less => Timeout::Default,
            Equal => Timeout::Never,
        }
    }
}

// notify-rust-4.18.0/src/timeout.rs:46-56
impl From<Duration> for Timeout {
    fn from(duration: Duration) -> Timeout {
        if duration.is_zero() {
            Timeout::Never
        } else if duration.as_millis() > u32::MAX as u128 {
            Timeout::Default
        } else {
            Timeout::Milliseconds(duration.as_millis().try_into().unwrap_or(u32::MAX))
        }
    }
}
```

So `.timeout(0)` means **Never**, and `.timeout(-1)` means **server default** — the inverse of
what the raw D-Bus protocol number does. `Duration::ZERO` is likewise `Never`.

### 3.6 Blocking vs async — read before calling `.show()`

Every sync entry point is `zbus::block_on` over the async one:

```rust
// notify-rust-4.18.0/src/xdg/mod.rs:8
use zbus::{block_on, zvariant};
// notify-rust-4.18.0/src/xdg/mod.rs:413
    block_on(zbus_rs::connect_and_send_notification(notification)).map(Into::into)
```

and `zbus::block_on` on the `tokio` feature is:

```rust
// zbus-5.19.0/src/utils.rs:37-53
#[cfg(feature = "tokio")]
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::OnceLock;

    static TOKIO_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    TOKIO_RT
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("launch of single-threaded tokio runtime")
        })
        .block_on(future)
}
```

Practical rules:

* **From a sync context (egui update, plain thread): `.show()` is fine.**
* **From inside a tokio task with zbus/tokio: `.show()` panics** — "Cannot block the current
  thread from within a runtime". Use `.show_async().await`.
* **From inside a tokio task with zbus/async-io (the default here): `.show()` blocks a tokio
  worker** for the duration of a D-Bus round trip. Wrap it in `spawn_blocking`, or use
  `.show_async().await`.
* The same applies to `get_capabilities()`, `get_server_information()`,
  `handle_action()`, `handle.close()`, `handle.update()`, `handle.wait_for_action()` and
  `handle.wait_for_response()` — all `block_on`
  (`src/xdg/mod.rs:116,138,217,279,490,520,582`).

### 3.7 `NotificationHandle`

```rust
// notify-rust-4.18.0/src/xdg/mod.rs:64-70
/// A handle to a shown notification.
///
/// Keeps a connection alive to ensure actions work on certain desktops.
#[derive(Debug)]
pub struct NotificationHandle {
    inner: NotificationHandleInner,
}
```

> "Keeps a connection alive to ensure actions work on certain desktops" — **drop the handle and
> your action buttons may stop working.** Keep it alive for as long as the notification matters.

```rust
// notify-rust-4.18.0/src/xdg/mod.rs
:99    pub fn wait_for_action<F>(self, invocation_closure: F) where F: FnOnce(&str)
:132   pub fn wait_for_response(self, handler: impl ResponseHandler) -> Result<()>
:181   pub async fn wait_for_action_async<F>(&self, invocation_closure: F)
                where F: FnOnce(&NotificationResponse)
:212   pub fn close(self)
:227   pub async fn close_async(&self)
:267   pub fn on_close<A>(self, handler: impl CloseHandler<A>)
:308   pub fn update(&mut self) -> Result<()>
:318   pub fn id(&self) -> u32
```

`wait_for_action`, `wait_for_response`, `close` and `on_close` **consume `self`**; `update` takes
`&mut self`; the two `_async` ones take `&self` (with a `// TODO: make this consume self in 5.0`
at `src/xdg/mod.rs:179`).

`NotificationHandle` implements `Deref<Target = Notification>` and `DerefMut`
(`src/xdg/mod.rs:329,343`), which is how `handle.summary("…"); handle.update()?;` works — you are
mutating the stored `Notification` through the handle, then re-sending it with the same id. The
doc example is at `src/xdg/mod.rs:296-307`, and it warns that on Plasma you should change the
**appname** too, or the old message is amended rather than replaced.

### 3.8 Responses

```rust
// notify-rust-4.18.0/src/response.rs:63-83
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NotificationResponse {
    /// The default action was invoked — the user activated the notification without
    /// choosing a specific button (e.g. clicked the body, tapped the banner).
    Default,
    /// The user invoked a named action button.
    Action(String),
    /// The user submitted an inline text reply.
    Reply(String),
    /// The notification was closed without any action being taken.
    Closed(CloseReason),
}
```

`NotificationResponse::Reply` is macOS-only; on XDG it is never emitted
(`src/response.rs:75-78`). `is_default_action()` at `src/response.rs:87`.

```rust
// notify-rust-4.18.0/src/response.rs:21-34
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CloseReason {
    Expired,
    Dismissed,
    CloseAction,
    /// An unrecognised or reserved reason was reported by the platform.
    Other(u32),
}
```

Wire mapping (`src/response.rs:36-45`): `1 => Expired`, `2 => Dismissed`, `3 => CloseAction`,
anything else `Other(n)`.

```rust
// notify-rust-4.18.0/src/response.rs:124-166
pub trait ResponseHandler {
    /// Invoke the handler with the given response.
    fn call(self, response: &NotificationResponse);
}
impl<F> ResponseHandler for F where F: FnOnce(&NotificationResponse) { .. }

pub trait CloseHandler<T> {
    /// Called with the [`CloseReason`].
    fn call(&self, reason: CloseReason);
}
impl<F> CloseHandler<CloseReason> for F where F: Fn(CloseReason) { .. }
impl<F> CloseHandler<()>          for F where F: Fn()            { .. }
```

`CloseHandler` is implemented for **both** `Fn(CloseReason)` and `Fn()` — that is why
`on_close` carries a phantom type parameter `A`; if inference complains, annotate the closure
argument (`|reason: CloseReason| ..`).

Legacy note: `wait_for_action`'s `&str` form maps `Closed(_)` to the pseudo-action
`"__closed"` (`src/xdg/mod.rs:110,119`). Prefer `wait_for_response`.

### 3.9 Server introspection

```rust
// notify-rust-4.18.0/src/xdg/mod.rs
:401   pub enum DbusStack { Dbus, Zbus }   // variants are feature-gated
:453   pub fn dbus_stack() -> Option<DbusStack>
:489   pub fn get_capabilities() -> Result<Vec<String>>
:519   pub fn get_server_information() -> Result<ServerInformation>
:578   pub fn handle_action<F>(id: u32, func: F) -> Result<()>
```

Re-exported at the crate root (`src/lib.rs:200-203`):
`dbus_stack, get_capabilities, get_server_information, handle_action, DbusStack, NotificationHandle`.

Query `get_capabilities()` before relying on `body-markup`, `body-hyperlinks`, `actions`,
`persistence`, `icon-static` — the source says most hints have no effect on the big desktops
(`src/notification.rs:300-301`).

### 3.10 Compiled examples

```rust
use std::time::Duration;

use notify_rust::{
    CloseReason, Hint, Notification, NotificationHandle, NotificationResponse, Timeout, Urgency,
    get_capabilities, get_server_information,
};

fn notify_simple() -> Result<NotificationHandle, notify_rust::error::Error> {
    Notification::new()
        .appname("FxSound")
        .summary("Preset applied")
        .body("Bass Boost is now active")
        .icon("fxsound")
        .timeout(Timeout::Milliseconds(4000))
        .urgency(Urgency::Normal)
        .hint(Hint::Category("device".to_owned()))
        .hint(Hint::DesktopEntry("fxsound".to_owned()))
        .hint(Hint::Transient(true))
        .hint(Hint::SuppressSound(true))
        .hint(Hint::CustomInt("value".to_owned(), 72))
        .hint(Hint::Custom("x-dunst-stack-tag".to_owned(), "fxsound-vol".to_owned()))
        .show()
}

fn notify_timeout_conversions() {
    let mut n = Notification::new();
    n.timeout(Timeout::Never);
    n.timeout(Duration::from_secs(5));   // -> Milliseconds(5000)
    n.timeout(0);                        // -> Never
    n.timeout(2500);                     // -> Milliseconds(2500)
    let _owned: Notification = n.summary("x").finalize();
}

fn notify_actions() -> Result<(), notify_rust::error::Error> {
    let handle = Notification::new()
        .summary("Device changed")
        .action("open", "Open FxSound")
        .action("ignore", "Ignore")
        .action("default", "default")   // "default" = body click
        .show()?;

    // Blocks until the user acts. Consumes the handle.
    handle.wait_for_response(|response: &NotificationResponse| match response {
        NotificationResponse::Default => {}
        NotificationResponse::Action(key) if key == "open" => {}
        NotificationResponse::Action(_) => {}
        NotificationResponse::Reply(_) => {}   // never on XDG
        NotificationResponse::Closed(CloseReason::Expired) => {}
        NotificationResponse::Closed(_) => {}
    })
}

fn notify_update_and_close() -> Result<(), notify_rust::error::Error> {
    let mut handle = Notification::new().summary("0%").show()?;
    let _id: u32 = handle.id();
    handle.summary("100%");   // Deref to Notification
    handle.update()?;
    handle.close();
    Ok(())
}

fn notify_on_close() -> Result<(), notify_rust::error::Error> {
    let handle = Notification::new().summary("bye").show()?;
    handle.on_close(|reason: CloseReason| { let _ = reason; });
    Ok(())
}

async fn notify_async() -> Result<(), notify_rust::error::Error> {
    let handle = Notification::new().summary("async").show_async().await?;
    handle
        .wait_for_action_async(|r: &NotificationResponse| { let _ = r; })
        .await;
    handle.close_async().await;
    Ok(())
}

fn caps() {
    let _c: Result<Vec<String>, _> = get_capabilities();
    let _s = get_server_information();
    let _stack = notify_rust::dbus_stack();
}
```

---

## 4. rfd 0.17.2 — native file and message dialogs

Crate dir: `rfd-0.17.2/`.

### 4.1 Features, verbatim

```toml
# rfd-0.17.2/Cargo.toml
[features]
common-controls-v6 = ["windows-sys/Win32_UI_Controls"]
default = ["xdg-portal", "wayland"]
file-handle-inner = []
gtk3 = ["gtk-sys", "glib-sys", "gobject-sys"]
wayland = ["wayland-backend", "wayland-client", "wayland-protocols"]
xdg-portal = ["pollster"]
```

| Feature | Effect |
|---|---|
| `xdg-portal` (default) | portal backend via `libdbus`, zenity fallback. No build-time C deps. |
| `gtk3` | GTK3 backend instead. Requires `gtk3-devel` / `libgtk-3-dev` to **build**. |
| `wayland` (default) | enables the Wayland window-identifier export, so `set_parent` can hand the portal a real parent handle on Wayland. |
| `common-controls-v6` | Windows only. |

Backend selection is `cfg`, not runtime:

```rust
// rfd-0.17.2/src/backend.rs:8-18 and 37-47
#[cfg(all(
    any(target_os = "linux", target_os = "freebsd", target_os = "dragonfly",
        target_os = "netbsd", target_os = "openbsd"),
    not(feature = "gtk3")
))]
mod linux;
...
#[cfg(all(
    any(target_os = "linux", ...),
    not(feature = "gtk3")
))]
mod xdg_desktop_portal;
```

So on Linux it is **`gtk3` off ⇒ portal+zenity**, `gtk3` on ⇒ GTK. `xdg-portal`'s only job is
pulling in `pollster` (`Cargo.toml`): the portal module itself is gated on `not(feature = "gtk3")`.
Disabling `xdg-portal` without enabling `gtk3` will not build.

### 4.2 Runtime requirements — what to put in the package

From the crate docs (`src/lib.rs:37-77`), paraphrased and cited:

* Portal backend **`dlopen`s libdbus at runtime**, falling back to zenity:
  ```rust
  // rfd-0.17.2/src/backend/xdg_desktop_portal/portal/mod.rs:194-203
      pub fn open_libdbus() -> Option<&'static Self> {
          if let Some(lib) = LIB.get() { .. }
          let lib = Liblary::open(c"libdbus-1.so.3").or_else(|| Liblary::open(c"libdbus-1.so"))?;
          ..
      }
  ```
* Every dialog method falls through to zenity on portal failure, logging at `warn`:
  ```rust
  // rfd-0.17.2/src/backend/xdg_desktop_portal.rs:105-112
              warn!("Using zenity fallback");
              match block_on(zenity::pick_file(&self)) {
                  Ok(res) => res,
                  Err(err) => {
                      error!("Failed to pick file with zenity: {err}");
                      None
                  }
              }
  ```
  (same shape at `:134, :188, :218, :272`)
* **Message dialogs are zenity-only** on the portal backend (`src/backend/xdg_desktop_portal.rs:303,324`)
  — there is no portal API for them. `zenity` is a hard runtime requirement if you show any.
* The wlroots portal backend "does not implement the D-Bus API that RFD requires"
  (`src/lib.rs:69-71`). Ship a dependency on xdg-desktop-portal-gtk / -gnome / -kde.
* A failed dialog is indistinguishable from "user cancelled": both are `None`. Turn on `log` to
  see the `warn!`/`error!` lines.

### 4.3 `FileDialog` (sync)

```rust
// rfd-0.17.2/src/file_dialog.rs:15-27
/// Synchronous File Dialog. Supported platforms:
///   * Linux
///   * Windows
///   * Mac
#[derive(Default, Debug, Clone)]
pub struct FileDialog {
    pub(crate) filters: Vec<Filter>,
    pub(crate) starting_directory: Option<PathBuf>,
    pub(crate) file_name: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) parent: Option<RawWindowHandle>,
    pub(crate) parent_display: Option<RawDisplayHandle>,
    pub(crate) can_create_directories: Option<bool>,
}
```

`unsafe impl Send for FileDialog {}` / `unsafe impl Sync for FileDialog {}`
(`src/file_dialog.rs:31-32`) — it can cross threads despite holding raw handles.

```rust
// rfd-0.17.2/src/file_dialog.rs — builders (all consume self)
:38    pub fn new() -> Self
:51    pub fn add_filter(mut self, name: impl Into<String>, extensions: &[impl ToString]) -> Self
:63    pub fn set_directory<P: AsRef<Path>>(mut self, path: P) -> Self
:77    pub fn set_file_name(mut self, file_name: impl Into<String>) -> Self
:86    pub fn set_title(mut self, title: impl Into<String>) -> Self
:96    pub fn set_parent<W: HasWindowHandle + HasDisplayHandle + ?Sized>(mut self, parent: &W) -> Self
:107   pub fn set_can_create_directories(mut self, can: bool) -> Self   // macOS only

// rfd-0.17.2/src/file_dialog.rs — terminals
:122   pub fn pick_file(self)   -> Option<PathBuf>
:127   pub fn pick_files(self)  -> Option<Vec<PathBuf>>
:132   pub fn pick_folder(self) -> Option<PathBuf>
:137   pub fn pick_folders(self)-> Option<Vec<PathBuf>>
:172   pub fn save_file(self)   -> Option<PathBuf>
```

`pick_file_or_folder` / `pick_files_or_folders` are **`#[cfg(target_os = "macos")]`**
(`src/file_dialog.rs:141-155`) — do not reach for them on Linux.

`add_filter` extensions are bare, **no dot and no glob**: `&["fac"]`, not `&["*.fac"]`. The
portal backend turns them into globs itself, and treats `"*"` or `""` specially:

```rust
// rfd-0.17.2/src/backend/xdg_desktop_portal.rs:53-61
                if file_extension == "*" || file_extension.is_empty() {
                    c"*".to_owned()
                } else {
                    CString::new(format!("*.{file_extension}")).unwrap()
                }
```

`set_parent` takes **raw-window-handle 0.6** traits (`Cargo.toml:79-80`, resolved 0.6.2), which
is the same major eframe 0.36 uses (`eframe-0.36.0/Cargo.toml:164-165`), and
`eframe::Frame` implements both (`eframe-0.36.0/src/epi.rs:701,710`). So
`FileDialog::new().set_parent(frame)` compiles directly — verified.

Save-dialog behaviour differs per platform; the exact wording is at `src/file_dialog.rs:158-171`.
For GTK/portal: *filters only hide existing files, extensions are not appended, and unsupported
extensions are not prevented.* **Append the extension yourself.**

### 4.4 `AsyncFileDialog`

```rust
// rfd-0.17.2/src/file_dialog.rs:178-185
/// Asynchronous File Dialog. Supported platforms:
///  * Linux
///  * Windows
///  * Mac
///  * WASM32
#[derive(Default, Debug, Clone)]
pub struct AsyncFileDialog {
    file_dialog: FileDialog,
}
```

```rust
// rfd-0.17.2/src/file_dialog.rs — same builders, then
:268   pub fn pick_file(self)    -> impl Future<Output = Option<FileHandle>>
:273   pub fn pick_files(self)   -> impl Future<Output = Option<Vec<FileHandle>>>
:281   pub fn pick_folder(self)  -> impl Future<Output = Option<FileHandle>>
:289   pub fn pick_folders(self) -> impl Future<Output = Option<Vec<FileHandle>>>
:328   pub fn save_file(self)    -> impl Future<Output = Option<FileHandle>>
```

These are **not `async fn`** — they return an opaque future built from:

```rust
// rfd-0.17.2/src/backend.rs:88-92
// Return type of async dialogs:
#[cfg(not(target_arch = "wasm32"))]
pub type DialogFutureType<T> = Pin<Box<dyn Future<Output = T> + Send>>;
```

so the future **is** `Send` on native, even though `impl Future` at the call site does not say
so. On Linux the portal backend runs the blocking work on its own thread and wakes the future
through a oneshot:

```rust
// rfd-0.17.2/src/backend/xdg_desktop_portal.rs:19-33
fn async_thread<T, F>(f: F) -> DialogFutureType<Option<T>>
where
    F: FnOnce() -> Option<T>,
    F: Send + 'static,
    T: Send + 'static,
{
    Box::pin(async move {
        let (tx, rx) = crate::oneshot::channel();

        std::thread::spawn(move || {
            tx.send(f()).ok();
        });

        rx.await.ok()?
    })
}
```

Meaning: on Linux **`AsyncFileDialog` is safe to await from any executor** and does not need
the main thread. (macOS is the opposite — see `src/lib.rs:79-86`.)

**Choose async in an egui app.** `FileDialog::pick_file()` blocks the calling thread until the
user answers; called from `App::ui` it freezes the window. Either await
`AsyncFileDialog` on a runtime, or `std::thread::spawn` the sync call and poll a channel.

### 4.5 `FileHandle`

```rust
// rfd-0.17.2/src/file_handle/native.rs:114-168
pub struct FileHandle(PathBuf);

impl FileHandle {
    pub fn file_name(&self) -> String
    pub fn path(&self) -> &Path
    pub async fn read(&self) -> Vec<u8>
    pub async fn write(&self, data: &[u8]) -> std::io::Result<()>
    pub fn inner(&self) -> &Path          // feature "file-handle-inner"
}
```

`read()` returns `Vec<u8>` with **no error channel** — the implementation does
`Poll::Ready(res.unwrap())` (`src/file_handle/native.rs:50-51`), i.e. it **panics** on an I/O
error. On native, prefer `std::fs::read(handle.path())` if you need to handle failure.
`write()` does return `io::Result`.

### 4.6 Message dialogs

```rust
// rfd-0.17.2/src/message_dialog.rs
:29    pub fn new() -> Self
:37    pub fn set_level(mut self, level: MessageLevel) -> Self
:43    pub fn set_title(mut self, text: impl Into<String>) -> Self
:51    pub fn set_description(mut self, text: impl Into<String>) -> Self
:62    pub fn set_buttons(mut self, btn: MessageButtons) -> Self
:72    pub fn set_parent<W: HasWindowHandle + HasDisplayHandle + ?Sized>(mut self, parent: &W) -> Self
:82    pub fn show(self) -> MessageDialogResult

:93    pub struct AsyncMessageDialog(MessageDialog);
:148   pub fn show(self) -> impl Future<Output = MessageDialogResult>
```

```rust
// rfd-0.17.2/src/message_dialog.rs:153-186
#[derive(Debug, Clone, Copy, Default)]
pub enum MessageLevel {
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Default)]
pub enum MessageButtons {
    #[default]
    Ok,
    OkCancel,
    YesNo,
    YesNoCancel,
    /// One customizable button.
    OkCustom(String),
    /// Two customizable buttons.
    OkCancelCustom(String, String),
    /// Three customizable buttons.
    YesNoCancelCustom(String, String, String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum MessageDialogResult {
    Yes,
    No,
    Ok,
    #[default]
    Cancel,
    Custom(String),
}
```

`MessageDialogResult` has a `Display` impl (`src/message_dialog.rs:189`). Note `Cancel` is the
`Default`, which is also what you get when zenity is missing — treat "Cancel" as "no answer".

### 4.7 Crate exports

```rust
// rfd-0.17.2/src/lib.rs:131-148
mod backend;

mod file_handle;
pub use file_handle::FileHandle;

mod file_dialog;
mod oneshot;

#[cfg(not(target_arch = "wasm32"))]
pub use file_dialog::FileDialog;

pub use file_dialog::AsyncFileDialog;

mod message_dialog;
pub use message_dialog::{
    AsyncMessageDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel,
};
```

### 4.8 Compiled examples

```rust
use std::path::PathBuf;

use rfd::{
    AsyncFileDialog, AsyncMessageDialog, FileDialog, FileHandle, MessageButtons, MessageDialog,
    MessageDialogResult, MessageLevel,
};

fn sync_open() -> Option<PathBuf> {
    FileDialog::new()
        .set_title("Import preset")
        .add_filter("FxSound preset", &["fac"])   // no dot, no glob
        .add_filter("All files", &["*"])
        .set_directory("/home/user")
        .set_file_name("My Preset.fac")
        .pick_file()
}

fn sync_many()    -> Option<Vec<PathBuf>> { FileDialog::new().pick_files() }
fn sync_folder()  -> Option<PathBuf>      { FileDialog::new().pick_folder() }
fn sync_folders() -> Option<Vec<PathBuf>> { FileDialog::new().pick_folders() }
fn sync_save()    -> Option<PathBuf> {
    FileDialog::new().set_file_name("preset.fac").save_file()
}

async fn async_open() -> Option<Vec<u8>> {
    let handle: FileHandle = AsyncFileDialog::new()
        .set_title("Import preset")
        .add_filter("FxSound preset", &["fac"])
        .set_directory("/tmp")
        .pick_file()
        .await?;
    let _name: String = handle.file_name();
    let _path: &std::path::Path = handle.path();
    Some(handle.read().await)   // NB: panics on I/O error
}

async fn async_save(bytes: &[u8]) -> std::io::Result<()> {
    if let Some(h) = AsyncFileDialog::new().set_file_name("preset.fac").save_file().await {
        h.write(bytes).await?;
    }
    Ok(())
}

fn message() -> MessageDialogResult {
    MessageDialog::new()
        .set_level(MessageLevel::Warning)
        .set_title("Discard changes?")
        .set_description("The preset has unsaved edits.")
        .set_buttons(MessageButtons::YesNoCancel)
        .show()
}

async fn message_async() -> MessageDialogResult {
    AsyncMessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title("No PipeWire")
        .set_description("Could not connect.")
        .set_buttons(MessageButtons::OkCustom("Retry".into()))
        .show()
        .await
}

fn match_result(r: MessageDialogResult) {
    match r {
        MessageDialogResult::Yes => {}
        MessageDialogResult::No => {}
        MessageDialogResult::Ok => {}
        MessageDialogResult::Cancel => {}
        MessageDialogResult::Custom(s) => { let _ = s; }
    }
}

// Parent it to the eframe window: raw-window-handle 0.6 on both sides.
fn with_parent<W>(w: &W) -> Option<PathBuf>
where
    W: raw_window_handle::HasWindowHandle + raw_window_handle::HasDisplayHandle,
{
    FileDialog::new().set_parent(w).pick_file()
}
```

Inside `eframe::App::ui` the parent is the `Frame`:

```rust
// Verified to compile against eframe 0.36.0 + rfd 0.17.2.
fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
    if ui.button("Import…").clicked() {
        let picked = rfd::FileDialog::new()
            .set_parent(frame)
            .add_filter("preset", &["fac"])
            .pick_file();
        let _ = picked;
    }
}
```

---

## 5. egui_extras 0.36.0 — image loaders

Crate dir: `egui_extras-0.36.0/`.

### 5.1 Features, verbatim

```toml
# egui_extras-0.36.0/Cargo.toml
[features]
all_loaders = ["file", "http", "image", "svg", "gif", "webp"]
datepicker = ["jiff"]
default = ["dep:mime_guess2"]
file = ["dep:mime_guess2"]
gif = ["image", "image/gif"]
http = ["dep:ehttp"]
image = ["dep:image"]
serde = ["egui/serde", "enum-map/serde", "dep:serde"]
svg = ["resvg"]
svg_text = ["svg", "resvg/text", "resvg/system-fonts"]
syntect = ["dep:syntect"]
webp = ["image", "image/webp"]
```

* **`svg` = `["resvg"]`** — nothing else. It turns on `resvg` with `default-features = false`
  (`Cargo.toml:150-153`), so **no text support**: an SVG containing `<text>` renders with the
  glyphs missing.
* **`svg_text`** adds `resvg/text` + `resvg/system-fonts` and makes the loader call
  `options.fontdb_mut().load_system_fonts()` (`src/loaders/svg_loader.rs:40-41`). That is a
  startup cost of tens of ms; only enable it if your assets really have live text.
* `default` is **just `mime_guess2`** — no loaders. `default-features = false` (this workspace)
  leaves you with none.

### 5.2 The resvg version split — the single biggest trap

```toml
# egui_extras-0.36.0/Cargo.toml:150-153
[dependencies.resvg]
version = "0.45.1"
optional = true
default-features = false
```

`egui_extras` 0.36.0 is pinned to **resvg 0.45.1** (which brings `usvg` 0.45.1 and
`tiny-skia` 0.11.4). This workspace depends on **resvg 0.48.1** (`usvg` 0.48.1,
`tiny-skia` 0.12.0). Cargo keeps both because 0.45 and 0.48 are semver-incompatible; the
registry already has all six crates vendored.

The public APIs are *source-identical* — a diff of the public items in
`usvg-0.45.1/src/parser/options.rs` against `usvg-0.48.1/src/parser/options.rs` is empty, and
`resvg::render` has the same signature in both (`resvg-0.45.1/src/lib.rs:34-38`,
`resvg-0.48.1/src/lib.rs:34-38`). **But the types are distinct.** Verbatim rustc output from
trying to pass 0.48's `Options` to `egui_extras`:

```text
error[E0308]: mismatched types
  --> src/main.rs:53:47
   |
53 |     egui_extras::image::load_svg_bytes(bytes, &opt)
   |     ----------------------------------        ^^^^ expected `usvg::parser::options::Options<'_>`, found `resvg::usvg::Options<'static>`
   |     |
   |     arguments to this function are incorrect
   |
note: there are multiple different versions of crate `usvg` in the dependency graph
  --> .../usvg-0.45.1/src/parser/options.rs:13:1
   |
13 | pub struct Options<'a> {
   | ^^^^^^^^^^^^^^^^^^^^^^ this is the expected type
   |
  ::: .../usvg-0.48.1/src/parser/options.rs:13:1
   |
13 | pub struct Options<'a> {
   | ---------------------- this is the found type
```

`egui_extras` does **not** re-export `resvg`, so there is no way to name the 0.45 types unless
you add `resvg = "0.45.1"` as a second direct dependency. Pick one:

1. **Do your own rasterisation** with resvg 0.48 (what §8 does, and what this workspace's
   feature flags already imply). Recommended.
2. Drop the direct `resvg`/`usvg`/`tiny-skia` deps, enable `egui_extras`'s `svg` feature, and
   use `egui::Image::new("file://icons/foo.svg")` — never touching `usvg::Options` yourself.
3. Keep both and add `resvg = "0.45.1"` alongside 0.48 just to talk to `egui_extras`. This
   doubles the SVG stack in the binary. Don't.

### 5.3 `install_image_loaders`

```rust
// egui_extras-0.36.0/src/loaders.rs:58
pub fn install_image_loaders(ctx: &egui::Context) {
```

```rust
// egui_extras-0.36.0/src/lib.rs:32
pub use loaders::install_image_loaders;
```

Doc comment, verbatim (`src/loaders.rs:3-16`):

> Installs a set of image loaders.
>
> Calling this enables the use of `egui::Image` and `egui::Ui::image`.
>
> ⚠ This will do nothing and you won't see any images unless you also enable some feature flags
> on `egui_extras`:
>
> - `file` feature: `file://` loader on non-Wasm targets
> - `http` feature: `http(s)://` loader
> - `image` feature: Loader of png, jpeg etc using the `image` crate
> - `svg` feature: `.svg` loader
>
> Calling this multiple times on the same `egui::Context` is safe. It will never install
> duplicate loaders.

The body is a chain of `#[cfg(feature = ...)]` blocks (`src/loaders.rs:59-97`), each guarded by
`ctx.is_loader_installed(..)`. With no features it falls through to:

```rust
// egui_extras-0.36.0/src/loaders.rs:99-105
    #[cfg(all(
        any(target_arch = "wasm32", not(feature = "file")),
        not(feature = "http"),
        not(feature = "image"),
        not(feature = "svg")
    ))]
    log::warn!("`install_image_loaders` was called, but no loaders are enabled");
```

**That warning is what this workspace currently gets.** Calling it is harmless; it is also
pointless until a loader feature is on.

Loader dispatch rules, verbatim from `src/loaders.rs:44-55`:

> The `image` loader ... will attempt to load any URI with any extension other than `svg`. It
> will also try to load any URI without an extension.
>
> The `svg` loader ... will attempt to load any URI with an `svg` extension. It will *not*
> attempt to load a URI without an extension. The content type specified by `BytesPoll::Ready::mime`
> always takes precedence, and must include `svg` for it to be considered supported. For
> example, `image/svg+xml` would be loaded by the `svg` loader.

Confirmed by the loader itself:

```rust
// egui_extras-0.36.0/src/loaders/svg_loader.rs:30-32
fn is_supported(uri: &str) -> bool {
    uri.ends_with(".svg")
}
```

```rust
// egui_extras-0.36.0/src/loaders/svg_loader.rs:26-28
impl SvgLoader {
    pub const ID: &'static str = egui::generate_loader_id!(SvgLoader);
}
```

To actually use it:

```toml
egui_extras = { version = "=0.36.0", default-features = false, features = ["svg"] }
```

```rust
// Illustrative — requires the `svg` feature, which this workspace does not enable.
fn setup(cc: &eframe::CreationContext<'_>) {
    egui_extras::install_image_loaders(&cc.egui_ctx);
}

fn ui(ui: &mut egui::Ui) {
    // `file` feature needed for file://, or use include_bytes + `Image::from_bytes`.
    ui.add(egui::Image::new(egui::include_image!("../assets/power.svg")).fit_to_exact_size(egui::vec2(24.0, 24.0)));
}
```

### 5.4 `egui_extras::image` — the standalone rasterisers

```rust
// egui_extras-0.36.0/src/lib.rs:16-17
#[doc(hidden)]
pub mod image;
```

It is `pub` but `#[doc(hidden)]` — usable, undocumented on docs.rs, and not covered by semver
promises. Signatures:

```rust
// egui_extras-0.36.0/src/image.rs:13-14   (feature "image")
#[cfg(feature = "image")]
pub fn load_image_bytes(image_bytes: &[u8]) -> Result<egui::ColorImage, egui::load::LoadError>

// egui_extras-0.36.0/src/image.rs:61-65   (feature "svg")
#[cfg(feature = "svg")]
pub fn load_svg_bytes(
    svg_bytes: &[u8],
    options: &resvg::usvg::Options<'_>,
) -> Result<egui::ColorImage, String>

// egui_extras-0.36.0/src/image.rs:75-80   (feature "svg")
#[cfg(feature = "svg")]
pub fn load_svg_bytes_with_size(
    svg_bytes: &[u8],
    size_hint: SizeHint,
    options: &resvg::usvg::Options<'_>,
) -> Result<egui::ColorImage, String>
```

The body of `load_svg_bytes_with_size` is the canonical rasterisation and is worth reading in
full, because §8 reimplements it against resvg 0.48:

```rust
// egui_extras-0.36.0/src/image.rs:80-131
) -> Result<egui::ColorImage, String> {
    use egui::Vec2;
    use resvg::{
        tiny_skia::Pixmap,
        usvg::{Transform, Tree},
    };

    profiling::function_scope!();

    let rtree = Tree::from_data(svg_bytes, options).map_err(|err| err.to_string())?;

    let source_size = Vec2::new(rtree.size().width(), rtree.size().height());

    let scaled_size = match size_hint {
        SizeHint::Size {
            width,
            height,
            maintain_aspect_ratio,
        } => {
            if maintain_aspect_ratio {
                // As large as possible, without exceeding the given size:
                let mut size = source_size;
                size *= width as f32 / source_size.x;
                if size.y > height as f32 {
                    size *= height as f32 / size.y;
                }
                size
            } else {
                Vec2::new(width as _, height as _)
            }
        }
        SizeHint::Height(h) => source_size * (h as f32 / source_size.y),
        SizeHint::Width(w) => source_size * (w as f32 / source_size.x),
        SizeHint::Scale(scale) => scale.into_inner() * source_size,
    };

    let scaled_size = scaled_size.round();
    let (w, h) = (scaled_size.x as u32, scaled_size.y as u32);

    let mut pixmap =
        Pixmap::new(w, h).ok_or_else(|| format!("Failed to create SVG Pixmap of size {w}x{h}"))?;

    resvg::render(
        &rtree,
        Transform::from_scale(w as f32 / source_size.x, h as f32 / source_size.y),
        &mut pixmap.as_mut(),
    );

    let image = egui::ColorImage::from_rgba_premultiplied([w as _, h as _], pixmap.data())
        .with_source_size(source_size);

    Ok(image)
}
```

Note `resvg::usvg::Transform` — `usvg` re-exports `tiny_skia_path::Transform`
(`usvg-0.48.1/src/tree/geom.rs:6`), so `usvg::Transform` and `tiny_skia::Transform` are the
same type *within one version*.

### 5.5 `SizeHint`

```rust
// egui-0.36.0/src/load.rs:148-173
pub enum SizeHint {
    /// Scale original size by some factor, keeping the original aspect ratio.
    ///
    /// The original size of the image is usually its texel resolution,
    /// but for an SVG it's the point size of the SVG.
    Scale(OrderedFloat<f32>),

    /// Scale to exactly this pixel width, keeping the original aspect ratio.
    Width(u32),

    /// Scale to exactly this pixel height, keeping the original aspect ratio.
    Height(u32),

    /// Scale to this pixel size.
    Size {
        width: u32,
        height: u32,

        /// If true, the image will be as large as possible
        /// while still fitting within the given width/height.
        maintain_aspect_ratio: bool,
    },
}
```

`SizeHint::Size` gained `maintain_aspect_ratio` in this cycle — older code that wrote
`SizeHint::Size(w, h)` (tuple form) or `SizeHint::Size { width, height }` will not compile.

---

## 6. resvg / usvg 0.48.1 and tiny-skia 0.12.0

Crate dirs: `resvg-0.48.1/`, `usvg-0.48.1/`, `tiny-skia-0.12.0/`, `tiny-skia-path-0.12.0/`.

### 6.1 resvg features and re-exports

```toml
# resvg-0.48.1/Cargo.toml
[features]
default = ["svgz", "text", "system-fonts", "memmap-fonts", "raster-images"]
memmap-fonts = ["usvg/memmap-fonts"]
raster-images = ["gif", "image-webp", "dep:zune-jpeg"]
svgz = ["usvg/svgz"]
system-fonts = ["usvg/system-fonts"]
text = ["usvg/text"]
```

```toml
# resvg-0.48.1/Cargo.toml:103-108
[dependencies.tiny-skia]
version = "0.12.0"

[dependencies.usvg]
version = "0.48.1"
default-features = false
```

```rust
// resvg-0.48.1/src/lib.rs:17-18
pub use tiny_skia;
pub use usvg;
```

**Use `resvg::tiny_skia::…` and `resvg::usvg::…`** rather than the standalone crates, and you
can never get a version mismatch inside your own code. (This workspace also lists `usvg` and
`tiny-skia` directly at matching versions, so either path works here.)

For UI icons with no `<text>`, turning features off is a real win:

```toml
resvg = { version = "0.48.1", default-features = false, features = ["svgz"] }
```

That drops `fontdb`, `harfrust`, `skrifa`, the unicode tables, and the raster-image decoders.
`Options::fontdb_mut()` then does not exist (`#[cfg(feature = "text")]`,
`usvg-0.48.1/src/parser/options.rs:140-141`).

### 6.2 Rendering entry points, verbatim

```rust
// resvg-0.48.1/src/lib.rs:28-43
/// Renders a tree onto the pixmap.
///
/// `transform` will be used as a root transform.
/// Can be used to position SVG inside the `pixmap`.
///
/// The produced content is in the sRGB color space.
pub fn render(
    tree: &usvg::Tree,
    transform: tiny_skia::Transform,
    pixmap: &mut tiny_skia::PixmapMut,
) {
```

```rust
// resvg-0.48.1/src/lib.rs:45-59
/// Renders a node onto the pixmap.
///
/// The expected pixmap size can be retrieved from `usvg::Node::abs_layer_bounding_box()`.
///
/// Returns `None` when `node` has a zero size.
pub fn render_node(
    node: &usvg::Node,
    mut transform: tiny_skia::Transform,
    pixmap: &mut tiny_skia::PixmapMut,
) -> Option<()> {
```

Both are **free functions** and both take `&mut PixmapMut`, so the call is always
`&mut pixmap.as_mut()` when you own a `Pixmap`. `render` returns `()` — it cannot fail; a bad
transform simply paints nothing.

### 6.3 usvg parsing

```rust
// usvg-0.48.1/src/parser/mod.rs:101-105
impl crate::Tree {
    /// Parses `Tree` from an SVG data.
    ///
    /// Can contain an SVG string or a gzip compressed data.
    pub fn from_data(data: &[u8], opt: &Options) -> Result<Self, Error> {
```

```rust
// usvg-0.48.1/src/parser/mod.rs:125
    pub fn from_data_nested(data: &[u8], opt: &Options) -> Result<Self, Error>
// usvg-0.48.1/src/parser/mod.rs:159
    pub fn from_str(text: &str, opt: &Options) -> Result<Self, Error>
// usvg-0.48.1/src/parser/mod.rs:172
    pub fn from_xmltree(doc: &roxmltree::Document, opt: &Options) -> Result<Self, Error>
```

`from_data` sniffs the gzip magic `[0x1f, 0x8b]` and decompresses when the `svgz` feature is on;
without it you get `Error::SvgzFeatureNotEnabled` (`src/parser/mod.rs:106-118`).

```rust
// usvg-0.48.1/src/parser/mod.rs — Error variants, from the Display impl at :60-82
pub enum Error {
    NotAnUtf8Str,
    SvgzFeatureNotEnabled,
    MalformedGZip,
    ElementsLimitReached,
    InvalidSize,
    ParsingFailed(roxmltree::Error),
}
```

`Error: Display + std::error::Error` (`src/parser/mod.rs:60,85`).

**The `fontdb` argument is gone.** usvg ≤ 0.42 had
`Tree::from_data(data, opt, fontdb) `; in 0.48 the database lives on `Options`.

### 6.4 `usvg::Options`

```rust
// usvg-0.48.1/src/parser/options.rs:13-101 (doc comments elided; defaults from the Default impl)
pub struct Options<'a> {
    pub resources_dir: Option<std::path::PathBuf>,   // None
    pub dpi: f32,                                    // 96.0
    pub font_family: String,                         // "Times New Roman"
    pub font_size: f32,                              // 12.0
    pub languages: Vec<String>,                      // ["en"]
    pub shape_rendering: ShapeRendering,             // GeometricPrecision
    pub text_rendering: TextRendering,               // OptimizeLegibility
    pub image_rendering: ImageRendering,             // OptimizeQuality
    pub default_size: Size,                          // 100 x 100
    pub image_href_resolver: ImageHrefResolver<'a>,
    #[cfg(feature = "text")]
    pub font_resolver: FontResolver<'a>,
    #[cfg(feature = "text")]
    pub fontdb: Arc<fontdb::Database>,
    pub style_sheet: Option<String>,
}
```

```rust
// usvg-0.48.1/src/parser/options.rs:103-123
impl Default for Options<'_> {
    fn default() -> Options<'static> {
        Options {
            resources_dir: None,
            dpi: 96.0,
            font_family: "Times New Roman".to_owned(),
            font_size: 12.0,
            languages: vec!["en".to_string()],
            shape_rendering: ShapeRendering::default(),
            text_rendering: TextRendering::default(),
            image_rendering: ImageRendering::default(),
            default_size: Size::from_wh(100.0, 100.0).unwrap(),
            image_href_resolver: ImageHrefResolver::default(),
            #[cfg(feature = "text")]
            font_resolver: FontResolver::default(),
            #[cfg(feature = "text")]
            fontdb: Arc::new(fontdb::Database::new()),
            style_sheet: None,
        }
    }
}
```

```rust
// usvg-0.48.1/src/parser/options.rs:126-143
impl Options<'_> {
    /// Converts a relative path into absolute relative to the SVG file itself.
    pub fn get_abs_path(&self, rel_path: &std::path::Path) -> std::path::PathBuf

    /// Mutably acquires the database.
    ///
    /// This clones the database if it is currently shared.
    #[cfg(feature = "text")]
    pub fn fontdb_mut(&mut self) -> &mut fontdb::Database
}
```

Key facts:

* `Options::default()` ships an **empty `fontdb`** — no system fonts. Text renders blank until
  you call `fontdb_mut().load_system_fonts()`.
* `fontdb_mut()` is `Arc::make_mut`, so calling it after a `Tree` has borrowed the `Arc`
  deep-copies the whole database. Configure once, up front.
* `style_sheet: Option<String>` injects CSS into the document — a clean way to recolour
  an SVG at parse time (e.g. `Some(".fg{fill:#fff}".into())`) as an alternative to §8's
  pixel tint.
* `Options` is not `Clone` and not `Send`-obviously (it holds boxed resolver closures). Build it
  once in a `OnceLock` as §8 does.

### 6.5 `usvg::Tree`

```rust
// usvg-0.48.1/src/tree/mod.rs:1592-1604
pub struct Tree {
    pub(crate) size: Size,
    pub(crate) root: Group,
    ...
}
```

```rust
// usvg-0.48.1/src/tree/mod.rs:1606-1685
impl Tree {
    /// Image size.
    ///
    /// Size of an image that should be created to fit the SVG.
    ///
    /// `width` and `height` in SVG.
    pub fn size(&self) -> Size
    pub fn root(&self) -> &Group
    pub fn node_by_id(&self, id: &str) -> Option<&Node>
    pub fn has_text_nodes(&self) -> bool
    pub fn has_defs_nodes(&self) -> bool
    pub fn linear_gradients(&self) -> &[Arc<LinearGradient>]
    pub fn radial_gradients(&self) -> &[Arc<RadialGradient>]
    pub fn patterns(&self) -> &[Arc<Pattern>]
    pub fn clip_paths(&self) -> &[Arc<ClipPath>]
    pub fn masks(&self) -> &[Arc<Mask>]
    pub fn filters(&self) -> &[Arc<filter::Filter>]
    pub fn fontdb(&self) -> &Arc<fontdb::Database>     // feature "text"
}
```

`Tree::size()` is the **`width`/`height` box**, not the ink bounds. The doc warns
(`src/tree/mod.rs:1613-1616`): *"this does not necessarily represent the bounding box of the
rendered contents. Use `self.root().abs_layer_bounding_box()` to retrieve it instead."*
If your icon looks off-centre, that is why — trim with the group bbox, or fix the SVG's
`viewBox`.

`usvg` re-exports geometry straight from tiny-skia-path:

```rust
// usvg-0.48.1/src/tree/geom.rs:6
pub use tiny_skia_path::{NonZeroRect, Rect, Size, Transform};
// usvg-0.48.1/src/tree/mod.rs:13
pub use tiny_skia_path;
// usvg-0.48.1/src/lib.rs:61-74
pub use parser::*;
pub use text::*;
pub use tree::*;
pub use roxmltree;
pub use fontdb;                 // feature "text"
pub use writer::WriteOptions;   // feature "writer"
pub use xmlwriter::Indent;      // feature "writer"
```

### 6.6 `tiny_skia::Pixmap`

```rust
// tiny-skia-0.12.0/src/lib.rs:64,68-70
pub use pixmap::{Pixmap, PixmapMut, PixmapRef, BYTES_PER_PIXEL};
pub use tiny_skia_path::{IntRect, IntSize, NonZeroRect, Point, Rect, Size, Transform};
pub use tiny_skia_path::{LineCap, LineJoin, Stroke, StrokeDash};
pub use tiny_skia_path::{Path, PathBuilder, PathSegment, PathSegmentsIter, PathStroker};
```

```rust
// tiny-skia-0.12.0/src/pixmap.rs:36-43
    /// Allocates a new pixmap.
    ///
    /// A pixmap is filled with transparent black by default, aka (0, 0, 0, 0).
    ///
    /// Zero size in an error.
    ///
    /// Pixmap's width is limited by i32::MAX/4.
    pub fn new(width: u32, height: u32) -> Option<Self>
```

```rust
// tiny-skia-0.12.0/src/pixmap.rs
:62    pub fn from_vec(data: Vec<u8>, size: IntSize) -> Option<Self>
:186   pub fn as_ref(&self) -> PixmapRef<'_>
:194   pub fn as_mut(&mut self) -> PixmapMut<'_>
:203   pub fn width(&self) -> u32
:209   pub fn height(&self) -> u32
:220   pub fn fill(&mut self, color: Color)
:230   pub fn data(&self) -> &[u8]          // "Byteorder: RGBA"
:237   pub fn data_mut(&mut self) -> &mut [u8]
:244   pub fn pixel(&self, x: u32, y: u32) -> Option<PremultipliedColorU8>
:250   pub fn pixels_mut(&mut self) -> &mut [PremultipliedColorU8]
:255   pub fn pixels(&self) -> &[PremultipliedColorU8]
:262   pub fn take(self) -> Vec<u8>                    // "Byteorder: RGBA"
:269   pub fn take_demultiplied(mut self) -> Vec<u8>   // un-premultiplies for you
:285   pub fn clone_rect(&self, rect: IntRect) -> Option<Pixmap>
```

```rust
// tiny-skia-0.12.0/src/pixmap.rs:455-528
pub struct PixmapMut<'a> { .. }

impl<'a> PixmapMut<'a> {
    pub fn from_bytes(data: &'a mut [u8], width: u32, height: u32) -> Option<Self>
    pub fn to_owned(&self) -> Pixmap
    pub fn as_ref(&self) -> PixmapRef<'_>
    pub fn width(&self) -> u32
    pub fn height(&self) -> u32
    pub fn fill(&mut self, color: Color)
    pub fn data_mut(&mut self) -> &mut [u8]
    pub fn pixels_mut(&mut self) -> &mut [PremultipliedColorU8]
}
```

The element type is `PremultipliedColorU8` — the name is the documentation. `data()` /
`take()` say "Byteorder: RGBA", and the alpha is baked in. `take_demultiplied()` exists if you
need straight RGBA (e.g. for a PNG encoder), at the cost of a pass over the buffer.

`Pixmap::new` returns `None` for zero width/height **and** for widths above `i32::MAX / 4` —
always handle it rather than unwrapping, since a bad `SizeHint` can reach it.

### 6.7 `Transform`

```rust
// tiny-skia-path-0.12.0/src/transform.rs
:45    pub fn identity() -> Self
:52    pub fn from_row(sx: f32, ky: f32, kx: f32, sy: f32, tx: f32, ty: f32) -> Self
:64    pub fn from_translate(tx: f32, ty: f32) -> Self
:69    pub fn from_scale(sx: f32, sy: f32) -> Self
:74    pub fn from_skew(kx: f32, ky: f32) -> Self
:81    pub fn from_rotate(angle: f32) -> Self
:93    pub fn from_rotate_at(angle: f32, tx: f32, ty: f32) -> Self
:103   pub fn from_bbox(bbox: NonZeroRect) -> Self
:169   pub fn get_scale(&self) -> (f32, f32)
:177   pub fn pre_scale(&self, sx: f32, sy: f32) -> Self
:183   pub fn post_scale(&self, sx: f32, sy: f32) -> Self
:189   pub fn pre_translate(&self, tx: f32, ty: f32) -> Self
:195   pub fn post_translate(&self, tx: f32, ty: f32) -> Self
:203   pub fn pre_rotate(&self, angle: f32) -> Self
:233   pub fn pre_concat(&self, other: Self) -> Self
:239   pub fn post_concat(&self, other: Self) -> Self
```

Every combinator takes `&self` and **returns a new `Transform`** — nothing mutates in place.
`Transform::from_translate(tx, ty).pre_scale(s, s)` = "scale first, then translate", which is
the order you want for letterboxing an icon.

### 6.8 `Size`

```rust
// tiny-skia-path-0.12.0/src/size.rs
:139   pub fn from_wh(width: f32, height: f32) -> Option<Self>
:147   pub fn width(&self) -> f32
:152   pub fn height(&self) -> f32
:157   pub fn scale_to(&self, to: Self) -> Self
:162   pub fn expand_to(&self, to: Self) -> Self
:168   pub fn scale_by(&self, factor: f32) -> Option<Self>
:174   pub fn scale_to_width(&self, new_width: f32) -> Option<Self>
```

---

## 7. The egui side of the bridge (0.36.0 / epaint 0.36.2)

```rust
// epaint-0.36.2/src/image.rs:47-57
#[derive(Clone, Debug, PartialEq)]
pub struct ColorImage {
    /// width, height in texels.
    pub size: [usize; 2],

    /// Size of the original SVG image (if any), or just the texel size of the image.
    pub source_size: Vec2,

    /// The pixels, row by row, from top to bottom.
    pub pixels: Vec<Color32>,
}
```

```rust
// epaint-0.36.2/src/image.rs
:61    pub fn new(size: [usize; 2], pixels: Vec<Color32>) -> Self
:75    pub fn filled(size: [usize; 2], color: Color32) -> Self
:113   pub fn from_rgba_unmultiplied(size: [usize; 2], rgba: &[u8]) -> Self
:128   pub fn from_rgba_premultiplied(size: [usize; 2], rgba: &[u8]) -> Self
:146   pub fn from_gray(size: [usize; 2], gray: &[u8]) -> Self
:163   pub fn from_gray_iter(size: [usize; 2], gray_iter: impl Iterator<Item = u8>) -> Self
:177   pub fn as_raw(&self) -> &[u8]
:183   pub fn as_raw_mut(&mut self) -> &mut [u8]
:193   pub fn from_rgb(size: [usize; 2], rgb: &[u8]) -> Self
:227   pub fn with_source_size(mut self, source_size: Vec2) -> Self
:233   pub fn width(&self) -> usize
:238   pub fn height(&self) -> usize
:249   pub fn region(&self, region: &emath::Rect, pixels_per_point: Option<f32>) -> Self
:273   pub fn region_by_pixels(&self, [x, y]: [usize; 2], [w, h]: [usize; 2]) -> Self
```

Three changes from older egui:

1. **`source_size` is a new public field.** It carries the SVG's *point* size so egui can size an
   `Image` sensibly while the texture is at device resolution. `ColorImage::new` sets it to the
   texel size; `with_source_size` overrides it. Set it whenever you rasterise an SVG.
2. **`new(size, pixels: Vec<Color32>)`** — not `new(size, Color32)`. The fill constructor is
   `filled(size, color)`.
3. Both `from_rgba_*` constructors **assert** `size[0] * size[1] * 4 == rgba.len()`
   (`src/image.rs:114-120`, `:129-135`) — a size/stride mismatch is a panic, not a garbled image.

```rust
// egui-0.36.0/src/context.rs:2387-2392
    pub fn load_texture(
        &self,
        name: impl Into<String>,
        image: impl Into<ImageData>,
        options: TextureOptions,
    ) -> TextureHandle {
```

`TextureOptions` presets: `LINEAR`, `NEAREST`, `LINEAR_REPEAT`, `LINEAR_MIRRORED_REPEAT`,
`NEAREST_REPEAT`, `NEAREST_MIRRORED_REPEAT` (`epaint-0.36.2/src/textures.rs:183-223`).

```rust
// egui-0.36.0/src/lib.rs:442-451 (re-exports used below)
pub use ecolor::{Color32, Rgba};
pub use emath::{Align, Align2, NumExt, Pos2, Rangef, Rect, RectAlign, Vec2, Vec2b, lerp, ..};
pub use epaint::{
    ClippedPrimitive, ColorImage, CornerRadius, Direction, ImageData, Margin, Mesh, ..
    TextureHandle, TextureId, mutex,
    textures::{TextureFilter, TextureOptions, TextureWrapMode, TexturesDelta},
};
```

```rust
// ecolor-0.36.2/src/color32.rs
:108   pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self
:122   pub const fn from_rgba_premultiplied(r: u8, g: u8, b: u8, a: u8) -> Self
:133   pub fn from_rgba_unmultiplied(r: u8, g: u8, b: u8, a: u8) -> Self
:139   pub const fn from_rgba_unmultiplied_const(r: u8, g: u8, b: u8, a: u8) -> Self
:188   pub const fn r(&self) -> u8
:194   pub const fn g(&self) -> u8
:200   pub const fn b(&self) -> u8
:206   pub const fn a(&self) -> u8
:231   pub const fn to_array(&self) -> [u8; 4]
:237   pub const fn to_tuple(&self) -> (u8, u8, u8, u8)
```

`Color32` stores **premultiplied** components, so `to_tuple()` on a semi-transparent colour
returns premultiplied bytes — relevant to §8's tint maths.

---

## 8. The helper module

Drop this in as `crates/fxsound-ui/src/svg.rs` (add `pub mod svg;` to `lib.rs`).
It needs only what `fxsound-ui` already depends on: `egui`, `resvg` (which re-exports `usvg`
and `tiny_skia`).

**Verified**: this exact source compiled with `cargo check --offline -p fxsound-ui` and ran
successfully with its assertions intact, against egui 0.36.0 / resvg 0.48.1 / usvg 0.48.1 /
tiny-skia 0.12.0.

```rust
//! Load an SVG byte slice and rasterise it to an `egui::ColorImage` at a requested pixel size,
//! with an optional colour tint.
//!
//! Verified against resvg 0.48.1 / usvg 0.48.1 / tiny-skia 0.12.0 / egui 0.36.0.

use std::sync::OnceLock;

use egui::{Color32, ColorImage, TextureHandle, TextureOptions, Vec2};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

/// How the SVG's intrinsic size is mapped onto the requested pixel box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvgFit {
    /// Fill the box exactly; aspect ratio is not preserved.
    Stretch,
    /// Largest size that fits inside the box, aspect ratio preserved.
    Contain,
}

#[derive(Debug)]
pub enum SvgError {
    /// `usvg` could not parse the bytes.
    Parse(resvg::usvg::Error),
    /// Requested size was zero, or too large for a `Pixmap`.
    BadSize { width: u32, height: u32 },
    /// The SVG declared a zero width or height.
    ZeroSourceSize,
}

impl std::fmt::Display for SvgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "failed to parse SVG: {e}"),
            Self::BadSize { width, height } => {
                write!(f, "cannot allocate a {width}x{height} pixmap")
            }
            Self::ZeroSourceSize => f.write_str("SVG has a zero width or height"),
        }
    }
}

impl std::error::Error for SvgError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            _ => None,
        }
    }
}

/// Shared parse options. `Options` is cheap to build but carries a `fontdb`, so build it once.
///
/// With `resvg`'s default features the `text` feature is on and `load_system_fonts()` costs tens
/// of milliseconds; if the icons carry no `<text>`, leave it commented out, or build `resvg` with
/// `default-features = false` and the call will not even exist.
fn options() -> &'static Options<'static> {
    static OPTIONS: OnceLock<Options<'static>> = OnceLock::new();
    OPTIONS.get_or_init(|| {
        let mut opt = Options::default();
        opt.dpi = 96.0;
        // Only needed if your SVGs contain <text>; requires resvg/usvg feature "text".
        // opt.fontdb_mut().load_system_fonts();
        opt
    })
}

/// Rasterise `svg_bytes` into a `ColorImage` of at most `width` x `height` device pixels.
///
/// `tint` replaces every pixel's colour while keeping the SVG's own alpha, which is what you
/// want for monochrome UI and tray glyphs. Pass `None` to keep the SVG's own colours.
///
/// Pass *physical* pixels: multiply your logical size by `ctx.pixels_per_point()` before calling.
pub fn svg_to_color_image(
    svg_bytes: &[u8],
    width: u32,
    height: u32,
    fit: SvgFit,
    tint: Option<Color32>,
) -> Result<ColorImage, SvgError> {
    let tree = Tree::from_data(svg_bytes, options()).map_err(SvgError::Parse)?;

    let source = tree.size();
    let (sw, sh) = (source.width(), source.height());
    if !(sw > 0.0 && sh > 0.0) {
        return Err(SvgError::ZeroSourceSize);
    }

    // Decide the destination pixel box and the scale that maps the SVG into it.
    let (w, h, scale_x, scale_y) = match fit {
        SvgFit::Stretch => (width, height, width as f32 / sw, height as f32 / sh),
        SvgFit::Contain => {
            let scale = (width as f32 / sw).min(height as f32 / sh);
            let w = (sw * scale).round().max(1.0) as u32;
            let h = (sh * scale).round().max(1.0) as u32;
            // Recompute from the rounded box so the drawing fills it exactly.
            (w, h, w as f32 / sw, h as f32 / sh)
        }
    };

    // `Pixmap::new` returns `None` for a zero size or a width above i32::MAX / 4.
    let mut pixmap = Pixmap::new(w, h).ok_or(SvgError::BadSize {
        width: w,
        height: h,
    })?;

    resvg::render(
        &tree,
        Transform::from_scale(scale_x, scale_y),
        &mut pixmap.as_mut(),
    );

    // `Pixmap::data()` is RGBA8, *premultiplied*, row-major, top-down — exactly what
    // `ColorImage::from_rgba_premultiplied` expects. Do not use `from_rgba_unmultiplied` here.
    if let Some(tint) = tint {
        // Premultiplied recolour: (r,g,b) := tint.rgb * a/255, alpha untouched.
        let (tr, tg, tb, ta) = tint.to_tuple();
        for px in pixmap.data_mut().chunks_exact_mut(4) {
            let a = px[3] as u32;
            let a = a * ta as u32 / 255; // honour the tint's own alpha too
            px[0] = (tr as u32 * a / 255) as u8;
            px[1] = (tg as u32 * a / 255) as u8;
            px[2] = (tb as u32 * a / 255) as u8;
            px[3] = a as u8;
        }
    }

    Ok(
        ColorImage::from_rgba_premultiplied([w as usize, h as usize], pixmap.data())
            // Tells egui the SVG's point size, so `Image::new` can size itself sensibly.
            .with_source_size(Vec2::new(sw, sh)),
    )
}

/// Convenience: rasterise and upload in one step.
///
/// Call this once (e.g. from `App::new`) and keep the `TextureHandle`; calling it every frame
/// re-parses and re-uploads the SVG.
pub fn svg_to_texture(
    ctx: &egui::Context,
    name: impl Into<String>,
    svg_bytes: &[u8],
    size_points: f32,
    tint: Option<Color32>,
) -> Result<TextureHandle, SvgError> {
    let px = (size_points * ctx.pixels_per_point()).round().max(1.0) as u32;
    let image = svg_to_color_image(svg_bytes, px, px, SvgFit::Contain, tint)?;
    Ok(ctx.load_texture(name, image, TextureOptions::LINEAR))
}

/// Raw premultiplied RGBA8 bytes at `size` x `size`, centred, for callers that do not want a
/// `ColorImage` — e.g. `ksni::Icon` (rotate each 4-byte chunk right by 1 to get ARGB).
pub fn svg_to_rgba_premultiplied(svg_bytes: &[u8], size: u32) -> Result<Vec<u8>, SvgError> {
    let tree = Tree::from_data(svg_bytes, options()).map_err(SvgError::Parse)?;
    let s = tree.size();
    if !(s.width() > 0.0 && s.height() > 0.0) {
        return Err(SvgError::ZeroSourceSize);
    }
    let mut pixmap = Pixmap::new(size, size).ok_or(SvgError::BadSize {
        width: size,
        height: size,
    })?;
    let scale = (size as f32 / s.width()).min(size as f32 / s.height());
    let tx = (size as f32 - s.width() * scale) / 2.0;
    let ty = (size as f32 - s.height() * scale) / 2.0;
    resvg::render(
        &tree,
        Transform::from_translate(tx, ty).pre_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24"><circle cx="12" cy="12" r="10" fill="#3b82f6"/></svg>"##;

    #[test]
    fn rasterises_and_keeps_source_size() {
        let img = svg_to_color_image(SVG, 64, 64, SvgFit::Contain, None).unwrap();
        assert_eq!(img.size, [64, 64]);
        assert_eq!(img.source_size, Vec2::new(24.0, 24.0));
    }

    #[test]
    fn tint_and_stretch() {
        let img = svg_to_color_image(SVG, 32, 16, SvgFit::Stretch, Some(Color32::WHITE)).unwrap();
        assert_eq!(img.size, [32, 16]);
    }

    #[test]
    fn raw_bytes_are_the_right_length() {
        let raw = svg_to_rgba_premultiplied(SVG, 22).unwrap();
        assert_eq!(raw.len(), 22 * 22 * 4);
    }
}
```

### 8.1 Using it

```rust
// Once, at startup — not per frame.
struct Icons { power: egui::TextureHandle }

impl Icons {
    fn load(ctx: &egui::Context, accent: egui::Color32) -> Self {
        const POWER: &[u8] = include_bytes!("../../../assets/power.svg");
        Self {
            power: crate::svg::svg_to_texture(ctx, "icon:power", POWER, 24.0, Some(accent))
                .expect("bundled asset is valid SVG"),
        }
    }
}

// Per frame — cheap; this is just a texture id.
fn draw(ui: &mut egui::Ui, icons: &Icons) {
    ui.add(egui::Image::new(&icons.power).fit_to_exact_size(egui::vec2(24.0, 24.0)));
}
```

### 8.2 Things to get right

* **Rasterise at device pixels, display at points.** `svg_to_texture` multiplies by
  `ctx.pixels_per_point()`. If the user moves the window to a different-DPI monitor,
  `pixels_per_point` changes and the icon goes soft — re-load on change, or rasterise at 2×
  and let `Image::fit_to_exact_size` downsample.
* **Do not call this in `App::ui`.** Parsing plus rasterising a 24 px icon is on the order of
  a millisecond, and `load_texture` uploads to the GPU. Cache the `TextureHandle`.
* **Premultiplied everywhere.** `Pixmap` is premultiplied, `Color32` is premultiplied, and
  `from_rgba_premultiplied` is the matching constructor. Using `from_rgba_unmultiplied` here
  double-applies alpha and gives you dark halos on anti-aliased edges.
* **The tint is a recolour, not a multiply.** It discards the SVG's own hues and keeps only the
  coverage. For a multi-colour SVG that you want *darkened* rather than flattened, multiply all
  four premultiplied channels by the tint instead — that stays valid premultiplied data.
* **Alternative: tint at parse time.** `Options::style_sheet` injects CSS
  (`usvg-0.48.1/src/parser/options.rs:98-100`), so `Some("path{fill:#fff}".into())` recolours
  without a pixel pass — at the cost of needing a distinct `Options` (and hence no shared
  `OnceLock`) per colour.
* **`SvgFit::Contain` returns a `ColorImage` smaller than the box you asked for** when the
  aspect ratios differ. That is deliberate: no wasted transparent margin in the texture.
  Use `Stretch` if you need the exact dimensions.
