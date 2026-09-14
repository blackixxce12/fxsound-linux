# eframe 0.36.0 — verified API cheatsheet

Every signature below was read verbatim out of the vendored source at
`/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/eframe-0.36.0/`.
Citations are `<file>:<line>` relative to that directory.

**Do not trust anything you remember about eframe 0.2x–0.3x.** This version changed the
central trait method, deleted `run_simple_native`, moved `vsync`/`hardware_acceleration`
off `NativeOptions`, and defaults to the **wgpu** renderer.

---

## 0. Read this first: the 10 things that break a build

| You probably remember | eframe 0.36 reality | Cite |
|---|---|---|
| `eframe::run_simple_native(...)` | **Gone.** It is `run_ui_native` (and the callback takes `&mut Ui`, not `&Context`). | grep: zero hits in `src/` |
| `fn update(&mut self, ctx: &egui::Context, frame: &mut Frame)` | **Gone.** The required method is `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)`. | `src/epi.rs:182` |
| `CentralPanel::default().show(ctx, ...)` | Takes a **`&mut Ui`**: `show(self, ui: &mut Ui, ...)`. `show_inside` is the deprecated alias now. | `egui-0.36.0/src/containers/panel.rs:1212`, `:1217` |
| `egui::TopBottomPanel` / `egui::SidePanel` | **Both types are gone** (zero hits in egui 0.36). One `Panel` type with `Panel::top(id)` / `bottom` / `left` / `right`, each taking a globally-unique `Id`. `Panel::show(self, ui: &mut Ui, ..)`. | `egui-0.36.0/src/containers/panel.rs:206,249,256,265,274,422` |
| `NativeOptions { vsync: true, .. }` | **No `vsync` field.** glow: `glow_options.vsync`; wgpu: `wgpu_options.surface.present_mode`. | `src/epi.rs:296-403` |
| `NativeOptions { hardware_acceleration: .. }` | **No such field.** It is `glow_options.hardware_acceleration` (`egui_glow::HardwareAcceleration`). | `src/native/glow_integration.rs:1054` |
| `NativeOptions { follow_system_theme, default_theme }` | **Both gone.** Use `egui::Context::set_theme(ThemePreference)`. | grep: zero hits in `src/` |
| glow is the default renderer | **wgpu is**, whenever the `wgpu` feature is on (it is on by `default`). | `src/epi.rs:599-621` |
| `epaint::Rounding` | Renamed **`CornerRadius`**; no `Rounding` type exists. | `epaint-0.36.2/src/corner_radius.rs:13` |
| `ctx.set_style(..)` / `ctx.set_visuals(..)` only | `set_global_style`, `set_style_of(theme, ..)`, `set_visuals_of(theme, ..)`, `set_theme(..)` all exist. | `egui-0.36.0/src/context.rs:2197,2247,2264,2167` |
| `eframe::winit::...` | eframe re-exports **only two** winit items. Add `winit = "0.30"` yourself for `create_native`. | `src/epi.rs:26` |

Toolchain floor: `edition = "2024"`, `rust-version = "1.95"` (`Cargo.toml`).

---

## 1. This workspace's feature set — what actually compiles here

`fxsound-linux/Cargo.toml` pins:

```toml
eframe = { version = "=0.36.0", default-features = false, features = ["default_fonts", "glow", "wayland", "x11"] }
```

That is **not** the default feature set. Concrete consequences, all verified:

* **`Renderer` has exactly one variant, `Renderer::Glow`.** `Renderer::Wgpu` is `#[cfg(feature = "wgpu_no_default_features")]` and will not compile (`src/epi.rs:588-596`). `Renderer::default()` returns `Self::Glow` (`src/epi.rs:607-609`).
* **`NativeOptions` has `glow_options`, has NO `wgpu_options`** (`src/epi.rs:370-375`).
* **`CreationContext` has `gl` + `get_proc_address`, has NO `wgpu_render_state`** (`src/epi.rs:70-84`).
* **`Frame` has `gl()` / `register_native_glow_texture()`; the three `wgpu_*` methods do not exist** (`src/epi.rs:783-827`).
* **`App::on_exit` is the one-argument glow form**: `fn on_exit(&mut self, _gl: Option<&glow::Context>)` (`src/epi.rs:222`). The zero-arg form at `:228` is `#[cfg(not(feature = "glow"))]`.
* **`persistence` is OFF.** Therefore:
  * `eframe::storage_dir` is **not exported** (`src/lib.rs:206-207` gates it on the feature).
  * `eframe::get_value` / `eframe::set_value` are **not available** (`#[cfg(feature = "ron")]`, `src/epi.rs:959,974`; `ron` is only pulled in by `persistence`).
  * `App::save` still exists on the trait but **is never called** — the whole save path is `#[cfg(feature = "persistence")]` (`src/native/epi_integration.rs:407`).
  * `CreationContext::storage` and `Frame::storage()` will always be `None` (`src/native/epi_integration.rs:130-136` returns `None` without the feature).
* **`accesskit` is OFF** → `UserEvent` has only the `RequestRepaint` variant (`src/native/winit_integration.rs:64-80`); no screen-reader/AT-SPI support.
* **`links` is OFF** → `egui::Hyperlink` / `ui.hyperlink` will not open a browser (`egui-winit` `links` feature gates `webbrowser`).
* **`winit/default` is OFF.** eframe's `default` feature is what pulls it in (`Cargo.toml` `default = [... "winit/default" ...]`). winit 0.30.13's own defaults are `["rwh_06", "x11", "wayland", "wayland-dlopen", "wayland-csd-adwaita"]` (`winit-0.30.13/Cargo.toml`). The `x11` / `wayland` eframe features re-enable `winit/x11` and `winit/wayland`, but **`wayland-dlopen` and `wayland-csd-adwaita` stay off**. Practical effect on Linux: libwayland is link-time rather than dlopen'd, and there are **no client-side decorations** on compositors that don't draw server-side ones (GNOME/Mutter). If the window shows up with no titlebar, that's this, not a bug in your code. Fix by adding `winit = { version = "0.30.13", features = ["wayland-csd-adwaita", "wayland-dlopen"] }` to the workspace (feature unification does the rest).
* Clipboard **is** on regardless: eframe hard-codes `egui-winit` with `features = ["clipboard"]` (`Cargo.toml`).

---

## 2. Crate-level re-exports

```rust
pub use {egui, egui::emath, egui::epaint};                                         // src/lib.rs:156

#[cfg(feature = "glow")]
pub use {egui_glow, glow};                                                         // src/lib.rs:158-159

#[cfg(feature = "wgpu_no_default_features")]
pub use {egui_wgpu, egui_wgpu::SurfaceConfig, egui_wgpu::WgpuConfiguration, egui_wgpu::wgpu}; // src/lib.rs:161-162

pub use epi::*;                                                                    // src/lib.rs:167

#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub use native::run::EframeWinitApplication;                                       // src/lib.rs:196-198

#[cfg(not(any(target_arch = "wasm32", target_os = "ios")))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub use native::run::EframePumpStatus;                                             // src/lib.rs:200-202

#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
#[cfg(feature = "persistence")]
pub use native::file_storage::storage_dir;                                         // src/lib.rs:204-207

#[cfg(not(target_arch = "wasm32"))]
pub mod icon_data;                                                                 // src/lib.rs:209-210

#[cfg(target_os = "macos")]
pub use native::macos::WindowChromeMetrics;                                        // src/lib.rs:193-194
```

From `epi` (all reachable as `eframe::*` via the glob at `src/lib.rs:167`):

```rust
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub use crate::native::winit_integration::UserEvent;                               // src/epi.rs:12-14

#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub use winit::{event_loop::EventLoopBuilder, window::WindowAttributes};           // src/epi.rs:24-26
```

**Gotcha:** `mod native;` is private (`src/lib.rs:191`) and `winit_integration` is `pub(crate)`
(`src/native/mod.rs:13`). So `eframe::native::run::run_glow`, `eframe::WinitApp` and
`eframe::EventResult` **do not exist** — only the four `pub use` items above escape.

---

## 3. Entry points

```rust
// src/lib.rs:285-294
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
#[allow(clippy::allow_attributes, clippy::needless_pass_by_value)]
pub fn run_native(
    app_name: &str,
    native_options: NativeOptions,
    app_creator: AppCreator<'_>,
) -> Result
```

```rust
// src/lib.rs:303-311
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
#[allow(clippy::allow_attributes, clippy::needless_pass_by_value)]
pub fn run_native_ext(
    app_name: &str,
    mut native_options: NativeOptions,
    egui_ctx: Option<egui::Context>,
    app_creator: AppCreator<'_>,
) -> Result
```

`run_native` is literally `run_native_ext(app_name, native_options, None, app_creator)`
(`src/lib.rs:293`). Pass `Some(ctx)` to `run_native_ext` to reuse a `Context` you built
earlier (e.g. one you already installed fonts/plugins on) instead of letting eframe make one.

```rust
// src/lib.rs:476-482
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub fn run_ui_native(
    app_name: &str,
    native_options: NativeOptions,
    ui_fun: impl FnMut(&mut egui::Ui, &mut Frame) + 'static,
) -> Result
```

This is the replacement for the old `run_simple_native`. Note the closure receives
**`&mut egui::Ui`**, not `&egui::Context`. Internally it wraps your closure in a private
`SimpleApp<U>` whose `ui` just forwards (`src/lib.rs:483-497`). Doc comment: "This does NOT
support persistence of custom user data" — egui memory still persists if the feature is on
(`src/lib.rs:443-445`).

```rust
// src/lib.rs:374-381
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub fn create_native<'a>(
    app_name: &str,
    mut native_options: NativeOptions,
    app_creator: AppCreator<'a>,
    event_loop: &winit::event_loop::EventLoop<UserEvent>,
) -> EframeWinitApplication<'a>
```

**Gotcha:** the fourth parameter's type is spelled with a `winit::` path that eframe does
**not** re-export. To call `create_native` you must add `winit = "0.30.13"` (matching
version — see §8) to your own `Cargo.toml`.

### What `run_native` does before dispatching

```rust
// src/lib.rs:411-437
fn init_native(app_name: &str, native_options: &mut NativeOptions) -> Renderer {
    #[cfg(not(feature = "__screenshot"))]
    assert!(
        std::env::var("EFRAME_SCREENSHOT_TO").is_err(),
        "EFRAME_SCREENSHOT_TO found without compiling with the '__screenshot' feature"
    );

    if native_options.viewport.title.is_none() {
        native_options.viewport.title = Some(app_name.to_owned());
    }
    if native_options.viewport.app_id.is_none() {
        native_options.viewport.app_id = Some(app_name.to_owned());
    }
    // ...
    renderer
}
```

Two consequences worth knowing: the `app_name` argument becomes both the window **title**
and the **Wayland app id** unless you set them on `viewport`; and having `EFRAME_SCREENSHOT_TO`
set in the environment **panics** a normally-compiled binary.

### Error / Result

```rust
// src/lib.rs:503-531
#[derive(Debug)]
pub enum Error {
    AppCreation(Box<dyn std::error::Error + Send + Sync>),
    #[cfg(not(target_arch = "wasm32"))] Winit(winit::error::OsError),
    #[cfg(not(target_arch = "wasm32"))] WinitEventLoop(winit::error::EventLoopError),
    #[cfg(all(feature = "glow", not(target_arch = "wasm32")))] Glutin(glutin::error::Error),
    #[cfg(all(feature = "glow", not(target_arch = "wasm32")))]
    NoGlutinConfigs(glutin::config::ConfigTemplate, Box<dyn std::error::Error>),
    #[cfg(feature = "glow")] OpenGL(egui_glow::PainterError),
    #[cfg(feature = "wgpu_no_default_features")] Wgpu(egui_wgpu::WgpuError),
}

// src/lib.rs:616-617
pub type Result<T = (), E = Error> = std::result::Result<T, E>;
```

`Error` is **not** `std::error::Error + Send + Sync` in a useful sense for `anyhow` —
it implements `std::error::Error` (`src/lib.rs:533`) and `Display` (`:575`), but
`NoGlutinConfigs` holds a non-`Send` `Box<dyn std::error::Error>`. Convert at the boundary
(`.map_err(|e| anyhow::anyhow!("{e}"))`) rather than `?`-ing it into an `anyhow::Result`.

`fn main() -> eframe::Result` works because `T` defaults to `()`.

---

## 4. `AppCreator` and `CreationContext`

```rust
// src/epi.rs:44
type DynError = Box<dyn std::error::Error + Send + Sync>;

// src/epi.rs:49-50
pub type AppCreator<'app> =
    Box<dyn 'app + FnOnce(&CreationContext<'_>) -> Result<Box<dyn 'app + App>, DynError>>;
```

**Gotcha:** the closure returns a `Result`. `Box::new(|cc| Box::new(MyApp::new(cc)))` (the
pre-0.25 shape) will not compile; you need `Ok(Box::new(...))`. Errors here surface as
`Error::AppCreation` (`src/native/glow_integration.rs:366`).

```rust
// src/epi.rs:53-97
pub struct CreationContext<'s> {
    pub egui_ctx: egui::Context,
    pub integration_info: IntegrationInfo,
    pub storage: Option<&'s dyn Storage>,

    #[cfg(feature = "glow")]
    pub gl: Option<std::sync::Arc<glow::Context>>,

    #[cfg(feature = "glow")]
    pub get_proc_address:
        Option<std::sync::Arc<dyn Fn(&std::ffi::CStr) -> *const std::ffi::c_void + Send + Sync>>,

    #[cfg(feature = "wgpu_no_default_features")]
    pub wgpu_render_state: Option<egui_wgpu::RenderState>,

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) window: Option<std::sync::Arc<winit::window::Window>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) raw_window_handle: Result<RawWindowHandle, HandleError>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) raw_display_handle: Result<RawDisplayHandle, HandleError>,
}
```

The last three are `pub(crate)`; reach them through these instead:

```rust
// src/epi.rs:143-146
#[cfg(not(target_arch = "wasm32"))]
pub fn winit_window(&self) -> Option<&std::sync::Arc<winit::window::Window>>

// src/epi.rs:102 (impl HasWindowHandle for CreationContext<'_>)
fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError>

// src/epi.rs:111 (impl HasDisplayHandle for CreationContext<'_>)
fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError>

// src/epi.rs:119-120  — testing only
#[doc(hidden)]
pub fn _new_kittest(egui_ctx: egui::Context) -> Self
```

`get_proc_address` is new-ish and glow-only; it wraps `gl_config.display().get_proc_address`
(`src/native/glow_integration.rs:351`). Use it to load GL extension entry points yourself.

---

## 5. The `App` trait — every method

```rust
// src/epi.rs:152
pub trait App {
```

```rust
// src/epi.rs:167-169   DEFAULT: no-op
fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame) {
    _ = (ctx, frame);
}
```
Called **once before each `ui`**, and *also* while the window is hidden/minimized/occluded,
when no egui pass runs at all. In that state eframe calls `egui::Context::run_logic` instead
of `run_ui` (`src/native/epi_integration.rs:332-335`), so all your UI state is left exactly
as it was. You may **not** paint or show UI here. Use it for "keep ticking, e.g. so I can
ask to be shown again".

```rust
// src/epi.rs:182   REQUIRED — no default
fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);
```
The one method you must implement. The `Ui` handed to you has **no margin and no background
colour** — wrap in `egui::CentralPanel` or `egui::Frame::central_panel` (`src/epi.rs:173-174`).
Called for the root viewport only (`ViewportId::ROOT`); spawn extra OS windows with
`egui::Context::show_viewport_deferred`.

```rust
// src/epi.rs:199-202   DEFAULT: None   (wasm only)
#[cfg(target_arch = "wasm32")]
fn as_any_mut(&mut self) -> Option<&mut dyn Any> { None }
```

```rust
// src/epi.rs:212   DEFAULT: no-op
fn save(&mut self, _storage: &mut dyn Storage) {}
```
Only called when the `persistence` feature is enabled. Called on shutdown and on the
auto-save timer.

```rust
// src/epi.rs:221-222   DEFAULT: no-op   (glow build)
#[cfg(feature = "glow")]
fn on_exit(&mut self, _gl: Option<&glow::Context>) {}

// src/epi.rs:227-228   DEFAULT: no-op   (non-glow build)
#[cfg(not(feature = "glow"))]
fn on_exit(&mut self) {}
```
**Gotcha:** the arity of `on_exit` depends on the `glow` *feature*, not on which renderer you
actually run. A wgpu-backed app compiled with `glow` in the feature set still gets the
one-argument form, called with `None` (`src/native/wgpu_integration.rs:587-591`). Called
once on shutdown, **after** `save` (`src/native/glow_integration.rs:447-451`).

```rust
// src/epi.rs:234-236   DEFAULT: 30 s
fn auto_save_interval(&self) -> std::time::Duration {
    std::time::Duration::from_secs(30)
}
```

```rust
// src/epi.rs:248-255   DEFAULT: rgba(12,12,12,180) in gamma space
fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
    egui::Color32::from_rgba_unmultiplied(12, 12, 12, 180).to_normalized_gamma_f32()
}
```
Note the default is **semi-transparent** (alpha 180) so `viewport.with_transparent(true)`
gives an immediate visible effect. If you want an opaque window, override this. Values go to
the renderer as-is — keep them in sRGB gamma space; do **not** round-trip through
`egui::Rgba`.

```rust
// src/epi.rs:259-261   DEFAULT: true
fn persist_egui_memory(&self) -> bool { true }
```

```rust
// src/epi.rs:279   DEFAULT: no-op
fn raw_input_hook(&mut self, _ctx: &egui::Context, _raw_input: &mut egui::RawInput) {}
```
Mutate `_raw_input` in place to filter or inject events (virtual keyboard, shortcut
swallowing). Runs before every pass, including logic-only passes
(`src/native/epi_integration.rs:352-365`).

That is the complete method list. **There is no `update`, no `on_close_event`, no
`warm_up_enabled`, no `max_size_points`, no `post_rendering`** on the public trait in 0.36.

---

## 6. `Frame` — every public method

```rust
// src/epi.rs:661-693
pub struct Frame {
    pub(crate) info: IntegrationInfo,
    pub(crate) storage: Option<Box<dyn Storage>>,
    #[cfg(feature = "glow")]
    pub(crate) gl: Option<std::sync::Arc<glow::Context>>,
    #[cfg(all(feature = "glow", not(target_arch = "wasm32")))]
    pub(crate) glow_register_native_texture: Option<Box<dyn FnMut(glow::Texture) -> egui::TextureId>>,
    #[cfg(feature = "wgpu_no_default_features")]
    #[doc(hidden)]
    pub wgpu_render_state: Option<egui_wgpu::RenderState>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) window: Option<std::sync::Arc<winit::window::Window>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) raw_window_handle: Result<RawWindowHandle, HandleError>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) raw_display_handle: Result<RawDisplayHandle, HandleError>,
}
```

`Frame` deliberately does **not** implement `Clone` — there is a compile-time assertion
`assert_not_impl_any!(Frame: Clone);` (`src/epi.rs:697`) because cloning would break the
`HasWindowHandle`/`HasDisplayHandle` safety contract.

```rust
#[doc(hidden)] pub fn _new_kittest() -> Self                                        // src/epi.rs:719-720
pub fn is_web(&self) -> bool                                                        // src/epi.rs:743
pub fn info(&self) -> &IntegrationInfo                                              // src/epi.rs:748
pub fn storage(&self) -> Option<&dyn Storage>                                       // src/epi.rs:753
pub fn storage_mut(&mut self) -> Option<&mut (dyn Storage + 'static)>               // src/epi.rs:758

#[cfg(not(target_arch = "wasm32"))]
pub fn winit_window(&self) -> Option<&std::sync::Arc<winit::window::Window>>        // src/epi.rs:765-766

#[cfg(feature = "glow")]
pub fn gl(&self) -> Option<&std::sync::Arc<glow::Context>>                          // src/epi.rs:782-783

#[cfg(all(feature = "glow", not(target_arch = "wasm32")))]
pub fn register_native_glow_texture(&mut self, native: glow::Texture) -> egui::TextureId  // src/epi.rs:791-792

#[cfg(feature = "wgpu_no_default_features")]
pub fn wgpu_render_state(&self) -> Option<&egui_wgpu::RenderState>                  // src/epi.rs:802-803

#[cfg(feature = "wgpu_no_default_features")]
pub fn wgpu_surface_config(&self) -> Option<egui_wgpu::SurfaceConfig>               // src/epi.rs:811-812

#[cfg(feature = "wgpu_no_default_features")]
pub fn set_wgpu_surface_config(&mut self, config: egui_wgpu::SurfaceConfig)         // src/epi.rs:822-823
```

Trait impls:

```rust
// src/epi.rs:702 (HasWindowHandle for Frame)
fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError>
// src/epi.rs:711 (HasDisplayHandle for Frame)
fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError>
```

**Gotcha:** `register_native_glow_texture` does `.unwrap()` on the internal callback
(`src/epi.rs:793-794`). It panics if you call it on a `Frame` that wasn't built by the glow
backend (e.g. `Frame::_new_kittest()`, or a wgpu run in a glow-featured build).

`wgpu_surface_config` / `set_wgpu_surface_config` are **new in this line** — runtime
present-mode / frame-latency switching without rebuilding the renderer. The surface is
reconfigured on the next paint (`src/epi.rs:818-827`).

---

## 7. `NativeOptions` — every field, its type, and its default

```rust
// src/epi.rs:295-403
#[cfg(not(target_arch = "wasm32"))]
pub struct NativeOptions { /* fields below */ }
```

| Field | Type | Default | `cfg` gate | Cite (decl / default) |
|---|---|---|---|---|
| `viewport` | `egui::ViewportBuilder` | `Default::default()` | — | `:303` / `:437` |
| `multisampling` | `u16` | `0` | — | `:314` / `:439` |
| `depth_buffer` | `u8` | `0` | — | `:319` / `:440` |
| `stencil_buffer` | `u8` | `0` | — | `:324` / `:441` |
| `renderer` | `Renderer` | `Renderer::default()` (= `Wgpu` if wgpu on, else `Glow`) | `any(glow, wgpu_no_default_features)` | `:327-328` / `:443-444` |
| `run_and_return` | `bool` | `true` | — | `:342` / `:446` |
| `event_loop_builder` | `Option<EventLoopBuilderHook>` | `None` | `any(glow, wgpu_no_default_features)` | `:350-351` / `:448-449` |
| `window_builder` | `Option<WindowBuilderHook>` | `None` | `any(glow, wgpu_no_default_features)` | `:359-360` / `:451-452` |
| `centered` | `bool` | `false` | — | `:367` / `:454` |
| `glow_options` | `egui_glow::GlowConfiguration` | `GlowConfiguration::default()` | `glow` | `:370-371` / `:456-457` |
| `wgpu_options` | `egui_wgpu::WgpuConfiguration` | `WgpuConfiguration::default().with_surface_config(SurfaceConfig::LOW_LATENCY)` | `wgpu_no_default_features` | `:374-375` / `:459-461` |
| `persist_window` | `bool` | `true` | — | `:379` / `:463` |
| `persistence_path` | `Option<std::path::PathBuf>` | `None` | — | `:383` / `:465` |
| `dithering` | `bool` | `true` | — | `:392` / `:467` |
| `android_app` | `Option<winit::platform::android::activity::AndroidApp>` | `None` | `target_os = "android"` | `:401-402` / `:469-470` |

That is the **complete** field list — 14 fields on Linux with both renderers, 13 with only
glow. `NativeOptions` derives neither `Default` nor `Clone`; both are hand-written
(`src/epi.rs:406-431`, `:434-473`).

**Clone gotcha:** `NativeOptions::clone()` silently drops the two hook fields:

```rust
// src/epi.rs:411-415
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
event_loop_builder: None, // Skip any builder callbacks if cloning
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
window_builder: None,     // Skip any builder callbacks if cloning
```

Both hooks are also `std::mem::take`n on first use, so they fire exactly once
(`src/native/run.rs:44-46`, `src/native/epi_integration.rs:86-89`).

Hook type aliases:

```rust
// src/epi.rs:32-34
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub type EventLoopBuilderHook = Box<dyn FnOnce(&mut EventLoopBuilder<UserEvent>)>;

// src/epi.rs:40-42
#[cfg(not(target_arch = "wasm32"))]
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
pub type WindowBuilderHook = Box<dyn FnOnce(egui::ViewportBuilder) -> egui::ViewportBuilder>;
```

Where the numeric fields land:

* glow: `depth_buffer` → `.with_depth_size()`, `stencil_buffer` → `.with_stencil_size()`,
  `multisampling` → `.with_multisampling()` only when `> 0` (`src/native/glow_integration.rs:1073-1081`);
  `dithering` → 4th arg of `egui_glow::Painter::new` (`:232-237`).
* wgpu: `msaa_samples: multisampling as _`, depth/stencil, `dithering`
  (`src/native/wgpu_integration.rs:229-234`).

Supporting types you'll set through `NativeOptions`:

```rust
// egui_glow-0.36.0/src/lib.rs:117-135
pub struct GlowConfiguration {
    #[cfg(not(target_arch = "wasm32"))] pub vsync: bool,                       // default true
    #[cfg(not(target_arch = "wasm32"))] pub hardware_acceleration: HardwareAcceleration, // default Preferred
    pub shader_version: Option<ShaderVersion>,
}
// variants: HardwareAcceleration::{Required, Preferred, Off}  — glow_integration.rs:1055-1057

// egui-wgpu-0.36.0/src/lib.rs:71-83
pub struct SurfaceConfig {
    pub present_mode: wgpu::PresentMode,
    pub desired_maximum_frame_latency: Option<u32>,
}
pub const LOW_LATENCY: Self      = { AutoVsync, Some(1) /* None on iOS */ };   // :87-95
pub const HIGH_THROUGHPUT: Self  = { AutoVsync, Some(2) };                     // :99-102

// egui-wgpu-0.36.0/src/lib.rs:334-353
pub struct WgpuConfiguration {
    pub surface: SurfaceConfig,
    pub wgpu_setup: WgpuSetup,
    pub on_surface_status: Arc<dyn Fn(&wgpu::CurrentSurfaceTexture) -> SurfaceErrorAction + Send + Sync>,
}
pub fn with_surface_config(mut self, surface_config: SurfaceConfig) -> Self     // :377
```

Note eframe's `NativeOptions::default()` overrides egui-wgpu's own default with
`SurfaceConfig::LOW_LATENCY` — i.e. `desired_maximum_frame_latency: Some(1)`.

---

## 8. Renderer selection, and the glow vs wgpu features

```rust
// src/epi.rs:584-596
#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Renderer {
    #[cfg(feature = "glow")]
    Glow,
    #[cfg(feature = "wgpu_no_default_features")]
    Wgpu,
}
```

```rust
// src/epi.rs:599-621 — Default
//   glow only                      -> Renderer::Glow
//   wgpu only                      -> Renderer::Wgpu
//   BOTH                           -> Renderer::Wgpu   ("let's pick the better of the two")
//   NEITHER                        -> compile_error!("eframe: you must enable at least one
//                                     of the rendering backend features: 'glow' or 'wgpu'")
```

```rust
// src/epi.rs:624   impl Display  -> "glow" / "wgpu"
fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result

// src/epi.rs:637-640   impl FromStr, Err = String; matches on name.to_lowercase()
fn from_str(name: &str) -> Result<Self, String>
```

Selection is pure dispatch on `native_options.renderer` (`src/lib.rs:314-326`,
`:384-406`); there is **no runtime fallback** from wgpu to glow. If wgpu fails to get an
adapter you get `Error::Wgpu`, not a glow window.

If both backends are compiled in, eframe logs at startup:
`"Both the glow and wgpu renderers are available. Using {renderer}."` (`src/lib.rs:433`).

### Exact Cargo feature list (from `Cargo.toml`, doc text from `Cargo.toml.orig`)

`default = ["accesskit", "default_fonts", "links", "wayland", "web_screen_reader", "wgpu", "winit/default", "x11"]`

| Feature | Enables | What it buys you |
|---|---|---|
| `accesskit` | `egui-winit/accesskit` | Platform accessibility (AT-SPI/UIA). Adds `UserEvent::AccessKitActionRequest`. `compile_error!` on Android with `android-native-activity` (`src/lib.rs:148-153`). |
| `android-game-activity` | `egui-winit/android-game-activity` | GameActivity backend. |
| `android-native-activity` | `egui-winit/android-native-activity` | NativeActivity backend. |
| `default_fonts` | `egui/default_fonts` | `include_bytes!`-bundled fonts. Drop it only if you ship your own. |
| `glow` | `dep:egui_glow`, `dep:glow`, `dep:glutin-winit`, `dep:glutin` | OpenGL backend; enables `Renderer::Glow`, `CreationContext::gl`, `Frame::gl()`, the 1-arg `on_exit`, and the `egui_glow`/`glow` re-exports. |
| `inspection` | `dep:egui_inspection`, `accesskit` | TCP inspection port when `EGUI_INSPECTION`/`EGUI_INSPECTION_ADDR` is set; lets external tools read the AccessKit tree, inject input, capture screenshots. Without it, setting the env var only logs a warning (`src/lib.rs:226-239`). |
| `links` | `egui-winit/links` | Hyperlinks open in the system browser. |
| `persistence` | `egui-winit/serde`, `egui/persistence`, `ron`, `serde` | The whole save/restore path: `storage_dir`, `FileStorage`, `get_value`/`set_value`, `App::save` actually being called, window geometry restore. |
| `wayland` | `egui-winit/wayland`, `egui-wgpu?/wayland`, `egui_glow?/wayland`, `glutin?/wayland`, `glutin-winit?/wayland` | Wayland support + clipboard fix. Required for Linux. |
| `web_screen_reader` | `web-sys/SpeechSynthesis(+Utterance)` | Web only; needs `ctx.options_mut(\|o\| o.screen_reader = true)`. |
| `wgpu` | `wgpu_no_default_features`, `egui-wgpu/default` | wgpu backend **with** wgpu's default backends. |
| `wgpu_no_default_features` | `dep:wgpu`, `dep:egui-wgpu`, `dep:pollster` | Same, but you must select wgpu backends (`dx12`/`metal`/`vulkan`/`webgl`) yourself. **This is the cfg name used throughout the source** — every `#[cfg(feature = "wgpu")]` you might expect is actually `#[cfg(feature = "wgpu_no_default_features")]`. |
| `x11` | `egui-winit/x11`, `egui-wgpu?/x11`, `egui_glow?/x11`, `glutin?/{x11,glx}`, `glutin-winit?/{x11,glx}` | X11 support. |
| `__screenshot` | — | Honours `EFRAME_SCREENSHOT_TO`, writes a PNG, quits. Internal, glow only — wgpu asserts it is unset (`src/native/wgpu_integration.rs:139-140`). |

Switching from wgpu to glow "can significantly reduce your binary size" per the upstream
feature doc (`Cargo.toml.orig`, `wgpu` feature).

---

## 9. Storage and persistence

```rust
// src/epi.rs:944-956
pub trait Storage {
    fn get_string(&self, key: &str) -> Option<String>;
    fn set_string(&mut self, key: &str, value: String);
    fn remove_string(&mut self, key: &str);
    fn flush(&mut self);
}

// src/epi.rs:959-960
#[cfg(feature = "ron")]
pub fn get_value<T: serde::de::DeserializeOwned>(storage: &dyn Storage, key: &str) -> Option<T>

// src/epi.rs:974-975
#[cfg(feature = "ron")]
pub fn set_value<T: serde::Serialize>(storage: &mut dyn Storage, key: &str, value: &T)

// src/epi.rs:984
pub const APP_KEY: &str = "app";
```

`get_value` swallows deserialisation failures and returns `None` after a `log::debug!`
(`src/epi.rs:965-969`) — expected when you change your struct shape between releases.

### Where files land

```rust
// src/native/file_storage.rs:17-40
pub fn storage_dir(app_id: &str) -> Option<PathBuf>
```

Documented layout (`src/native/file_storage.rs:13-16`):

* **Linux:** `/home/UserName/.local/share/APP_ID`
* macOS: `/Users/UserName/Library/Application Support/APP_ID`
* Windows: `C:\Users\UserName\AppData\Roaming\APP_ID\data`

The Linux branch, verbatim (`:21-31`):

```rust
OS::Nix => var_os("XDG_DATA_HOME")
    .map(PathBuf::from)
    .filter(|p| p.is_absolute())
    .or_else(|| std::env::home_dir().map(|p| p.join(".local").join("share")))
    .map(|p| {
        p.join(
            app_id
                .to_lowercase()
                .replace(|c: char| c.is_ascii_whitespace(), ""),
        )
    }),
```

**Gotchas worth pinning down:**

* On Linux the app id is **lowercased and stripped of ASCII whitespace**. `"FxSound Linux"`
  → `~/.local/share/fxsoundlinux`. macOS replaces whitespace with `-` instead and does *not*
  lowercase (`:32-36`). Do not assume the two agree.
* `XDG_DATA_HOME` is honoured only when **absolute** (`:23`).
* `OS::Unknown | OS::Android | OS::IOS => None` (`:38`) — no storage at all.
* The file inside that directory is **`app.ron`** (`:141`).
* Which id? `native_options.viewport.app_id` if set, else the `app_name` argument to
  `run_native` (`src/native/glow_integration.rs:251-257`, `src/native/wgpu_integration.rs:455-461`).
  Recall `init_native` copies `app_name` into `app_id` when it's `None`.
* `NativeOptions::persistence_path` bypasses all of the above and names the .ron file
  directly (`src/native/glow_integration.rs:248-250`).
* `storage_dir` is only exported with the `persistence` feature (`src/lib.rs:206-207`), and
  there is a unit test asserting it matches `directories::ProjectDirs` (`:262-269`).

```rust
// src/native/file_storage.rs:100-105
pub struct FileStorage { /* ron_filepath, kv, dirty, last_save_join_handle — all private */ }

// src/native/file_storage.rs:131
pub fn from_app_id(app_id: &str) -> Option<Self>
```

`FileStorage::flush()` spawns a **background thread named `eframe_persist`** and joins the
previous one first (`:167-194`); `Drop` joins the outstanding save (`:107-114`). So saving is
off the UI thread, but shutdown blocks on it.

### The save cycle

```rust
// src/native/epi_integration.rs:391-401
pub fn maybe_autosave(&mut self, app: &mut dyn epi::App, window: Option<&winit::window::Window>)
// src/native/epi_integration.rs:403
pub fn save(&mut self, app: &mut dyn epi::App, window: Option<&winit::window::Window>)
```

Order inside `save` (`:407-433`, all `#[cfg(feature = "persistence")]`):
1. window geometry under key `"window"` — only if `NativeOptions::persist_window`
   (`:411-420`, key at `:447`);
2. egui memory under key `"egui"` — only if `App::persist_egui_memory()` (`:421-425`, key at `:444`);
3. `App::save(storage)` (`:426-429`);
4. `Storage::flush()` (`:431-432`).

`maybe_autosave` runs after every paint (`src/native/glow_integration.rs:868`,
`src/native/wgpu_integration.rs:874`) and fires when
`now - last_auto_save > app.auto_save_interval()`. Frame-time reporting deliberately excludes
auto-save time (`glow_integration.rs:866`).

On shutdown: `save_and_destroy` → `integration.save(...)` → `app.on_exit(...)` →
`painter.destroy()` (`src/native/glow_integration.rs:443-453`). It also runs from
`ApplicationHandler::exiting`, because on macOS Cmd-Q never returns from
`run_app_on_demand` (`src/native/run.rs:266-273`).

---

## 10. Event loop and repaint scheduling

**winit version: `0.30.13`.** Declared as `winit = { version = "0.30.13", features = ["rwh_06"], default-features = false }` (`Cargo.toml`), locked at `0.30.13` (`Cargo.lock:3849-3850`), and that exact source tree is vendored next to eframe. Companions: `egui-winit 0.36.0`, `egui_glow 0.36.0`, `egui-wgpu 0.36.0`, `glutin 0.32.3`, `glutin-winit 0.5.0`, `glow 0.17.0`, `wgpu 30.0.0` (wasm target), `accesskit_winit 0.32.2`, `raw-window-handle 0.6.2`.

### Startup path

`run_native` → `run_native_ext` → `init_native` → `run_glow` / `run_wgpu`
(`src/native/run.rs:399`, `:438`). Then:

```rust
// src/native/run.rs:407-419 (glow; wgpu is identical at :446-458)
#[cfg(not(target_os = "ios"))]
if native_options.run_and_return {
    return with_event_loop(native_options, |event_loop, native_options| {
        let glow_eframe = GlowWinitApp::new(event_loop, app_name, native_options, egui_ctx, app_creator);
        run_and_return(event_loop, glow_eframe)
    })?;
}
let event_loop = create_event_loop(&mut native_options)?;
let glow_eframe = GlowWinitApp::new(&event_loop, app_name, native_options, egui_ctx, app_creator);
run_and_exit(event_loop, glow_eframe)
```

* `run_and_return == true` (the default) → the `EventLoop` is kept in a **thread-local**
  and reused so you can open and close an eframe window repeatedly (`src/native/run.rs:57-75`);
  it is driven with `EventLoopExtRunOnDemand::run_app_on_demand` (`:374-383`).
* `run_and_return == false` → `EventLoop::run_app`, and on exit eframe calls
  `std::process::exit(0)` after saving (`:176-181`). Destructors after `run_native` will not run.

The egui context is built by `create_egui_context` (`src/native/winit_integration.rs:33-60`),
which restores memory from storage, sets `set_embed_viewports(!IS_DESKTOP)` (so on Linux
child viewports are **real OS windows**), and sets `max_passes = 2` — eframe supports
multi-pass layout via `egui::Context::request_discard`.

### The wrapper

```rust
// src/native/run.rs:79-84
struct WinitAppWrapper<T: WinitApp> {
    windows_next_repaint_times: HashMap<WindowId, Instant>,
    winit_app: T,
    return_result: Result<(), crate::Error>,
    run_and_return: bool,
}
// impl ApplicationHandler<UserEvent> for WinitAppWrapper<T>   — src/native/run.rs:246
```

### How `request_repaint` becomes a frame

1. At init, eframe installs a repaint callback on the context
   (`src/native/glow_integration.rs:302-314`, `src/native/wgpu_integration.rs:280-293`):

```rust
egui_ctx.set_request_repaint_callback(move |info| {
    let when = Instant::now() + info.delay;
    let cumulative_pass_nr = info.current_cumulative_pass_nr;
    event_loop_proxy.lock().send_event(UserEvent::RequestRepaint {
        viewport_id: info.viewport_id, when, cumulative_pass_nr,
    }).ok();
});
```

So `ctx.request_repaint()` **from any thread** works — it goes through the winit
`EventLoopProxy`.

2. `user_event` filters stale requests (`src/native/run.rs:304-332`): the request is honoured
   only if `current_pass_nr == cumulative_pass_nr || current_pass_nr == cumulative_pass_nr + 1`;
   otherwise it is dropped as "we've already repainted". It resolves to
   `EventResult::RepaintAt(window_id, when)`.

3. `handle_event_result` folds each result into `windows_next_repaint_times`
   (`src/native/run.rs:119-158`), taking the **earliest** of the existing and new time:

```rust
EventResult::RepaintAt(window_id, repaint_time) => {
    self.windows_next_repaint_times.insert(
        window_id,
        self.windows_next_repaint_times.get(&window_id)
            .map_or(repaint_time, |last| (*last).min(repaint_time)),
    );
}
```

4. `check_redraw_requests` (`src/native/run.rs:188-243`) runs after every handled event and
   from `new_events`. For each window whose time has arrived it sets
   `ControlFlow::Poll` and calls `window.request_redraw()`; afterwards it sets
   `ControlFlow::WaitUntil(min(next_repaint_times))`. With nothing scheduled,
   `EventResult::Wait` leaves `ControlFlow::Wait` — **the app sleeps at 0% CPU**.

5. `WindowEvent::RedrawRequested` → `winit_app.run_ui_and_paint(...)`
   (`src/native/run.rs:361-366`).

### `EventResult` (internal, `pub(crate)` module — listed so you can read logs)

```rust
// src/native/winit_integration.rs:128-170
pub enum EventResult {
    Wait,
    RepaintNow(WindowId),    // synchronous repaint inside the handler; Windows resize only
    RepaintNext(WindowId),
    RepaintAt(WindowId, Instant),
    Save,
    CloseRequested,          // MUST precede Exit
    Exit,
}
```

`RepaintNow` is additionally special-cased on Windows to paint synchronously and stop
flicker (`src/native/run.rs:108-117`).

### `UserEvent`

```rust
// src/native/winit_integration.rs:63-80  (exported as eframe::UserEvent)
#[derive(Debug)]
pub enum UserEvent {
    RequestRepaint {
        viewport_id: ViewportId,
        when: Instant,
        cumulative_pass_nr: u64,
    },
    #[cfg(feature = "accesskit")]
    AccessKitActionRequest(accesskit_winit::Event),
}
```

### Hidden / minimized / occluded windows

* `INVISIBLE_WINDOW_REPAINT_INTERVAL: Duration = Duration::from_millis(100)` (`src/native/run.rs:26`)
  throttles repaints of invisible windows (they get no `RedrawRequested` on Windows, so eframe
  paints them directly at `:221-224` and clamps the next time at `:230-237`).
* `sleep_if_invisible_or_minimized` sleeps **10 ms** per iteration (`src/native/winit_integration.rs:25-30`).
* When nothing will be shown, eframe runs **no egui pass at all** and calls only
  `App::logic` via `Context::run_logic` (`src/native/glow_integration.rs:637-684`,
  `src/native/epi_integration.rs:323-349`). The unconsumed `RawInput` is stashed in
  `pending_raw_input` and prepended to the next real pass (`:338`, `:357-358`).
* The window is **created hidden** and revealed after the first successful paint
  (`src/native/epi_integration.rs:380-386`) — avoids a white flash.

### Driving your own event loop

```rust
// src/native/run.rs:480-483
pub struct EframeWinitApplication<'a> {
    wrapper: Box<dyn ApplicationHandler<UserEvent> + 'a>,
    control_flow: ControlFlow,
}

// src/native/run.rs:549-554
#[cfg(not(target_os = "ios"))]
pub fn pump_eframe_app(
    &mut self,
    event_loop: &mut EventLoop<UserEvent>,
    timeout: Option<std::time::Duration>,
) -> EframePumpStatus

// src/native/run.rs:567-575
#[cfg(not(target_os = "ios"))]
pub enum EframePumpStatus {
    Continue(ControlFlow),
    Exit(i32),
}
```

`EframeWinitApplication` implements `ApplicationHandler<UserEvent>` with `resumed`,
`window_event`, `new_events`, `user_event`, `device_event`, `about_to_wait`, `suspended`,
`exiting`, `memory_warning` (`src/native/run.rs:485-532`) — so `event_loop.run_app(&mut app)`
just works.

---

## 11. Misc public API

```rust
// src/icon_data.rs:6-18
pub trait IconDataExt {
    fn to_image(&self) -> Result<image::RgbaImage, String>;
    fn to_png_bytes(&self) -> Result<Vec<u8>, String>;
}
// impl IconDataExt for IconData   — src/icon_data.rs:39

// src/icon_data.rs:24
pub fn from_png_bytes(png_bytes: &[u8]) -> Result<IconData, image::ImageError>
```

```rust
// src/epi.rs:897-911
#[derive(Clone, Debug)]
pub struct IntegrationInfo {
    #[cfg(target_arch = "wasm32")]
    pub web_info: WebInfo,
    pub cpu_usage: Option<f32>,   // seconds of the previous frame; None on frame 1
}
```

If you set **no** icon, eframe bundles its own egui logo (`data/icon.png`, loaded at
`src/native/epi_integration.rs:437-441`). To have no icon at all, set
`viewport.icon = Some(Arc::new(egui::IconData::default()))` (`src/epi.rs:301-302`).

Web side (present but irrelevant to a Linux target), for completeness:
`WebRunner::new()` (`src/web/web_runner.rs:41`),
`pub async fn start(&self, canvas: web_sys::HtmlCanvasElement, web_options: crate::WebOptions, app_creator: epi::AppCreator<'static>) -> Result<(), JsValue>` (`:57-62`),
`has_panicked` (`:99`), `panic_summary` (`:104`), `destroy` (`:128`), `app_mut` (`:159`),
`add_event_listener` (`:171`). `WebOptions` at `src/epi.rs:479-530`;
`WebGlContextOption::{WebGl1, WebGl2, BestFirst, CompatibilityFirst}` at `:565-577`.

---

## 12. Complete minimal `main.rs` (native Linux, this workspace's feature set)

`Cargo.toml`:

```toml
[package]
name = "fxsound-app"
version = "0.1.0"
edition = "2024"
rust-version = "1.95"

[dependencies]
egui   = "=0.36.0"
eframe = { version = "=0.36.0", default-features = false, features = [
    "default_fonts", "glow", "wayland", "x11",
] }
```

`src/main.rs` — compiles as written under exactly those features (no `persistence`, glow only):

```rust
use eframe::egui;

fn main() -> eframe::Result {
    env_logger::init(); // optional; eframe logs through `log`

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FxSound")
            .with_app_id("fxsound")          // Wayland app id + persistence folder name
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([480.0, 320.0]),
        renderer: eframe::Renderer::Glow,    // only variant that exists without the wgpu feature
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "fxsound",
        native_options,
        Box::new(|cc| Ok(Box::new(FxSoundApp::new(cc)))), // note the Ok(..)
    )
}

struct FxSoundApp {
    gain_db: f32,
    enabled: bool,
}

impl FxSoundApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Customise egui here: cc.egui_ctx.set_fonts(..), cc.egui_ctx.set_global_style(..),
        // cc.egui_ctx.set_theme(egui::ThemePreference::System), etc.
        // cc.gl is Some(Arc<glow::Context>) under the `glow` feature.
        // cc.storage is always None unless the `persistence` feature is enabled.
        cc.egui_ctx.set_theme(egui::ThemePreference::System);
        Self { gain_db: 0.0, enabled: true }
    }
}

impl eframe::App for FxSoundApp {
    // Runs even while the window is hidden/minimized. No painting allowed here.
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {}

    // The one required method. `ui` has NO margin and NO background of its own.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // NOT `TopBottomPanel` — that type no longer exists in egui 0.36.
        egui::Panel::top("menu").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.enabled, "Enabled");
                ui.separator();
                if ui.button("Reset").clicked() {
                    self.gain_db = 0.0;
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("FxSound");
            ui.add_enabled_ui(self.enabled, |ui| {
                ui.add(
                    egui::Slider::new(&mut self.gain_db, -24.0..=24.0)
                        .suffix(" dB")
                        .text("Output gain"),
                );
            });
            ui.label(format!("gain = {:.1} dB", self.gain_db));
        });
    }

    // Opaque background; the trait default is semi-transparent (alpha 180).
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    // `glow` feature is on, so this is the ONE-argument form.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {}
}
```

**To add persistence**, turn on the feature (`features = [..., "persistence"]`), derive
`serde::{Serialize, Deserialize}` plus `Default` on `FxSoundApp`, and add:

```rust
impl FxSoundApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        if let Some(storage) = cc.storage {
            if let Some(state) = eframe::get_value::<Self>(storage, eframe::APP_KEY) {
                return state;
            }
        }
        Self::default()
    }
}

impl eframe::App for FxSoundApp {
    // ... ui(), etc.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }
    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30) // the default; override if you want
    }
}
```

State then lands in `~/.local/share/fxsound/app.ron` (app id lowercased, whitespace stripped).

### egui 0.36 signatures the example relies on (verified, not remembered)

```rust
// egui-0.36.0/src/containers/panel.rs:206,265,422 — one Panel type, side constructors take an Id
pub struct Panel { /* … */ }
pub fn top(id: impl Into<Id>) -> Self       // also: bottom(:274), left(:249), right(:256)
pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>

// egui-0.36.0/src/containers/panel.rs:1187,1212
pub struct CentralPanel { /* … */ }
pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>
#[deprecated = "Renamed to `show`"] pub fn show_inside<R>(...)   // :1216-1218

// egui-0.36.0/src/widgets/slider.rs:128-131, :192, :199
pub fn new<Num: emath::Numeric>(value: &'a mut Num, range: impl Into<RangeInclusive<Num>>) -> Self
pub fn suffix(mut self, suffix: impl ToString) -> Self
pub fn text(mut self, text: impl Into<WidgetText>) -> Self

// egui-0.36.0/src/ui.rs:1619-1623, :1865
pub fn add_enabled_ui<R>(&mut self, enabled: bool, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>
pub fn checkbox<'a>(&mut self, checked: &'a mut bool, atoms: impl IntoAtoms<'a>) -> Response

// egui-0.36.0/src/context.rs:1869, :2167, :2197
pub fn request_repaint_after(&self, duration: Duration)
pub fn set_theme(&self, theme_preference: impl Into<crate::ThemePreference>)
pub fn set_global_style(&self, style: impl Into<Arc<Style>>)

// egui-0.36.0/src/memory/theme.rs:67-77
pub enum ThemePreference { Dark, Light, #[default] System }

// egui-0.36.0/src/id.rs:143,150 — why Panel::top("menu") compiles
impl From<&'static str> for Id { /* … */ }
impl From<String> for Id { /* … */ }

// egui-0.36.0/src/style.rs:1071   /   ecolor-0.36.2/src/color32.rs:320
pub panel_fill: Color32
pub fn to_normalized_gamma_f32(self) -> [f32; 4]

// egui-0.36.0/src/viewport.rs:355,366,400,424,434,531,544,645
pub fn with_title(mut self, title: impl Into<String>) -> Self
pub fn with_decorations(mut self, decorations: bool) -> Self
pub fn with_resizable(mut self, resizable: bool) -> Self
pub fn with_transparent(mut self, transparent: bool) -> Self
pub fn with_icon(mut self, icon: impl Into<Arc<IconData>>) -> Self
pub fn with_inner_size(mut self, size: impl Into<Vec2>) -> Self
pub fn with_min_inner_size(mut self, size: impl Into<Vec2>) -> Self
pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self
```

`ui.checkbox` / `Slider::suffix` now take `impl IntoAtoms` / `impl ToString` — plain `&str`
still works, but `WidgetText`-typed helpers you remember may not.

### Repainting from an audio/worker thread

```rust
let ctx = cc.egui_ctx.clone();              // egui::Context is cheap to clone and Send+Sync
std::thread::spawn(move || {
    loop {
        // ... do work ...
        ctx.request_repaint();              // -> UserEvent::RequestRepaint via EventLoopProxy
    }
});
// or, to schedule one:
ctx.request_repaint_after(std::time::Duration::from_millis(16));
```
