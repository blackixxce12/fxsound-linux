# egui 0.36 — Widgets, Interaction, Layout & Input (verified cheatsheet)

Every signature below was read out of the vendored source. Nothing here is from memory.

**Source root** (all `file:line` citations are relative to it):

```
/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.0/
```

Sibling crates cited where re-exported:

```
.../emath-0.36.2/    .../epaint-0.36.2/    .../ecolor-0.36.2/
```

**Verification status:** every example in this document was compiled against `egui = "0.36.0"`
in a scratch crate (`cargo build --offline`, rustc 1.98.1) with zero warnings — including the
complete fader in §15.1, extracted from this file verbatim. The "does not exist" claims in the
gotcha table were verified by compiling code that uses them and reading the resulting
`E0599` / `E0425` / `E0433` / `E0609` / `E0277` errors. Every `file:line` citation in the
signature blocks was then re-checked mechanically against the vendored source: the named item is
present on the cited line.

---

## 0. Gotcha table — things that changed and will bite you

| You probably remember | Reality in 0.36 | Evidence |
|---|---|---|
| `Rounding`, `Rounding::same(4.0)` | **Gone.** It is `CornerRadius`, and fields are `u8`, not `f32` | `epaint/src/corner_radius.rs:13`; `cannot find type Rounding` |
| `Margin { left: f32, .. }` | `Margin` fields are **`i8`** (`MarginF32` is the f32 variant) | `epaint/src/margin.rs:15-19`, `epaint/src/margin_f32.rs:13` |
| `Shadow { offset: Vec2, blur: f32, spread: f32 }` | `offset: [i8; 2]`, `blur: u8`, `spread: u8` | `epaint/src/shadow.rs:15-26` |
| `Frame::none()` | **Gone.** Use `Frame::NONE` (const) or `Frame::new()` | `src/containers/frame.rs:161`, `:173` |
| `egui::ImageButton` | **Gone.** Use `Button::image(..)` / `Button::image_and_text(..)` | `src/widgets/button.rs:89`, `:97`; not in `widgets/mod.rs:29-45` |
| `egui::SelectableLabel` | **Gone** as a type. Use `Button::selectable(selected, atoms)` or `ui.selectable_label(..)` | `src/widgets/button.rs:78`, `src/ui.rs:1928` |
| `TopBottomPanel::top(..)`, `SidePanel::left(..)` | **Both gone.** One `Panel` type: `Panel::top/bottom/left/right(id)` | `src/containers/panel.rs:249-276`; `grep TopBottomPanel src/` → nothing |
| `SidePanel::show(ctx, f)` takes a `&Context` | `Panel::show(self, ui: &mut Ui, f)` takes a **`&mut Ui`**. Same for `CentralPanel::show` | `src/containers/panel.rs:422`, `:1212` |
| `panel.show_inside(ui, f)` | Deprecated: *"Renamed to `show`"* — use `show` | `src/containers/panel.rs:427`, `:1217` |
| `panel.show_animated_inside(ui, bool, f)` | `#[deprecated]` → `show_collapsible(ui, &mut bool, f)` (note the `&mut`) | `src/containers/panel.rs:496`, `:451` |
| `ui.allocate_ui_at_rect(rect, f)` | **Gone.** Use `ui.scope_builder(UiBuilder::new().max_rect(rect), f)` | `src/ui.rs:2193`; no such method |
| `ui.allocate_new_ui(builder, f)` | **Gone.** Same replacement as above | no such method |
| `InputState::raw_scroll_delta` | **Gone.** Only `smooth_scroll_delta` (field *and* method). Raw wheel = `Event::MouseWheel` | `src/input_state/mod.rs:243`, `:552`; `no field raw_scroll_delta` |
| `ctx.style()` | **Gone.** It is `ctx.global_style() -> Arc<Style>` | `src/context.rs:2172` |
| `egui::menu::bar(ui, f)` | **Gone.** Use `MenuBar::new().ui(ui, f)` | `src/containers/menu.rs:232`, `:257`; `cannot find function bar` |
| `Sense::click()` was replaced by a struct literal | **Still a fn** — but `Sense` is now a `bitflags` newtype over `u8`. Both `Sense::click()` *and* `Sense::CLICK` exist and differ (focusable!) | `src/sense.rs:4-23`, `:60` |
| `Painter::rect_stroke(rect, cr, stroke)` | Takes a **4th arg** `StrokeKind` | `src/painter.rs:406-412` |
| `Painter::rect(rect, cr, fill, stroke)` | Takes a **5th arg** `StrokeKind` | `src/painter.rs:380-387` |
| `Button::new("x")` takes `impl Into<WidgetText>` | Takes **`impl IntoAtoms<'a>`** — a tuple of text/images/atoms | `src/widgets/button.rs:45` |
| `ui.button(text)` | Also `impl IntoAtoms<'a>`, so `ui.button((image, "Click me!"))` works | `src/ui.rs:1847` |
| `DragValue::prefix(impl ToString)` | Now `impl IntoAtoms<'a>` (Slider's is still `impl ToString`!) | `src/widgets/drag_value.rs:152` vs `src/widgets/slider.rs:185` |
| `ColorImage::new(size, color)` (fills) | `ColorImage::new(size, pixels: Vec<Color32>)`; the filling ctor is `ColorImage::filled(size, color)` | `epaint/src/image.rs:61`, `:75` |
| `ui.input(..)` is an inherent `Ui` method | `Ui: Deref<Target = Context>` — `input`, `input_mut`, `memory_mut`, `data_mut`, `animate_bool`, `request_repaint` all come from `Context` via deref. There is **no** `Ui::input` | `src/ui.rs:91-98`, `src/context.rs:990` |
| `ui.make_persistent_id(impl Hash)` | The bound is `impl AsIdSalt` = **`Hash + Debug`**; a bare `impl Hash` fails with `E0277` | `src/id_salt.rs:7-9`, `src/ui.rs:883` |
| `ColorImage::new(size, color)` → `TextureHandle` in one step | You still need `ctx.load_texture(name, image, TextureOptions)`; there is no `Ui::load_texture` | `src/context.rs:2387` |
| `Response` has `pub clicked: bool` etc. | All booleans live in a private-ish `pub flags: Flags` bitfield; use the **methods** | `src/response.rs:73-74`, `:91-150` |
| `Event::MouseWheel { unit, delta, modifiers }` | Has **4** fields — `phase: TouchPhase` was added | `src/data/input/event.rs:150-172` |
| `egui::Modal` doesn't exist | **It does exist**, and so does `ModalResponse` | `src/containers/modal.rs:16`, `:124` |
| `Popup` is `egui::popup::popup_below_widget` | 0.36 has a full `Popup<'a>` builder; `Popup::menu`, `Popup::context_menu`, `Popup::from_response` | `src/containers/popup.rs:165`, `:217-248` |
| `ui.put` is the only manual-placement fn | `ui.put` (advances cursor) **and** `ui.place` (does not) | `src/ui.rs:1552`, `:1565` |
| `Response::drag_released()` | Renamed long ago; it is `drag_stopped()` / `drag_stopped_by()` | `src/response.rs:444`, `:449` |
| `WidgetVisuals::rounding` | `WidgetVisuals::corner_radius` | `src/style.rs:1307` |

---

## 1. The `Widget` trait

```rust
#[must_use = "You should put this widget in a ui with `ui.add(widget);`"]
pub trait Widget {
    /// Allocate space, interact, paint, and return a [`Response`].
    fn ui(self, ui: &mut Ui) -> Response;

    /// Box this widget for dynamic dispatch.
    #[inline]
    fn boxed<'a>(self) -> BoxedWidget<'a>
    where
        Self: Sized + 'a,
    { Box::new(move |ui: &mut Ui| ui.add(self)) }
}
```
`src/widgets/mod.rs:62-81`

```rust
/// A dynamically dispatched [`Widget`].
pub type BoxedWidget<'a> = Box<dyn FnOnce(&mut Ui) -> Response + 'a>;
```
`src/widgets/mod.rs:13`

Any `FnOnce(&mut Ui) -> Response` is a `Widget`:

```rust
impl<F> Widget for F
where
    F: FnOnce(&mut Ui) -> Response,
{
    fn ui(self, ui: &mut Ui) -> Response { self(ui) }
}
```
`src/widgets/mod.rs:104-111`

Helper trait for `TextEdit::State::load`-style access:

```rust
pub trait WidgetWithState { type State; }
```
`src/widgets/mod.rs:114-116`

---

## 2. `Response`

### 2.1 Fields (all of them)

```rust
pub struct Response {
    pub ctx: Context,
    pub layer_id: LayerId,
    pub id: Id,
    pub rect: Rect,
    pub interact_rect: Rect,
    pub sense: Sense,
    #[doc(hidden)] pub interact_pointer_pos_or_nan: Pos2,
    #[doc(hidden)] pub intrinsic_size_or_nan: Vec2,
    #[doc(hidden)] pub flags: Flags,
}
```
`src/response.rs:23-75`. `Response` is asserted to be exactly 88 bytes (`src/response.rs:77-84`).

`rect` is the full widget rect; `interact_rect` is `rect` **after clipping** by the parent clip
rect (`src/response.rs:38-42`).

The boolean state is a bitflag set — you do not read it directly, but knowing the names helps
when debugging:

```rust
#[doc(hidden)]
#[derive(Copy, Clone, Debug)]
pub struct Flags(u16);

bitflags::bitflags! {
    impl Flags: u16 {
        const ENABLED                 = 1<<0;
        const CONTAINS_POINTER        = 1<<1;
        const HOVERED                 = 1<<2;
        const HIGHLIGHTED             = 1<<3;
        const CLICKED                 = 1<<4;
        const FAKE_PRIMARY_CLICKED    = 1<<5;
        const LONG_TOUCHED            = 1<<6;
        const DRAG_STARTED            = 1<<7;
        const DRAGGED                 = 1<<8;
        const DRAG_STOPPED            = 1<<9;
        const IS_POINTER_BUTTON_DOWN_ON = 1<<10;
        const CHANGED                 = 1<<11;
        const CLOSE                   = 1<<12;
    }
}
```
`src/response.rs:86-151`

### 2.2 Click / touch

```rust
pub fn parent_id(&self) -> Id                                          // :157
pub fn clicked(&self) -> bool                                          // :183
pub fn clicked_by(&self, button: PointerButton) -> bool                // :196
pub fn secondary_clicked(&self) -> bool                                // :210
pub fn long_touched(&self) -> bool                                     // :218
pub fn middle_clicked(&self) -> bool                                   // :230
pub fn double_clicked(&self) -> bool                                   // :236
pub fn triple_clicked(&self) -> bool                                   // :242
pub fn double_clicked_by(&self, button: PointerButton) -> bool         // :248
pub fn triple_clicked_by(&self, button: PointerButton) -> bool         // :255
pub fn clicked_with_open_in_background(&self) -> bool                  // :264
pub fn clicked_elsewhere(&self) -> bool                                // :274
```
`src/response.rs`

`clicked()` returns true for a real mouse click **and** for Space/Enter on a focused widget
(`FAKE_PRIMARY_CLICKED`, `src/response.rs:183-184`) and for AccessKit-driven activation.

### 2.3 Hover / enable / highlight / focus

```rust
pub fn enabled(&self) -> bool                                          // :304
pub fn hovered(&self) -> bool                                          // :319
pub fn contains_pointer(&self) -> bool                                 // :332
pub fn highlighted(&self) -> bool                                      // :339
pub fn has_focus(&self) -> bool                                        // :348
pub fn gained_focus(&self) -> bool                                     // :353
pub fn lost_focus(&self) -> bool                                       // :371
pub fn request_focus(&self)                                            // :376
pub fn surrender_focus(&self)                                          // :381
pub fn highlight(mut self) -> Self                                     // :742
```

`hovered()` vs `contains_pointer()`: `HOVERED` also covers "clicked/tapped this frame"; use
`contains_pointer()` when you want strict "the pointer is over me and nothing blocks it"
(`src/response.rs:97-101`).

### 2.4 Drag

```rust
pub fn drag_started(&self) -> bool                                     // :392
pub fn drag_started_by(&self, button: PointerButton) -> bool           // :403
pub fn dragged(&self) -> bool                                          // :432
pub fn dragged_by(&self, button: PointerButton) -> bool                // :438
pub fn drag_stopped(&self) -> bool                                     // :444
pub fn drag_stopped_by(&self, button: PointerButton) -> bool           // :449
pub fn drag_delta(&self) -> Vec2                                       // :455
pub fn total_drag_delta(&self) -> Option<Vec2>                         // :469
pub fn drag_motion(&self) -> Vec2                                      // :487
pub fn is_pointer_button_down_on(&self) -> bool                        // :594
```

Drag-and-drop payloads:

```rust
pub fn dnd_set_drag_payload<Payload: Any + Send + Sync>(&self, payload: Payload)              // :498
pub fn dnd_hover_payload<Payload: Any + Send + Sync>(&self) -> Option<Arc<Payload>>           // :515
pub fn dnd_release_payload<Payload: Any + Send + Sync>(&self) -> Option<Arc<Payload>>         // :531
```

### 2.5 Positions, size, change flag, close

```rust
pub fn interact_pointer_pos(&self) -> Option<Pos2>                     // :545
pub fn intrinsic_size(&self) -> Option<Vec2>                           // :557
pub fn set_intrinsic_size(&mut self, size: Vec2)                       // :564
pub fn hover_pos(&self) -> Option<Pos2>                                // :572
pub fn changed(&self) -> bool                                          // :612
pub fn mark_changed(&mut self)                                         // :624
pub fn should_close(&self) -> bool                                     // :632
pub fn set_close(&mut self)                                            // :639
```

`interact_pointer_pos()` is `None` unless the widget is currently being clicked or dragged.
`hover_pos()` is the pointer position while merely hovering.

### 2.6 Tooltips

```rust
pub fn on_hover_ui(self, add_contents: impl FnOnce(&mut Ui)) -> Self            // :664
pub fn on_disabled_hover_ui(self, add_contents: impl FnOnce(&mut Ui)) -> Self   // :670
pub fn on_hover_ui_at_pointer(self, add_contents: impl FnOnce(&mut Ui)) -> Self // :676
pub fn show_tooltip_ui(&self, add_contents: impl FnOnce(&mut Ui))               // :687
pub fn show_tooltip_text(&self, text: impl Into<WidgetText>)                    // :696
pub fn is_tooltip_open(&self) -> bool                                           // :703
pub fn on_hover_text_at_pointer(self, text: impl Into<WidgetText>) -> Self      // :709
pub fn on_hover_text(self, text: impl Into<WidgetText>) -> Self                 // :726
pub fn on_disabled_hover_text(self, text: impl Into<WidgetText>) -> Self        // :749
pub fn on_hover_cursor(self, cursor: CursorIcon) -> Self                        // :761
pub fn on_hover_and_drag_cursor(self, cursor: CursorIcon) -> Self               // :770
```

### 2.7 Re-interaction, scrolling, a11y, context menu, union

```rust
#[must_use]
pub fn interact(&self, sense: Sense) -> Self                                    // :802
pub fn scroll_to_me(&self, align: Option<Align>)                                // :841
pub fn scroll_to_me_animation(
    &self,
    align: Option<Align>,
    animation: crate::style::ScrollAnimation,
)                                                                               // :846-850
pub fn widget_info(&self, make_info: impl Fn() -> crate::WidgetInfo)            // :868
pub fn output_event(&self, event: crate::output::OutputEvent)                   // :896
pub fn labelled_by(self, id: Id) -> Self                                        // :1002
pub fn context_menu(&self, add_contents: impl FnOnce(&mut Ui)) -> Option<InnerResponse<()>>  // :1027
pub fn context_menu_opened(&self) -> bool                                       // :1034
pub fn paint_debug_info(&self)                                                  // :1048
pub fn union(&self, other: Self) -> Self                                        // :1070
pub fn with_new_rect(self, rect: Rect) -> Self                                  // :1098
```

`Response` implements `BitOr` / `BitOrAssign` (`src/response.rs:1118`, `:1137`), so
`response | other_response` works. Note the warning at `src/response.rs:786-787`: calling
`.interact()` on a `union`ed response is **undefined behavior**.

`Response::interact` **ORs** the new sense into the old one (`sense: self.sense | sense`,
`src/response.rs:814`) — it never removes senses.

### 2.8 `InnerResponse`

```rust
#[derive(Debug)]
pub struct InnerResponse<R> {
    pub inner: R,
    pub response: Response,
}

impl<R> InnerResponse<R> {
    #[inline]
    pub fn new(inner: R, response: Response) -> Self { Self { inner, response } }
}
```
`src/response.rs:1165-1178`

---

## 3. `Sense` — a bitflags newtype in 0.36

```rust
/// What sort of interaction is a widget sensitive to?
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Sense(u8);

bitflags::bitflags! {
    impl Sense: u8 {
        const HOVER     = 0;
        const CLICK     = 1<<0;
        const DRAG      = 1<<1;
        const FOCUSABLE = 1<<2;
    }
}
```
`src/sense.rs:1-23`

The classic constructors **still exist** and are *not* the same as the bare flags — they add
`FOCUSABLE`:

```rust
#[inline] pub fn hover() -> Self                  { Self::empty() }                        // :45
#[inline] pub fn focusable_noninteractive() -> Self{ Self::FOCUSABLE }                     // :52
#[inline] pub fn click() -> Self                  { Self::CLICK | Self::FOCUSABLE }        // :60
#[inline] pub fn drag() -> Self                   { Self::DRAG | Self::FOCUSABLE }         // :68
#[inline] pub fn click_and_drag() -> Self         { Self::CLICK | Self::FOCUSABLE | Self::DRAG } // :81
```

Predicates:

```rust
#[inline] pub fn interactive(&self) -> bool   { self.intersects(Self::CLICK | Self::DRAG) } // :87
#[inline] pub fn senses_click(&self) -> bool  { self.contains(Self::CLICK) }                // :92
#[inline] pub fn senses_drag(&self) -> bool   { self.contains(Self::DRAG) }                 // :97
#[inline] pub fn is_focusable(&self) -> bool  { self.contains(Self::FOCUSABLE) }            // :102
```

> **Rule of thumb:** use `Sense::click()` for anything keyboard-reachable; use `Sense::CLICK`
> (bare flag) only when you deliberately want a widget that is *not* focusable. Because it is
> `bitflags`, `Sense::empty()`, `|`, `contains`, `intersects`, `union` are all available.

---

## 4. `Ui` — space allocation, interaction, scopes

### 4.1 Construction

```rust
pub fn new(ctx: Context, id: Id, ui_builder: UiBuilder) -> Self      // src/ui.rs:108
pub fn new_child(&mut self, ui_builder: UiBuilder) -> Self           // src/ui.rs:208
```

`new_child` does **not** allocate the space in the parent; `scope_builder` does
(`src/ui.rs:2189-2191`).

```rust
impl Deref for Ui {
    type Target = Context;
    #[inline]
    fn deref(&self) -> &Self::Target { self.ctx() }
}
```
`src/ui.rs:91-98` — this is why `ui.input(..)`, `ui.input_mut(..)`, `ui.memory_mut(..)`,
`ui.data_mut(..)`, `ui.animate_bool(..)`, `ui.request_repaint()` compile.

### 4.2 Allocation

```rust
pub fn allocate_response(&mut self, desired_size: Vec2, sense: Sense) -> Response          // :1138
pub fn allocate_exact_size(&mut self, desired_size: Vec2, sense: Sense) -> (Rect, Response)// :1150
pub fn allocate_at_least(&mut self, desired_size: Vec2, sense: Sense) -> (Rect, Response)  // :1161
pub fn allocate_space(&mut self, desired_size: Vec2) -> (Id, Rect)                         // :1187
pub fn allocate_rect(&mut self, rect: Rect, sense: Sense) -> Response                      // :1256
pub fn advance_cursor_after_rect(&mut self, rect: Rect) -> Id                              // :1263
pub fn allocate_painter(&mut self, desired_size: Vec2, sense: Sense) -> (Response, Painter)// :1370
```

`allocate_response` also records the intrinsic size for external layout crates
(`src/ui.rs:1138-1143`).

`allocate_exact_size` returns a `Rect` aligned inside the (possibly larger, justified)
`response.rect` — the response can therefore react to input **outside** the returned rect
(`src/ui.rs:1145-1149`).

Sub-`Ui` allocation:

```rust
#[inline]
pub fn allocate_ui<R>(
    &mut self,
    desired_size: Vec2,
    add_contents: impl FnOnce(&mut Self) -> R,
) -> InnerResponse<R>                                                                      // :1308

#[inline]
pub fn allocate_ui_with_layout<R>(
    &mut self,
    desired_size: Vec2,
    layout: Layout,
    add_contents: impl FnOnce(&mut Self) -> R,
) -> InnerResponse<R>                                                                      // :1321
```

> **`allocate_ui_at_rect` and `allocate_new_ui` do not exist in 0.36.** The replacement:
> ```rust
> ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| { /* … */ });
> ```
> That is exactly what the stdlib does internally (`src/containers/scroll_area.rs:1008`).

### 4.3 Interaction

```rust
pub fn interact(&self, rect: Rect, id: Id, sense: Sense) -> Response                       // :906
pub fn interact_opt(
    &self,
    rect: Rect,
    id: Id,
    sense: Sense,
    options: crate::InteractOptions,
) -> Response                                                                              // :911-918
pub fn response(&self) -> Response                                                         // :943
pub fn rect_contains_pointer(&self, rect: Rect) -> bool                                    // :1003
pub fn ui_contains_pointer(&self) -> bool                                                  // :1015
```

`Ui::interact` clips the interaction rect for you: `interact_rect: self.clip_rect().intersect(rect)`
(`src/ui.rs:927`).

```rust
/// How to handle multiple calls to [`Response::interact`] and [`Ui::interact_opt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InteractOptions {
    pub move_to_top: bool,     // default: false
}
```
`src/widget_rect.rs:74-87`

Closing containers (0.36's `ClosableTag` mechanism):

```rust
pub fn close(&self)                              // :1039
pub fn close_kind(&self, ui_kind: UiKind)        // :1063
pub fn should_close(&self) -> bool               // :1091
pub fn will_parent_close(&self) -> bool          // :1105
```

### 4.4 Adding widgets / manual placement

```rust
#[inline]
pub fn add(&mut self, widget: impl Widget) -> Response                                     // :1520
pub fn add_sized(&mut self, max_size: impl Into<Vec2>, widget: impl Widget) -> Response    // :1537
pub fn place(&mut self, max_rect: Rect, widget: impl Widget) -> Response                   // :1552
pub fn put(&mut self, max_rect: Rect, widget: impl Widget) -> Response                     // :1565
pub fn add_enabled(&mut self, enabled: bool, widget: impl Widget) -> Response              // :1587
pub fn add_visible(&mut self, visible: bool, widget: impl Widget) -> Response              // :1646
pub fn add_space(&mut self, amount: f32)                                                   // :1674
```

`place` uses `new_child` (does **not** advance the cursor, `src/ui.rs:1552-1559`);
`put` uses `scope_builder` (**does** advance the cursor, `src/ui.rs:1565-1573`). Both wrap the
widget in `Layout::centered_and_justified(Direction::TopDown)`.

### 4.5 Scopes

```rust
pub fn group<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>    // :2146
pub fn push_id<R>(
    &mut self,
    id_salt: impl AsIdSalt,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R>                                                                      // :2163
pub fn scope<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>    // :2185
pub fn scope_builder<R>(
    &mut self,
    ui_builder: UiBuilder,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R>                                                                      // :2193
pub fn scope_dyn<'c, R>(
    &mut self,
    ui_builder: UiBuilder,
    add_contents: Box<dyn FnOnce(&mut Ui) -> R + 'c>,
) -> InnerResponse<R>                                                                      // :2202
pub fn add_enabled_ui<R>(..)                                                               // :1619
pub fn indent<R>(..)                                                                       // :2233
pub fn with_visual_transform<R>(..)                                                        // :2745
```

Direction helpers (all `-> InnerResponse<R>`):
`horizontal` `:2314`, `horizontal_centered` `:2319`, `horizontal_top` `:2334`,
`horizontal_wrapped` `:2363`, `vertical` `:2404`, `vertical_centered` `:2423`,
`vertical_centered_justified` `:2444`, `with_layout` `:2469`, `centered_and_justified` `:2480`,
`columns` `:2525`, `columns_const::<NUM_COL, R>` `:2592`.

Drag & drop: `dnd_drag_source` `:2641`, `dnd_drop_zone` `:2693`.

### 4.6 `UiBuilder`

```rust
#[must_use]
#[derive(Clone, Default)]
pub struct UiBuilder {
    pub id_source: Option<IdSource>,
    pub ui_stack_info: UiStackInfo,
    pub layer_id: Option<LayerId>,
    pub max_rect: Option<Rect>,
    pub layout: Option<Layout>,
    pub disabled: bool,
    pub invisible: bool,
    pub sizing_pass: bool,
    pub style: Option<Arc<Style>>,
    pub sense: Option<Sense>,
    pub accessibility_parent: Option<Id>,
    pub classes: Classes,
}
```
`src/ui_builder.rs:19-32`

```rust
#[inline] pub fn new() -> Self                                        // :46
#[inline] pub fn id_salt(mut self, id_salt: impl AsIdSalt) -> Self    // :56
#[inline] pub fn id(mut self, id: Id) -> Self                         // :71
#[inline] pub fn ui_stack_info(mut self, ui_stack_info: UiStackInfo) -> Self // :78
#[inline] pub fn layer_id(mut self, layer_id: LayerId) -> Self        // :85
#[inline] pub fn max_rect(mut self, max_rect: Rect) -> Self           // :103
#[inline] pub fn layout(mut self, layout: Layout) -> Self             // :112
#[inline] pub fn disabled(mut self) -> Self                           // :123
#[inline] pub fn invisible(mut self) -> Self                          // :134
#[inline] pub fn sizing_pass(mut self) -> Self                        // :146
#[inline] pub fn style(mut self, style: impl Into<Arc<Style>>) -> Self// :155
#[inline] pub fn sense(mut self, sense: Sense) -> Self                // :168
#[inline] pub fn closable(mut self) -> Self                           // :181
#[inline] pub fn accessibility_parent(mut self, parent_id: Id) -> Self// :193
```

`UiBuilder::sense` registers the `Ui`'s own background sense **below** contained widgets, and
you read it early with `Ui::response()` (`src/ui_builder.rs:160-171`, `src/ui.rs:936-943`).

```rust
#[derive(Clone)]
pub enum IdSource {
    Explicit(Id),
    Child(IdSalt),
}
```
`src/ui_builder.rs:36-42`

---

## 5. Layout, `Align`, `Direction`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub main_dir: Direction,
    pub main_wrap: bool,
    pub main_align: Align,
    pub main_justify: bool,
    pub cross_align: Align,
    pub cross_justify: bool,
}
```
`src/layout.rs:100-126`. `Default` is `Layout::top_down(Align::LEFT)` (`:128-133`).

```rust
#[inline(always)] pub fn left_to_right(valign: Align) -> Self                       // :141
                  pub fn right_to_left(valign: Align) -> Self                       // :156
#[inline(always)] pub fn top_down(halign: Align) -> Self                            // :171
                  pub fn top_down_justified(halign: Align) -> Self                  // :184
                  pub fn bottom_up(halign: Align) -> Self                           // :192
                  pub fn from_main_dir_and_cross_align(main_dir: Direction, cross_align: Align) -> Self // :204
                  pub fn centered_and_justified(main_dir: Direction) -> Self        // :220
#[inline] pub fn with_main_wrap(self, main_wrap: bool) -> Self                      // :236
#[inline] pub fn with_main_align(self, main_align: Align) -> Self                   // :242
#[inline] pub fn with_cross_align(self, cross_align: Align) -> Self                 // :251
#[inline] pub fn with_main_justify(self, main_justify: bool) -> Self                // :262
#[inline] pub fn with_cross_justify(self, cross_justify: bool) -> Self              // :276
pub fn main_dir(&self) -> Direction                                                 // :287
pub fn main_wrap(&self) -> bool                                                     // :292
pub fn cross_align(&self) -> Align                                                  // :297
pub fn cross_justify(&self) -> bool                                                 // :302
pub fn is_horizontal(&self) -> bool                                                 // :307
pub fn is_vertical(&self) -> bool                                                   // :312
pub fn prefer_right_to_left(&self) -> bool                                          // :316
pub fn horizontal_placement(&self) -> Align                                         // :324
pub fn horizontal_align(&self) -> Align                                             // :333
pub fn vertical_align(&self) -> Align                                               // :342
pub fn horizontal_justify(&self) -> bool                                            // :355
pub fn vertical_justify(&self) -> bool                                              // :363
pub fn align_size_within_rect(&self, size: Vec2, outer: Rect) -> Rect               // :374
```

```rust
pub enum Direction {
    LeftToRight,
    RightToLeft,
    TopDown,
    BottomUp,
}
```
`epaint/src/direction.rs:4-9` (re-exported as `egui::Direction`, `src/lib.rs:448`)

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Align {
    #[default] Min,   // left or top
    Center,
    Max,              // right or bottom
}

impl Align {
    pub const LEFT:   Self = Self::Min;   // emath/src/align.rs:22
    pub const RIGHT:  Self = Self::Max;   // :25
    pub const TOP:    Self = Self::Min;   // :28
    pub const BOTTOM: Self = Self::Max;   // :31
    pub fn to_factor(self) -> f32;        // :35
    pub fn to_sign(self) -> f32;          // :45
    pub fn flip(self) -> Self;            // :55
    pub fn align_size_within_range(self, size: f32, range: impl Into<Rangef>) -> Rangef; // :123
}
```
`emath/src/align.rs:6-18`

`Align2` constants (`emath/src/align.rs:154-162`): `LEFT_BOTTOM`, `LEFT_CENTER`, `LEFT_TOP`,
`CENTER_BOTTOM`, `CENTER_CENTER`, `CENTER_TOP`, `RIGHT_BOTTOM`, `RIGHT_CENTER`, `RIGHT_TOP`.
Methods: `x` `:168`, `y` `:174`, `to_sign` `:179`, `flip_x/flip_y/flip` `:185/:191/:197`,
`anchor_rect` `:203`, `anchor_size(pos, size) -> Rect` `:220`,
`align_size_within_rect(size, frame) -> Rect` `:235`, `pos_in_rect(&Rect) -> Pos2` `:261`.

`RectAlign` (used by `Popup`) — `emath/src/rect_align.rs:30`; consts `TOP_START` `:46`, `TOP` `:52`,
`TOP_END` `:58`, `RIGHT_START` `:64`, `RIGHT` `:70`, `RIGHT_END` `:76`, `BOTTOM_END` `:82`,
`BOTTOM` `:88`, `BOTTOM_START` `:94`, `LEFT_END` `:100`, `LEFT` `:106`, `LEFT_START` `:112`;
methods `parent` `:135`, `child` `:140`, `from_align2` `:145`, `over_corner` `:153`,
`outside` `:161`, `align_rect(&self, parent_rect: &Rect, size: Vec2, gap: f32) -> Rect` `:169`,
`symmetries` `:234`, `find_best_align` `:247`.

---

## 6. `Frame` (margins, fill, stroke, corner radius, shadow)

```rust
pub struct Frame {
    pub inner_margin: Margin,        // :104
    pub fill: Color32,               // :110
    pub stroke: Stroke,              // :116
    pub corner_radius: CornerRadius, // :122
    pub outer_margin: Margin,        // :137
    pub shadow: Shadow,              // :140
}
```
`src/containers/frame.rs:96-141`

```rust
pub const NONE: Self = Self {
    inner_margin: Margin::ZERO,
    stroke: Stroke::NONE,
    fill: Color32::TRANSPARENT,
    corner_radius: CornerRadius::ZERO,
    outer_margin: Margin::ZERO,
    shadow: Shadow::NONE,
};                                                       // :161-168
pub const fn new() -> Self { Self::NONE }                // :173
```

Presets (all take `&Style`): `group` `:178`, `side_top_panel` `:185`, `central_panel` `:191`,
`window` `:196`, `menu` `:205`, `popup` `:214`, `canvas` `:227`, `dark_canvas` `:236`.

Builders:

```rust
#[inline] pub fn inner_margin(mut self, inner_margin: impl Into<Margin>) -> Self            // :248
#[inline] pub fn fill(mut self, fill: Color32) -> Self                                      // :258
#[inline] pub fn stroke(mut self, stroke: impl Into<Stroke>) -> Self                        // :267
#[inline] pub fn corner_radius(mut self, corner_radius: impl Into<CornerRadius>) -> Self    // :277
#[inline] pub fn outer_margin(mut self, outer_margin: impl Into<Margin>) -> Self            // :296
#[inline] pub fn shadow(mut self, shadow: Shadow) -> Self                                   // :303
pub fn multiply_with_opacity(mut self, opacity: f32) -> Self                                // :313
pub fn total_margin(&self) -> MarginF32                                                     // :327
pub fn fill_rect(&self, content_rect: Rect) -> Rect                                         // :336
pub fn widget_rect(&self, content_rect: Rect) -> Rect                                       // :343
pub fn outer_rect(&self, content_rect: Rect) -> Rect                                        // :350
pub fn begin(self, ui: &mut Ui) -> Prepared                                                 // :378
pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> // :404
pub fn show_dyn<'c, R>(..)                                                                  // :411
pub fn paint(&self, content_rect: Rect) -> Shape                                            // :423
```

`Prepared` (`:357`) has `pub frame: Frame` `:362` and `pub content_ui: Ui` `:368`, plus
`allocate_space(&self, ui: &mut Ui) -> Response` `:466`, `paint(&self, ui: &Ui)` `:473`,
`end(self, ui: &mut Ui) -> Response` `:486`.

### Geometry types used by `Frame`

```rust
pub struct Margin { pub left: i8, pub right: i8, pub top: i8, pub bottom: i8 }
pub const ZERO: Self;                                   // epaint/src/margin.rs:23
pub const fn same(margin: i8) -> Self;                  // :33
pub const fn symmetric(x: i8, y: i8) -> Self;           // :44
pub const fn leftf/rightf/topf/bottomf(self) -> f32;    // :55/:61/:67/:73
pub fn sum(self) -> Vec2;                               // :79
pub const fn left_top(self) -> Vec2;                    // :84
pub const fn right_bottom(self) -> Vec2;                // :89
```
`epaint/src/margin.rs:15-96`. `From<i8>` `:101`, `From<f32>` (rounds) `:108`, `From<Vec2>` `:115`.

```rust
pub struct CornerRadius { pub nw: u8, pub ne: u8, pub sw: u8, pub se: u8 }
pub const ZERO: Self;                    // epaint/src/corner_radius.rs:50
pub const fn same(radius: u8) -> Self;   // :59
pub fn is_same(self) -> bool;            // :70
pub fn at_least(self, min: u8) -> Self;  // :76
pub fn at_most(self, max: u8) -> Self;   // :87
pub fn average(&self) -> f32;            // :97
```
`epaint/src/corner_radius.rs:13-97`. `From<u8>` `:34`, `From<f32>` (rounds) `:41` — so both
`.corner_radius(6)` and `.corner_radius(6.0)` compile.

```rust
pub struct Shadow {
    pub offset: [i8; 2],
    pub blur: u8,
    pub spread: u8,
    pub color: Color32,
}
```
`epaint/src/shadow.rs:10-27`

```rust
pub struct Stroke { pub width: f32, pub color: Color32 }
pub const NONE: Self;                                    // epaint/src/stroke.rs:20
#[inline] pub fn new(width: f32, color: impl Into<Color32>) -> Self;   // :26

pub enum StrokeKind { Inside, Middle, Outside }          // epaint/src/stroke.rs:102-111

pub struct PathStroke { pub width: f32, pub color: ColorMode, pub kind: StrokeKind } // :118-122
```

---

## 7. Panels — `Panel` and `CentralPanel`

> **`TopBottomPanel` and `SidePanel` do not exist in 0.36** (`grep -rn 'TopBottomPanel\|SidePanel' src/`
> returns nothing). There is a single `Panel` type with four constructors, and its `show` takes a
> **`&mut Ui`**, not a `&Context`.

```rust
#[must_use = "You should call .show()"]
pub struct Panel { /* all fields private */ }
```
`src/containers/panel.rs:205-243`

```rust
pub fn left(id: impl Into<Id>) -> Self    // :249
pub fn right(id: impl Into<Id>) -> Self   // :256
pub fn top(id: impl Into<Id>) -> Self     // :265  — NOT resizable by default
pub fn bottom(id: impl Into<Id>) -> Self  // :274  — NOT resizable by default
```

Defaults, from the private `Panel::new` (`:281-305`):

| | left / right | top / bottom |
|---|---|---|
| `default_outer_size` | `Some(200.0)` | `None` (sized by content) |
| `outer_size_range` | `96.0..=INFINITY` | `20.0..=INFINITY` |
| `resizable` | `true` | `false` (set by `top`/`bottom`) |

All sizes are **outer** sizes, i.e. they include the `Frame` margin and border (`:214-220`).

Builders:

```rust
#[inline] pub fn resizable(mut self, resizable: bool) -> Self                       // :322
#[inline] pub fn drag_to_open(mut self, drag_to_open: bool) -> Self                 // :340
#[inline] pub fn show_separator_line(mut self, show_separator_line: bool) -> Self   // :362
#[inline] pub fn default_size(mut self, default_size: f32) -> Self                  // :369
#[inline] pub fn min_size(mut self, min_size: f32) -> Self                          // :380
#[inline] pub fn max_size(mut self, max_size: f32) -> Self                          // :387
#[inline] pub fn size_range(mut self, size_range: impl Into<Rangef>) -> Self        // :394
#[inline] pub fn exact_size(mut self, size: f32) -> Self                            // :405
#[inline] pub fn frame(mut self, frame: Frame) -> Self                              // :413
```

Show methods:

```rust
pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> // :422

#[deprecated = "Renamed to `show`"]
pub fn show_inside<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R)
    -> InnerResponse<R>                                                                        // :427-432

pub fn show_collapsible<R>(
    self,
    ui: &mut Ui,
    is_expanded: &mut bool,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> Option<InnerResponse<R>>                                                                  // :451-456

#[deprecated = "Renamed to `show_collapsible`"]
pub fn show_animated_inside<R>(self, ui: &mut Ui, is_expanded: bool, add_contents: ..)
    -> Option<InnerResponse<R>>                                                                // :496-502

pub fn show_switched<R>(
    ui: &mut Ui,
    is_expanded: &mut bool,
    collapsed_panel: Self,
    expanded_panel: Self,
    add_contents: impl FnOnce(&mut Ui, bool) -> R,
) -> InnerResponse<R>                                                                          // :563-569

#[deprecated = "Renamed to `show_switched`"]
pub fn show_animated_between_inside<R>(
    ui: &mut Ui,
    is_expanded: bool,
    collapsed_panel: Self,
    expanded_panel: Self,
    add_contents: impl FnOnce(&mut Ui, f32) -> R,
) -> InnerResponse<R>                                                                          // :659-666
```

Note the `&mut bool`: `show_collapsible` / `show_switched` can flip your flag themselves when the
user drags the resize edge past the size limits, or double-clicks it (`:443-450`, `:517-534`).
`show_switched` `debug_assert!`s that the two panels have **distinct ids** (`:570-576`).

```rust
#[must_use = "You should call .show()"]
#[derive(Default)]
pub struct CentralPanel { /* frame: Option<Frame> */ }

pub fn no_frame() -> Self                           // :1193  (Frame::NONE)
pub fn default_margins() -> Self                    // :1200
#[inline] pub fn frame(mut self, frame: Frame) -> Self // :1206
pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> // :1212
#[deprecated = "Renamed to `show`"]
pub fn show_inside<R>(..) -> InnerResponse<R>       // :1217-1223
```
`src/containers/panel.rs:1185-1224`. `CentralPanel::default()` works (it derives `Default`), and
it lays out `Layout::top_down(Align::Min)` over `ui.available_rect_before_wrap()` (`:1234-1240`).

```rust
#[derive(Clone, Copy, Debug)]
pub struct PanelState {
    /// The _outer_ rect of the panel, i.e. including the `Frame` margin & border.
    pub outer_rect: Rect,                                   // src/containers/panel.rs:48
}
pub fn load(ctx: &Context, bar_id: Id) -> Option<Self>      // :52
pub fn size(&self) -> Vec2                                  // :58
```

**Ordering rules** (module docs, `src/containers/panel.rs:9-16`): the first panel you add is the
outermost, the last is the innermost; never open one top-level panel from inside another; always
add `CentralPanel` last; add `Window`s after all top-level panels.

```rust
// In eframe 0.36, `App::ui` hands you a `&mut Ui`, so panels nest naturally:
fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    egui::Panel::top(egui::Id::new("menu_bar")).show(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| { /* … */ });
    });
    egui::Panel::left("sidebar")
        .default_size(240.0)
        .size_range(160.0..=420.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| { /* … */ });
        });
    egui::CentralPanel::default().show(ui, |ui| { /* … */ });
}
```

---

## 8. `Window`, `Area`, `Modal`

### 8.1 `Window`

```rust
pub fn new(title: impl IntoAtoms<'a>) -> Self                          // src/containers/window.rs:102
pub fn from_viewport(id: ViewportId, viewport: ViewportBuilder) -> Self // :126
pub fn show<R>(
    self,
    ctx: &Context,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> Option<InnerResponse<Option<R>>>                                   // :543-547
```

Builders (`src/containers/window.rs`):
`id` `:162`, `open(&'a mut bool)` `:173`, `enabled` `:180`, `interactable` `:191`, `movable` `:201`,
`drag_area(WindowDrag)` `:215`, `order(Order)` `:222`, `fade_in` `:231`, `fade_out` `:242`,
`mutate` `:250`, `resize(impl Fn(Resize) -> Resize)` `:258`, `frame(Frame)` `:265`,
`title_frame(Frame)` `:272`, `min_width` `:279`, `min_height` `:286`, `min_size` `:296`,
`max_width` `:303`, `max_height` `:310`, `max_size` `:320`, `current_pos` `:328`,
`default_pos` `:335`, `fixed_pos` `:342`, `constrain` `:353`, `constrain_to(Rect)` `:362`,
`pivot(Align2)` `:375`, `anchor(Align2, impl Into<Vec2>)` `:392`, `default_open` `:399`,
`default_size` `:409`, `default_width` `:418`, `default_height` `:426`, `fixed_size` `:437`,
`default_rect` `:443`, `fixed_rect` `:448`, `resizable(impl Into<Vec2b>)` `:462`,
`collapsible` `:470`, `title_bar` `:478`, `auto_sized` `:487`, `scroll(impl Into<Vec2b>)` `:498`,
`hscroll` `:505`, `vscroll` `:512`, `drag_to_scroll(DragScroll)` `:523`,
`scroll_bar_visibility(ScrollBarVisibility)` `:533`.

Note `Window::new` takes `impl IntoAtoms<'a>`, not `impl Into<WidgetText>` (`:102`).

### 8.2 `Area`

```rust
pub fn show<R>(
    self,
    ctx: &Context,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R>                                                  // src/containers/area.rs:406-410
```

Builders: `new(id)` `:133`, `id` `:159`, `kind(UiKind)` `:168`, `info(UiStackInfo)` `:177`,
`layer() -> LayerId` `:182`, `enabled` `:191`, `movable` `:198`, `is_enabled` `:204`,
`is_movable` `:208`, `interactable` `:218`, `sense(Sense)` `:228`, `order(Order)` `:235`,
`default_pos` `:241`, `default_size` `:256`, `default_width` `:263`, `default_height` `:270`,
`fixed_pos` `:277`, `constrain` `:287`, `constrain_to` `:296`, `pivot(Align2)` `:310`,
`current_pos` `:317`, `anchor(Align2, impl Into<Vec2>)` `:334`, `fade_in` `:351`,
`layout(Layout)` `:358`, `sizing_pass(bool)` `:379`.

`AreaState` (`:18`): `load(ctx, id) -> Option<Self>` `:58`, `left_top_pos() -> Pos2` `:64`,
`set_left_top_pos(&mut self, pos: Pos2)` `:75`, `rect() -> Rect` `:84`.

### 8.3 `Modal` — **yes, `egui::Modal` exists in 0.36**

```rust
/// A modal dialog.
pub struct Modal {
    pub area: Area,
    pub backdrop_color: Color32,
    pub frame: Option<Frame>,
}

impl Modal {
    /// The id is passed to the area.
    pub fn new(id: Id) -> Self;                                        // src/containers/modal.rs:26
    pub fn default_area(id: Id) -> Area;                               // :40
    #[inline] pub fn frame(mut self, frame: Frame) -> Self;            // :53
    #[inline] pub fn backdrop_color(mut self, color: Color32) -> Self; // :62
    #[inline] pub fn area(mut self, area: Area) -> Self;               // :71
    pub fn show<T>(self, ctx: &Context, content: impl FnOnce(&mut Ui) -> T) -> ModalResponse<T>; // :77
}
```
`src/containers/modal.rs:16-77`

```rust
pub struct ModalResponse<T> {
    pub response: Response,
    pub backdrop_response: Response,
    pub inner: T,
    pub is_top_modal: bool,
    pub any_popup_open: bool,
}

impl<T> ModalResponse<T> {
    /// true if: backdrop clicked, `ui.close()` called, or (topmost && no popup && Escape)
    pub fn should_close(&self) -> bool;                                // :151
}
```
`src/containers/modal.rs:124-163`. Default backdrop is `Color32::from_black_alpha(100)` (`:29`);
default frame is `Frame::popup(ui.style())` (`:100`).

```rust
egui::Modal::new(egui::Id::new("my_modal")).show(ctx, |ui| {
    ui.label("Are you sure?");
    if ui.button("OK").clicked() {
        ui.close();
    }
});
```

---

## 9. `Popup`, `ComboBox`, menus, tooltips

### 9.1 `Popup`

```rust
#[must_use = "Call `.show()` to actually display the popup"]
pub struct Popup<'a> { /* private */ }
```
`src/containers/popup.rs:165`

Constructors:

```rust
pub fn new(id: Id, ctx: Context, anchor: impl Into<PopupAnchor>, layer_id: LayerId) -> Self // :191
pub fn from_response(response: &Response) -> Self                                           // :217
pub fn from_toggle_button_response(button_response: &Response) -> Self                      // :230
pub fn menu(button_response: &Response) -> Self                                             // :237
pub fn context_menu(response: &Response) -> Self                                            // :248
```

Builders: `kind(PopupKind)` `:264`, `info(UiStackInfo)` `:271`, `align(RectAlign)` `:280`,
`align_alternatives(&'a [RectAlign])` `:289`, `open(bool)` `:296`,
`open_memory(impl Into<Option<SetOpenCommand>>)` `:308`, `open_bool(&'a mut bool)` `:317`,
`close_behavior(PopupCloseBehavior)` `:326`, `at_pointer()` `:333`, `at_pointer_fixed()` `:341`,
`at_position(Pos2)` `:348`, `anchor(impl Into<PopupAnchor>)` `:355`, `gap(f32)` `:362`,
`frame(Frame)` `:369`, `interactable(bool)` `:378`, `sense(Sense)` `:385`, `layout(Layout)` `:392`,
`width(f32)` `:399`, `id(Id)` `:406`, `style(impl Into<StyleModifier>)` `:417`.

Queries: `ctx()` `:423`, `get_anchor()` `:428`, `get_anchor_rect()` `:435`, `get_popup_rect()` `:443`,
`get_id()` `:454`, `is_open()` `:459`, `get_expected_size()` `:469`, `get_best_align()` `:474`.

```rust
/// Returns `None` if the popup is not open, or anchor is Pointer with no pointer.
pub fn show<R>(self, content: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>> // :508
```

Statics:

```rust
pub fn default_response_id(response: &Response) -> Id            // :653
pub fn is_id_open(ctx: &Context, popup_id: Id) -> bool           // :667
pub fn is_any_open(ctx: &Context) -> bool                        // :674
pub fn open_id(ctx: &Context, popup_id: Id)                      // :679
pub fn toggle_id(ctx: &Context, popup_id: Id)                    // :686
pub fn close_all(ctx: &Context)                                  // :691
pub fn close_id(ctx: &Context, popup_id: Id)                     // :698
pub fn position_of_id(ctx: &Context, popup_id: Id) -> Option<Pos2> // :703
```

Enums:

```rust
pub enum PopupAnchor {
    ParentRect(Rect),   // :26 — relative to some parent rect (global coords)
    Pointer,            // :29 — follows the mouse
    PointerFixed,       // :32 — remembers the press position (context-menu style)
    Position(Pos2),     // :35
}                                                                    // :24-36
impl PopupAnchor { pub fn rect(self, popup_id: Id, ctx: &Context) -> Option<Rect>; } // :65
// `From<Rect>` :38, `From<Pos2>` :44, `From<&Response>` :50

pub enum PopupCloseBehavior {
    CloseOnClick,          // :82
    CloseOnClickOutside,   // :86
    IgnoreClicks,          // :90
}                                                                    // :77-91

pub enum SetOpenCommand {
    Bool(bool),   // :96 — set the open state
    Toggle,       // :99
}                                                                    // :93-100
impl From<bool> for SetOpenCommand { .. }                            // :102

pub enum PopupKind { Popup, Tooltip, Menu }                          // :137-140
impl PopupKind { pub fn order(self) -> Order; }                      // :145
```

### 9.2 `ComboBox`

```rust
pub fn new(id_salt: impl AsIdSalt, label: impl Into<WidgetText>) -> Self  // src/containers/combo_box.rs:54
pub fn from_label(label: impl Into<WidgetText>) -> Self                   // :69
pub fn from_id_salt(id_salt: impl AsIdSalt) -> Self                       // :85
pub fn width(mut self, width: f32) -> Self                                // :103
pub fn height(mut self, height: f32) -> Self                              // :112
pub fn selected_text(mut self, selected_text: impl Into<WidgetText>) -> Self // :119
pub fn icon(mut self, icon_fn: impl FnOnce(&Ui, Rect, &WidgetVisuals, bool) + 'static) -> Self // :155
pub fn wrap_mode(mut self, wrap_mode: TextWrapMode) -> Self               // :166
pub fn wrap(mut self) -> Self                                             // :173
pub fn truncate(mut self) -> Self                                         // :180
pub fn close_behavior(mut self, close_behavior: PopupCloseBehavior) -> Self // :189
pub fn popup_style(mut self, popup_style: StyleModifier) -> Self          // :199

/// Returns `InnerResponse { inner: None }` if the combo box is closed.
pub fn show_ui<R>(
    self,
    ui: &mut Ui,
    menu_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<Option<R>>                                             // :207-211

pub fn show_index<Text: Into<WidgetText>>(
    self,
    ui: &mut Ui,
    selected: &mut usize,
    len: usize,
    get: impl Fn(usize) -> Text,
) -> Response                                                             // :280-286

pub fn is_open(ctx: &Context, id: Id) -> bool                             // :309
```

### 9.3 Menus (`egui::menu` module, `egui::MenuBar` at crate root)

```rust
pub fn menu_style(style: &mut Style)                                      // src/containers/menu.rs:22
pub fn find_menu_root(ui: &Ui) -> &UiStack                                // :32
pub fn is_in_menu(ui: &Ui) -> bool                                        // :47

pub struct MenuConfig { .. }                                              // :65
impl MenuConfig {
    pub fn new() -> Self;                                                 // :92
    pub fn close_behavior(mut self, close_behavior: PopupCloseBehavior) -> Self; // :98
    pub fn style(mut self, style: impl Into<StyleModifier>) -> Self;      // :107
    pub fn find(ui: &Ui) -> Self;                                         // :124
}

pub struct MenuBar { .. }                                                 // :217
impl MenuBar {
    pub fn new() -> Self;                                                 // :232
    pub fn style(mut self, style: impl Into<StyleModifier>) -> Self;      // :241
    pub fn config(mut self, config: MenuConfig) -> Self;                  // :250
    pub fn ui<R>(self, ui: &mut Ui, content: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>; // :257
}

pub struct MenuButton<'a> { .. }                                          // :290
impl<'a> MenuButton<'a> {
    pub fn new(atoms: impl IntoAtoms<'a>) -> Self;                        // :296
    pub fn config(mut self, config: MenuConfig) -> Self;                  // :302
    pub fn from_button(button: Button<'a>) -> Self;                       // :309
    pub fn ui<R>(..);                                                     // :317
}

pub struct SubMenuButton<'a> { .. }  // :337, new :346, from_button :354, config :365, ui :371
pub struct SubMenu { .. }            // :399, new :404, config :412,
                                     // id_from_widget_id(Id) -> Id :418, show :426
```

On `Ui`:

```rust
pub fn menu_button<'a, R>(
    &mut self,
    atoms: impl IntoAtoms<'a>,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<Option<R>>                                             // src/ui.rs:2787-2791
pub fn menu_image_button<'a, R>(..)                                       // :2821
pub fn menu_image_text_button<'a, R>(..)                                  // :2858
```

`Ui::menu_button` dispatches to `SubMenuButton` when already inside a menu (`src/ui.rs:2792-2796`).

### 9.4 `Tooltip`

```rust
pub struct Tooltip<'a> {
    pub popup: Popup<'a>,
    /* parent_layer, parent_widget private */
}
```
`src/containers/tooltip.rs:8-16`

```rust
pub fn always_open(
    ctx: Context,
    parent_layer: LayerId,
    parent_widget: Id,
    anchor: impl Into<PopupAnchor>,
) -> Self                                                     // :20-25
pub fn for_widget(response: &Response) -> Self                // :39
pub fn for_enabled(response: &Response) -> Self               // :53
pub fn for_disabled(response: &Response) -> Self              // :62
pub fn at_pointer(mut self) -> Self                           // :72
pub fn gap(mut self, gap: f32) -> Self                        // :81
pub fn layout(mut self, layout: Layout) -> Self               // :88
pub fn width(mut self, width: f32) -> Self                    // :95
pub fn show<R>(self, content: impl FnOnce(&mut crate::Ui) -> R) -> Option<InnerResponse<R>> // :101
pub fn seconds_since_last_tooltip(ctx: &Context) -> f32       // :171
pub fn next_tooltip_id(ctx: &Context, widget_id: Id) -> Id    // :189
pub fn tooltip_id(widget_id: Id, tooltip_count: usize) -> Id  // :199
pub fn should_show_tooltip(response: &Response, allow_interactive_tooltip: bool) -> bool // :221
pub fn was_tooltip_open_last_frame(ctx: &Context, widget_id: Id) -> bool // :394
```

For everyday use, prefer the `Response::on_hover_*` family in §2.6.

---

## 10. `ScrollArea`

```rust
pub fn horizontal() -> Self                                          // src/containers/scroll_area.rs:369
pub fn vertical() -> Self                                            // :375
pub fn both() -> Self                                                // :381
pub fn neither() -> Self                                             // :388
pub fn new(direction_enabled: impl Into<Vec2b>) -> Self              // :394
pub fn max_width(mut self, max_width: f32) -> Self                   // :421
pub fn max_height(mut self, max_height: f32) -> Self                 // :432
pub fn min_scrolled_width(mut self, min_scrolled_width: f32) -> Self // :444
pub fn min_scrolled_height(mut self, min_scrolled_height: f32) -> Self // :456
pub fn scroll_bar_visibility(mut self, scroll_bar_visibility: ScrollBarVisibility) -> Self // :465
pub fn scroll_bar_rect(mut self, scroll_bar_rect: Rect) -> Self      // :475
pub fn id_salt(mut self, id_salt: impl AsIdSalt) -> Self             // :482
pub fn scroll_offset(mut self, offset: Vec2) -> Self                 // :495
pub fn vertical_scroll_offset(mut self, offset: f32) -> Self         // :508
pub fn horizontal_scroll_offset(mut self, offset: f32) -> Self       // :520
pub fn on_hover_cursor(mut self, cursor: CursorIcon) -> Self         // :532
pub fn on_drag_cursor(mut self, cursor: CursorIcon) -> Self          // :544
pub fn hscroll(mut self, hscroll: bool) -> Self                      // :551
pub fn vscroll(mut self, vscroll: bool) -> Self                      // :558
pub fn scroll(mut self, direction_enabled: impl Into<Vec2b>) -> Self // :567
pub fn scroll_source(mut self, scroll_source: ScrollSource) -> Self  // :582
pub fn wheel_scroll_multiplier(mut self, multiplier: Vec2) -> Self   // :593
pub fn auto_shrink(mut self, auto_shrink: impl Into<Vec2b>) -> Self  // :605
pub fn animated(mut self, animated: bool) -> Self                    // :614
pub fn content_margin(mut self, margin: impl Into<Margin>) -> Self   // :631
pub fn stick_to_right(mut self, stick: bool) -> Self                 // :643
pub fn stick_to_bottom(mut self, stick: bool) -> Self                // :655
```

```rust
pub fn show<R>(
    self,
    ui: &mut Ui,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> ScrollAreaOutput<R>                                             // :959-963

pub fn show_rows<R>(
    self,
    ui: &mut Ui,
    row_height_sans_spacing: f32,
    total_rows: usize,
    add_contents: impl FnOnce(&mut Ui, std::ops::Range<usize>) -> R,
) -> ScrollAreaOutput<R>                                             // :983-989

pub fn show_viewport<R>(
    self,
    ui: &mut Ui,
    add_contents: impl FnOnce(&mut Ui, Rect) -> R,
) -> ScrollAreaOutput<R>                                             // :1020-1024
```

```rust
pub struct ScrollAreaOutput<R> {
    pub inner: R,
    pub id: Id,
    pub state: State,
    pub content_size: Vec2,
    pub inner_rect: Rect,
}
```
`:89-105`

```rust
pub enum ScrollBarVisibility { AlwaysHidden, VisibleWhenNeeded, AlwaysVisible } // :110-127
// Default = VisibleWhenNeeded (:129-134); pub const ALL: [Self; 3] (:137)

pub enum DragScroll { Never, #[default] OnTouch, Always }                       // :147-158
impl DragScroll { pub fn enabled(self, ctx: &Context) -> bool; }                // :165

pub struct ScrollSource {
    pub scroll_bar: bool,     // :197
    pub drag: DragScroll,     // :204
    pub mouse_wheel: bool,    // :208
}
// consts: NONE :223, ALL :228, SCROLL_BAR :233, DRAG :238, MOUSE_WHEEL :243
// fns: is_none :251, any :257, is_all :263
```

`State`: `load(ctx, id) -> Option<Self>` `:75`, `store(self, ctx, id)` `:79`,
`velocity() -> Vec2` `:84`.

`Ui`-side scrolling: `scroll_to_rect` `src/ui.rs:1399`, `scroll_to_rect_animation` `:1404`,
`scroll_to_cursor` `:1441`, `scroll_to_cursor_animation` `:1446`, `scroll_with_delta` `:1490`,
`scroll_with_delta_animation(delta: Vec2, animation: style::ScrollAnimation)` `:1495`.

---

## 11. Widgets

`widgets/mod.rs:29-45` is the definitive export list:
`Button`, `Checkbox`, `DragValue`, `Hyperlink`, `Link`, `Image` (+ `FrameDurations`, `ImageFit`,
`ImageOptions`, `ImageSize`, `ImageSource`, `decode_animated_image_uri`, `has_gif_magic_header`,
`has_webp_header`, `paint_texture_at`), `Label`, `ProgressBar`, `RadioButton`, `Separator`,
`Slider`, `SliderClamping`, `SliderOrientation`, `Spinner`, `TextBuffer`, `TextEdit`.
**No `ImageButton`, no `SelectableLabel`.**

### 11.1 `Button<'a>`

```rust
pub fn new(atoms: impl IntoAtoms<'a>) -> Self                                   // src/widgets/button.rs:45
pub fn selectable(selected: bool, atoms: impl IntoAtoms<'a>) -> Self            // :78
pub fn image(image: impl Into<Image<'a>>) -> Self                               // :89
pub fn image_and_text(image: impl Into<Image<'a>>, text: impl Into<WidgetText>) -> Self // :97
pub fn opt_image_and_text(image: Option<Image<'a>>, text: Option<WidgetText>) -> Self   // :105
pub fn wrap_mode(mut self, wrap_mode: TextWrapMode) -> Self                     // :123
pub fn wrap(self) -> Self                                                       // :130
pub fn truncate(self) -> Self                                                   // :136
pub fn fill(mut self, fill: impl Into<Color32>) -> Self                         // :143
pub fn stroke(mut self, stroke: impl Into<Stroke>) -> Self                      // :151
pub fn small(mut self) -> Self                                                  // :159
pub fn frame(mut self, frame: bool) -> Self                                     // :166
pub fn frame_when_inactive(mut self, frame_when_inactive: bool) -> Self         // :178
pub fn sense(mut self, sense: Sense) -> Self                                    // :186
pub fn min_size(mut self, min_size: Vec2) -> Self                               // :193
pub fn corner_radius(mut self, corner_radius: impl Into<CornerRadius>) -> Self  // :200
pub fn image_tint_follows_text_color(mut self, image_tint_follows_text_color: bool) -> Self // :212
pub fn shortcut_text(mut self, shortcut_text: impl IntoAtoms<'a>) -> Self       // :225
pub fn left_text(mut self, left_text: impl IntoAtoms<'a>) -> Self               // :241
pub fn right_text(mut self, right_text: impl IntoAtoms<'a>) -> Self             // :253
pub fn selected(mut self, selected: bool) -> Self                               // :270
pub fn gap(mut self, gap: f32) -> Self                                          // :277
pub fn atoms(&self) -> &Atoms<'a>                                               // :285
pub fn atom_ui(self, ui: &mut Ui) -> AtomLayoutResponse                         // :290
```

The "image button" of old:

```rust
// 0.36 idiom — no ImageButton type
ui.add(egui::Button::image(egui::include_image!("icon.png")));
ui.add(egui::Button::image_and_text(egui::include_image!("icon.png"), "Save"));
ui.button((egui::include_image!("icon.png"), "Save")); // via IntoAtoms tuple
```

### 11.2 `Slider<'a>`

```rust
pub fn new<Num: emath::Numeric>(
    value: &'a mut Num,
    range: impl Into<RangeInclusive<Num>>,
) -> Self                                                             // src/widgets/slider.rs:128-131

pub fn from_get_set(
    range: RangeInclusive<f64>,
    get_set_value: impl 'a + FnMut(Option<f64>) -> f64,
) -> Self                                                             // :144-147
```

Builders:

```rust
pub fn show_value(mut self, show_value: bool) -> Self                 // :178
pub fn prefix(mut self, prefix: impl ToString) -> Self                // :185   <-- ToString, NOT IntoAtoms
pub fn suffix(mut self, suffix: impl ToString) -> Self                // :192
pub fn text(mut self, text: impl Into<WidgetText>) -> Self            // :199
pub fn text_color(mut self, text_color: Color32) -> Self              // :205
pub fn orientation(mut self, orientation: SliderOrientation) -> Self  // :212
pub fn vertical(mut self) -> Self                                     // :219
pub fn logarithmic(mut self, logarithmic: bool) -> Self               // :229
pub fn smallest_positive(mut self, smallest_positive: f64) -> Self    // :238
pub fn largest_finite(mut self, largest_finite: f64) -> Self          // :247
pub fn clamping(mut self, clamping: SliderClamping) -> Self           // :290
pub fn smart_aim(mut self, smart_aim: bool) -> Self                   // :298
pub fn step_by(mut self, step: f64) -> Self                           // :310
pub fn drag_value_speed(mut self, drag_value_speed: f64) -> Self      // :324
pub fn min_decimals(mut self, min_decimals: usize) -> Self            // :335
pub fn max_decimals(mut self, max_decimals: usize) -> Self            // :347
pub fn max_decimals_opt(mut self, max_decimals: Option<usize>) -> Self// :353
pub fn fixed_decimals(mut self, num_decimals: usize) -> Self          // :364
pub fn trailing_fill(mut self, trailing_fill: bool) -> Self           // :377
pub fn handle_shape(mut self, handle_shape: HandleShape) -> Self      // :387
pub fn custom_formatter(
    mut self,
    formatter: impl 'a + Fn(f64, RangeInclusive<usize>) -> String,
) -> Self                                                             // :429-432
pub fn custom_parser(mut self, parser: impl 'a + Fn(&str) -> Option<f64>) -> Self // :473
pub fn binary(self, min_width: usize, twos_complement: bool) -> Self  // :497
pub fn octal(self, min_width: usize, twos_complement: bool) -> Self   // :532
pub fn hexadecimal(self, min_width: usize, twos_complement: bool, upper: bool) -> Self // :567
pub fn integer(self) -> Self                                          // :594
pub fn update_while_editing(mut self, update: bool) -> Self           // :642
```

```rust
pub enum SliderOrientation { Horizontal, Vertical }                   // :51-54
pub enum SliderClamping {
    /// Not clamped (the slider part still is). Keyboard/drag-value edits can leave the range.
    Never,      // :66
    /// New values the user enters are clamped; existing out-of-range values survive.
    Edits,      // :71
    /// Always clamp, even existing values. **Default.**
    Always,     // :75
}                                                                     // :59-76
pub enum HandleShape { Circle, Rect { aspect_ratio: f32 } }           // src/style.rs:1234-1243
```

`Slider::new` auto-calls `.integer()` for integral `Num` (`src/widgets/slider.rs:141`).

### 11.3 `DragValue<'a>`

```rust
pub fn new<Num: emath::Numeric>(value: &'a mut Num) -> Self           // src/widgets/drag_value.rs:53
pub fn from_get_set(get_set_value: impl 'a + FnMut(Option<f64>) -> f64) -> Self // :68
pub fn speed(mut self, speed: impl Into<f64>) -> Self                 // :89
pub fn range<Num: emath::Numeric>(mut self, range: RangeInclusive<Num>) -> Self // :99
pub fn clamp_existing_to_range(mut self, clamp_existing_to_range: bool) -> Self // :145
pub fn prefix(mut self, prefix: impl IntoAtoms<'a>) -> Self           // :152   <-- IntoAtoms!
pub fn suffix(mut self, suffix: impl IntoAtoms<'a>) -> Self           // :159
pub fn min_decimals(mut self, min_decimals: usize) -> Self            // :169
pub fn max_decimals(mut self, max_decimals: usize) -> Self            // :180
pub fn max_decimals_opt(mut self, max_decimals: Option<usize>) -> Self// :186
pub fn fixed_decimals(mut self, num_decimals: usize) -> Self          // :196
pub fn custom_formatter(..)                                           // :240
pub fn custom_parser(mut self, parser: impl 'a + Fn(&str) -> Option<f64>) -> Self // :285
pub fn binary(self, min_width: usize, twos_complement: bool) -> Self  // :309
pub fn octal(self, min_width: usize, twos_complement: bool) -> Self   // :344
pub fn hexadecimal(self, min_width: usize, twos_complement: bool, upper: bool) -> Self // :379
pub fn update_while_editing(mut self, update: bool) -> Self           // :408
pub fn atoms(&self) -> &Atoms<'a>                                     // :416
```

> Note the asymmetry: `DragValue::prefix` takes `IntoAtoms`, `Slider::prefix` takes `ToString`.
> `DragValue` uses `.range(..)`, **not** `.clamp_range(..)`.

### 11.4 `TextEdit<'t>`

```rust
pub fn load_state(ctx: &Context, id: Id) -> Option<TextEditState>     // src/widgets/text_edit/builder.rs:102
pub fn store_state(ctx: &Context, id: Id, state: TextEditState)       // :106
pub fn singleline(text: &'t mut dyn TextBuffer) -> Self               // :113
pub fn multiline(text: &'t mut dyn TextBuffer) -> Self                // :123
pub fn code_editor(self) -> Self                                      // :162
pub fn id(mut self, id: Id) -> Self                                   // :168
pub fn id_source(self, id_salt: impl AsIdSalt) -> Self                // :175
pub fn id_salt(mut self, id_salt: impl AsIdSalt) -> Self              // :181
pub fn hint_text(mut self, hint_text: impl IntoAtoms<'static>) -> Self// :209
pub fn prefix(mut self, prefix: impl IntoAtoms<'static>) -> Self      // :216
pub fn suffix(mut self, suffix: impl IntoAtoms<'static>) -> Self      // :223
pub fn background_color(mut self, color: Color32) -> Self             // :231
pub fn password(mut self, password: bool) -> Self                     // :238
pub fn font(mut self, font_selection: impl Into<FontSelection>) -> Self // :245
pub fn text_color(mut self, text_color: Color32) -> Self              // :251
pub fn text_color_opt(mut self, text_color: Option<Color32>) -> Self  // :257
pub fn layouter(..)                                                   // :286
pub fn interactive(mut self, interactive: bool) -> Self               // :299
pub fn frame(mut self, frame: Frame) -> Self                          // :306   <-- takes a Frame, not a bool
pub fn margin(mut self, margin: impl Into<Margin>) -> Self            // :313
pub fn desired_width(mut self, desired_width: f32) -> Self            // :321
pub fn desired_rows(mut self, desired_height_rows: usize) -> Self     // :330
pub fn lock_focus(mut self, tab_will_indent: bool) -> Self            // :341
pub fn cursor_at_end(mut self, b: bool) -> Self                       // :350
pub fn clip_text(mut self, b: bool) -> Self                           // :361
pub fn char_limit(mut self, limit: usize) -> Self                     // :373
pub fn horizontal_align(mut self, align: Align) -> Self               // :380
pub fn vertical_align(mut self, align: Align) -> Self                 // :387
pub fn min_size(mut self, min_size: Vec2) -> Self                     // :394
pub fn return_key(mut self, return_key: impl Into<Option<KeyboardShortcut>>) -> Self // :406
pub fn show(self, ui: &mut Ui) -> TextEditOutput                      // :436
```

```rust
pub struct TextEditOutput {
    pub response: crate::AtomLayoutResponse,   // NOT a plain Response
    pub galley: Arc<crate::Galley>,
    pub galley_pos: crate::Pos2,
    pub text_clip_rect: crate::Rect,
    pub state: super::TextEditState,
    pub cursor_range: Option<CCursorRange>,
}
```
`src/widgets/text_edit/output.rs:6-24`

```rust
pub struct AtomLayoutResponse { pub response: Response, /* private custom_rects */ }
pub fn empty(response: Response) -> Self;                         // src/atomics/atom_layout.rs:708
pub fn custom_rects(&self) -> impl Iterator<Item = (Id, Rect)> + '_; // :715
pub fn rect(&self, id: Id) -> Option<Rect>;                       // :722
```

### 11.5 Other widgets

```rust
// Label — src/widgets/label.rs
pub fn new(text: impl Into<WidgetText>) -> Self                    // :35
pub fn text(&self) -> &str                                        // :46
pub fn wrap_mode(mut self, wrap_mode: TextWrapMode) -> Self        // :56
pub fn wrap(mut self) -> Self / truncate :71 / extend :79          // :63
pub fn halign(mut self, align: Align) -> Self                     // :86
pub fn selectable(mut self, selectable: bool) -> Self             // :95
pub fn sense(mut self, sense: Sense) -> Self                      // :115
pub fn show_tooltip_when_elided(mut self, show: bool) -> Self     // :132
pub fn layout_in_ui(self, ui: &mut Ui) -> (Pos2, Arc<Galley>, Response) // :140

// Checkbox<'a> — src/widgets/checkbox.rs
pub fn new(checked: &'a mut bool, atoms: impl IntoAtoms<'a>) -> Self // :31
pub fn without_text(checked: &'a mut bool) -> Self                 // :40
pub fn atoms(&self) -> &Atoms<'a>                                  // :47
pub fn indeterminate(mut self, indeterminate: bool) -> Self        // :56

// RadioButton<'a> — src/widgets/radio_button.rs
pub fn new(checked: bool, atoms: impl IntoAtoms<'a>) -> Self       // :32
pub fn atoms(&self) -> &Atoms<'a>                                  // :42

// ProgressBar — src/widgets/progress_bar.rs
pub fn new(progress: f32) -> Self                                  // :27
pub fn desired_width(f32) :41 / desired_height(f32) :48
pub fn fill(Color32) :55 / text(impl Into<WidgetText>) :62
pub fn show_percentage() :69 / animate(bool) :82
pub fn corner_radius(impl Into<CornerRadius>) :93

// Separator — src/widgets/separator.rs
pub fn spacing(f32) :45 / horizontal() :55 / vertical() :65 / grow(f32) :76 / shrink(f32) :87

// Spinner — src/widgets/spinner.rs
pub fn new() :18 / size(f32) :25 / color(impl Into<Color32>) :32
pub fn paint_at(&self, ui: &Ui, rect: Rect) :38

// Link / Hyperlink — src/widgets/hyperlink.rs
Link::new(text: impl Into<WidgetText>) :32
Hyperlink::new(url: impl ToString) :100
Hyperlink::from_label_and_url(text: impl Into<WidgetText>, url: impl ToString) :110
Hyperlink::open_in_new_tab(bool) :120
```

### 11.6 `Image<'a>` and `ImageSource<'a>`

```rust
pub fn new(source: impl Into<ImageSource<'a>>) -> Self             // src/widgets/image.rs:63
pub fn from_uri(uri: impl Into<Cow<'a, str>>) -> Self              // :94
pub fn from_texture(texture: impl Into<SizedTexture>) -> Self      // :101
pub fn from_bytes(uri: impl Into<Cow<'static, str>>, bytes: impl Into<Bytes>) -> Self // :110
pub fn texture_options(TextureOptions) :119
pub fn max_width(f32) :128 / max_height(f32) :137 / max_size(Vec2) :146
pub fn maintain_aspect_ratio(bool) :153
pub fn fit_to_original_size(f32) :167 / fit_to_exact_size(Vec2) :176 / fit_to_fraction(Vec2) :185
pub fn shrink_to_fit() :196
pub fn sense(Sense) :202 / uv(impl Into<Rect>) :209
pub fn bg_fill(impl Into<Color32>) :216 / tint(impl Into<Color32>) :223
pub fn rotate(angle: f32, origin: Vec2) :238
pub fn corner_radius(impl Into<CornerRadius>) :251
pub fn show_loading_spinner(bool) :263 / alt_text(impl Into<String>) :272
pub fn calc_size(&self, available_size: Vec2, image_source_size: Option<Vec2>) -> Vec2 :287
pub fn load_and_calc_size(&self, ui: &Ui, available_size: Vec2) -> Option<Vec2> :292
pub fn size(&self) -> Option<Vec2> :298 / uri(&self) -> Option<&str> :309
pub fn image_options(&self) -> &ImageOptions :320
pub fn load_for_size(&self, ctx: &Context, available_size: Vec2) -> TextureLoadResult :349
pub fn paint_at(&self, ui: &Ui, rect: Rect) :368
```

```rust
pub enum ImageSource<'a> {
    Uri(Cow<'a, str>),                 // :579
    Texture(SizedTexture),             // :585
    Bytes { uri: Cow<'static, str>, bytes: Bytes },  // :598
}
```
`src/widgets/image.rs:570-600`

```rust
// egui::load — src/load.rs
pub struct SizedTexture { pub id: TextureId, pub size: Vec2 }                 // :444-449
pub fn new(id: impl Into<TextureId>, size: impl Into<Vec2>) -> Self           // :453
pub fn from_handle(handle: &TextureHandle) -> Self                            // :461
impl From<(TextureId, Vec2)> for SizedTexture                                 // :470
impl<'a> From<&'a TextureHandle> for SizedTexture                             // :477
```
(`pub mod load;` at `src/lib.rs:409`.)

---

## 12. Atoms (`IntoAtoms`) — the 0.36 text/image argument type

```rust
pub trait IntoAtoms<'a> {
    fn collect(self, atoms: &mut Atoms<'a>);

    fn into_atoms(self) -> Atoms<'a>
    where
        Self: Sized,
    { let mut atoms = Atoms::default(); self.collect(&mut atoms); atoms }
}
```
`src/atomics/atoms.rs:209-220`

Implemented for `Atoms<'a>` (`:222`) and for tuples of arity **0, 2, 3, 4, 5 and 6**
(`all_the_atoms!` at `:243-248`). Note the gap: there is **no 1-tuple impl**, so `("x",)` does
not compile — pass the bare value (`"x"`), which converts via `impl<'a, T> From<T> for Atom<'a>`. Also `From<Vec<T>>` `:264`, `From<&[T]>` `:270`,
`FromIterator<Item>` `:276`. A single value gets in via `impl<'a, T> From<T> for Atom<'a>`
(`src/atomics/atom.rs:166`).

```rust
pub struct Atom<'a> {
    pub id: Option<Id>,
    pub size: Option<Vec2>,
    pub max_size: Vec2,
    pub grow: bool,
    pub shrink: bool,
    pub align: Align2,
    pub kind: AtomKind<'a>,
}
pub fn grow() -> Self                                     // src/atomics/atom.rs:74
pub fn custom(id: Id, size: impl Into<Vec2>) -> Self      // :97
pub fn layout(layout: AtomLayout<'a>) -> Self             // :110
```
`src/atomics/atom.rs:32-110`

```rust
pub enum AtomKind<'a> {
    #[default] Empty,              // :29 — pair with `atom_grow(true)` to reserve space
    Text(WidgetText),              // :50
    Image(Image<'a>),              // :58
    Closure(AtomClosure<'a>),      // :67 — experimental; clones as `Empty`
    Layout(Box<AtomLayout<'a>>),   // :74 — nest an atom-based widget as one atom
}
```
`src/atomics/atom_kind.rs:26-75`

```rust
pub trait AtomExt<'a> {
    fn atom_id(self, id: Id) -> Atom<'a>;                  // src/atomics/atom_ext.rs:12
    fn atom_size(self, size: Vec2) -> Atom<'a>;            // :23
    fn atom_grow(self, grow: bool) -> Atom<'a>;            // :32
    fn atom_shrink(self, shrink: bool) -> Atom<'a>;        // :43
    fn atom_max_size(self, max_size: Vec2) -> Atom<'a>;    // :49
    fn atom_max_width(self, max_width: f32) -> Atom<'a>;   // :55
    fn atom_max_height(self, max_height: f32) -> Atom<'a>; // :58
    fn atom_max_height_font_size(self, ui: &Ui) -> Atom<'a>; // :63
    fn atom_align(self, align: emath::Align2) -> Atom<'a>; // :76
}
```

Practical effect:

```rust
ui.button("Save");                                  // 1-tuple? no — a bare value
ui.button((egui::include_image!("save.png"), "Save"));  // image + text
ui.button(());                                      // empty button
```

---

## 13. Input: `InputState`, `PointerState`, `Key`, `Event`, `Modifiers`

### 13.1 `InputState` fields

```rust
pub struct InputState {
    pub raw: RawInput,                     // src/input_state/mod.rs:217
    pub pointer: PointerState,             // :220
    pub smooth_scroll_delta: Vec2,         // :243   <-- the only scroll field
    pub pixels_per_point: f32,             // :265
    pub max_texture_side: usize,           // :270
    pub time: f64,                         // :273
    pub unstable_dt: f32,                  // :279
    pub predicted_dt: f32,                 // :287
    pub stable_dt: f32,                    // :312
    pub focused: bool,                     // :317
    pub modifiers: Modifiers,              // :320
    pub keys_down: HashSet<Key>,           // :325
    pub events: Vec<Event>,                // :328
    /* private: wheel, zoom_factor_delta, rotation_radians, viewport_rect, safe_area_insets */
}
```
`src/input_state/mod.rs:215-328`

`ScrollArea` both reads and writes `smooth_scroll_delta`, so at end-of-frame it is zero if a
scroll area consumed it (`:241-243`). **There is no `raw_scroll_delta`** — raw wheel data is only
in `Event::MouseWheel`.

### 13.2 `InputState` methods

```rust
pub fn viewport(&self) -> &ViewportInfo                            // :500
pub fn content_rect(&self) -> Rect                                 // :512
pub fn viewport_rect(&self) -> Rect                                // :526
pub fn safe_area_insets(&self) -> SafeAreaInsets                   // :536
pub fn smooth_scroll_delta(&self) -> Vec2                          // :552
pub fn zoom_delta(&self) -> f32                                    // :564
pub fn zoom_delta_2d(&self) -> Vec2                                // :587
pub fn rotation_delta(&self) -> f32                                // :619
pub fn translation_delta(&self) -> Vec2                            // :632
pub fn is_scrolling(&self) -> bool                                 // :640
pub fn time_since_last_scroll(&self) -> f32                        // :646

pub fn count_and_consume_key(&mut self, modifiers: Modifiers, logical_key: Key) -> usize // :693
pub fn consume_key(&mut self, modifiers: Modifiers, logical_key: Key) -> bool            // :724
pub fn consume_shortcut(&mut self, shortcut: &KeyboardShortcut) -> bool                  // :737
pub fn key_pressed(&self, desired_key: Key) -> bool                // :748
pub fn num_presses(&self, desired_key: Key) -> usize               // :755
pub fn key_down(&self, desired_key: Key) -> bool                   // :771
pub fn key_released(&self, desired_key: Key) -> bool               // :776

pub fn pixels_per_point(&self) -> f32                              // :791
pub fn physical_pixel_size(&self) -> f32                           // :797
pub fn aim_radius(&self) -> f32                                    // :804
pub fn multi_touch(&self) -> Option<MultiTouchInfo>                // :836
pub fn any_touches(&self) -> bool                                  // :842
pub fn has_touch_screen(&self) -> bool                             // :847
pub fn accesskit_action_requests(..)                               // :863
pub fn consume_accesskit_action_requests(..)                       // :881
pub fn has_accesskit_action_request(&self, id: crate::Id, action: accesskit::Action) -> bool // :898
pub fn num_accesskit_action_requests(&self, id: crate::Id, action: accesskit::Action) -> usize // :902
pub fn filtered_events(&self, filter: &EventFilter) -> Vec<Event>  // :907
```

`consume_key` / `consume_shortcut` need `&mut InputState`, i.e. `ctx.input_mut(|i| ...)`
(`src/context.rs:1002`), not `ctx.input(..)`.

```rust
pub struct InputOptions {
    pub line_scroll_speed: f32,                // :61
    pub scroll_zoom_speed: f32,                // :64
    pub max_click_dist: f32,                   // :67
    pub max_click_duration: f64,               // :75
    pub max_double_click_delay: f64,           // :79
    pub zoom_modifier: Modifiers,              // :84
    pub horizontal_scroll_modifier: Modifiers, // :91
    pub vertical_scroll_modifier: Modifiers,   // :96
    pub surrender_focus_on: SurrenderFocusOn,  // :99
}
```
`src/input_state/mod.rs:59-99`

### 13.3 `PointerState`

```rust
pub fn delta(&self) -> Vec2                     // :1261
pub fn motion(&self) -> Option<Vec2>            // :1269
pub fn velocity(&self) -> Vec2                  // :1278
pub fn direction(&self) -> Vec2                 // :1286
pub fn press_origin(&self) -> Option<Pos2>      // :1293
pub fn total_drag_delta(&self) -> Option<Vec2>  // :1298
pub fn press_start_time(&self) -> Option<f64>   // :1305
pub fn latest_pos(&self) -> Option<Pos2>        // :1312
pub fn hover_pos(&self) -> Option<Pos2>         // :1318
pub fn interact_pos(&self) -> Option<Pos2>      // :1328
pub fn has_pointer(&self) -> bool               // :1336
pub fn is_still(&self) -> bool                  // :1343
pub fn is_moving(&self) -> bool                 // :1350
pub fn time_since_last_movement(&self) -> f32   // :1356
pub fn time_since_last_click(&self) -> f32      // :1362
pub fn any_pressed(&self) -> bool               // :1370
pub fn any_released(&self) -> bool              // :1375
pub fn button_pressed(&self, button: PointerButton) -> bool   // :1380
pub fn button_released(&self, button: PointerButton) -> bool  // :1387
pub fn primary_pressed(&self) -> bool           // :1394
pub fn secondary_pressed(&self) -> bool         // :1399
pub fn primary_released(&self) -> bool          // :1404
pub fn secondary_released(&self) -> bool        // :1409
pub fn any_down(&self) -> bool                  // :1416
pub fn any_click(&self) -> bool                 // :1421
pub fn button_clicked(&self, button: PointerButton) -> bool         // :1431
pub fn button_double_clicked(&self, button: PointerButton) -> bool  // :1438
pub fn button_triple_clicked(&self, button: PointerButton) -> bool  // :1451
pub fn primary_clicked(&self) -> bool           // :1467
pub fn secondary_clicked(&self) -> bool         // :1475
pub fn button_down(&self, button: PointerButton) -> bool // :1483
pub fn could_any_button_be_click(&self) -> bool // :1490
pub fn is_decidedly_dragging(&self) -> bool     // :1517
pub fn primary_down(&self) -> bool              // :1541
pub fn secondary_down(&self) -> bool            // :1549
pub fn middle_down(&self) -> bool               // :1557
pub fn is_moving_towards_rect(&self, rect: &Rect) -> bool // :1562
```

Click info struct (`:930-944`): `pub pos: Pos2`, `pub count: u32`, `pub modifiers: Modifiers`,
with `is_double()` `:940`, `is_triple()` `:944`.

### 13.4 `Key`

`Key` is a **plain fieldless enum** (`src/data/key.rs:7`), not a struct. Variants, in order:

- Commands: `ArrowDown`, `ArrowLeft`, `ArrowRight`, `ArrowUp`, `Escape`, `Tab`, `Backspace`,
  `Enter`, `Space`, `Insert`, `Delete`, `Home`, `End`, `PageUp`, `PageDown`, `Copy`, `Cut`,
  `Paste` (`:10-30`)
- Punctuation: `Colon`, `Comma`, `Backslash`, `Slash`, `Pipe`, `Questionmark`, `Exclamationmark`,
  `OpenBracket`, `CloseBracket`, `OpenCurlyBracket`, `CloseCurlyBracket`, `Backtick`, `Minus`,
  `Period`, `Plus`, `Equals`, `Semicolon`, `Quote` (`:35-86`)
- Digits: `Num0` … `Num9` (`:91-118`)
- Letters: `A` … `Z` (`:122-147`)
- Function keys: `F1` … `F35` (`:151-185`)
- `BrowserBack` (`:190`)
- Modifier keys as keys: `ShiftLeft`, `ShiftRight`, `ControlLeft`, `ControlRight`, `AltLeft`,
  `AltRight`, `SuperLeft`, `SuperRight` (`:199-220`)
- `IntlBackslash` (`:228`)

```rust
pub const ALL: &'static [Self]                        // src/data/key.rs:240
pub fn from_name(key: &str) -> Option<Self>           // :377
pub fn symbol_or_name(self) -> &'static str           // :513
pub fn name(self) -> &'static str                     // :546
```

### 13.5 `Event`

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Copy,                                                            // :19
    Cut,                                                             // :22
    Paste(String),                                                   // :25
    Text(String),                                                    // :30
    Key {
        key: Key,                                                    // :44
        physical_key: Option<Key>,                                   // :54
        pressed: bool,                                               // :57
        repeat: bool,                                                // :66
        modifiers: Modifiers,                                        // :69
    },
    ModifiersChanged(Modifiers),                                     // :73
    PointerMoved(Pos2),                                              // :76
    MouseMoved(Vec2),                                                // :82
    PointerButton {
        pos: Pos2,
        button: PointerButton,
        pressed: bool,
        modifiers: Modifiers,
    },                                                               // :85-97
    PointerGone,                                                     // :104
    Zoom(f32),                                                       // :116
    Rotate(f32),                                                     // :119
    Ime(ImeEvent),                                                   // :122
    Touch {
        device_id: TouchDeviceId,
        id: TouchId,
        phase: TouchPhase,
        pos: Pos2,
        force: Option<f32>,
    },                                                               // :126-145
    MouseWheel {
        unit: MouseWheelUnit,                                        // :152
        delta: Vec2,                                                 // :163
        phase: TouchPhase,                                           // :168
        modifiers: Modifiers,                                        // :171
    },
    WindowFocused(bool),                                             // :175
    AccessKitActionRequest(accesskit::ActionRequest),                // :178
    Screenshot {
        viewport_id: crate::ViewportId,
        user_data: crate::UserData,
        image: std::sync::Arc<ColorImage>,
    },                                                               // :181-188
}
```
`src/data/input/event.rs:17-189`

Raw scroll, done right in 0.36:

```rust
let raw_wheel: egui::Vec2 = ctx.input(|i| {
    i.events
        .iter()
        .filter_map(|e| match e {
            egui::Event::MouseWheel { unit, delta, .. } => Some((*unit, *delta)),
            _ => None,
        })
        .fold(egui::Vec2::ZERO, |acc, (_unit, d)| acc + d)
});
```

### 13.6 `Modifiers`

```rust
pub struct Modifiers {
    pub alt: bool,      // :21
    pub ctrl: bool,     // :25
    pub shift: bool,    // :28
    pub mac_cmd: bool,  // :31
    pub command: bool,  // :37
}
```
`src/data/input/modifiers.rs:19-38`

```rust
pub const NONE:    Self;                     // :75
pub const ALT:     Self;                     // :83
pub const CTRL:    Self;                     // :90
pub const SHIFT:   Self;                     // :97
pub const MAC_CMD: Self;                     // :106
pub const COMMAND: Self;                     // :115
pub const fn plus(self, rhs: Self) -> Self;  // :139
pub fn is_none(&self) -> bool;               // :150
pub fn any(&self) -> bool;                   // :155
pub fn all(&self) -> bool;                   // :160
pub fn shift_only(&self) -> bool;            // :166
pub fn command_only(&self) -> bool;          // :172
pub fn matches_logically(&self, pattern: Self) -> bool; // :211
pub fn matches_exact(&self, pattern: Self) -> bool;     // :253
pub fn matches_any(&self, pattern: Self) -> bool;       // :274
pub fn cmd_ctrl_matches(&self, pattern: Self) -> bool;  // :300
pub fn contains(&self, query: Self) -> bool;            // :345
```

### 13.7 `EventFilter` (focus locking)

```rust
pub struct EventFilter {
    pub tab: bool,               // :16
    pub horizontal_arrows: bool, // :22
    pub vertical_arrows: bool,   // :28
    pub escape: bool,            // :34
}
```
`src/data/input/event_filter.rs:11-35`, all default `false`.

```rust
pub fn set_focus_lock_filter(&mut self, id: Id, event_filter: EventFilter)
```
`src/memory/mod.rs:903`

---

## 14. `Painter` (what a custom widget draws with)

```rust
pub fn with_clip_rect(&self, rect: Rect) -> Self                                     // src/painter.rs:71
pub fn set_opacity(&mut self, opacity: f32)                                          // :91
pub fn clip_rect(&self) -> Rect                                                      // :163
pub fn round_to_pixel_center(&self, point: f32) -> f32                               // :189
pub fn add(&self, shape: impl Into<Shape>) -> ShapeIdx                               // :213
pub fn extend<I: IntoIterator<Item = Shape>>(&self, shapes: I)                       // :226
pub fn set(&self, idx: ShapeIdx, shape: impl Into<Shape>)                            // :242
pub fn debug_rect(&self, rect: Rect, color: Color32, text: impl ToString)            // :266
pub fn error(&self, pos: Pos2, text: impl std::fmt::Display) -> Rect                 // :283

pub fn line_segment(&self, points: [Pos2; 2], stroke: impl Into<Stroke>) -> ShapeIdx // :318
pub fn line(&self, points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> ShapeIdx     // :327
pub fn hline(&self, x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) -> ShapeIdx // :332
pub fn vline(&self, x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) -> ShapeIdx // :337

pub fn circle(
    &self, center: Pos2, radius: f32,
    fill_color: impl Into<Color32>, stroke: impl Into<Stroke>,
) -> ShapeIdx                                                                        // :341-347
pub fn circle_filled(&self, center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> ShapeIdx // :356
pub fn circle_stroke(&self, center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> ShapeIdx      // :370

pub fn rect(
    &self,
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    fill_color: impl Into<Color32>,
    stroke: impl Into<Stroke>,
    stroke_kind: StrokeKind,
) -> ShapeIdx                                                                        // :380-387

pub fn rect_filled(
    &self, rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>,
) -> ShapeIdx                                                                        // :397-402

pub fn rect_stroke(
    &self, rect: Rect, corner_radius: impl Into<CornerRadius>,
    stroke: impl Into<Stroke>, stroke_kind: StrokeKind,
) -> ShapeIdx                                                                        // :406-412

pub fn arrow(&self, origin: Pos2, vec: Vec2, stroke: impl Into<Stroke>)              // :417

pub fn image(
    &self, texture_id: epaint::TextureId, rect: Rect, uv: Rect, tint: Color32,
) -> ShapeIdx                                                                        // :447-453

pub fn text(
    &self, pos: Pos2, anchor: Align2, text: impl ToString,
    font_id: FontId, text_color: Color32,
) -> Rect                                                                            // :469-476
#[must_use] pub fn layout(&self, text: String, ..) -> Arc<Galley>                    // :488
#[must_use] pub fn layout_no_wrap(..) -> Arc<Galley>                                 // :503
pub fn layout_job(&self, layout_job: LayoutJob) -> Arc<Galley>                       // :517
pub fn galley(&self, pos: Pos2, galley: Arc<Galley>, fallback_color: Color32)        // :529
pub fn galley_with_override_text_color(..)                                           // :541
```

Style lookup while painting:

```rust
pub fn interact(&self, response: &Response) -> &WidgetVisuals                        // src/style.rs:354
pub fn interact_selectable(&self, response: &Response, selected: bool) -> WidgetVisuals // :358
pub fn noninteractive(&self) -> &WidgetVisuals                                       // :370
```

```rust
pub struct WidgetVisuals {
    pub bg_fill: Color32,            // src/style.rs:1294
    pub weak_bg_fill: Color32,       // :1299
    pub bg_stroke: Stroke,           // :1304
    pub corner_radius: CornerRadius, // :1307
    pub fg_stroke: Stroke,           // :1310
    pub expansion: f32,              // :1318
}
```

The state-selection rule (`src/style.rs:1272-1283`): non-interactive sense → `noninteractive`;
`is_pointer_button_down_on() || has_focus() || clicked()` → `active`;
`hovered() || highlighted()` → `hovered`; else `inactive`.

Textures:

```rust
pub fn load_texture(
    &self,
    name: impl Into<String>,
    image: impl Into<ImageData>,
    options: TextureOptions,
) -> TextureHandle                                        // src/context.rs:2387-2392

// TextureHandle — epaint/src/texture_handle.rs
pub fn id(&self) -> TextureId              // :64
pub fn set(&mut self, image: impl Into<ImageData>, options: TextureOptions) // :70
pub fn set_partial(..)                     // :78
pub fn size(&self) -> [usize; 2]           // :90
pub fn size_vec2(&self) -> crate::Vec2     // :98
pub fn aspect_ratio(&self) -> f32          // :112
pub fn name(&self) -> String               // :118

// ColorImage — epaint/src/image.rs
pub fn new(size: [usize; 2], pixels: Vec<Color32>) -> Self          // :61
pub fn filled(size: [usize; 2], color: Color32) -> Self             // :75
pub fn from_rgba_unmultiplied(size: [usize; 2], rgba: &[u8]) -> Self// :113
pub fn from_gray(size: [usize; 2], gray: &[u8]) -> Self             // :146
pub fn from_rgb(size: [usize; 2], rgb: &[u8]) -> Self               // :193
```

Accessibility payload for a custom widget:

```rust
pub fn new(typ: WidgetType) -> Self                                            // src/data/output.rs:643
pub fn labeled(typ: WidgetType, enabled: bool, label: impl ToString) -> Self   // :658
pub fn selected(typ: WidgetType, enabled: bool, selected: bool, label: impl ToString) -> Self // :668
pub fn drag_value(enabled: bool, value: f64) -> Self                           // :677
pub fn slider(enabled: bool, value: f64, label: impl ToString) -> Self         // :686
pub fn text_edit(..)                                                           // :697
pub fn text_selection_changed(..)                                              // :721
```

---

## 15. The idiomatic 0.36 way to write a fully custom widget

There is no trait for "custom paint" and no `CustomWidget` type. The pattern the standard
widgets themselves use (see `Slider::slider_ui`, `src/widgets/slider.rs:659-830`) is:

1. `impl Widget for YourThing` (or `for &mut YourThing`) with `fn ui(self, ui: &mut Ui) -> Response`.
2. **Allocate + register hit-testing in one call**: `ui.allocate_exact_size(size, sense)`.
   This is where hit-testing happens — the `Rect` + `Sense` you pass become the widget's
   `WidgetRect` (`src/ui.rs:906-934`), and egui's hit-test (`src/hit_test.rs`) resolves overlaps
   in painting order, last-added-wins.
3. For **sub-region hit-testing** (a thumb, a handle, a close button), call
   `ui.interact(sub_rect, id.with("something"), Sense::drag())` with a *derived, stable* `Id`.
   Do **not** try to do your own `rect.contains(pointer_pos)` test: that bypasses layer ordering
   and blocking, so your widget would react through windows drawn on top of it.
4. Mutate your value from `response.drag_delta()` / `response.interact_pointer_pos()` /
   keyboard, then call `response.mark_changed()` if anything changed.
5. Guard painting with `if ui.is_rect_visible(rect)`, get colors from
   `ui.style().interact(&response)`, and paint through `ui.painter()` (or
   `ui.painter_at(rect)` / `ui.allocate_painter(..)` if you want clipping to your own rect).
6. Report a11y with `response.widget_info(|| WidgetInfo::slider(..))`.
7. Return the `Response`. Union sub-responses with `|` if you want callers to see either.

Use `ui.allocate_painter(size, sense) -> (Response, Painter)` (`src/ui.rs:1370`) when you want a
painter pre-clipped to exactly your rect and nothing else.

### 15.1 Complete compiling example — vertical fader

Drag (thumb-relative **and** track-absolute), mouse wheel, double-click-to-reset, arrow keys,
custom thumb texture, custom painting, custom sub-region hit testing.

*Verified: `cargo build --offline` against `egui = "0.36.0"`, rustc 1.98.1, zero warnings.*

```rust
use egui::{
    Color32, CornerRadius, EventFilter, Key, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
    Widget, WidgetInfo, emath, load::SizedTexture, pos2, vec2,
};

/// A vertical fader that owns all of its painting and hit-testing.
pub struct VerticalFader<'a> {
    value: &'a mut f32,
    range: std::ops::RangeInclusive<f32>,
    default_value: f32,
    /// The thumb ("cap") texture. Build one with `SizedTexture::from_handle(&handle)`,
    /// or just pass `&TextureHandle` thanks to `impl From<&TextureHandle> for SizedTexture`.
    thumb: SizedTexture,
    /// Total widget size, in points.
    size: Vec2,
    /// Fraction of the range one wheel notch (~50 points of scroll) moves.
    wheel_step: f32,
}

impl<'a> VerticalFader<'a> {
    pub fn new(
        value: &'a mut f32,
        range: std::ops::RangeInclusive<f32>,
        thumb: impl Into<SizedTexture>,
    ) -> Self {
        let default_value = *range.start();
        Self {
            value,
            range,
            default_value,
            thumb: thumb.into(),
            size: vec2(28.0, 160.0),
            wheel_step: 0.02,
        }
    }

    #[inline]
    pub fn default_value(mut self, default_value: f32) -> Self {
        self.default_value = default_value;
        self
    }

    #[inline]
    pub fn size(mut self, size: Vec2) -> Self {
        self.size = size;
        self
    }

    #[inline]
    pub fn wheel_step(mut self, wheel_step: f32) -> Self {
        self.wheel_step = wheel_step;
        self
    }

    /// Where the centre of the thumb sits for the current value.
    fn thumb_center_y(&self, rect: Rect, thumb_h: f32) -> f32 {
        let travel_top = rect.top() + thumb_h * 0.5;
        let travel_bottom = rect.bottom() - thumb_h * 0.5;
        // High value = up, so the output range is reversed.
        emath::remap_clamp(*self.value, self.range.clone(), travel_bottom..=travel_top)
    }

    /// Inverse of [`Self::thumb_center_y`].
    fn value_from_y(&self, rect: Rect, thumb_h: f32, y: f32) -> f32 {
        let travel_top = rect.top() + thumb_h * 0.5;
        let travel_bottom = rect.bottom() - thumb_h * 0.5;
        emath::remap_clamp(y, travel_bottom..=travel_top, self.range.clone())
    }
}

impl Widget for VerticalFader<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let me = self;

        // 1. Allocate space AND register our hit-test rect + Sense in one call.
        let (rect, mut response) = ui.allocate_exact_size(me.size, Sense::click_and_drag());

        let thumb_h = me.thumb.size.y.min(me.size.y * 0.4).max(12.0);
        let old_value = *me.value;

        // 2. Custom sub-region hit-testing: only the thumb starts a "grab" drag.
        //    A second `Ui::interact` with a derived Id is the idiomatic way.
        let thumb_rect = Rect::from_center_size(
            pos2(rect.center().x, me.thumb_center_y(rect, thumb_h)),
            vec2(me.size.x, thumb_h),
        );
        let thumb_response = ui.interact(thumb_rect, response.id.with("thumb"), Sense::drag());

        // 3. Pointer interaction.
        if thumb_response.dragged() {
            // Relative dragging keeps the grab point under the cursor.
            let dy = thumb_response.drag_delta().y;
            let new_y = me.thumb_center_y(rect, thumb_h) + dy;
            *me.value = me.value_from_y(rect, thumb_h, new_y);
        } else if response.dragged() || response.clicked() {
            // Absolute jump when the track (not the thumb) is used.
            if let Some(pos) = response.interact_pointer_pos() {
                *me.value = me.value_from_y(rect, thumb_h, pos.y);
            }
        }

        // 4. Double click resets to the default value.
        if response.double_clicked() || thumb_response.double_clicked() {
            *me.value = me.default_value;
        }

        // 5. Mouse wheel while hovering. We *consume* the delta so a parent
        //    `ScrollArea` does not also scroll. (`ui.input_mut` comes from
        //    `Ui: Deref<Target = Context>`.)
        if response.contains_pointer() {
            let scroll_y = ui.input_mut(|i| {
                let dy = i.smooth_scroll_delta.y;
                i.smooth_scroll_delta.y = 0.0;
                dy
            });
            if scroll_y != 0.0 {
                let span = *me.range.end() - *me.range.start();
                *me.value += (scroll_y / 50.0) * me.wheel_step * span;
            }
        }

        // 6. Keyboard, when focused. Lock vertical arrows so they don't move focus.
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    EventFilter {
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
            let (up, down) = ui.input(|i| {
                (i.num_presses(Key::ArrowUp), i.num_presses(Key::ArrowDown))
            });
            let steps = up as f32 - down as f32;
            if steps != 0.0 {
                let span = *me.range.end() - *me.range.start();
                *me.value += steps * 0.01 * span;
            }
        }

        *me.value = me.value.clamp(*me.range.start(), *me.range.end());

        if *me.value != old_value {
            response.mark_changed();
        }

        // 7. Paint. Skip the work entirely when scrolled out of view.
        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact(&response);
            let painter = ui.painter();

            // Track.
            let track = Rect::from_center_size(
                pos2(rect.center().x, rect.center().y),
                vec2(6.0, rect.height()),
            );
            painter.rect(
                track,
                CornerRadius::same(3),
                ui.visuals().widgets.inactive.bg_fill,
                Stroke::NONE,
                StrokeKind::Inside, // 5th arg is new-ish: StrokeKind
            );

            // Filled part below the thumb.
            let y = me.thumb_center_y(rect, thumb_h);
            let filled = Rect::from_min_max(pos2(track.left(), y), track.max);
            painter.rect_filled(filled, CornerRadius::same(3), visuals.fg_stroke.color);

            // Thumb texture. `uv` is in 0..=1 texture space.
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            let thumb_draw_rect =
                Rect::from_center_size(pos2(rect.center().x, y), vec2(me.size.x, thumb_h));
            painter.image(me.thumb.id, thumb_draw_rect, uv, Color32::WHITE);

            // Focus outline drawn by us, not by a Frame.
            if response.has_focus() {
                painter.rect_stroke(
                    thumb_draw_rect,
                    CornerRadius::same(2),
                    ui.visuals().selection.stroke,
                    StrokeKind::Outside,
                );
            }
        }

        // 8. Cursor + accessibility.
        let value_for_info = *me.value;
        response = response.on_hover_and_drag_cursor(egui::CursorIcon::ResizeVertical);
        response.widget_info(|| {
            WidgetInfo::slider(ui.is_enabled(), value_for_info as f64, "Fader")
        });

        response | thumb_response
    }
}

/// Upload a thumb texture once and keep the handle in your app state.
pub fn load_thumb(ctx: &egui::Context) -> egui::TextureHandle {
    let image = egui::ColorImage::filled([24, 40], Color32::from_gray(200));
    ctx.load_texture("fader_thumb", image, egui::TextureOptions::LINEAR)
}

pub fn demo(ui: &mut Ui, gain: &mut f32, thumb: &egui::TextureHandle) {
    let response = ui.add(
        VerticalFader::new(gain, -60.0..=12.0, thumb)
            .default_value(0.0)
            .size(vec2(30.0, 180.0)),
    );
    if response.changed() {
        ui.ctx().request_repaint();
    }
    response.on_hover_text(format!("{gain:.1} dB"));
}
```

**Notes for adapting this**

- `impl Into<SizedTexture>` accepts `&TextureHandle` (`src/load.rs:477`) and
  `(TextureId, Vec2)` (`:470`).
- If you keep per-widget state across frames, use `ui.data_mut(|d| ...)` with
  `get_temp_mut_or_insert_with::<T>(id, || ...)` (`src/util/id_type_map.rs:514`) or
  `get_temp_mut_or_default` (`:502`). Persisted variants at `:510` / `:536` need the
  `persistence` feature.
- Prefer `Sense::click_and_drag()` over `Sense::CLICK | Sense::DRAG` unless you want to opt out
  of keyboard focus — see §3.
- A `Sense::click_and_drag()` widget has an inherent click/drag disambiguation latency
  (`src/sense.rs:72-79`, `PointerState::is_decidedly_dragging`).

---

## 16. Feature flags

From `Cargo.toml:56-77`:

| Feature | Effect |
|---|---|
| `default` | `["default_fonts"]` |
| `default_fonts` | `epaint/default_fonts` |
| `serde` | `dep:serde`, `epaint/serde`, `accesskit/serde` — gates `Serialize`/`Deserialize` on `Align`, `Key`, `Event`, `Modifiers`, `SliderOrientation`, `SliderClamping`, `ScrollBarVisibility`, `DragScroll`, `HandleShape`, `WidgetVisuals`, … |
| `persistence` | `["serde", "epaint/serde", "ron"]` — needed for `IdTypeMap::get_persisted*` / `insert_persisted` to actually persist |
| `callstack` | `dep:backtrace` — widget-callstack debugging (`src/callstack.rs`) |
| `color-hex` | re-exports `ecolor::hex_color` (`src/lib.rs:440-441`) |
| `bytemuck`, `cint`, `mint`, `rayon`, `unity`, `_override_unity` | forwarded to `epaint` |

`accesskit` is an **unconditional** dependency in 0.36 (`Cargo.toml:82-83`) — it is not behind a
feature, which is why `WidgetInfo`, `Event::AccessKitActionRequest`, and
`InputState::accesskit_action_requests` are always present.

---

## 17. Quick reference — crate-root re-exports

`src/lib.rs:443-497` is the authoritative list. Highlights:

```rust
pub use emath::{Align, Align2, NumExt, Pos2, Rangef, Rect, RectAlign, Vec2, Vec2b,
                lerp, pos2, remap, remap_clamp, vec2};                        // :443-446
pub use epaint::{ClippedPrimitive, ColorImage, CornerRadius, Direction, ImageData,
                 Margin, Mesh, PaintCallback, PaintCallbackInfo, Shadow, Shape,
                 Stroke, StrokeKind, TextureHandle, TextureId, mutex, ...};   // :447-452
pub use self::{
    atomics::*,                                   // Atom, Atoms, AtomExt, IntoAtoms, AtomKind…
    containers::{menu::MenuBar, *},               // Area, Frame, Modal, Popup, ScrollArea, Window, Sides, Scene…
    context::{Context, RepaintCause, RequestRepaintInfo},
    data::{Key, UserData, input::*, output::{CursorIcon, WidgetInfo, ...}},
    input_state::{InputOptions, InputState, MultiTouchInfo, PointerState, SurrenderFocusOn},
    layers::{LayerId, Order},
    layout::*,                                    // Layout
    response::{InnerResponse, Response},
    sense::Sense,
    style::{FontSelection, Spacing, Style, TextStyle, Visuals},
    ui::Ui,
    ui_builder::{IdSource, UiBuilder},
    widget_rect::{InteractOptions, WidgetRect, WidgetRects},
    widget_text::{RichText, WidgetText},
    widgets::*,
};                                                                            // :462-497
```

`src/containers/mod.rs:21-36` exports: `Area`, `AreaState`, `ClosableTag`, `CollapsingHeader`,
`CollapsingResponse`, `combo_box::*`, `Frame`, `Modal`, `ModalResponse`, `panel::*`, `popup::*`,
`Resize`, `DragPanButtons`, `Scene`, `ScrollArea`, `Sides`, `tooltip::*`, `Window`, `WindowDrag`.
