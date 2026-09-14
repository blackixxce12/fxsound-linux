# egui 0.36 / epaint 0.36 — 2D Painting Surface (VERIFIED)

Every signature below was read verbatim out of the vendored sources. Citations are
`<crate-dir>/<file>:<line>` relative to
`/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.

Crates in play (exact versions on disk — note they are **not** all `0.36.0`):

| crate | version on disk |
|---|---|
| `egui` | `0.36.0` |
| `epaint` | `0.36.2` |
| `ecolor` | `0.36.2` |
| `emath` | `0.36.2` |
| `egui_extras` | `0.36.0` |
| `eframe` | `0.36.0` |

**How this was verified.** Beyond reading the sources verbatim, the two complete programs in §16.0
and §16.1 were extracted from *this file* into a scratch crate pinned to `egui = "=0.36.0"` and
`eframe = "=0.36.0"` (default features off; `default_fonts, glow, wayland, x11`) and run through
`cargo check --offline`: **zero errors, zero warnings**. A separate scratch module exercised the
rest of the surface documented here — every `Painter` primitive, both gradient helpers, the
`Shape`/`RectShape`/`PathStroke`/`TextShape` constructors, clipping, layers, opacity, the `Color32`
constructors and blending helpers, texture upload and painting, text layout and measurement, and
the `TessellationOptions` fields — and also compiled clean. The "NOT at the root" claims in §9.8
were proven by observing the expected `E0425` failures. A deliberate type error was injected once
to confirm the checker was actually running.

Shorter snippets elsewhere in the file are excerpts (they reference surrounding variables such as
`painter`, `ui` or `rect`) and are not standalone programs, but the signatures they call were all
compile-exercised as described above.

---

## 0. TL;DR — the gotcha list (read this first)

Things that changed and *will* break code written from memory of 0.27–0.31:

1. **`Rounding` no longer exists.** Not even as a deprecated alias. A full-tree grep for
   `\bRounding\b` in `epaint-0.36.2/src` and `egui-0.36.0/src` returns only `GuiRounding` (a
   different, unrelated trait) and doc-comment prose. The type is
   **`CornerRadius`** (`epaint-0.36.2/src/corner_radius.rs:13`), and its fields are **`u8`**, not
   `f32`: `nw, ne, sw, se: u8`.
2. **`CornerRadius` field names are `nw/ne/sw/se`** (`corner_radius.rs:15–24`) — not
   `top_left/top_right/bottom_left/bottom_right`.
3. **`RectShape` field is `corner_radius`, not `rounding`**
   (`epaint-0.36.2/src/shapes/rect_shape.rs:21`).
4. **`Painter::rect` takes 5 args** — a trailing `StrokeKind` was added
   (`egui-0.36.0/src/painter.rs:380`). Likewise `Painter::rect_stroke` takes **4**
   (`painter.rs:406`), `RectShape::new` takes **5** (`rect_shape.rs:78`), `RectShape::stroke` takes
   **4** (`rect_shape.rs:114`), and `Shape::rect_stroke` takes **4** (`shapes/shape.rs:291`).
   `rect_filled` is the only one still at 3 (`painter.rs:397`).
5. **`StrokeKind` is `Inside | Middle | Outside`** (`epaint-0.36.2/src/stroke.rs:102`). There is
   no `Center` variant; it is spelled `Middle`.
6. **`PathStroke` now has a third field `kind: StrokeKind`** (`stroke.rs:118–122`). Constructing it
   with a struct literal from memory (`PathStroke { width, color }`) will not compile.
7. **`ctx.fonts(..)` hands you `&FontsView<'_>`, which cannot lay out text.** Layout methods take
   `&mut self`, so text layout goes through **`fonts_mut`**
   (`egui-0.36.0/src/context.rs:1113`, `egui-0.36.0/src/painter.rs:150`). `Painter::layout*`
   already does this for you.
8. **`Fonts` vs `FontsView`**: `Fonts` is the owner; `Fonts::with_pixels_per_point(&mut self, f32)
   -> FontsView<'_>` (`epaint-0.36.2/src/text/fonts.rs:801`) is the borrow you actually lay out
   with.
9. **`Shape::gradient_rect(rect, Direction, [Color32; 2])` exists in 0.36**
   (`shapes/shape.rs:306`) — do not hand-roll a two-colour gradient mesh.
10. **`epaint::Direction`** (`epaint-0.36.2/src/direction.rs:4`) is re-exported as `egui::Direction`
    (`egui-0.36.0/src/lib.rs:448`) and is the *same type* that `egui::Layout::main_dir` uses
    (`egui-0.36.0/src/layout.rs:4` imports it from the crate root). There is no separate
    `egui::Direction`.
11. **Many painting types are NOT re-exported at `egui::` root.** `RectShape`, `CircleShape`,
    `PathShape`, `TextShape`, `EllipseShape`, `Vertex`, `Mesh16`, `PathStroke`, `ColorMode`,
    `Brush`, `CornerRadiusF32`, `TessellationOptions`, `Tessellator`, `ClippedShape`,
    `PaintStats` all live behind `egui::epaint::…`. The re-export list is exactly
    `egui-0.36.0/src/lib.rs:447–452`.
12. **`Margin` is `i8`-based** (`epaint-0.36.2/src/margin.rs:16–19`) and **`Shadow` is
    `offset: [i8; 2]`, `blur: u8`, `spread: u8`** (`epaint-0.36.2/src/shadow.rs:15–26`).
13. **`Color32::from_white_alpha` is NOT `const`** (`ecolor-0.36.2/src/color32.rs:171`) while
    `from_black_alpha` IS (`color32.rs:165`). Likewise `from_rgba_unmultiplied` is not const;
    use **`from_rgba_unmultiplied_const`** (`color32.rs:139`) in const contexts.
14. **`Shape::Mesh` holds `Arc<Mesh>`, not `Mesh`** (`shapes/shape.rs:61`). `Shape::mesh` takes
    `impl Into<Arc<Mesh>>` (`shapes/shape.rs:361`) and `debug_assert!`s validity.
15. **`ColorImage` gained a `source_size: Vec2` field** (`epaint-0.36.2/src/image.rs:53`), so
    `ColorImage { size, pixels }` struct literals no longer compile. Use `ColorImage::new(size,
    pixels)` (`image.rs:61`).
16. **`ColorImage::new` now takes `(size, pixels)`** and `ColorImage::filled(size, color)` is the
    separate "fill with a colour" constructor (`image.rs:75`). In older versions `new` was the
    filled one.
17. **`ImageData` has only one variant, `Color(Arc<ColorImage>)`** (`image.rs:16–19`) — the
    `Font(..)` variant is gone.
18. `egui::Painter` has **no** `add_callback`; use `painter.add(Shape::Callback(..))`.
19. `TessellationOptions` gained **`round_line_segments_to_pixels`** and
    **`round_rects_to_pixels`** (`epaint-0.36.2/src/tessellator.rs:692,700`) and
    **`validate_meshes`** (`tessellator.rs:724`). `anti_alias` was long ago renamed `feathering`.
20. `eframe` 0.36 has **no `run_simple_native`**. The closest equivalent is
    **`run_ui_native`** (`eframe-0.36.0/src/lib.rs:478`), whose closure is
    `impl FnMut(&mut egui::Ui, &mut Frame) + 'static` — a `&mut Ui`, not a `&Context`:
    ```rust
    pub fn run_ui_native(
        app_name: &str,
        native_options: NativeOptions,
        ui_fun: impl FnMut(&mut egui::Ui, &mut Frame) + 'static,
    ) -> Result;
    ```
    The full trait route is `run_native` (`lib.rs:288`) with
    `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)` (`src/epi.rs:182`). Both are gated on
    `#[cfg(any(feature = "glow", feature = "wgpu_no_default_features"))]` (`lib.rs:477`).
21. **`Context::screen_rect` is gone.** Use `Context::content_rect()` (`context.rs:2904`) for the
    safe area, or `Context::viewport_rect()` (`context.rs:2918`) to include notches. See §2.
22. **Only `lerp`, `remap` and `remap_clamp` of the `emath` interpolation helpers are at the
    `egui` root.** `fast_midpoint`, `inverse_lerp`, `normalized_angle` and `ease_in_ease_out` are
    `egui::emath::*` only — verified by an `E0425` compile failure. See §9.8.

---

## 1. `Painter` — every public method

`egui-0.36.0/src/painter.rs`. `Painter` is `#[derive(Clone)]`, holds a `Context`, a `LayerId`, a
`clip_rect`, an optional fade colour and an opacity factor (`painter.rs:20–43`). It never outlives a
single pass.

### 1.1 Construction & configuration

```rust
// painter.rs:47
pub fn new(ctx: Context, layer_id: LayerId, clip_rect: Rect) -> Self;

// painter.rs:62   — #[must_use] #[inline]
pub fn with_layer_id(mut self, layer_id: LayerId) -> Self;

// painter.rs:71   — intersects with the parent's clip rect
pub fn with_clip_rect(&self, rect: Rect) -> Self;

// painter.rs:81
pub fn set_layer_id(&mut self, layer_id: LayerId);

// painter.rs:91   — clamped to 0.0..=1.0, ignored if not finite
pub fn set_opacity(&mut self, opacity: f32);

// painter.rs:100
pub fn multiply_opacity(&mut self, opacity: f32);

// painter.rs:110  — #[inline]
pub fn opacity(&self) -> f32;

// painter.rs:117
pub fn is_visible(&self) -> bool;

// painter.rs:122
pub fn set_invisible(&mut self);

// painter.rs:128  — #[inline]
pub fn ctx(&self) -> &Context;

// painter.rs:134  — #[inline]
pub fn pixels_per_point(&self) -> f32;

// painter.rs:142  — #[inline]
pub fn fonts<R>(&self, reader: impl FnOnce(&FontsView<'_>) -> R) -> R;

// painter.rs:150  — #[inline]  (this is the one that can lay out text)
pub fn fonts_mut<R>(&self, reader: impl FnOnce(&mut FontsView<'_>) -> R) -> R;

// painter.rs:156  — #[inline]
pub fn layer_id(&self) -> LayerId;
```

### 1.2 Clipping

```rust
// painter.rs:163  — #[inline]
pub fn clip_rect(&self) -> Rect;

// painter.rs:173  — #[inline]  short for set_clip_rect(clip_rect().intersect(new))
pub fn shrink_clip_rect(&mut self, new_clip_rect: Rect);

// painter.rs:183  — #[inline]  WARNING: growing the clip rect may surprise you
pub fn set_clip_rect(&mut self, clip_rect: Rect);

// painter.rs:189  — #[inline]  pixel-perfect 1px lines
pub fn round_to_pixel_center(&self, point: f32) -> f32;
```

Prefer `shrink_clip_rect` / `with_clip_rect` over `set_clip_rect`. `with_clip_rect` returns a new
`Painter` whose clip rect is `rect.intersect(self.clip_rect)` (`painter.rs:73`).

### 1.3 Low level shape list

```rust
// painter.rs:213  — returns an index you can later `set`
pub fn add(&self, shape: impl Into<Shape>) -> ShapeIdx;

// painter.rs:226  — faster than repeated `add`
pub fn extend<I: IntoIterator<Item = Shape>>(&self, shapes: I);

// painter.rs:242  — retro-actively replace a shape (the "reserve a slot" trick)
pub fn set(&self, idx: ShapeIdx, shape: impl Into<Shape>);

// painter.rs:252
pub fn for_each_shape(&self, mut reader: impl FnMut(&ClippedShape));
```

The reserve-a-slot idiom (paint a frame behind contents whose size you don't know yet) is documented
at `egui-0.36.0/src/layers.rs:143–147`:

```rust
let idx = painter.add(egui::Shape::Noop);
// … lay out contents, learn `frame_rect` …
painter.set(idx, egui::epaint::RectShape::filled(frame_rect, 6, bg));
```

### 1.4 Debug painting

```rust
// painter.rs:266
pub fn debug_rect(&self, rect: Rect, color: Color32, text: impl ToString);

// painter.rs:283
pub fn error(&self, pos: Pos2, text: impl std::fmt::Display) -> Rect;

// painter.rs:292
pub fn debug_text(&self, pos: Pos2, anchor: Align2, color: Color32, text: impl ToString) -> Rect;
```

### 1.5 Primitives

```rust
// painter.rs:318
pub fn line_segment(&self, points: [Pos2; 2], stroke: impl Into<Stroke>) -> ShapeIdx;

// painter.rs:327  — NOTE: PathStroke, and it takes an owned Vec<Pos2>
pub fn line(&self, points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> ShapeIdx;

// painter.rs:332
pub fn hline(&self, x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) -> ShapeIdx;

// painter.rs:337
pub fn vline(&self, x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) -> ShapeIdx;

// painter.rs:341
pub fn circle(
    &self,
    center: Pos2,
    radius: f32,
    fill_color: impl Into<Color32>,
    stroke: impl Into<Stroke>,
) -> ShapeIdx;

// painter.rs:356
pub fn circle_filled(&self, center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> ShapeIdx;

// painter.rs:370
pub fn circle_stroke(&self, center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> ShapeIdx;

// painter.rs:380  — FIVE arguments
pub fn rect(
    &self,
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    fill_color: impl Into<Color32>,
    stroke: impl Into<Stroke>,
    stroke_kind: StrokeKind,
) -> ShapeIdx;

// painter.rs:397
pub fn rect_filled(
    &self,
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    fill_color: impl Into<Color32>,
) -> ShapeIdx;

// painter.rs:406  — FOUR arguments
pub fn rect_stroke(
    &self,
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    stroke: impl Into<Stroke>,
    stroke_kind: StrokeKind,
) -> ShapeIdx;

// painter.rs:417  — returns () , not ShapeIdx (it emits three line segments)
pub fn arrow(&self, origin: Pos2, vec: Vec2, stroke: impl Into<Stroke>);

// painter.rs:447
pub fn image(&self, texture_id: epaint::TextureId, rect: Rect, uv: Rect, tint: Color32) -> ShapeIdx;
```

There is **no** `Painter::ellipse*` convenience — use `painter.add(Shape::ellipse_filled(..))`
(`shapes/shape.rs:270`).

### 1.6 Text

```rust
// painter.rs:469  — returns where the text ended up
pub fn text(
    &self,
    pos: Pos2,
    anchor: Align2,
    text: impl ToString,
    font_id: FontId,
    text_color: Color32,
) -> Rect;

// painter.rs:488  — #[must_use] #[inline]
pub fn layout(&self, text: String, font_id: FontId, color: Color32, wrap_width: f32) -> Arc<Galley>;

// painter.rs:503  — #[must_use] #[inline]  (wrap_width = f32::INFINITY internally)
pub fn layout_no_wrap(&self, text: String, font_id: FontId, color: Color32) -> Arc<Galley>;

// painter.rs:517  — #[must_use] #[inline]
pub fn layout_job(&self, layout_job: LayoutJob) -> Arc<Galley>;

// painter.rs:529  — #[inline]  Color32::PLACEHOLDER parts get `fallback_color`
pub fn galley(&self, pos: Pos2, galley: Arc<Galley>, fallback_color: Color32);

// painter.rs:541  — #[inline]  ALL glyph colour is replaced
pub fn galley_with_override_text_color(&self, pos: Pos2, galley: Arc<Galley>, text_color: Color32);
```

`layout`/`layout_no_wrap`/`layout_job` take `String` **by value** (not `&str`, not `impl ToString`).

---

## 2. Getting a `Painter`

```rust
// egui-0.36.0/src/ui.rs:457   — #[inline]
pub fn painter(&self) -> &Painter;

// egui-0.36.0/src/ui.rs:619   — == self.painter().with_clip_rect(rect)
pub fn painter_at(&self, rect: Rect) -> Painter;

// egui-0.36.0/src/ui.rs:1370  — clips to the allocated rect for you
pub fn allocate_painter(&mut self, desired_size: Vec2, sense: Sense) -> (Response, Painter);

// egui-0.36.0/src/context.rs:1584
pub fn layer_painter(&self, layer_id: LayerId) -> Painter;

// egui-0.36.0/src/context.rs:1590  — on top of everything, incl. tooltips/popups
pub fn debug_painter(&self) -> Painter;
```

`layer_painter` builds a painter clipped to `Context::content_rect()` (`context.rs:1585–1586`).

⚠ **`Context::screen_rect` no longer exists** in 0.36 — a full-tree grep for `pub fn screen_rect`
over `egui-0.36.0/src` finds nothing. The replacements are:

```rust
// egui-0.36.0/src/context.rs:2904  — viewport minus OS status bar / display notches; USE THIS
pub fn content_rect(&self) -> Rect;

// egui-0.36.0/src/context.rs:2918  — the full area, including the notch / dynamic island
pub fn viewport_rect(&self) -> Rect;
```

Both are `round_ui()`-snapped. The underlying `InputState` methods are
`input_state/mod.rs:512` and `:526`.

`Ui` clip helpers mirror the painter's (`ui.rs:639`, `ui.rs:649`, `ui.rs:658`):

```rust
pub fn clip_rect(&self) -> Rect;
pub fn shrink_clip_rect(&mut self, new_clip_rect: Rect);
pub fn set_clip_rect(&mut self, clip_rect: Rect);
pub fn is_rect_visible(&self, rect: Rect) -> bool;   // ui.rs:666 — cull with this
pub fn layer_id(&self) -> LayerId;                   // ui.rs:625
```

---

## 3. `Shape` — every variant and every constructor

`epaint-0.36.2/src/shapes/shape.rs`. `#[must_use]`, `#[derive(Clone, Debug, PartialEq)]`,
`size_of::<Shape>() == 64` (asserted at `shape.rs:76`).

### 3.1 Variants (`shape.rs:27–71`)

```rust
pub enum Shape {
    Noop,                                                   // shape.rs:29
    Vec(Vec<Self>),                                         // shape.rs:33
    Circle(CircleShape),                                    // shape.rs:36
    Ellipse(EllipseShape),                                  // shape.rs:39
    LineSegment { points: [Pos2; 2], stroke: Stroke },       // shape.rs:42  (Stroke, not PathStroke)
    Path(PathShape),                                        // shape.rs:46
    Rect(RectShape),                                        // shape.rs:49
    Text(TextShape),                                        // shape.rs:54
    Mesh(Arc<Mesh>),                                        // shape.rs:61  (Arc!)
    QuadraticBezier(QuadraticBezierShape),                  // shape.rs:64
    CubicBezier(CubicBezierShape),                          // shape.rs:67
    Callback(PaintCallback),                                // shape.rs:70
}
```

`From` impls: `Vec<Shape>` (`shape.rs:92`), `Mesh` (`shape.rs:99`), `Arc<Mesh>` (`shape.rs:106`),
plus per-shape `From<RectShape>` (`rect_shape.rs:218`), `From<CircleShape>`
(`circle_shape.rs:47`), `From<PathShape>` (`path_shape.rs:76`), `From<TextShape>`
(`text_shape.rs:173`).

### 3.2 Constructors

```rust
// shape.rs:118  — #[inline]  more efficient than `line`
pub fn line_segment(points: [Pos2; 2], stroke: impl Into<Stroke>) -> Self;

// shape.rs:126
pub fn hline(x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) -> Self;

// shape.rs:135
pub fn vline(x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) -> Self;

// shape.rs:147  — #[inline]
pub fn line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self;

// shape.rs:153  — #[inline]
pub fn closed_line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self;

// shape.rs:158  — returns MANY shapes
pub fn dotted_line(path: &[Pos2], color: impl Into<Color32>, spacing: f32, radius: f32) -> Vec<Self>;

// shape.rs:170
pub fn dashed_line(
    path: &[Pos2],
    stroke: impl Into<Stroke>,
    dash_length: f32,
    gap_length: f32,
) -> Vec<Self>;

// shape.rs:189
pub fn dashed_line_with_offset(
    path: &[Pos2],
    stroke: impl Into<Stroke>,
    dash_lengths: &[f32],
    gap_lengths: &[f32],
    dash_offset: f32,
) -> Vec<Self>;

// shape.rs:210  — appends into `shapes` instead of allocating
pub fn dashed_line_many(
    points: &[Pos2],
    stroke: impl Into<Stroke>,
    dash_length: f32,
    gap_length: f32,
    shapes: &mut Vec<Self>,
);

// shape.rs:229
pub fn dashed_line_many_with_offset(
    points: &[Pos2],
    stroke: impl Into<Stroke>,
    dash_lengths: &[f32],
    gap_lengths: &[f32],
    dash_offset: f32,
    shapes: &mut Vec<Self>,
);

// shape.rs:251  — #[inline]  fill only works for CONVEX polygons; clockwise is fastest
pub fn convex_polygon(
    points: Vec<Pos2>,
    fill: impl Into<Color32>,
    stroke: impl Into<PathStroke>,
) -> Self;

// shape.rs:260 / 265  — #[inline]
pub fn circle_filled(center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> Self;
pub fn circle_stroke(center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> Self;

// shape.rs:270 / 275  — #[inline]  radius is a Vec2 (a, b)
pub fn ellipse_filled(center: Pos2, radius: Vec2, fill_color: impl Into<Color32>) -> Self;
pub fn ellipse_stroke(center: Pos2, radius: Vec2, stroke: impl Into<Stroke>) -> Self;

// shape.rs:281 / 291  — #[inline]
pub fn rect_filled(rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>) -> Self;
pub fn rect_stroke(
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    stroke: impl Into<Stroke>,
    stroke_kind: StrokeKind,
) -> Self;

// shape.rs:306  — #[inline]  NEW-ish; builds a 4-vertex Mesh for you
pub fn gradient_rect(rect: Rect, direction: Direction, [from, to]: [Color32; 2]) -> Self;

// shape.rs:327  — note: &mut FontsView, not &Fonts
pub fn text(
    fonts: &mut FontsView<'_>,
    pos: Pos2,
    anchor: Align2,
    text: impl ToString,
    font_id: FontId,
    color: Color32,
) -> Self;

// shape.rs:344 / 350  — #[inline]
pub fn galley(pos: Pos2, galley: Arc<Galley>, fallback_color: Color32) -> Self;
pub fn galley_with_override_text_color(pos: Pos2, galley: Arc<Galley>, text_color: Color32) -> Self;

// shape.rs:361  — #[inline]  debug_assert!(mesh.is_valid())
pub fn mesh(mesh: impl Into<Arc<Mesh>>) -> Self;

// shape.rs:373  — builds a textured Mesh via Mesh::add_rect_with_uv
pub fn image(texture_id: TextureId, rect: Rect, uv: Rect, tint: Color32) -> Self;
```

### 3.3 Inspection & transforms

```rust
// shape.rs:380  — includes stroke widths; Rect::NOTHING for Noop/empty
pub fn visual_bounding_rect(&self) -> Rect;

// shape.rs:413  — #[inline(always)]
pub fn texture_id(&self) -> crate::TextureId;

// shape.rs:427 / 435  — #[inline(always)]  thin wrappers over `transform`
pub fn scale(&mut self, factor: f32);
pub fn translate(&mut self, delta: Vec2);

// shape.rs:443  — for PaintCallback only the rect is scaled, not the stroke
pub fn transform(&mut self, transform: TSTransform);
```

Colour rewriting lives in a free function
(`epaint-0.36.2/src/shape_transform.rs:9`) — remember `Color32::PLACEHOLDER` is special:

```rust
pub fn adjust_colors(
    shape: &mut Shape,
    adjust_color: impl Fn(&mut Color32) + Send + Sync + Copy + 'static,
);
```

### 3.4 `Shape::gradient_rect` direction table

Read straight off `shape.rs:307–312`:

| `Direction` | left_top | right_top | left_bottom | right_bottom |
|---|---|---|---|---|
| `TopDown` | `from` | `from` | `to` | `to` |
| `BottomUp` | `to` | `to` | `from` | `from` |
| `LeftToRight` | `from` | `to` | `from` | `to` |
| `RightToLeft` | `to` | `from` | `to` | `from` |

```rust
// epaint-0.36.2/src/direction.rs:4
pub enum Direction { LeftToRight, RightToLeft, TopDown, BottomUp }
// direction.rs:13 / 21
pub fn is_horizontal(self) -> bool;
pub fn is_vertical(self) -> bool;
```

---

## 4. `RectShape`

`epaint-0.36.2/src/shapes/rect_shape.rs`. `size_of::<RectShape>() == 56` (`rect_shape.rs:65`).

```rust
// rect_shape.rs:8–60
pub struct RectShape {
    pub rect: Rect,                     // :9
    pub corner_radius: CornerRadius,    // :21   <-- NOT `rounding`
    pub fill: Color32,                  // :24
    pub stroke: Stroke,                 // :30
    pub stroke_kind: StrokeKind,        // :35
    pub round_to_pixels: Option<bool>,  // :42   None => TessellationOptions::round_rects_to_pixels
    pub blur_width: f32,                // :50   > 0 blurs BOTH fill and stroke (shadows/glow)
    pub brush: Option<Arc<Brush>>,      // :56   texturing
    pub angle: f32,                     // :59   radians, clockwise, about the rect center
}
```

Constructors and builders:

```rust
// rect_shape.rs:78  — #[inline]
pub fn new(
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    fill_color: impl Into<Color32>,
    stroke: impl Into<Stroke>,
    stroke_kind: StrokeKind,
) -> Self;

// rect_shape.rs:99  — #[inline]  stroke = Stroke::NONE, stroke_kind = Outside (irrelevant)
pub fn filled(rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>) -> Self;

// rect_shape.rs:114 — #[inline]  fill = Color32::TRANSPARENT
pub fn stroke(
    rect: Rect,
    corner_radius: impl Into<CornerRadius>,
    stroke: impl Into<Stroke>,
    stroke_kind: StrokeKind,
) -> Self;

// rect_shape.rs:126 / 137 / 149 / 156 / 167 / 174  — all #[inline], all `mut self -> Self`
pub fn with_stroke_kind(mut self, stroke_kind: StrokeKind) -> Self;
pub fn with_round_to_pixels(mut self, round_to_pixels: bool) -> Self;
pub fn with_blur_width(mut self, blur_width: f32) -> Self;
pub fn with_texture(mut self, fill_texture_id: TextureId, uv: Rect) -> Self;
pub fn with_angle(mut self, angle: f32) -> Self;
pub fn with_angle_and_pivot(mut self, angle: f32, pivot: Pos2) -> Self;

// rect_shape.rs:185 — #[inline]
pub fn visual_bounding_rect(&self) -> Rect;
// rect_shape.rs:211 — TextureId::default() when untextured
pub fn fill_texture_id(&self) -> TextureId;
```

`Brush` (`epaint-0.36.2/src/brush.rs:6`) is `{ pub fill_texture_id: TextureId, pub uv: Rect }`.
The texture is **multiplied** with `RectShape::fill` (`brush.rs:9`), so pass `Color32::WHITE` as the
fill for an untinted texture. `Rect::ZERO` as `uv` turns texturing off (`brush.rs:17`).

`visual_bounding_rect` expansion by stroke kind (`rect_shape.rs:189–193`):
`Inside => 0.0`, `Middle => stroke.width / 2.0`, `Outside => stroke.width`; plus `blur_width / 2.0`.

---

## 5. `CircleShape` / `EllipseShape` / `PathShape` / `TextShape`

### 5.1 `CircleShape` (`epaint-0.36.2/src/shapes/circle_shape.rs`) — `Copy`

```rust
// circle_shape.rs:6
pub struct CircleShape { pub center: Pos2, pub radius: f32, pub fill: Color32, pub stroke: Stroke }

pub fn filled(center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> Self;  // :15 #[inline]
pub fn stroke(center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> Self;       // :25 #[inline]
pub fn visual_bounding_rect(&self) -> Rect;                                        // :35
```

Note there is **no `stroke_kind`** on a circle; the stroke is centred on the radius.

### 5.2 `EllipseShape` (`epaint-0.36.2/src/shapes/ellipse_shape.rs`) — `Copy`

```rust
// ellipse_shape.rs:6
pub struct EllipseShape {
    pub center: Pos2,
    pub radius: Vec2,   // :10  half-width, half-height
    pub fill: Color32,
    pub stroke: Stroke,
    pub angle: f32,     // :15  radians, clockwise
}

pub fn filled(center: Pos2, radius: Vec2, fill_color: impl Into<Color32>) -> Self;   // :20
pub fn stroke(center: Pos2, radius: Vec2, stroke: impl Into<Stroke>) -> Self;        // :31
pub fn with_angle(mut self, angle: f32) -> Self;                                     // :44
pub fn with_angle_and_pivot(mut self, angle: f32, pivot: Pos2) -> Self;              // :51
pub fn visual_bounding_rect(&self) -> Rect;                                          // :59
```

### 5.3 `PathShape` (`epaint-0.36.2/src/shapes/path_shape.rs`)

```rust
// path_shape.rs:6
pub struct PathShape {
    pub points: Vec<Pos2>,   // :8   filled paths should prefer CLOCKWISE order
    pub closed: bool,        // :12  required if fill != TRANSPARENT
    pub fill: Color32,       // :15  CONVEX polygons only
    pub stroke: PathStroke,  // :18  PathStroke, not Stroke
}

pub fn line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self;         // :28 #[inline]
pub fn closed_line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self;  // :39 #[inline]
pub fn convex_polygon(
    points: Vec<Pos2>,
    fill: impl Into<Color32>,
    stroke: impl Into<PathStroke>,
) -> Self;                                                                      // :52 #[inline]
pub fn visual_bounding_rect(&self) -> Rect;                                     // :67 #[inline]
```

`PathShape` has **no texture support** (see the `TODO` at `path_shape.rs:19`). For a textured or
per-pixel-coloured path use `PathStroke::new_uv`, or build a `Mesh`.

### 5.4 `TextShape` (`epaint-0.36.2/src/shapes/text_shape.rs`)

```rust
// text_shape.rs:12
pub struct TextShape {
    pub pos: Pos2,                              // :16  origin = top-left of first char
    pub galley: Arc<Galley>,                    // :19
    pub underline: Stroke,                      // :23  applied to the WHOLE text
    pub fallback_color: Color32,                // :27  replaces Color32::PLACEHOLDER everywhere
    pub override_text_color: Option<Color32>,   // :33  glyphs only; not bg/underline/strikethrough
    pub opacity_factor: f32,                    // :37  gamma-space
    pub angle: f32,                             // :41  radians clockwise, pivot = `pos`
}

pub fn new(pos: Pos2, galley: Arc<Galley>, fallback_color: Color32) -> Self;  // :49 #[inline]
pub fn visual_bounding_rect(&self) -> Rect;                                   // :63 #[inline]
pub fn with_underline(mut self, underline: Stroke) -> Self;                   // :71 #[inline]
pub fn with_override_text_color(mut self, override_text_color: Color32) -> Self; // :78
pub fn with_angle(mut self, angle: f32) -> Self;                              // :86 #[inline]
pub fn with_angle_and_anchor(mut self, angle: f32, anchor: Align2) -> Self;   // :94 #[inline]
pub fn with_opacity_factor(mut self, opacity_factor: f32) -> Self;            // :104 #[inline]
pub fn transform(&mut self, transform: emath::TSTransform);                   // :110
```

`Shape::Text` must be **recreated whenever `pixels_per_point` changes** (`shape.rs:23`,
`text_shape.rs:9`).

### 5.5 Bézier shapes (`epaint-0.36.2/src/shapes/bezier_shape.rs`)

```rust
// bezier_shape.rs:15
pub struct CubicBezierShape { pub points: [Pos2; 4], pub closed: bool, pub fill: Color32, pub stroke: PathStroke }
// bezier_shape.rs:30
pub fn from_points_stroke(points: [Pos2; 4], closed: bool, fill: Color32, stroke: impl Into<PathStroke>) -> Self;

// bezier_shape.rs:385
pub struct QuadraticBezierShape { pub points: [Pos2; 3], pub closed: bool, pub fill: Color32, pub stroke: PathStroke }
// bezier_shape.rs:401  — points are [start, control, end]
pub fn from_points_stroke(points: [Pos2; 3], closed: bool, fill: Color32, stroke: impl Into<PathStroke>) -> Self;
```

Other useful members: `to_path_shapes(tolerance: Option<f32>, epsilon: Option<f32>) -> Vec<PathShape>`
(`:63`), `to_path_shape(tolerance: Option<f32>) -> PathShape` (`:431`),
`sample(t: f32) -> Pos2` (`:278`, `:503`), `flatten(tolerance: Option<f32>) -> Vec<Pos2>`
(`:299`, `:522`), `visual_bounding_rect()` (`:79`, `:442`),
`logical_bounding_rect()` (`:88`, `:451`), `split_range(t_range: Range<f32>) -> Self` (`:142`).

### 5.6 `PaintCallback` (`epaint-0.36.2/src/shapes/paint_callback.rs`)

```rust
// paint_callback.rs:60
pub struct PaintCallback {
    pub rect: Rect,                           // :64
    pub callback: Arc<dyn Any + Send + Sync>, // :82  downcast in your renderer
}

// paint_callback.rs:7
pub struct PaintCallbackInfo {
    pub viewport: Rect,          // :16
    pub clip_rect: Rect,         // :19
    pub pixels_per_point: f32,   // :22
    pub screen_size_px: [u32; 2],// :25
}
pub fn viewport_in_pixels(&self) -> ViewportInPixels;   // :46
pub fn clip_rect_in_pixels(&self) -> ViewportInPixels;  // :51
```

---

## 6. `Stroke` vs `PathStroke` vs `StrokeKind`

`epaint-0.36.2/src/stroke.rs`.

### 6.1 `Stroke` — `Copy`, used by rects, circles, ellipses, line segments, underlines

```rust
// stroke.rs:13
pub struct Stroke { pub width: f32, pub color: Color32 }

pub const NONE: Self = Self { width: 0.0, color: Color32::TRANSPARENT };  // stroke.rs:20 (== Default)
pub fn new(width: f32, color: impl Into<Color32>) -> Self;                // stroke.rs:26 #[inline]
pub fn is_empty(&self) -> bool;                                           // stroke.rs:35 width<=0 || color transparent
pub fn round_center_to_pixel(&self, pixels_per_point: f32, coord: &mut f32); // stroke.rs:41
```

Conversion: `impl<Color: Into<Color32>> From<(f32, Color)> for Stroke` (`stroke.rs:80`), so
`(1.0, Color32::RED)` works anywhere an `impl Into<Stroke>` is wanted.

### 6.2 `StrokeKind` — `Copy`, `Eq`

```rust
// stroke.rs:102
pub enum StrokeKind {
    Inside,   // :104  entirely inside the shape  — use this to tile rects perfectly
    Middle,   // :107  half in, half out          — NOT spelled `Center`
    Outside,  // :110  entirely outside
}
```

### 6.3 `PathStroke` — NOT `Copy` (holds an `Arc` in the UV variant)

```rust
// stroke.rs:118
pub struct PathStroke {
    pub width: f32,
    pub color: ColorMode,     // Solid or UV callback
    pub kind: StrokeKind,     // <-- third field, new relative to older versions
}

pub const NONE: Self = Self { width: 0.0, color: ColorMode::TRANSPARENT, kind: StrokeKind::Middle }; // :133
pub fn new(width: f32, color: impl Into<Color32>) -> Self;   // :140  kind defaults to Middle
pub fn new_uv(
    width: f32,
    callback: impl Fn(Rect, Pos2) -> Color32 + Send + Sync + 'static,
) -> Self;                                                   // :152
pub fn with_kind(self, kind: StrokeKind) -> Self;            // :164 #[inline]
pub fn middle(self) -> Self;                                 // :170
pub fn outside(self) -> Self;                                // :179
pub fn inside(self) -> Self;                                 // :188
pub fn is_empty(&self) -> bool;                              // :197
```

Conversions: `From<(f32, Color)>` (`stroke.rs:202`) and `From<Stroke>` (`stroke.rs:212`). The
`From<Stroke>` impl maps an **empty** `Stroke` to `PathStroke::NONE` on purpose — the stroke colour
is used when feathering the fill (`stroke.rs:215–216`).

`new_uv`'s callback receives a bounding box that has been expanded by
`TessellationOptions::feathering_size_in_pixels` (`stroke.rs:150`). This is the per-pixel gradient
hook for *strokes*.

### 6.4 `ColorMode` (`epaint-0.36.2/src/color.rs`)

```rust
// color.rs:10
pub enum ColorMode {
    Solid(Color32),                                       // :12
    UV(Arc<dyn Fn(Rect, Pos2) -> Color32 + Send + Sync>), // :19  #[serde(skip)] — NOT serializable
}
pub const TRANSPARENT: Self = Self::Solid(Color32::TRANSPARENT);  // color.rs:48
```

`PartialEq` on two `UV` variants always returns **`false`** (`color.rs:41`).

---

## 7. `CornerRadius` (and `CornerRadiusF32`)

`epaint-0.36.2/src/corner_radius.rs`. `Copy, Clone, Debug, PartialEq, Eq, Hash`. **`Rounding` does
not exist in this version, under any spelling.**

```rust
// corner_radius.rs:13
pub struct CornerRadius { pub nw: u8, pub ne: u8, pub sw: u8, pub se: u8 }

pub const ZERO: Self;                       // :50
pub const fn same(radius: u8) -> Self;      // :59
pub fn is_same(self) -> bool;               // :70
pub fn at_least(self, min: u8) -> Self;     // :76
pub fn at_most(self, max: u8) -> Self;      // :87
pub fn average(&self) -> f32;               // :97
```

`Default` is `ZERO` (`corner_radius.rs:29`). Conversions:

```rust
impl From<u8>  for CornerRadius;   // corner_radius.rs:34  Self::same(radius)
impl From<f32> for CornerRadius;   // corner_radius.rs:41  Self::same(radius.round() as u8)
```

Because only the `u8` impl is applicable to an integer literal, **`corner_radius(6)` infers `u8`
and compiles** — this is what egui's own doc examples use (`egui-0.36.0/src/painter.rs:442`,
`src/lib.rs:527`). `6.0` also compiles, via the `f32` impl.

Operator impls (all saturating on `u8`): `Add`/`AddAssign` for `Self` and for `u8`
(`:102, :115, :128, :140`), `Sub`/`SubAssign` for `Self` and `u8` (`:152, :165, :178, :190`),
`Div<f32>`/`DivAssign<f32>` (`:202, :215`), `Mul<f32>`/`MulAssign<f32>` (`:227, :240`).

For maths where `u8` truncation hurts, use **`CornerRadiusF32`**
(`epaint-0.36.2/src/corner_radius_f32.rs:8`):

```rust
pub struct CornerRadiusF32 { /* nw, ne, sw, se: f32 */ }
pub const ZERO: Self;                        // corner_radius_f32.rs:67
pub const fn same(radius: f32) -> Self;      // :76
pub fn is_same(&self) -> bool;               // :87
pub fn at_least(&self, min: f32) -> Self;    // :93
pub fn at_most(&self, max: f32) -> Self;     // :104
```

The tessellator works in `CornerRadiusF32` internally (`epaint-0.36.2/src/tessellator.rs:13`).

---

## 8. `Color32` — constructors, accessors, blending

`ecolor-0.36.2/src/color32.rs`. `#[repr(C)] #[repr(align(4))]`, internally
`[u8; 4]` sRGB gamma space with **premultiplied alpha** (`color32.rs:31`, doc `:8–25`).
`alpha == 0` means the colour is **additive**, not invisible (`color32.rs:25`).

### 8.1 Constants (`color32.rs:60–104`)

`TRANSPARENT, BLACK, DARK_GRAY, GRAY, LIGHT_GRAY, WHITE, BROWN, DARK_RED, RED, LIGHT_RED, CYAN,
MAGENTA, YELLOW, ORANGE, LIGHT_YELLOW, KHAKI, DARK_GREEN, GREEN, LIGHT_GREEN, DARK_BLUE, BLUE,
LIGHT_BLUE, PURPLE, GOLD, DEBUG_COLOR, PLACEHOLDER`.

`PLACEHOLDER` (`color32.rs:104`) is `from_rgba_premultiplied(64, 254, 0, 128)` — an intentionally
invalid colour used as the "fill this in later" key by galleys and `Painter::galley`.

### 8.2 Constructors

```rust
pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self;                              // :108  alpha 255
pub const fn from_rgb_additive(r: u8, g: u8, b: u8) -> Self;                     // :114  alpha 0
pub const fn from_rgba_premultiplied(r: u8, g: u8, b: u8, a: u8) -> Self;        // :122
pub fn from_rgba_unmultiplied(r: u8, g: u8, b: u8, a: u8) -> Self;               // :133  NOT const
pub const fn from_rgba_unmultiplied_const(r: u8, g: u8, b: u8, a: u8) -> Self;   // :139  const version
pub const fn from_gray(l: u8) -> Self;                                           // :159
pub const fn from_black_alpha(a: u8) -> Self;                                    // :165  const
pub fn from_white_alpha(a: u8) -> Self;                                          // :171  NOT const
pub const fn from_additive_luminance(l: u8) -> Self;                             // :177
```

### 8.3 Accessors

```rust
pub const fn r(&self) -> u8;   // :188  (premultiplied)
pub const fn g(&self) -> u8;   // :194
pub const fn b(&self) -> u8;   // :200
pub const fn a(&self) -> u8;   // :206
pub const fn is_opaque(&self) -> bool;          // :182
pub fn is_additive(self) -> bool;               // :225  a() == 0
pub const fn to_array(&self) -> [u8; 4];        // :231  premultiplied
pub const fn to_tuple(&self) -> (u8,u8,u8,u8);  // :237
pub fn to_srgba_unmultiplied(&self) -> [u8; 4]; // :248  un-premultiplies (lossy for a∉{0,255})
pub fn to_normalized_gamma_f32(self) -> [f32;4];// :320  NO gamma conversion — raw /255
pub fn intensity(&self) -> f32;                 // :351  0.299r + 0.587g + 0.114b, /255
```

`Color32` also implements `Index<usize>`/`IndexMut<usize>` over the raw `[u8;4]`
(`color32.rs:41, :50`).

### 8.4 Blending helpers

```rust
pub fn to_opaque(self) -> Self;                       // :212  via Rgba
pub const fn additive(self) -> Self;                  // :218  zero the alpha
pub fn gamma_multiply(self, factor: f32) -> Self;     // :269  FAST, perceptually even — prefer this
pub fn gamma_multiply_u8(self, factor: u8) -> Self;   // :289  integer version, 255 == 1.0
pub fn linear_multiply(self, factor: f32) -> Self;    // :305  goes through Rgba; slower, not even
pub fn lerp_to_gamma(&self, other: Self, t: f32) -> Self; // :331  gamma-space lerp
pub fn blend(self, on_top: Self) -> Self;             // :343  self BEHIND on_top, gamma space
```

`blend` is literally `self.gamma_multiply_u8(255 - on_top.a()) + on_top` (`color32.rs:344`).

Operators: `Mul` is a fast gamma-space component-wise multiply (`color32.rs:356`), `Add` is a
saturating component-wise add (`color32.rs:371`).

### 8.5 Free functions in `ecolor` (a.k.a. `egui::ecolor`)

```rust
pub fn linear_f32_from_gamma_u8(s: u8) -> f32;                  // ecolor-0.36.2/src/lib.rs:97
pub fn gamma_u8_from_linear_f32(l: f32) -> u8;                  // :114
pub fn linear_u8_from_linear_f32(a: f32) -> u8;                 // :129
pub fn linear_from_gamma(gamma: f32) -> f32;                    // :159
pub fn gamma_from_linear(linear: f32) -> f32;                   // :171
pub fn tint_color_towards(color: Color32, target: Color32) -> Color32;  // :185
```

Also re-exported from `ecolor`: `Rgba`, `Hsva`, `HsvaGamma` (`epaint-0.36.2/src/lib.rs:74`); at the
`egui` root only `Color32` and `Rgba` are re-exported (`egui-0.36.0/src/lib.rs:442`), so reach for
`egui::ecolor::Hsva` for the rest.

---

## 9. `Rect` / `Pos2` / `Vec2` / `Rangef` / `Align2` — the layout maths that matters

`emath-0.36.2`. All are `Copy`.

### 9.1 `Rect` constructors (`emath-0.36.2/src/rect.rs`)

```rust
pub const fn from_min_max(min: Pos2, max: Pos2) -> Self;                       // :73
pub fn from_min_size(min: Pos2, size: Vec2) -> Self;                           // :79
pub fn from_center_size(center: Pos2, size: Vec2) -> Self;                     // :87
pub fn from_x_y_ranges(x_range: impl Into<Rangef>, y_range: impl Into<Rangef>) -> Self;  // :95
pub fn from_two_pos(a: Pos2, b: Pos2) -> Self;                                 // :106
pub fn from_pos(point: Pos2) -> Self;                                          // :115
pub fn from_points(points: &[Pos2]) -> Self;                                   // :123
pub fn everything_right_of(left_x: f32) -> Self;                               // :133
pub fn everything_left_of(right_x: f32) -> Self;                               // :141
pub fn everything_below(top_y: f32) -> Self;                                   // :149
pub fn everything_above(bottom_y: f32) -> Self;                                // :157
```

Constants: `EVERYTHING` (`:35`), `NOTHING` (`:55`), `NAN` (`:61`), `ZERO` (`:67`).

### 9.2 `Rect` — resizing, hit-testing, splitting

```rust
pub fn with_min_x(mut self, min_x: f32) -> Self;    // :165   (also with_min_y :172, with_max_x :179, with_max_y :186)
pub fn expand(self, amnt: f32) -> Self;             // :193
pub fn expand2(self, amnt: Vec2) -> Self;           // :199
pub fn shrink(self, amnt: f32) -> Self;             // :217
pub fn shrink2(self, amnt: Vec2) -> Self;           // :223
pub fn scale_from_center(self, scale_factor: f32) -> Self;    // :205
pub fn scale_from_center2(self, scale_factor: Vec2) -> Self;  // :211
pub fn translate(self, amnt: Vec2) -> Self;         // :229
pub fn rotate_bb(self, rot: Rot2) -> Self;          // :236   bounding box of the rotated rect
pub fn intersects(self, other: Self) -> bool;       // :250
pub fn contains(&self, p: Pos2) -> bool;            // :274
pub fn contains_rect(&self, other: Self) -> bool;   // :279
pub fn clamp(&self, p: Pos2) -> Pos2;               // :286
pub fn extend_with(&mut self, p: Pos2);             // :291   (also extend_with_x :298, _y :305)
pub fn union(self, other: Self) -> Self;            // :314   (operator: `a |= b`)
pub fn intersect(self, other: Self) -> Self;        // :324   (this is what clip rects use)
pub fn split_left_right_at_fraction(&self, t: f32) -> (Self, Self);   // :654
pub fn split_left_right_at_x(&self, split_x: f32) -> (Self, Self);    // :659
pub fn split_top_bottom_at_fraction(&self, t: f32) -> (Self, Self);   // :666
pub fn split_top_bottom_at_y(&self, split_y: f32) -> (Self, Self);    // :671
pub fn lerp_inside(&self, t: impl Into<Vec2>) -> Pos2;                // :452
pub fn lerp_towards(&self, other: &Self, t: f32) -> Self;             // :462
pub fn intersects_ray(&self, o: Pos2, d: Vec2) -> bool;               // :682
pub fn intersects_ray_from_center(&self, d: Vec2) -> Pos2;            // :714
```

### 9.3 `Rect` — queries

```rust
pub fn center(&self) -> Pos2;    // :332      pub fn size(&self) -> Vec2;   // :341
pub fn width(&self) -> f32;      // :347      pub fn height(&self) -> f32;  // :353
pub fn aspect_ratio(&self) -> f32;            // :362
pub fn square_proportions(&self) -> Vec2;     // :369
pub fn area(&self) -> f32;                    // :381
pub fn distance_to_pos(&self, pos: Pos2) -> f32;        // :391
pub fn distance_sq_to_pos(&self, pos: Pos2) -> f32;     // :401
pub fn signed_distance_to_pos(&self, pos: Pos2) -> f32; // :438   negative inside
pub fn x_range(&self) -> Rangef;   // :470    pub fn y_range(&self) -> Rangef;   // :475
pub fn range_along(&self, axis: usize) -> Rangef;       // :486
pub fn size_along(&self, axis: usize) -> f32;           // :501
pub fn bottom_up_range(&self) -> Rangef;                // :506
pub fn is_negative(&self) -> bool;  // :512   pub fn is_positive(&self) -> bool;  // :518
pub fn is_finite(&self) -> bool;    // :524   pub fn any_nan(self) -> bool;       // :530
```

Edges and corners: `left/right/top/bottom` (`:539, :557, :575, :593`), their `_mut` variants
(`:545, :563, :581, :599`), `set_left/right/top/bottom` (`:551, :569, :587, :605`), and
`left_top` (`:611`), `center_top` (`:616`), `right_top` (`:622`), `left_center` (`:627`),
`right_center` (`:632`), `left_bottom` (`:638`), `center_bottom` (`:643`), `right_bottom` (`:649`).
Mutators: `set_width` (`:258`), `set_height` (`:263`), `set_center` (`:268`).

### 9.4 `Pos2` (`emath-0.36.2/src/pos2.rs`)

```rust
pub const fn pos2(x: f32, y: f32) -> Pos2;   // :29   free fn, re-exported as egui::pos2
pub const ZERO: Self;  // :120        pub const NAN: Self;  // :122
pub const fn new(x: f32, y: f32) -> Self;    // :128
pub fn to_vec2(self) -> Vec2;                // :135
pub fn distance(self, other: Self) -> f32;   // :143
pub fn distance_sq(self, other: Self) -> f32;// :148
pub fn floor(self) -> Self;  // :153   pub fn round(self) -> Self;  // :158   pub fn ceil(self) -> Self; // :163
pub fn is_finite(self) -> bool;  // :169      pub fn any_nan(self) -> bool;  // :175
pub fn min(self, other: Self) -> Self;  // :181   pub fn max(self, other: Self) -> Self;  // :187
pub fn clamp(self, min: Self, max: Self) -> Self;  // :193
pub fn lerp(&self, other: Self, t: f32) -> Self;   // :201
```

### 9.5 `Vec2` (`emath-0.36.2/src/vec2.rs`)

```rust
pub const fn vec2(x: f32, y: f32) -> Vec2;   // :26   free fn, re-exported as egui::vec2
pub const X / Y / RIGHT / LEFT / UP / DOWN / ZERO / ONE / INFINITY / NAN;  // :125–145
pub const fn new(x: f32, y: f32) -> Self;    // :148
pub const fn splat(v: f32) -> Self;          // :154
pub fn to_pos2(self) -> Pos2;                // :161
pub fn normalized(self) -> Self;             // :171
pub fn is_normalized(self) -> bool;          // :178
pub fn rot90(self) -> Self;                  // :185
pub fn length(self) -> f32;                  // :190
pub fn length_sq(self) -> f32;               // :195
pub fn angle(self) -> f32;                   // :216
pub fn angled(angle: f32) -> Self;           // :232   unit vector at `angle` radians
pub fn floor/round/ceil/abs(self) -> Self;   // :239 :245 :251 :257
pub fn is_finite(self) -> bool;  // :263      pub fn any_nan(self) -> bool;  // :269
pub fn min/max(self, other: Self) -> Self;   // :275 :281
pub fn dot(self, other: Self) -> f32;        // :287
pub fn min_elem(self) -> f32;  // :294        pub fn max_elem(self) -> f32;  // :301
pub fn yx(self) -> Self;                     // :308
pub fn clamp(self, min: Self, max: Self) -> Self;  // :317
```

`f32 * Vec2` is implemented (egui's own image code relies on it —
`egui-0.36.0/src/widgets/image.rs:375` writes `(pixels_per_point * rect.size()).round()`).

### 9.6 `Rangef` (`emath-0.36.2/src/range.rs`)

```rust
pub const EVERYTHING: Self;  // :17    pub const NOTHING: Self;  // :24    pub const NAN: Self;  // :30
pub fn new(min: f32, max: f32) -> Self;   // :36
pub fn point(min_and_max: f32) -> Self;   // :41
pub fn span(self) -> f32;                 // :50
pub fn center(self) -> f32;               // :56
pub fn contains(self, x: f32) -> bool;    // :62
pub fn clamp(self, x: f32) -> f32;        // :69
pub fn as_positive(self) -> Self;         // :75
pub fn shrink(self, amnt: f32) -> Self;   // :85
pub fn expand(self, amnt: f32) -> Self;   // :95
pub fn flip(self) -> Self;                // :105
pub fn intersection(self, other: Self) -> Self;  // :124
pub fn intersects(self, other: Self) -> bool;    // :142
```

`Painter::hline`/`vline` take `impl Into<Rangef>`, so `rect.x_range()` or `0.0..=100.0` both work.

### 9.7 `Align2` (`emath-0.36.2/src/align.rs`)

```rust
pub const LEFT_BOTTOM / LEFT_CENTER / LEFT_TOP
       / CENTER_BOTTOM / CENTER_CENTER / CENTER_TOP
       / RIGHT_BOTTOM / RIGHT_CENTER / RIGHT_TOP;   // align.rs:154–162

pub fn x(self) -> Align;   // :168      pub fn y(self) -> Align;   // :174
pub fn to_sign(self) -> Vec2;           // :179
pub fn flip_x/flip_y/flip(self) -> Self;// :185 :191 :197
pub fn anchor_rect(self, rect: Rect) -> Rect;                     // :203
pub fn anchor_size(self, pos: Pos2, size: Vec2) -> Rect;          // :220  <-- what Painter::text uses
pub fn align_size_within_rect(self, size: Vec2, frame: Rect) -> Rect;  // :235
pub fn pos_in_rect(self, frame: &Rect) -> Pos2;                   // :261
```

`Align` itself (`align.rs`): `Min | Center | Max` with aliases `LEFT`(`:22`), `RIGHT`(`:25`),
`TOP`(`:28`), `BOTTOM`(`:31`); `to_factor` (`:35`), `to_sign` (`:45`), `flip` (`:55`),
`align_size_within_range(self, size: f32, range: impl Into<Rangef>) -> Rangef` (`:123`).

### 9.8 Pixel snapping — the `GuiRounding` trait

`emath-0.36.2/src/gui_rounding.rs`. `pub const GUI_ROUNDING: f32 = 1.0 / 32.0;` (`:18`).

```rust
pub trait GuiRounding {
    fn round_ui(self) -> Self;                                // :31  snap to 1/32 of a point
    fn floor_ui(self) -> Self;                                // :34
    fn round_to_pixels(self, pixels_per_point: f32) -> Self;  // :43  snap to physical pixel edges
    fn round_to_pixel_center(self, pixels_per_point: f32) -> Self;  // :53  snap to pixel centres
}
```

Implemented for `f32` (`:56`), `f64` (`:78`), `Vec2` (`:100`), `Pos2` (`:129`), `Rect` (`:157`).
Not re-exported at the `egui` root — import as `use egui::emath::GuiRounding as _;`.

Rule of thumb (from `stroke.rs:44–52`): **odd**-pixel-wide strokes want `round_to_pixel_center`;
**even**-pixel-wide strokes and fills want `round_to_pixels`.

Interpolation helpers (`emath-0.36.2/src/lib.rs`):

```rust
pub fn lerp<R, T>(range: impl Into<RangeInclusive<R>>, t: T) -> R;   // :106   egui::lerp        ✅ at root
pub fn remap<T>(x: T, from: impl Into<RangeInclusive<T>>, to: impl Into<RangeInclusive<T>>) -> T;  // :161  egui::remap       ✅ at root
pub fn remap_clamp<T>(/* same shape */) -> T;                        // :176   egui::remap_clamp ✅ at root
pub fn fast_midpoint<R>(a: R, b: R) -> R;                            // :122   egui::emath::fast_midpoint     ❌ NOT at root
pub fn inverse_lerp<R>(range: RangeInclusive<R>, value: R) -> Option<R>;  // :145  egui::emath::inverse_lerp  ❌ NOT at root
pub fn normalized_angle(mut angle: f32) -> f32;                      // :367   egui::emath::normalized_angle  ❌ NOT at root
pub fn ease_in_ease_out(t: f32) -> f32;                              // :460   egui::emath::ease_in_ease_out  ❌ NOT at root
```

⚠ The root re-export list is exactly `Align, Align2, NumExt, Pos2, Rangef, Rect, RectAlign, Vec2,
Vec2b, lerp, pos2, remap, remap_clamp, vec2` (`egui-0.36.0/src/lib.rs:443–446`). The four marked
❌ above were verified to fail compilation as `egui::fast_midpoint` etc. (`E0425: cannot find
function … in crate egui`); reach them through `egui::emath::`.

---

## 10. `Mesh`, `Vertex`, and drawing a gradient

`epaint-0.36.2/src/mesh.rs`.

```rust
// mesh.rs:12  #[repr(C)]
pub struct Vertex {
    pub pos: Pos2,      // :15  logical points, (0,0) = top-left of screen
    pub uv: Pos2,       // :20  normalized texture coords
    pub color: Color32, // :23  sRGBA premultiplied
}
pub fn untextured(pos: Pos2, color: Color32) -> Self;  // mesh.rs:29 #[inline]  (uv = WHITE_UV)

// mesh.rs:60
pub struct Mesh {
    pub indices: Vec<u32>,        // :66  always a multiple of 3; winding is NOT consistent —
                                  //      turn off backface culling in your renderer (:65)
    pub vertices: Vec<Vertex>,    // :69
    pub texture_id: TextureId,    // :72
}
```

`WHITE_UV: Pos2 = pos2(0.0, 0.0)` (`epaint-0.36.2/src/lib.rs:88`) is the fully-white top-left pixel
of the default font atlas. `TextureId::default()` is `TextureId::Managed(0)`, the font texture
(`lib.rs:106–111`). So an untextured coloured mesh is just `Mesh::default()` + `untextured`
vertices.

### 10.1 `Mesh` methods

```rust
pub fn with_texture(texture_id: TextureId) -> Self;   // mesh.rs:77
pub fn clear(&mut self);                              // :85   keeps capacity
pub fn bytes_used(&self) -> usize;                    // :92
pub fn is_valid(&self) -> bool;                       // :99   all indices < vertices.len()
pub fn is_empty(&self) -> bool;                       // :109
pub fn triangles(&self) -> impl Iterator<Item = [u32; 3]> + '_;  // :114
pub fn calc_bounds(&self) -> Rect;                    // :121
pub fn append(&mut self, other: Self);                // :132   PANICS on texture mismatch
pub fn append_ref(&mut self, other: &Self);           // :147   PANICS on texture mismatch
pub fn colored_vertex(&mut self, pos: Pos2, color: Color32);  // :169  #[inline(always)]
                                                      //        debug_asserts texture_id == default
pub fn add_triangle(&mut self, a: u32, b: u32, c: u32);       // :179  #[inline(always)]
pub fn reserve_triangles(&mut self, additional_triangles: usize);  // :186
pub fn reserve_vertices(&mut self, additional: usize);            // :193
pub fn add_rect_with_uv(&mut self, rect: Rect, uv: Rect, color: Color32);  // :199
pub fn add_colored_rect(&mut self, rect: Rect, color: Color32);            // :231
pub fn split_to_u16(self) -> Vec<Mesh16>;             // :243   for 16-bit-index backends
pub fn translate(&mut self, delta: Vec2);             // :309
pub fn transform(&mut self, transform: TSTransform);  // :316
pub fn rotate(&mut self, rot: Rot2, origin: Pos2);    // :325
```

`Mesh16` (`mesh.rs:337`) has the same three fields with `Vec<u16>` indices and
`pub fn is_valid(&self) -> bool` (`mesh.rs:352`).

### 10.2 Two-colour gradient — use the built-in

```rust
use egui::{Color32, Direction, Rect, Shape};

painter.add(Shape::gradient_rect(
    bar_rect,
    Direction::LeftToRight,
    [Color32::from_rgb(0x2b, 0x7a, 0xff), Color32::from_rgb(0x00, 0xd0, 0xa0)],
));
```

`Shape::gradient_rect` (`epaint-0.36.2/src/shapes/shape.rs:306`) emits exactly 4 vertices and
2 triangles with `indices: vec![0, 1, 2, 2, 1, 3]` (`shape.rs:315`).

### 10.3 N-stop gradient — complete, copy-pasteable helper

`gradient_rect` only handles two stops. Here is a verified multi-stop version built on the same
primitives (`Mesh::colored_vertex` `mesh.rs:169`, `Mesh::add_triangle` `mesh.rs:179`,
`Shape::mesh` `shape.rs:361`, `emath::lerp` `emath-0.36.2/src/lib.rs:106`):

```rust
use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, pos2};

/// Paint an N-stop linear gradient across `rect` using one untextured triangle strip.
///
/// `stops` are `(t, color)` pairs with `t` in `0.0..=1.0`, and must be sorted ascending.
/// `horizontal == true` runs left→right; otherwise top→bottom.
///
/// The mesh is untextured, so it samples `WHITE_UV` of the font atlas — i.e. pure vertex colour.
pub fn paint_gradient(
    painter: &Painter,
    rect: Rect,
    stops: &[(f32, Color32)],
    horizontal: bool,
) {
    if stops.len() < 2 || !rect.is_positive() {
        return;
    }

    // `Mesh::default()` has `texture_id == TextureId::Managed(0)` (the font atlas),
    // which is exactly what `colored_vertex` requires.
    let mut mesh = Mesh::default();
    mesh.reserve_vertices(stops.len() * 2);
    mesh.reserve_triangles((stops.len() - 1) * 2);

    for (i, &(t, color)) in stops.iter().enumerate() {
        let t = t.clamp(0.0, 1.0);
        let (a, b): (Pos2, Pos2) = if horizontal {
            let x = egui::lerp(rect.left()..=rect.right(), t);
            (pos2(x, rect.top()), pos2(x, rect.bottom()))
        } else {
            let y = egui::lerp(rect.top()..=rect.bottom(), t);
            (pos2(rect.left(), y), pos2(rect.right(), y))
        };

        mesh.colored_vertex(a, color);
        mesh.colored_vertex(b, color);

        if i > 0 {
            let base = (i as u32 - 1) * 2;
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 2, base + 1, base + 3);
        }
    }

    painter.add(Shape::mesh(mesh));
}
```

Notes that matter:

- Colours are interpolated by the GPU in **premultiplied sRGB gamma space**, matching
  `Color32::lerp_to_gamma` (`ecolor-0.36.2/src/color32.rs:331`) rather than linear light. If you
  want perceptually-linear stops, pre-compute them with `lerp_to_gamma` and pass more stops.
- `Shape::mesh` `debug_assert!`s `mesh.is_valid()` (`shape.rs:363`) — a debug build will panic on a
  malformed index buffer, a release build will not.
- For a gradient *inside a rounded rect*, paint the gradient first and clip it:
  `let p = painter.with_clip_rect(rect);` then paint the rounded fill on top, or use
  `PathStroke::new_uv` for gradient outlines.

### 10.4 Gradient along a stroked path

```rust
use egui::epaint::PathStroke;

let stroke = PathStroke::new_uv(2.0, |bbox, pos| {
    let t = egui::remap_clamp(pos.x, bbox.left()..=bbox.right(), 0.0..=1.0);
    egui::Color32::RED.lerp_to_gamma(egui::Color32::BLUE, t)
});
painter.add(egui::Shape::line(points, stroke));
```

`new_uv` is `epaint-0.36.2/src/stroke.rs:152`; the bbox handed to the callback is expanded by
`TessellationOptions::feathering_size_in_pixels` (`stroke.rs:150`).

---

## 11. Layers

`egui-0.36.0/src/layers.rs`.

```rust
// layers.rs:10  painted back-to-front in declaration order
pub enum Order { Background, Middle, Foreground, Tooltip, Debug }
pub const TOP: Self = Self::Debug;                      // layers.rs:38
pub fn allow_interaction(&self) -> bool;                // :41  (currently true for all)
pub fn short_debug_format(&self) -> &'static str;       // :50

// layers.rs:65
pub struct LayerId { pub order: Order, pub id: Id }
pub fn new(order: Order, id: Id) -> Self;               // :71
pub fn debug() -> Self;                                 // :75   Order::Debug + Id::new("debug")
pub fn background() -> Self;                            // :82   Order::Background + Id::new("background")
pub fn short_debug_format(&self) -> String;             // :90

// layers.rs:109
pub struct ShapeIdx(pub usize);
```

`PaintList` (`layers.rs:113`) is the per-layer shape buffer:

```rust
pub fn is_empty(&self) -> bool;                                            // :117
pub fn next_idx(&self) -> ShapeIdx;                                        // :121
pub fn add(&mut self, clip_rect: Rect, shape: Shape) -> ShapeIdx;          // :127
pub fn extend<I: IntoIterator<Item = Shape>>(&mut self, clip_rect: Rect, shapes: I);  // :133
pub fn set(&mut self, idx: ShapeIdx, clip_rect: Rect, shape: Shape);       // :149  logs a warn if OOB
pub fn reset_shape(&mut self, idx: ShapeIdx);                              // :160
pub fn mutate_shape(&mut self, idx: ShapeIdx, f: impl FnOnce(&mut ClippedShape));  // :165
pub fn transform(&mut self, transform: TSTransform);                       // :170
pub fn transform_range(&mut self, start: ShapeIdx, end: ShapeIdx, transform: TSTransform);  // :178
pub fn all_entries(&self) -> impl ExactSizeIterator<Item = &ClippedShape>; // :186
```

`GraphicLayers` (`layers.rs:193`): `entry(LayerId) -> &mut PaintList` (`:197`),
`get(LayerId) -> Option<&PaintList>` (`:204`), `get_mut` (`:209`), `drain(..)` (`:213`).

Typical use — paint behind or in front of everything:

```rust
let bg = ui.ctx().layer_painter(egui::LayerId::background());
let top = ui.ctx().debug_painter();               // == layer_painter(LayerId::debug())
let own = ui.ctx().layer_painter(egui::LayerId::new(
    egui::Order::Foreground,
    egui::Id::new("my_overlay"),
));
```

`ClippedShape` (`epaint-0.36.2/src/lib.rs:117`) is `{ pub clip_rect: Rect, pub shape: Shape }` with
`pub fn transform(&mut self, transform: emath::TSTransform)` (`lib.rs:131`).

---

## 12. Textures

### 12.1 `TextureId` (`epaint-0.36.2/src/lib.rs:95`)

```rust
pub enum TextureId {
    Managed(u64),  // :99   allocated via TextureManager; Managed(0) is the font atlas
    User(u64),     // :103  your own; the renderer backend resolves it
}
// lib.rs:106 — Default == Managed(0)
```

### 12.2 `ColorImage` (`epaint-0.36.2/src/image.rs`)

```rust
// image.rs:48
pub struct ColorImage {
    pub size: [usize; 2],      // :50  [width, height] in texels
    pub source_size: Vec2,     // :53  original SVG point size, else the texel size — NEW FIELD
    pub pixels: Vec<Color32>,  // :56  row-major, top to bottom
}

pub fn new(size: [usize; 2], pixels: Vec<Color32>) -> Self;                   // :61
pub fn filled(size: [usize; 2], color: Color32) -> Self;                      // :75
pub fn from_rgba_unmultiplied(size: [usize; 2], rgba: &[u8]) -> Self;         // :113  ← what you want after decoding a PNG
pub fn from_rgba_premultiplied(size: [usize; 2], rgba: &[u8]) -> Self;        // :128
pub fn from_rgb(size: [usize; 2], rgb: &[u8]) -> Self;                        // :193
pub fn from_gray(size: [usize; 2], gray: &[u8]) -> Self;                      // :146
pub fn from_gray_iter(size: [usize; 2], gray_iter: impl Iterator<Item = u8>) -> Self;  // :163
pub fn example() -> Self;                                                     // :209  128x64 HSV test image
pub fn with_source_size(mut self, source_size: Vec2) -> Self;                 // :227  #[inline]
pub fn width(&self) -> usize;   // :233     pub fn height(&self) -> usize;     // :238
pub fn region(&self, region: &emath::Rect, pixels_per_point: Option<f32>) -> Self;   // :249
pub fn region_by_pixels(&self, [x, y]: [usize; 2], [w, h]: [usize; 2]) -> Self;      // :273
pub fn as_raw(&self) -> &[u8];          // :177   ← feature = "bytemuck" ONLY
pub fn as_raw_mut(&mut self) -> &mut [u8];  // :183 ← feature = "bytemuck" ONLY
```

`from_rgba_unmultiplied` / `from_rgb` / `from_gray` **`assert_eq!`** on the length
(`image.rs:114, 194, 147`) — they panic, not return `Result`.

`ImageData` (`image.rs:16`): single variant `Color(Arc<ColorImage>)`, with
`size()` (`:22`), `width()` (`:28`), `height()` (`:32`), `bytes_per_pixel()` (`:36`).

`ImageDelta` (`image.rs:456`): `full(image: impl Into<ImageData>, options: TextureOptions)` (`:474`),
`partial(pos: [usize; 2], image: impl Into<ImageData>, options: TextureOptions)` (`:483`),
`is_whole(&self) -> bool` (`:493`).

### 12.3 `TextureOptions` (`epaint-0.36.2/src/textures.rs:160`)

```rust
pub struct TextureOptions {
    pub magnification: TextureFilter,      // :162
    pub minification: TextureFilter,       // :165
    pub wrap_mode: TextureWrapMode,        // :168
    pub mipmap_mode: Option<TextureFilter>,// :178  egui_glow only, currently
}

pub const LINEAR: Self;                    // :183   == Default (:240)
pub const NEAREST: Self;                   // :191
pub const LINEAR_REPEAT: Self;             // :199
pub const LINEAR_MIRRORED_REPEAT: Self;    // :207
pub const NEAREST_REPEAT: Self;            // :215
pub const NEAREST_MIRRORED_REPEAT: Self;   // :223
pub const fn with_mipmap_mode(self, mipmap_mode: Option<TextureFilter>) -> Self;  // :230
```

```rust
// epaint-0.36.2/src/textures.rs:248 / :262
pub enum TextureFilter { Nearest, Linear }
pub enum TextureWrapMode { ClampToEdge /* #[default] */, Repeat, MirroredRepeat }
```

Use `TextureOptions::NEAREST` for pixel-art / procedurally generated textures you do not want
smeared, `LINEAR` (the default) for photos and icons.

Re-exported as `egui::TextureOptions`, `egui::TextureFilter`, `egui::TextureWrapMode`
(`egui-0.36.0/src/lib.rs:451`).

### 12.4 `TextureHandle` (`epaint-0.36.2/src/texture_handle.rs:20`)

`#[must_use]`. Cheap to clone (ref-counts, `:31`); **the texture is freed when the last handle
drops** (`:25`). Keep it in your app state, never in a local.

```rust
pub fn new(tex_mngr: Arc<RwLock<TextureManager>>, id: TextureId) -> Self;   // :59
pub fn id(&self) -> TextureId;                                             // :64 #[inline]
pub fn set(&mut self, image: impl Into<ImageData>, options: TextureOptions);// :70
pub fn set_partial(&mut self, pos: [usize; 2], image: impl Into<ImageData>, options: TextureOptions); // :78
pub fn size(&self) -> [usize; 2];       // :90    width x height
pub fn size_vec2(&self) -> Vec2;        // :98
pub fn byte_size(&self) -> usize;       // :104
pub fn aspect_ratio(&self) -> f32;      // :112
pub fn name(&self) -> String;           // :118
```

`impl From<&TextureHandle> for TextureId` (`:126`) and `From<&mut TextureHandle>` (`:133`), so
`painter.image(&tex, ..)` does **not** compile but `painter.image(tex.id(), ..)` does (the param is
a concrete `TextureId`, not `impl Into<TextureId>`).

### 12.5 Uploading — `Context::load_texture`

```rust
// egui-0.36.0/src/context.rs:2387
pub fn load_texture(
    &self,
    name: impl Into<String>,
    image: impl Into<ImageData>,
    options: TextureOptions,
) -> TextureHandle;

// egui-0.36.0/src/context.rs:2414
pub fn tex_manager(&self) -> Arc<RwLock<epaint::textures::TextureManager>>;
```

⚠ Call `load_texture` **once per image**, never per frame (`context.rs:2357`). The canonical
pattern from the doc comment (`context.rs:2364–2384`):

```rust
struct MyImage { texture: Option<egui::TextureHandle> }

impl MyImage {
    fn ui(&mut self, ui: &mut egui::Ui) {
        let texture: &egui::TextureHandle = self.texture.get_or_insert_with(|| {
            ui.ctx().load_texture("my-image", egui::ColorImage::example(), Default::default())
        });
        ui.image((texture.id(), texture.size_vec2()));
    }
}
```

### 12.6 Painting a texture into a `Rect`

Three equivalent routes, cheapest first:

```rust
const UV_FULL: egui::Rect =
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

// (a) raw painter — builds a 2-triangle textured mesh (Shape::image, shape.rs:373)
painter.image(tex.id(), rect, UV_FULL, egui::Color32::WHITE);

// (b) rounded / tinted — RectShape with a Brush (rect_shape.rs:156)
painter.add(
    egui::epaint::RectShape::filled(rect, 8, egui::Color32::WHITE)
        .with_texture(tex.id(), UV_FULL),
);

// (c) widget-level, handles loading/rotation/bg-fill (widgets/image.rs:368)
egui::Image::new(egui::include_image!("../assets/logo.svg"))
    .corner_radius(5)
    .tint(egui::Color32::LIGHT_BLUE)
    .paint_at(ui, rect);
```

For (b): the texture is **multiplied** by `RectShape::fill` (`brush.rs:9`), so pass `Color32::WHITE`
to avoid tinting, or any other colour to tint.

The low-level helper egui itself uses is public
(`egui-0.36.0/src/widgets/image.rs:839`):

```rust
pub fn paint_texture_at(
    painter: &Painter,
    rect: Rect,
    options: &ImageOptions,
    texture: &SizedTexture,
);
```

with `ImageOptions` (`image.rs:797`):

```rust
pub struct ImageOptions {
    pub uv: Rect,                        // :799  default (0,0)-(1,1)
    pub bg_fill: Color32,                // :802
    pub tint: Color32,                   // :805  default WHITE
    pub rotation: Option<(Rot2, Vec2)>,  // :816  origin in normalized UV; turns OFF corner rounding
    pub corner_radius: CornerRadius,     // :824  turns OFF rotation
}
```

Rotation and corner rounding are **mutually exclusive** — `paint_texture_at` `debug_assert!`s that
`corner_radius == CornerRadius::ZERO` when a rotation is set (`image.rs:857`).

### 12.7 Loading an SVG as a texture

SVG rasterization is not in `egui` — it lives in `egui_extras` behind the **`svg`** feature
(`egui_extras-0.36.0/Cargo.toml:79`: `svg = ["resvg"]`; add **`svg_text`**
(`Cargo.toml:80–84`) if the SVG contains `<text>`, which pulls in system fonts).
`SvgLoader` only accepts URIs ending in `.svg` (`egui_extras-0.36.0/src/loaders/svg_loader.rs:30`,
`fn is_supported(uri: &str) -> bool { uri.ends_with(".svg") }`).

```toml
egui_extras = { version = "0.36", features = ["svg", "file"] }
```

```rust
// once, at startup:
egui_extras::install_image_loaders(&cc.egui_ctx);
```

Then either the widget route (`Image::paint_at`, which requests exactly the pixel size of the
destination rect so the SVG stays crisp — `widgets/image.rs:371–385`), or the painter route:

```rust
// egui-0.36.0/src/context.rs:3903
pub fn try_load_texture(
    &self,
    uri: &str,
    texture_options: TextureOptions,
    size_hint: load::SizeHint,
) -> load::TextureLoadResult;   // = Result<TexturePoll, LoadError>
```

```rust
// egui-0.36.0/src/load.rs:148 — SizeHint decides the SVG rasterization resolution
pub enum SizeHint {
    Scale(OrderedFloat<f32>),                                       // :156
    Width(u32),                                                     // :159
    Height(u32),                                                    // :162
    Size { width: u32, height: u32, maintain_aspect_ratio: bool },  // :165
}
pub fn scale_by(self, factor: f32) -> Self;   // :177
// Default == Scale(1.0)  (load.rs:195)
```

```rust
// egui-0.36.0/src/load.rs:444
pub struct SizedTexture { pub id: TextureId, pub size: Vec2 }   // size = SVG point size
pub fn new(id: impl Into<TextureId>, size: impl Into<Vec2>) -> Self;  // :453
pub fn from_handle(handle: &TextureHandle) -> Self;                   // :461

// egui-0.36.0/src/load.rs:490
pub enum TexturePoll {
    Pending { size: Option<Vec2> },   // :492
    Ready { texture: SizedTexture },  // :500
}
pub fn size(&self) -> Option<Vec2>;         // :506
pub fn texture_id(&self) -> Option<TextureId>;  // :514
pub fn is_pending(&self) -> bool;  // :522    pub fn is_ready(&self) -> bool;  // :527
```

Related `Context` calls: `try_load_bytes(&self, uri: &str) -> load::BytesLoadResult`
(`context.rs:3820`), `try_load_image(&self, uri: &str, size_hint: load::SizeHint) ->
load::ImageLoadResult` (`context.rs:3858`), `include_bytes(&self, uri: impl Into<Cow<'static,
str>>, bytes: impl Into<Bytes>)` (`context.rs:3716`), `forget_image(&self, uri: &str)`
(`context.rs:3761`), `is_loader_installed(&self, id: &str) -> bool` (`context.rs:3722`),
`has_pending_images(&self) -> bool` (`context.rs:3931`).

`ImageSource` (`egui-0.36.0/src/widgets/image.rs:570`) is `Uri(Cow<'a, str>) | Texture(SizedTexture)
| Bytes { uri: Cow<'static, str>, bytes: Bytes }`, with
`pub fn load(self, ctx: &Context, texture_options: TextureOptions, size_hint: SizeHint) ->
TextureLoadResult` (`image.rs:631`) and `pub fn texture_size(&self) -> Option<Vec2>` (`image.rs:622`).

---

## 13. Text: galleys, layout, measurement

### 13.1 `FontId` / `FontFamily` (`epaint-0.36.2/src/text/fonts.rs`)

```rust
// fonts.rs:21
pub struct FontId { pub size: f32, pub family: FontFamily }
pub const fn new(size: f32, family: FontFamily) -> Self;   // :42
pub const fn proportional(size: f32) -> Self;              // :47
pub const fn monospace(size: f32) -> Self;                 // :52
// Default == FontId { size: 14.0, family: Proportional }   (:30–38)

// fonts.rs:74
pub enum FontFamily { Proportional, Monospace, Name(Arc<str>) }   // Name is Arc<str>, :94
// Default == Proportional (:78)
```

### 13.2 `FontsView` — measurement and layout (`epaint-0.36.2/src/text/fonts.rs`)

Reach it via `Painter::fonts_mut` / `Context::fonts_mut`. All layout methods take `&mut self` and
are memoized (`fonts.rs:888`, `:914`, `:929`, `:943`).

```rust
pub fn glyph_width(&mut self, font_id: &FontId, c: char) -> f32;   // :845  0.0 if font missing
pub fn has_glyph(&mut self, font_id: &FontId, c: char) -> bool;    // :852
pub fn has_glyphs(&mut self, font_id: &FontId, s: &str) -> bool;   // :857
pub fn row_height(&mut self, font_id: &FontId) -> f32;             // :865  rounded to GUI_ROUNDING
pub fn families(&self) -> Vec<FontFamily>;                         // :878
pub fn layout_job(&mut self, job: LayoutJob) -> Arc<Galley>;       // :890
pub fn layout(&mut self, text: String, font_id: FontId, color: Color32, wrap_width: f32) -> Arc<Galley>;  // :916
pub fn layout_no_wrap(&mut self, text: String, font_id: FontId, color: Color32) -> Arc<Galley>;           // :931
pub fn layout_delayed_color(&mut self, text: String, font_id: FontId, wrap_width: f32) -> Arc<Galley>;    // :945
pub fn options(&self) -> &TextOptions;          // :821
pub fn definitions(&self) -> &FontDefinitions;  // :826
pub fn image(&self) -> crate::ColorImage;       // :832
pub fn font_image_size(&self) -> [usize; 2];    // :838
pub fn num_galleys_in_cache(&self) -> usize;    // :900
pub fn font_atlas_fill_ratio(&self) -> f32;     // :908
```

`layout_delayed_color` lays out with `Color32::PLACEHOLDER` (`fonts.rs:951`) so you can choose the
colour at paint time via `Painter::galley(pos, galley, fallback_color)`.

`Fonts` (the owner, `fonts.rs:707`):
`new(options: TextOptions, definitions: FontDefinitions)` (`:715`),
`begin_pass(&mut self, options: TextOptions)` (`:728`),
`font_image_delta(&mut self) -> Option<crate::ImageDelta>` (`:746`),
`texture_atlas(&self) -> &TextureAtlas` (`:762`),
`with_pixels_per_point(&mut self, pixels_per_point: f32) -> FontsView<'_>` (`:801`).

### 13.3 `Galley` (`epaint-0.36.2/src/text/text_layout_types.rs:736`)

```rust
pub struct Galley {
    pub job: Arc<LayoutJob>,      // :739
    pub rows: Vec<PlacedRow>,     // :748
    pub elided: bool,             // :751  true if TextWrapping::max_rows truncated it
    pub rect: Rect,               // :761  rect.top() is always 0.0
    pub mesh_bounds: Rect,        // :765  tight box around the glyph meshes — use for culling
    pub num_vertices: usize,      // :768
    pub num_indices: usize,       // :771
    pub pixels_per_point: f32,    // :777  recreate the galley if this changes
}

pub fn is_empty(&self) -> bool;      // :1006 #[inline]
pub fn text(&self) -> &str;          // :1012 #[inline]
pub fn size(&self) -> Vec2;          // :1017 #[inline]   == self.rect.size()
pub fn intrinsic_size(&self) -> Vec2;// :1026 #[inline]   un-wrapped / un-justified size
pub fn pos_from_layout_cursor(&self, layout_cursor: &LayoutCursor) -> Rect;  // :1160
```

**Measuring text** is therefore:

```rust
let galley = painter.layout_no_wrap(
    "Some text".to_owned(),
    egui::FontId::proportional(16.0),
    egui::Color32::PLACEHOLDER,      // colour chosen at paint time
);
let size: egui::Vec2 = galley.size();
let baseline_rect = egui::Align2::CENTER_CENTER.anchor_size(anchor_pos, size);
painter.galley(baseline_rect.min, galley, egui::Color32::WHITE);
```

That is exactly what `Painter::text` does internally (`egui-0.36.0/src/painter.rs:477–480`).

### 13.4 `LayoutJob` (`epaint-0.36.2/src/text/text_layout_types.rs:49`)

```rust
pub struct LayoutJob {
    pub text: String,                    // :51
    pub sections: Vec<LayoutSection>,    // :60  must tile `text` with no gaps/overlaps
    pub wrap: TextWrapping,              // :63
    pub first_row_min_height: f32,       // :70
    pub break_on_newline: bool,          // :79  default true
    pub halign: Align,                   // :82
    pub justify: bool,                   // :85
    pub round_output_to_gui: bool,       // :88
    pub keep_trailing_whitespace: bool,  // :96
}

pub fn clear(&mut self);                                                              // :119
pub fn simple(text: String, font_id: FontId, color: Color32, wrap_width: f32) -> Self; // :126
pub fn simple_format(text: String, format: TextFormat) -> Self;                        // :145
pub fn simple_singleline(text: String, font_id: FontId, color: Color32) -> Self;        // :160
pub fn single_section(text: String, format: TextFormat) -> Self;                        // :175
pub fn is_empty(&self) -> bool;                                                         // :190
pub fn append(&mut self, text: &str, leading_space: f32, format: TextFormat);           // :200
```

`append` merges into the previous section when the `TextFormat` matches and `leading_space == 0.0`,
so shaping/kerning works across the join (`text_layout_types.rs:196–199`).

### 13.5 `TextFormat` (`epaint-0.36.2/src/text/text_layout_types.rs:478`)

```rust
pub struct TextFormat {
    pub font_id: FontId,                // :479
    pub extra_letter_spacing: f32,      // :484  default 0.0
    pub line_height: Option<f32>,       // :493  None = from the font
    pub color: Color32,                 // :496  DEFAULT IS Color32::GRAY (:532), not WHITE
    pub background: Color32,            // :498
    pub expand_bg: f32,                 // :503  default 1.0
    pub coords: VariationCoords,        // :505  variable-font axes
    pub italics: bool,                  // :507
    pub underline: Stroke,              // :509
    pub strikethrough: Stroke,          // :511
    pub valign: Align,                  // :522  default Align::BOTTOM (:539)
}
```

`egui` re-exports `TextFormat` and `Galley` at the root (`egui-0.36.0/src/lib.rs:489`);
`LayoutJob`, `LayoutSection`, `TextWrapping`, `Fonts` live under `egui::text::…`
(`egui-0.36.0/src/lib.rs:454–460`). `FontsView` is **not** re-exported at either — use
`egui::epaint::text::FontsView`.

---

## 14. `epaint::tessellator` — line quality and feathering

`epaint-0.36.2/src/tessellator.rs`. `TessellationOptions` is at `tessellator.rs:658`
(`#[derive(Clone, Copy, Debug, PartialEq)]`, `#[serde(default)]` under the `serde` feature).

| field | line | default (`:727–746`) | what it does |
|---|---|---|---|
| `feathering: bool` | `:669` | `true` | anti-aliasing by fading each edge into transparency. **Does not affect text.** |
| `feathering_size_in_pixels: f32` | `:675` | `1.0` | physical-pixel width of that fade. `> 1.0` looks blurry. |
| `coarse_tessellation_culling: bool` | `:679` | `true` | drop primitives fully outside the clip rect before tessellating. |
| `prerasterized_discs: bool` | `:683` | `true` | small filled circles come from pre-rasterized discs in the font atlas. |
| `round_text_to_pixels: bool` | `:687` | `true` | snap text to the physical pixel grid (sharper on most platforms). |
| `round_line_segments_to_pixels: bool` | `:692` | `true` | snap right-angled line segments to the pixel grid. |
| `round_rects_to_pixels: bool` | `:700` | `true` | snap rects; makes strokes crisp and filled rects tile without feathering seams. Overridable per-shape via `RectShape::round_to_pixels`. |
| `debug_paint_clip_rects: bool` | `:703` | `false` | draw the clip rects. |
| `debug_paint_text_rects: bool` | `:706` | `false` | draw the text boxes. |
| `debug_ignore_clip_rects: bool` | `:709` | `false` | disable clipping entirely. |
| `bezier_tolerance: f32` | `:712` | `0.1` | max deviation when flattening Béziers. |
| `epsilon: f32` | `:715` | `1.0e-5` | float-compare epsilon. |
| `parallel_tessellation: bool` | `:718` | `true` | only meaningful with the `rayon` feature (`epaint::HAS_RAYON`, `lib.rs:161`). |
| `validate_meshes: bool` | `:724` | `false` | `true` silently drops invalid meshes; `false` panics. |

Reading and writing it through `egui`:

```rust
// egui-0.36.0/src/context.rs:1140
pub fn tessellation_options<R>(&self, reader: impl FnOnce(&TessellationOptions) -> R) -> R;
// egui-0.36.0/src/context.rs:1146
pub fn tessellation_options_mut<R>(&self, writer: impl FnOnce(&mut TessellationOptions) -> R) -> R;
```

```rust
// Crisper hairlines, no AA blur (e.g. for a spectrum-analyser grid):
ctx.tessellation_options_mut(|o| {
    o.feathering = true;
    o.feathering_size_in_pixels = 1.0;     // never raise this to "smooth" things
    o.round_line_segments_to_pixels = true;
    o.round_rects_to_pixels = true;
});
```

It also lives on `egui::Options` as `pub tessellation_options: epaint::TessellationOptions`
(`egui-0.36.0/src/memory/mod.rs:266`).

Driving the tessellator yourself:

```rust
// epaint-0.36.2/src/tessellator.rs:1329
pub fn new(
    pixels_per_point: f32,
    options: TessellationOptions,
    font_tex_size: [usize; 2],
    prepared_discs: Vec<PreparedDisc>,   // may safely be vec![]
) -> Self;

pub fn set_clip_rect(&mut self, clip_rect: Rect);                                     // :1354
pub fn tessellate_clipped_shape(&mut self, clipped_shape: ClippedShape, out_primitives: &mut Vec<ClippedPrimitive>);  // :1359
pub fn tessellate_shape(&mut self, shape: Shape, out: &mut Mesh);                     // :1422
pub fn tessellate_shapes(&mut self, mut shapes: Vec<ClippedShape>) -> Vec<ClippedPrimitive>;  // :2220
```

Per-primitive entry points, if you want to append into a `Mesh` you own:

```rust
pub fn tessellate_circle(&mut self, shape: CircleShape, out: &mut Mesh);        // :1486
pub fn tessellate_ellipse(&mut self, shape: EllipseShape, out: &mut Mesh);      // :1542
pub fn tessellate_mesh(&self, mesh: &Mesh, out: &mut Mesh);                     // :1618  (&self!)
pub fn tessellate_line_segment(/* points, stroke, out */);                      // :1637
pub fn tessellate_path(&mut self, path_shape: &PathShape, out: &mut Mesh);      // :1712
pub fn tessellate_rect(&mut self, rect_shape: &RectShape, out: &mut Mesh);      // :1757
pub fn tessellate_text(&mut self, text_shape: &TextShape, out: &mut Mesh);      // :1993
pub fn tessellate_quadratic_bezier(/* … */);                                    // :2118
pub fn tessellate_cubic_bezier(&mut self, cubic_shape: &CubicBezierShape, out: &mut Mesh);  // :2147
```

### 14.1 Building custom rounded outlines

The scratch-pad path type and its constructors are public — handy when you need a rounded outline
that `RectShape` cannot express (e.g. a tab with two rounded corners feeding into a custom `Mesh`).
The import path is `use egui::epaint::tessellator::{Path, path};` (verified to compile):

```rust
// epaint-0.36.2/src/tessellator.rs:326
pub struct Path(/* Vec<PathPoint> */);
pub fn clear(&mut self);                                        // :330
pub fn reserve(&mut self, additional: usize);                   // :335
pub fn add_point(&mut self, pos: Pos2, normal: Vec2);           // :340
pub fn add_circle(&mut self, center: Pos2, radius: f32);        // :344
pub fn add_line_segment(&mut self, points: [Pos2; 2]);          // :378
pub fn add_open_points(&mut self, points: &[Pos2]);             // :385
pub fn add_line_loop(&mut self, points: &[Pos2]);               // :431
pub fn fill_and_stroke(/* … */);                                // :484
pub fn stroke_open(&mut self, feathering: f32, stroke: &PathStroke, out: &mut Mesh);    // :495
pub fn stroke_closed(&mut self, feathering: f32, stroke: &PathStroke, out: &mut Mesh);  // :500
pub fn fill(&mut self, feathering: f32, color: Color32, out: &mut Mesh);                // :518
pub fn fill_with_uv(/* … */);                                   // :525

// epaint-0.36.2/src/tessellator.rs:537 — `pub mod path`, free helpers over Vec<Pos2>
pub fn rounded_rectangle(path: &mut Vec<Pos2>, rect: Rect, cr: CornerRadiusF32);   // :543  OVERWRITES `path`
pub fn add_circle_quadrant(path: &mut Vec<Pos2>, center: Pos2, radius: f32, quadrant: f32);  // :606
```

`add_circle_quadrant`'s `quadrant` is in units of `TAU / 4` measured clockwise from the X axis:
`0 = right bottom`, `1 = left bottom`, `2 = left top`, `3 = right top` (`tessellator.rs:590–593`).
Note `rounded_rectangle` takes a **`CornerRadiusF32`**, not a `CornerRadius`, and that `Path`'s
inner `Vec<PathPoint>` is private — build one with `Path::default()`, not the tuple constructor.

Output types: `ClippedPrimitive { clip_rect: Rect, primitive: Primitive }`
(`epaint-0.36.2/src/lib.rs:142`) and `Primitive { Mesh(Mesh), Callback(PaintCallback) }`
(`lib.rs:153`).

Feathering is converted to points once, at construction:
`feathering = options.feathering_size_in_pixels / pixels_per_point` (`tessellator.rs:1335–1340`).

---

## 15. Bonus: `Shadow` and `Margin` (for panel chrome)

```rust
// epaint-0.36.2/src/shadow.rs:10
pub struct Shadow { pub offset: [i8; 2], pub blur: u8, pub spread: u8, pub color: Color32 }
pub const NONE: Self;                                                        // :40
pub fn as_shape(&self, rect: Rect, corner_radius: impl Into<CornerRadius>) -> RectShape;  // :48
pub fn margin(&self) -> MarginF32;                                           // :68

// epaint-0.36.2/src/margin.rs:15
pub struct Margin { pub left: i8, pub right: i8, pub top: i8, pub bottom: i8 }
pub const ZERO: Self;                                // :23
pub const fn same(margin: i8) -> Self;               // :33
pub const fn symmetric(x: i8, y: i8) -> Self;        // :44
pub const fn leftf/rightf/topf/bottomf(self) -> f32; // :55 :61 :67 :73
pub fn sum(self) -> Vec2;                            // :79
pub const fn left_top(self) -> Vec2;                 // :84
pub const fn right_bottom(self) -> Vec2;             // :89
pub const fn is_same(self) -> bool;                  // :96
```

`Shadow::as_shape` returns a `RectShape::filled(...).with_blur_width(blur as f32)` whose rect is
translated by `offset` and expanded by `spread` (`shadow.rs:59–64`) — so a shadow is just a blurred
rect, and you can paint one by hand with `RectShape::with_blur_width`.

---

## 16. Complete compiling examples

### 16.0 Self-contained `eframe` app — rounded panel, VERTICAL gradient bar, texture, centred text

Zero external assets, zero image loaders, nothing to install. **This file was type-checked
verbatim** with `cargo check` against `egui 0.36.0` / `eframe 0.36.0` from this registry
(`default-features = false`, features `default_fonts, glow, wayland, x11`) — zero errors, zero
warnings.

```toml
[dependencies]
egui = "=0.36.0"
eframe = { version = "=0.36.0", default-features = false, features = ["default_fonts", "glow", "wayland", "x11"] }
```

Note the 0.36 `eframe::App` shape: the required method is
`fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)` (`eframe-0.36.0/src/epi.rs:182`) — it
receives a `&mut Ui`, **not** a `&Context`, and there is no `run_simple_native`
(`run_native` is `eframe-0.36.0/src/lib.rs:288`). `CentralPanel::show` also takes a `&mut Ui`
(`egui-0.36.0/src/containers/panel.rs:1212`).

```rust
use eframe::egui;
use egui::epaint::RectShape;
use egui::{
    Align2, Color32, ColorImage, CornerRadius, FontId, Mesh, Rect, Sense, Shape, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Vec2, pos2, vec2,
};

fn main() -> eframe::Result {
    eframe::run_native(
        "painting demo",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(DemoApp::default()))),
    )
}

#[derive(Default)]
struct DemoApp {
    // A TextureHandle frees its texture when the last clone drops, so it MUST live in app
    // state — never in a local (epaint-0.36.2/src/texture_handle.rs:25).
    tex: Option<TextureHandle>,
}

impl eframe::App for DemoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::no_frame().show(ui, |ui| {
            // Upload once, not per frame (egui-0.36.0/src/context.rs:2387).
            let tex = self.tex.get_or_insert_with(|| {
                ui.ctx()
                    .load_texture("checker", checker(64), TextureOptions::NEAREST)
            });

            // allocate_painter clips to the allocated rect for us (ui.rs:1370).
            let (_r, painter) = ui.allocate_painter(ui.available_size(), Sense::hover());
            let area = painter.clip_rect();

            // ---- 1. rounded panel -------------------------------------------------------
            // StrokeKind::Inside keeps the outer edge exactly on `panel`, so panels tile.
            let panel = area.shrink(16.0);
            painter.add(RectShape::new(
                panel,
                CornerRadius::same(14),
                Color32::from_rgb(0x1b, 0x1d, 0x24),
                Stroke::new(1.0, Color32::from_gray(64)),
                StrokeKind::Inside,
            ));

            // ---- 2. VERTICAL gradient bar ------------------------------------------------
            let bar = Rect::from_min_size(panel.left_top() + vec2(24.0, 24.0), vec2(44.0, 180.0));
            painter.add(Shape::mesh(vertical_gradient(
                bar,
                &[
                    (0.0, Color32::from_rgb(0x27, 0xc4, 0x7d)),
                    (0.6, Color32::from_rgb(0xff, 0xd1, 0x3b)),
                    (1.0, Color32::from_rgb(0xff, 0x45, 0x45)),
                ],
                48,
            )));
            painter.rect_stroke(
                bar,
                3,
                Stroke::new(1.0, Color32::from_gray(90)),
                StrokeKind::Outside,
            );

            // ---- 3. texture into a Rect (rounded, via RectShape + Brush) -----------------
            let img = Rect::from_min_size(bar.right_top() + vec2(28.0, 0.0), Vec2::splat(180.0));
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.add(
                RectShape::filled(img, CornerRadius::same(10), Color32::WHITE)
                    .with_texture(tex.id(), uv),
            );

            // ---- 4. centred text ---------------------------------------------------------
            painter.text(
                panel.center_bottom() - vec2(0.0, 32.0),
                Align2::CENTER_CENTER,
                "FxSound",
                FontId::proportional(24.0),
                Color32::from_gray(235),
            );
        });
    }
}

/// A procedurally generated checkerboard, so the example needs no asset files.
fn checker(size: usize) -> ColorImage {
    let mut img = ColorImage::filled([size, size], Color32::TRANSPARENT);
    for y in 0..size {
        for x in 0..size {
            img.pixels[y * size + x] = if (x / 8 + y / 8) % 2 == 0 {
                Color32::from_rgb(0x30, 0x30, 0x38)
            } else {
                Color32::from_rgb(0x50, 0x50, 0x60)
            };
        }
    }
    img
}

/// Vertical N-stop gradient as one untextured triangle strip subdivided into `steps` bands.
///
/// Unlike the 4-vertex `Shape::gradient_rect`, this honours more than two stops. Colours are
/// pre-sampled per band with `Color32::lerp_to_gamma` (ecolor color32.rs:331), then the GPU
/// interpolates between bands — also in premultiplied sRGB gamma space.
fn vertical_gradient(rect: Rect, stops: &[(f32, Color32)], steps: usize) -> Mesh {
    assert!(!stops.is_empty());
    let steps = steps.max(1);

    let sample = |t: f32| -> Color32 {
        let t = t.clamp(0.0, 1.0);
        let (mut lo, mut hi) = (stops[0], stops[stops.len() - 1]);
        for w in stops.windows(2) {
            if w[0].0 <= t && t <= w[1].0 {
                lo = w[0];
                hi = w[1];
                break;
            }
        }
        let span = hi.0 - lo.0;
        let local = if span.abs() < f32::EPSILON { 0.0 } else { (t - lo.0) / span };
        lo.1.lerp_to_gamma(hi.1, local)
    };

    // Mesh::default() carries TextureId::Managed(0) (the font atlas) and `colored_vertex`
    // uses WHITE_UV, i.e. pure vertex colour (epaint mesh.rs:169, lib.rs:88).
    let mut mesh = Mesh::default();
    mesh.reserve_vertices(2 * (steps + 1));
    mesh.reserve_triangles(2 * steps);

    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let y = egui::lerp(rect.top()..=rect.bottom(), t);
        let c = sample(t);
        mesh.colored_vertex(pos2(rect.left(), y), c);
        mesh.colored_vertex(pos2(rect.right(), y), c);
    }
    for i in 0..steps as u32 {
        mesh.add_triangle(2 * i, 2 * i + 1, 2 * i + 2);
        mesh.add_triangle(2 * i + 2, 2 * i + 1, 2 * i + 3);
    }
    mesh
}
```

---

### 16.1 Richer card — SVG texture, shadow, gradient-stroked path

Rounded panel + gradient bar + SVG texture + centred text, all through one `Painter`. Unlike
§16.0 this one needs `egui_extras` and its async texture polling.

Cargo requirements:

```toml
[dependencies]
egui = "0.36"
egui_extras = { version = "0.36", features = ["svg", "file"] }
```

and, once at startup, `egui_extras::install_image_loaders(&ctx);`

```rust
use egui::{
    epaint::{PathStroke, RectShape},
    load::{SizeHint, TexturePoll},
    Align2, Color32, CornerRadius, Direction, FontId, Mesh, Painter, Pos2, Rect, Shape, Stroke,
    StrokeKind, TextureOptions, Ui, Vec2, pos2, vec2,
};

/// Full-texture UV range. `Rect::from_min_max` is `const` (emath rect.rs:73),
/// as is `pos2` (emath pos2.rs:29).
const UV_FULL: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// Paint an N-stop linear gradient across `rect` with one untextured triangle strip.
///
/// `stops` are `(t, color)` with `t` in `0.0..=1.0`, sorted ascending.
pub fn paint_gradient(painter: &Painter, rect: Rect, stops: &[(f32, Color32)], horizontal: bool) {
    if stops.len() < 2 || !rect.is_positive() {
        return;
    }

    // Mesh::default() carries TextureId::Managed(0) (the font atlas), which is what
    // `colored_vertex` debug-asserts on, and WHITE_UV gives pure vertex colour.
    let mut mesh = Mesh::default();
    mesh.reserve_vertices(stops.len() * 2);
    mesh.reserve_triangles((stops.len() - 1) * 2);

    for (i, &(t, color)) in stops.iter().enumerate() {
        let t = t.clamp(0.0, 1.0);
        let (a, b): (Pos2, Pos2) = if horizontal {
            let x = egui::lerp(rect.left()..=rect.right(), t);
            (pos2(x, rect.top()), pos2(x, rect.bottom()))
        } else {
            let y = egui::lerp(rect.top()..=rect.bottom(), t);
            (pos2(rect.left(), y), pos2(rect.right(), y))
        };

        mesh.colored_vertex(a, color);
        mesh.colored_vertex(b, color);

        if i > 0 {
            let base = (i as u32 - 1) * 2;
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 2, base + 1, base + 3);
        }
    }

    painter.add(Shape::mesh(mesh));
}

/// Paint a complete card: rounded panel, multi-stop gradient bar, SVG icon, centred label.
///
/// `svg_uri` must end in `.svg` for `egui_extras::SvgLoader` to accept it
/// (egui_extras-0.36.0/src/loaders/svg_loader.rs:30), e.g. `"file://assets/logo.svg"`
/// or a `"bytes://logo.svg"` uri you registered with `ctx.include_bytes`.
pub fn paint_card(ui: &Ui, rect: Rect, svg_uri: &str, label: &str) {
    // Clip everything to the card. `painter_at` == `painter().with_clip_rect(rect)`.
    let painter = ui.painter_at(rect);
    let ppp = painter.pixels_per_point();

    // ---- 1. Soft shadow: a blurred, offset RectShape ------------------------------------
    painter.add(
        RectShape::filled(
            rect.translate(vec2(0.0, 2.0)),
            CornerRadius::same(14),
            Color32::from_black_alpha(90),
        )
        .with_blur_width(10.0),
    );

    // ---- 2. Rounded panel: fill + 1px INSIDE stroke ------------------------------------
    // StrokeKind::Inside keeps the outer edge exactly on `rect`, so cards tile perfectly.
    painter.add(RectShape::new(
        rect,
        CornerRadius::same(14),
        Color32::from_rgb(0x1E, 0x1E, 0x24),
        Stroke::new(1.0, Color32::from_rgb(0x3A, 0x3A, 0x46)),
        StrokeKind::Inside,
    ));

    // ---- 3. Gradient bar --------------------------------------------------------------
    let bar = Rect::from_min_size(
        pos2(rect.left() + 16.0, rect.bottom() - 28.0),
        vec2(rect.width() - 32.0, 10.0),
    );

    // Two stops: the built-in does it in 4 vertices.
    painter.add(Shape::gradient_rect(
        bar,
        Direction::LeftToRight,
        [
            Color32::from_rgb(0x2B, 0x7A, 0xFF),
            Color32::from_rgb(0x00, 0xD0, 0xA0),
        ],
    ));

    // Three-plus stops: use the helper above. (Painted just below the two-stop bar.)
    let bar2 = bar.translate(vec2(0.0, -14.0));
    paint_gradient(
        &painter,
        bar2,
        &[
            (0.00, Color32::from_rgb(0x18, 0x50, 0xB0)),
            (0.45, Color32::from_rgb(0x2B, 0x7A, 0xFF)),
            (0.75, Color32::from_rgb(0x00, 0xD0, 0xA0)),
            (1.00, Color32::from_rgb(0xF0, 0xE0, 0x60)),
        ],
        true,
    );

    // A hairline under the bars, snapped to a pixel centre so 1px stays 1px.
    let y = painter.round_to_pixel_center(bar.bottom() + 6.0);
    painter.hline(bar.x_range(), y, Stroke::new(1.0, Color32::from_gray(0x40)));

    // A gradient-stroked path, via PathStroke::new_uv.
    painter.add(Shape::line(
        vec![
            pos2(bar.left(), y + 8.0),
            pos2(bar.center().x, y + 2.0),
            pos2(bar.right(), y + 8.0),
        ],
        PathStroke::new_uv(2.0, |bbox, p| {
            let t = egui::remap_clamp(p.x, bbox.left()..=bbox.right(), 0.0..=1.0);
            Color32::from_rgb(0x2B, 0x7A, 0xFF).lerp_to_gamma(Color32::from_rgb(0xF0, 0x60, 0x60), t)
        }),
    ));

    // ---- 4. SVG texture into a Rect ---------------------------------------------------
    let icon_rect = Rect::from_min_size(rect.left_top() + vec2(16.0, 14.0), Vec2::splat(28.0));

    // Ask the loader for exactly the physical pixels we will cover, so the SVG stays crisp.
    // Same idea as `Image::paint_at` (egui widgets/image.rs:375), which computes
    // `(pixels_per_point * rect.size()).round()` — note it passes
    // `maintain_aspect_ratio: false` ("just get exactly what we asked for", image.rs:383);
    // `true` here letterboxes instead of stretching a non-square SVG.
    let pixel_size = (ppp * icon_rect.size()).round();
    let poll = painter.ctx().try_load_texture(
        svg_uri,
        TextureOptions::LINEAR,
        SizeHint::Size {
            width: pixel_size.x as u32,
            height: pixel_size.y as u32,
            maintain_aspect_ratio: true,
        },
    );

    match poll {
        Ok(TexturePoll::Ready { texture }) => {
            // (a) plain textured quad:
            painter.image(texture.id, icon_rect, UV_FULL, Color32::WHITE);

            // (b) if you wanted rounded corners instead, swap (a) for:
            // painter.add(
            //     RectShape::filled(icon_rect, 6, Color32::WHITE)
            //         .with_texture(texture.id, UV_FULL),
            // );
        }
        Ok(TexturePoll::Pending { .. }) => {
            // Placeholder while resvg rasterizes; repaint so we pick it up next frame.
            painter.rect_filled(icon_rect, CornerRadius::same(6), Color32::from_gray(0x2A));
            painter.ctx().request_repaint();
        }
        Err(err) => {
            painter.error(icon_rect.left_top(), err);
        }
    }

    // ---- 5. Centred text --------------------------------------------------------------
    let font = FontId::proportional(18.0);

    // Measure first (Color32::PLACEHOLDER = "decide the colour at paint time").
    let galley = painter.layout_no_wrap(label.to_owned(), font.clone(), Color32::PLACEHOLDER);
    let text_size: Vec2 = galley.size();

    // Centre in the upper part of the card, leaving room for the bars.
    let text_area = Rect::from_min_max(
        rect.left_top() + vec2(52.0, 0.0),
        pos2(rect.right() - 16.0, bar2.top() - 8.0),
    );
    let placed = Align2::CENTER_CENTER.anchor_size(text_area.center(), text_size);

    painter.galley(placed.min, galley, Color32::from_gray(0xE6));

    // The one-liner equivalent, when you do not need the measurement:
    // painter.text(text_area.center(), Align2::CENTER_CENTER, label, font, Color32::from_gray(0xE6));
}

/// Example call site.
pub fn card_demo(ui: &mut Ui) {
    let (_response, painter) = ui.allocate_painter(vec2(320.0, 140.0), egui::Sense::hover());
    paint_card(ui, painter.clip_rect(), "file://assets/logo.svg", "FxSound");
}
```

Import-path reminders for the above (`egui-0.36.0/src/lib.rs:447–452`, `:462–497`):

- `egui::{Align2, Color32, CornerRadius, Direction, FontId, Galley, Mesh, Painter, Pos2, Rect,
  Rangef, Shape, Stroke, StrokeKind, TextureHandle, TextureId, TextureOptions, Ui, Vec2, pos2,
  vec2, lerp, remap_clamp}` — all at the root.
- `egui::epaint::{RectShape, CircleShape, EllipseShape, PathShape, TextShape, PathStroke, ColorMode,
  Brush, Vertex, Mesh16, CornerRadiusF32, ClippedShape, TessellationOptions, Tessellator}` — **not**
  at the root.
- `egui::emath::GuiRounding` — **not** at the root.
- `egui::load::{SizeHint, SizedTexture, TexturePoll, TextureLoadResult}` — `SizeHint` is also
  re-exported at the root (`lib.rs:482`); the others are not.
- `egui::text::{LayoutJob, LayoutSection, TextWrapping, Fonts}`; `egui::epaint::text::FontsView`.
