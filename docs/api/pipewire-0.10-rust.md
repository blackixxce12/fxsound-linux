# `pipewire` 0.10.1 / `libspa` 0.10.1 — verified Rust API cheatsheet

> **Every signature below was read out of the vendored crate source and is quoted verbatim.**
> Citations are `<file>:<line>`, relative to
> `/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.
>
> Sources read in full: `pipewire-0.10.1/src/**`, `pipewire-0.10.1/examples/**`,
> `libspa-0.10.1/src/**`.
>
> **Do not trust memory of 0.8.x.** The 0.10 release renamed essentially every owning type.
> `docs/spec/12-audio-io.md:991` still says "use `0.8.x`" — that is stale; the workspace
> `Cargo.toml` pins `pipewire = "0.10.1"` / `libspa = "0.10.1"`.

---

## 0. The 0.8 → 0.10 rename table (read this first)

This is where the compile errors come from. **Left column does not exist in 0.10.1.**

| What you probably remember (0.8) | What 0.10.1 actually has | Cite |
| --- | --- | --- |
| `pipewire::MainLoop::new()` | `pipewire::main_loop::MainLoopBox::new(props)` / `MainLoopRc::new(props)` | `pipewire-0.10.1/src/main_loop/box_.rs:22`, `rc.rs:35` |
| `pipewire::Context::new(&mainloop)` | `pipewire::context::ContextBox::new(&loop_, props)` / `ContextRc::new(&loop_rc, props)` | `context/box_.rs:22`, `context/rc.rs:48` |
| `pipewire::Core` (owned) | `core::Core` is a `#[repr(transparent)]` **borrow**; owning types are `CoreBox<'c>` / `CoreRc` | `core/mod.rs:43`, `core/box_.rs:16`, `core/rc.rs:32` |
| `pipewire::Stream::new(&core, name, props)` | `stream::StreamBox::new(&core, name, props)` or `stream::StreamRc::new(core, name, props)` — **there is no `Stream::new`** | `stream/box_.rs:27`, `stream/rc.rs:39` |
| **`StreamRef`** | **Gone.** The non-owning wrapper is plain `stream::Stream`. Callbacks receive `&Stream`. | `stream/mod.rs:151`, `stream/mod.rs:405-406` |
| `pipewire::Properties` (owned) | `properties::Properties` is a borrow; the owner is `properties::PropertiesBox` | `properties/mod.rs:22`, `properties/box_.rs:25` |
| `pipewire::properties!` | `pipewire::properties::properties!` (module-scoped re-export) | `properties/box_.rs:184` |
| `context.connect(None)` returning an `Rc`-ish core | `Context::connect` → `CoreBox<'_>`; `ContextRc::connect_rc` → `CoreRc` | `context/mod.rs:60`, `context/rc.rs:80` |
| `core.get_registry()` on an `Rc` core | `Core::get_registry` → `RegistryBox<'_>`; `CoreRc::get_registry_rc` → `RegistryRc` | `core/mod.rs:63`, `core/rc.rs:56` |
| `Registry` / `Listener` names | unchanged, but `registry::Registry` is also a borrow, owners are `RegistryBox<'c>` / `RegistryRc` | `registry/mod.rs:54`, `registry/box_.rs:16`, `registry/rc.rs:28` |
| `pipewire::spa::pod::Rounding`-era pod builders | pods are built with `libspa::pod::{object!, property!}` **or** by `PodSerializer::serialize` of a `Value::Object` | `libspa-0.10.1/src/pod/mod.rs:1438,1514`, `pod/serialize.rs:268` |
| `pw::MainLoop::new()` auto-`pw_init` | `MainLoopBox::new` **does** call `crate::init()` internally (`box_.rs:23`), but examples still call `pw::init()` first | `lib.rs:180` |
| `pipewire::Filter` / `filter.rs` | **Does not exist in 0.10.1.** See §12. | — |

Other footguns that are *not* renames:

* **Nothing in `pipewire` or `libspa` is `Send` or `Sync`.** A repo-wide grep for
  `unsafe impl Send`/`Sync` in `pipewire-0.10.1/src/` returns **zero hits**; in `libspa-0.10.1/src/`
  the only hits are `utils/dict.rs:414-415` for `StaticDict`. Every loop/context/core/stream is
  thread-affine. Cross-thread communication goes through `pipewire::channel` (§10).
* **`loop_::Loop::add_signal_local` panics off the main thread.** It calls
  `assert_main_thread()` (`loop_/mod.rs:246`), which is
  `assert_eq!(thread::current().name(), Some("main"))` (`utils.rs:6-8`). If you run the PipeWire
  loop on a `std::thread::spawn`ed thread, **do not call `add_signal_local` there** — it will abort
  even if you name the thread, unless you literally name it `"main"`.
* **`spa_sys::…` in the upstream examples will not compile in this workspace.**
  `examples/tone.rs:77` writes `spa_sys::SPA_AUDIO_CHANNEL_FL` — that works only because examples are
  compiled *inside* the `pipewire` package, which has `spa_sys = libspa-sys` as a dependency
  (`pipewire-0.10.1/Cargo.toml:128-130`). From `fxsound-audio` you must write
  `libspa::sys::SPA_AUDIO_CHANNEL_FL` (`libspa-0.10.1/src/lib.rs:17` = `pub use spa_sys as sys;`) or
  `pipewire::spa::sys::SPA_AUDIO_CHANNEL_FL` (`pipewire-0.10.1/src/lib.rs:172` = `pub use spa;`).
* **🔴 Half of `pipewire::keys` does not exist with the workspace's current dependency line.**
  Twenty key constants — including `NODE_LINK_GROUP`, `TARGET_OBJECT`, `NODE_WANT_DRIVER`,
  `NODE_RATE` and `AUDIO_RATE`, all of which this document tells you to use — are
  `#[cfg(feature = ...)]`-gated, and **default features are empty**. Full list and the fix in
  §1.1. This is the first thing to change; everything else compiles on top of it.
* **🔴 `property!(key, Choice, <any>, Int | Long | Float | Double | Bool, …)` does not compile.**
  The macro expands to `Choice::<$crate::utils::Int>` and **`libspa::utils::Int` is not a type**
  (`libspa-0.10.1/src/utils/mod.rs` exports only `Id`, `Fd`, `Fraction`, `Rectangle`, `Point`,
  `Region`). Only `Id`, `Rectangle`, `Fraction` and `Fd` work in the `Choice` arms. See §8.2 for the
  form that does compile.

---

## 0.1 Compile-verification status

Everything in §8.1, §8.2, §13 and §16 was put through `cargo build` against these exact crates on
this machine (`libpipewire-0.3` 1.6.8, `pkg-config --modversion libpipewire-0.3`). Two snippets
that a careful reading of the source would have passed **failed to compile**, and both are now
corrected in place:

| Snippet | Error | Fixed in |
| --- | --- | --- |
| `property!(FormatProperties::AudioRate, Choice, Range, Int, 48000, 44100, 192000)` | `error[E0425]: cannot find type 'Int' in module '$crate::utils'` | §8.2 |
| `*pw::keys::NODE_LINK_GROUP` with no cargo features | `error[E0425]: cannot find value 'NODE_LINK_GROUP' in module 'pw::keys'` | §1.1 |

The full skeleton in §16 type-checks once §1.1's features are applied.

---

## 1. Crate roots, features, init

```rust
pub fn init();                   // pipewire-0.10.1/src/lib.rs:180
pub unsafe fn deinit();          // pipewire-0.10.1/src/lib.rs:191
```

`init()` is idempotent — it is guarded by a `OnceLock` (`lib.rs:181-184`).
`deinit()` is `unsafe`: "must only be called once during the lifetime of the process, once no
PipeWire threads are running anymore" (`lib.rs:187-190`).

Re-exports:

```rust
pub use pw_sys as sys;   // pipewire-0.10.1/src/lib.rs:171
pub use spa;             // pipewire-0.10.1/src/lib.rs:172  -> pipewire::spa == libspa
pub use spa_sys as sys;  // libspa-0.10.1/src/lib.rs:17     -> libspa::sys == libspa-sys
```

Modules (`pipewire-0.10.1/src/lib.rs:142-164`):
`buffer, channel, client, constants, context, core, device, factory, keys, link, loop_, main_loop,
metadata, module, node, permissions, port, properties, proxy, registry, stream, thread_loop, types`.

Modules (`libspa-0.10.1/src/lib.rs:9-15`):
`buffer, constants, node, param, pod, support, utils`.

## 1.1 🔴 Feature flags — change `Cargo.toml` before you write a line

`pipewire-0.10.1/Cargo.toml:44-74` — `v0_3_32 … v0_3_77, v1_0_0, v1_2_0`, each implying the previous.
`libspa-0.10.1/Cargo.toml:48-60` — `v0_3_21 … v1_6_0`.

**Default features are empty for both crates.** The workspace currently pins:

```toml
pipewire = "0.10.1"      # fxsound-linux/Cargo.toml:31 — no features
libspa   = "0.10.1"      # fxsound-linux/Cargo.toml:32 — no features
```

That is not enough to compile the code in §13/§16. Change it to:

```toml
# fxsound-linux/Cargo.toml — [workspace.dependencies]
pipewire = { version = "0.10.1", features = ["v0_3_65"] }
libspa   = { version = "0.10.1", features = ["v0_3_65"] }
```

`v0_3_65` is the sweet spot: it subsumes every gate below and matches libspa's own `v0_3_65`
(which is the last one that also flips `spa_sys/v0_3_65`). Higher levels (`v1_0_0`, `v1_2_0`,
libspa `v1_6_0`) also build here — the installed daemon is **1.6.8** — but buy nothing this project
uses. `v0_3_44` is the hard floor, because `TARGET_OBJECT` is behind it.

> `crates/fxsound-audio/Cargo.toml` currently lists only `fxsound-core`. Add
> `pipewire = { workspace = true }` and `libspa = { workspace = true }` there too.

### Gated *methods and types*

| Item | Feature | Cite |
| --- | --- | --- |
| `StreamFlags::TRIGGER` | `v0_3_41` | `stream/mod.rs:957-958` |
| `Stream::is_driving`, `Stream::trigger_process` | `v0_3_34` | `stream/mod.rs:357,362` |
| `Stream::time()` uses `pw_stream_get_time_n` and fills `buffered`/`queued_buffers`/`avail_buffers` | `v0_3_50` | `stream/mod.rs:386-391`, `stream/mod.rs:106-121` |
| `Buffer::requested()` | `v0_3_49` | `pipewire-0.10.1/src/buffer.rs:72-75` |
| stream `command` callback field | `v0_3_39` | `stream/mod.rs:419-420` |
| stream `trigger_done` callback field | `v0_3_40` | `stream/mod.rs:421-422` |
| `PropertyFlags::DONT_FIXATE` | `v0_3_33` (libspa) | `libspa-0.10.1/src/pod/mod.rs:1486-1487` |
| `FormatProperties::AudioBitrate`, `AudioBlockAlign`, `AudioAacStreamFormat`, `AudioWmaProfile`, `AudioAmrBandMode` | `v0_3_65` (libspa) | `libspa/src/param/format.rs:200-215` |

### 🔴 Gated *`keys::` constants* — the ones that actually bite

Every one of these is `#[cfg(feature = ...)]` in `pipewire-0.10.1/src/keys.rs` and is **absent**
with default features. Referencing one produces
`error[E0425]: cannot find value 'X' in module 'pw::keys'` — verified for `NODE_LINK_GROUP`.

| `keys::` constant | Feature gate | `keys.rs` |
| --- | --- | --- |
| `OBJECT_REGISTER` | `v0_3_32` | :73 |
| **`NODE_LINK_GROUP`** | **`v0_3_32`** | :270 |
| **`AUDIO_RATE`** | **`v0_3_32`** | :509 |
| `NODE_LOCK_QUANTUM` | `v0_3_33` | :214 |
| **`NODE_RATE`** | **`v0_3_33`** | :222 |
| `NODE_LOCK_RATE` | `v0_3_33` | :226 |
| **`NODE_WANT_DRIVER`** | **`v0_3_33`** | :240 |
| `NODE_NETWORK` | `v0_3_39` | :274 |
| `OBJECT_SERIAL` | `v0_3_41` | :66 |
| `NODE_TRIGGER` | `v0_3_41` | :278 |
| `AUDIO_ALLOWED_RATES` | `v0_3_43` | :519 |
| **`NODE_SUSPEND_ON_IDLE`** | **`v0_3_44`** | :247 |
| `NODE_TRANSPORT_SYNC` | `v0_3_44` | :254 |
| **`TARGET_OBJECT`** | **`v0_3_44`** | :532 |
| `NODE_FORCE_QUANTUM` | `v0_3_45` | :218 |
| `NODE_FORCE_RATE` | `v0_3_45` | :230 |
| `DEVICE_SYSFS_PATH` | `v0_3_53` | :388 |
| `CONFIG_OVERRIDE_PREFIX`, `CONFIG_OVERRIDE_NAME` | `v0_3_57` | :83, :87 |
| `NODE_CHANNELNAMES` | `v0_3_64` | :282 |

Bolded rows are used by §13 and §16. Everything else in `keys.rs` — `NODE_NAME`, `NODE_VIRTUAL`,
`NODE_PASSIVE`, `NODE_AUTOCONNECT`, `NODE_LATENCY`, `NODE_DONT_RECONNECT`, `NODE_ALWAYS_PROCESS`,
`MEDIA_*`, `AUDIO_CHANNELS`, `AUDIO_FORMAT`, `STREAM_*`, `APP_*`, `LINK_*`, `PORT_*` — is
ungated and always present.

If for some reason you cannot raise the feature level, every one of these is just a string: write
`"node.link-group"` / `"target.object"` literally (see §11 for the exact spellings). `properties!`
takes `impl Into<Vec<u8>>` for keys (`properties/mod.rs:69`), so a literal is a drop-in.

---

## 2. `MainLoop` / `Loop` — construction, lifetimes, run/quit

### The borrow type

```rust
#[repr(transparent)]
pub struct MainLoop(pw_sys::pw_main_loop);          // main_loop/mod.rs:23-24

impl MainLoop {
    pub fn as_raw(&self) -> &pw_sys::pw_main_loop;              // main_loop/mod.rs:27
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_main_loop;      // main_loop/mod.rs:31
    pub fn loop_(&self) -> &Loop;                               // main_loop/mod.rs:35
    pub fn run(&self);                                          // main_loop/mod.rs:43
    pub fn quit(&self);                                         // main_loop/mod.rs:49
}
impl std::convert::AsRef<Loop> for MainLoop                      // main_loop/mod.rs:56
```

`run()` blocks until `quit()`. Both take `&self`, so `quit()` can be called from a callback that
holds a clone of `MainLoopRc` — that is exactly what `examples/roundtrip.rs:36` does.

### The owners

```rust
pub struct MainLoopBox { ptr: std::ptr::NonNull<pw_sys::pw_main_loop> }   // main_loop/box_.rs:16-18

impl MainLoopBox {
    pub fn new(properties: Option<&spa::utils::dict::DictRef>) -> Result<Self, Error>;   // box_.rs:22
    pub unsafe fn from_raw(ptr: ptr::NonNull<pw_sys::pw_main_loop>) -> Self;             // box_.rs:43
    pub fn into_raw(self) -> std::ptr::NonNull<pw_sys::pw_main_loop>;                    // box_.rs:47
}
// Deref<Target = MainLoop> box_.rs:53 ; Drop calls pw_main_loop_destroy box_.rs:67-72
```

```rust
#[derive(Debug, Clone)]
pub struct MainLoopRc { inner: Rc<MainLoopRcInner> }             // main_loop/rc.rs:28-31

impl MainLoopRc {
    pub fn new(properties: Option<&spa::utils::dict::DictRef>) -> Result<Self, Error>;   // rc.rs:35
    pub unsafe fn from_raw(ptr: ptr::NonNull<pw_sys::pw_main_loop>) -> Self;             // rc.rs:49
    pub fn downgrade(&self) -> MainLoopWeak;                                             // rc.rs:57
}
unsafe impl IsLoopRc for MainLoopRc {}                            // rc.rs:65
impl AsRef<MainLoop> for MainLoopRc                               // rc.rs:75
impl AsRef<Loop>     for MainLoopRc                               // rc.rs:81

pub struct MainLoopWeak { weak: Weak<MainLoopRcInner> }           // rc.rs:90
impl MainLoopWeak { pub fn upgrade(&self) -> Option<MainLoopRc>; }// rc.rs:95
```

**Note the argument type asymmetry, it bites:** `MainLoopBox::new`/`MainLoopRc::new` and
`LoopBox::new`/`LoopRc::new` take `Option<&DictRef>` (borrowed, not consumed), whereas
`ContextBox::new`, `ContextRc::new`, `Context::connect` and `StreamBox::new` take
`Option<PropertiesBox>` / `PropertiesBox` **by value** (ownership is taken; internally
`props.into_raw()`).

### `Loop` — the source-attachment surface

```rust
#[repr(transparent)]
pub struct Loop(pw_sys::pw_loop);                                // loop_/mod.rs:30-31

impl Loop {
    pub fn as_raw(&self) -> &pw_sys::pw_loop;                    // loop_/mod.rs:34
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_loop;            // loop_/mod.rs:38
    pub fn fd(&self) -> BorrowedFd<'_>;                          // loop_/mod.rs:43
    pub unsafe fn enter(&self);                                  // loop_/mod.rs:65
    pub unsafe fn leave(&self);                                  // loop_/mod.rs:82
    pub fn iterate(&self, timeout: Timeout) -> i32;              // loop_/mod.rs:105
    pub unsafe fn iterate_unguarded(&self, timeout: Timeout) -> i32; // loop_/mod.rs:129

    pub fn add_io<I, F>(&self, io: I, event_mask: IoFlags, callback: F) -> IoSource<'_, I>
    where I: AsRawFd, F: Fn(&mut I) + 'static, Self: Sized;      // loop_/mod.rs:150-155

    pub fn add_idle<F>(&self, enabled: bool, callback: F) -> IdleSource<'_>;   // loop_/mod.rs:199
    pub fn add_signal_local<F>(&self, signal: Signal, callback: F) -> SignalSource<'_>
    where F: Fn() + 'static;                                     // loop_/mod.rs:241-243
    pub fn add_event<F>(&self, callback: F) -> EventSource<'_>
    where F: Fn() + 'static;                                     // loop_/mod.rs:286-288
    pub fn add_timer<F>(&self, callback: F) -> TimerSource<'_>
    where F: Fn(u64) + 'static;                                  // loop_/mod.rs:329-331
}
```

```rust
#[derive(Debug, Clone)]
pub enum Timeout { None, Infinite, Finite(Duration) }            // loop_/mod.rs:388-392
pub trait IsSource { ... }                                       // loop_/mod.rs:408
pub use rustix::process::Signal;                                 // loop_/mod.rs:13
```

Sources and their methods:

```rust
pub struct IoSource<'l, I>    { .. }                              // loop_/mod.rs:418
pub struct IdleSource<'l>     { .. }  pub fn enable(&self, enable: bool);      // :449, :458
pub struct SignalSource<'l>   { .. }                              // loop_/mod.rs:488
pub struct EventSource<'l>    { .. }  pub fn signal(&self) -> SpaResult;       // :513, :529
pub struct TimerSource<'l>    { .. }
    pub fn update_timer(&self, value: Option<Duration>, interval: Option<Duration>) -> SpaResult;
                                                                  // loop_/mod.rs:557, :575
```

All sources are `Drop`-destroyed off the loop (`loop_/mod.rs:545-549` for `EventSource`), and every
`add_*` is `#[must_use]` — **dropping the returned source immediately unregisters the callback.**

Owning loops:

```rust
pub struct LoopBox { .. }
    pub fn new(properties: Option<&spa::utils::dict::DictRef>) -> Result<Self, Error>; // loop_/box_.rs:21
pub struct LoopRc  { .. }
    pub fn new(properties: Option<&spa::utils::dict::DictRef>) -> Result<Self, Error>; // loop_/rc.rs:39
    pub fn downgrade(&self) -> LoopWeak;                                               // loop_/rc.rs:61

/// # Safety
/// The [`Loop`] returned by the implementation of `AsRef<Loop>` must remain valid as long as any
/// clone of the trait implementor is still alive.
pub unsafe trait IsLoopRc: Clone + AsRef<Loop> + 'static {}       // loop_/rc.rs:19
```

---

## 3. `Context` and `Core`

```rust
#[repr(transparent)]
pub struct Context(pw_sys::pw_context);                          // context/mod.rs:34-35

impl Context {
    pub fn as_raw(&self) -> &pw_sys::pw_context;                              // context/mod.rs:38
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_context;                      // context/mod.rs:42
    pub fn properties(&self) -> &Properties;                                  // context/mod.rs:46
    pub fn update_properties(&self, properties: &spa::utils::dict::DictRef);  // context/mod.rs:54
    pub fn connect(&self, properties: Option<PropertiesBox>) -> Result<CoreBox<'_>, Error>;
                                                                              // context/mod.rs:60
    pub fn connect_fd(&self, fd: OwnedFd, properties: Option<PropertiesBox>)
        -> Result<CoreBox<'_>, Error>;                                        // context/mod.rs:71
}
```

```rust
pub struct ContextBox<'l> { ptr: NonNull<pw_sys::pw_context>, loop_: PhantomData<&'l Loop> }
                                                                  // context/box_.rs:16-19
impl<'l> ContextBox<'l> {
    pub fn new(loop_: &'l Loop, properties: Option<PropertiesBox>) -> Result<ContextBox<'l>, Error>;
                                                                  // context/box_.rs:22-25
    pub unsafe fn from_raw(raw: NonNull<pw_sys::pw_context>) -> ContextBox<'l>;  // box_.rs:47
    pub fn into_raw(self) -> NonNull<pw_sys::pw_context>;                        // box_.rs:54
}
```

```rust
#[derive(Clone, Debug)]
pub struct ContextRc { inner: Rc<ContextRcInner> }                // context/rc.rs:42-45
impl ContextRc {
    pub fn new<T: IsLoopRc>(loop_: &T, properties: Option<PropertiesBox>) -> Result<Self, Error>;
                                                                  // context/rc.rs:48
    pub fn downgrade(&self) -> ContextWeak;                       // context/rc.rs:75
    pub fn connect_rc(&self, properties: Option<PropertiesBox>) -> Result<CoreRc, Error>;
                                                                  // context/rc.rs:80
    pub fn connect_fd_rc(&self, fd: OwnedFd, properties: Option<PropertiesBox>)
        -> Result<CoreRc, Error>;                                 // context/rc.rs:91
}
```

`ContextRcInner` deliberately stores the loop so drop order keeps the loop alive longer than the
context (`context/rc.rs:21-27`). `CoreRcInner` does the same with the context (`core/rc.rs:20-24`
in spirit; see `stream/rc.rs:18-25` for the same pattern with the core). **This is the whole point
of the `Rc` variants** — with the `Box` variants you carry the lifetime yourself.

```rust
pub const PW_ID_CORE: u32 = pw_sys::PW_ID_CORE;                   // core/mod.rs:32

#[repr(transparent)]
pub struct Core(pw_sys::pw_core);                                 // core/mod.rs:42-43

impl Core {
    pub fn as_raw(&self) -> &pw_sys::pw_core;                                       // core/mod.rs:46
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_core;                               // core/mod.rs:50
    pub fn add_listener_local(&self) -> ListenerLocalBuilder<'_>;                   // core/mod.rs:56
    pub fn get_registry(&self) -> Result<RegistryBox<'_>, Error>;                   // core/mod.rs:63
    pub fn sync(&self, seq: i32) -> Result<AsyncSeq, Error>;                        // core/mod.rs:83
    pub fn create_object<P: ProxyT>(
        &self,
        factory_name: &str,
        properties: &impl AsRef<spa::utils::dict::DictRef>,
    ) -> Result<P, Error>;                                                          // core/mod.rs:146
    pub fn create_object_cstr<P: ProxyT>(
        &self,
        factory_name: &CStr,
        properties: &impl AsRef<spa::utils::dict::DictRef>,
    ) -> Result<P, Error>;                                                          // core/mod.rs:156
    pub fn destroy_object<P: ProxyT>(&self, proxy: P) -> Result<AsyncSeq, Error>;   // core/mod.rs:186
}
```

Core listener builder (`core/mod.rs:230-350`):

```rust
pub fn info<F>(mut self, info: F) -> Self  where F: Fn(&Info) + 'static;             // core/mod.rs:279
pub fn done<F>(mut self, done: F) -> Self  where F: Fn(u32, AsyncSeq) + 'static;     // core/mod.rs:305
pub fn error<F>(mut self, error: F) -> Self
    where F: Fn(u32, i32, i32, &str) + 'static;                                      // core/mod.rs:338
pub fn register(self) -> Listener;                                                   // core/mod.rs:347
```

`CoreBox<'c>` (`core/box_.rs:16`) `Drop`s by calling `pw_core_disconnect` (`core/box_.rs:58-63`).
`CoreRc::from_raw(ptr, context)` (`core/rc.rs:37`), `CoreRc::downgrade` (`core/rc.rs:51`),
`CoreRc::get_registry_rc` (`core/rc.rs:56`), `CoreWeak::upgrade` (`core/rc.rs:93`).

### The canonical three-liner (from the examples, verbatim)

```rust
// pipewire-0.10.1/examples/audio-capture.rs:30-34
pw::init();
let mainloop = pw::main_loop::MainLoopRc::new(None)?;
let context  = pw::context::ContextRc::new(&mainloop, None)?;
let core     = context.connect_rc(None)?;
```

Box-flavoured equivalent (`pipewire-0.10.1/src/lib.rs:22-28`) — note `&mainloop.loop_()`:

```rust
use pipewire::{main_loop::MainLoopBox, context::ContextBox};
let mainloop = MainLoopBox::new(None)?;
let context  = ContextBox::new(&mainloop.loop_(), None)?;
let core     = context.connect(None)?;
let registry = core.get_registry()?;
```

---

## 4. `Registry`

```rust
#[repr(transparent)]
pub struct Registry(pw_sys::pw_registry);                         // registry/mod.rs:53-54

impl Registry {
    pub fn as_raw(&self) -> &pw_sys::pw_registry;                             // registry/mod.rs:57
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_registry;                     // registry/mod.rs:61
    pub fn add_listener_local(&self) -> ListenerLocalBuilder<'_>;             // registry/mod.rs:67
    pub fn bind<T: ProxyT, P: AsRef<spa::utils::dict::DictRef>>(
        &self, object: &GlobalObject<P>) -> Result<T, Error>;                 // registry/mod.rs:82
    pub fn destroy_global(&self, global_id: u32) -> spa::utils::result::SpaResult;
                                                                              // registry/mod.rs:112
}

pub fn global<F>(mut self, global: F) -> Self
    where F: Fn(&GlobalObject<&spa::utils::dict::DictRef>) + 'static;         // registry/mod.rs:194-196
pub fn global_remove<F>(mut self, global_remove: F) -> Self
    where F: Fn(u32) + 'static;                                               // registry/mod.rs:219-221
pub fn register(self) -> Listener;                                            // registry/mod.rs:228

#[derive(Debug)]
pub struct GlobalObject<P: AsRef<spa::utils::dict::DictRef>> {
    pub id: u32,
    pub permissions: PermissionFlags,
    pub type_: ObjectType,
    pub version: u32,
    pub props: Option<P>,
}                                                                             // registry/mod.rs:288-295
impl<P> GlobalObject<P> { pub fn to_owned(&self) -> GlobalObject<PropertiesBox>; } // :321
```

`ObjectType` (`types.rs:45-107`) is an enum with variants
`Client, ClientEndpoint, ClientNode, ClientSession, Core, Device, Endpoint, EndpointLink,
EndpointStream, Factory, Link, Metadata, Module, Node, Port, Profiler, Registry, Session,
Other(String)`, plus `pub fn to_str(&self) -> &str` (`types.rs:60`) yielding
`"PipeWire:Interface:Node"` and friends.

```rust
pub const ID_ANY: u32 = pw_sys::PW_ID_ANY;                        // pipewire-0.10.1/src/constants.rs:7
```

---

## 5. `properties!` and `Properties` / `PropertiesBox`

```rust
#[repr(transparent)]
pub struct Properties(pw_sys::pw_properties);                     // properties/mod.rs:21-22

impl Properties {
    pub fn as_raw(&self) -> &pw_sys::pw_properties;                        // properties/mod.rs:25
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_properties;                // properties/mod.rs:35
    pub fn dict(&self) -> &spa::utils::dict::DictRef;                      // properties/mod.rs:39
    pub fn to_owned(&self) -> PropertiesBox;                               // properties/mod.rs:44
    pub fn get(&self, key: &str) -> Option<&str>;                          // properties/mod.rs:51
    pub fn insert<K, V>(&mut self, key: K, value: V)
        where K: Into<Vec<u8>>, V: Into<Vec<u8>>;                          // properties/mod.rs:67-70
    pub fn remove<T>(&mut self, key: T) where T: Into<Vec<u8>>;            // properties/mod.rs:77-79
    pub fn clear(&mut self);                                               // properties/mod.rs:85
}
impl AsRef<spa::utils::dict::DictRef> for Properties                       // properties/mod.rs:90
impl<K, V> Extend<(K, V)> for Properties                                   // properties/mod.rs:103
```

```rust
pub struct PropertiesBox { ptr: ptr::NonNull<pw_sys::pw_properties> }      // properties/box_.rs:25-27

impl PropertiesBox {
    pub fn new() -> Self;                                                   // box_.rs:31
    pub unsafe fn from_raw(ptr: ptr::NonNull<pw_sys::pw_properties>) -> Self; // box_.rs:46
    pub fn into_raw(self) -> *mut pw_sys::pw_properties;                    // box_.rs:54
    pub fn from_dict(dict: &spa::utils::dict::DictRef) -> Self;             // box_.rs:66
}
impl Deref<Target = Properties>      // box_.rs:87
impl DerefMut                        // box_.rs:95   <- this is why `props.insert(..)` works
impl Default / Clone / FromIterator<(K,V)> / Drop / Debug   // box_.rs:101,107,118,132,138
```

The macro, verbatim (`properties/box_.rs:173-184`):

```rust
#[macro_export]
macro_rules! __properties__ {
    {$($k:expr => $v:expr),+ $(,)?} => {{
        let mut properties = $crate::properties::PropertiesBox::new();
        $(
            properties.insert($k, $v);
        )*
        properties
    }};
}
pub use __properties__ as properties;
```

So `properties! { .. }` evaluates to a **`PropertiesBox`**, which is what `StreamBox::new` wants.
Import path is `use pipewire::properties::properties;` (`properties/box_.rs:15`), **not**
`pipewire::properties!`.

Usage with `keys` (note the `*` deref — the constants are `LazyLock<&'static str>`,
`keys.rs:21-25`):

```rust
use pipewire as pw;
use pw::properties::properties;

let mut props = properties! {
    *pw::keys::MEDIA_TYPE     => "Audio",
    *pw::keys::MEDIA_CATEGORY => "Playback",
    *pw::keys::MEDIA_ROLE     => "Production",
};
props.insert(*pw::keys::TARGET_OBJECT, "alsa_output.pci-0000_00_1f.3.analog-stereo");
```

`libspa` also has a compile-time dict for static data:

```rust
pub struct StaticDict { .. }                                   // libspa/src/utils/dict.rs:338
pub const unsafe fn from_ptr(ptr: NonNull<spa_sys::spa_dict>) -> Self;  // dict.rs:349
#[macro_export] macro_rules! static_dict { {$($k:expr => $v:expr),+ $(,)?} => {{ .. }} } // dict.rs:369
unsafe impl Send for StaticDict {}  unsafe impl Sync for StaticDict {}  // dict.rs:414-415
```

`DictRef` API (`libspa/src/utils/dict.rs`):
`as_raw :16`, `as_raw_ptr :24`, `iter_cstr :31`, `iter :46`, `keys :54`, `values :62`, `len :70`,
`is_empty :75`, `flags :80`, `get(&self, key:&str)->Option<&str> :90`,
`parse<T: ParsableValue>(&self, key:&str)->Option<Result<T,ParseValueError>> :122`.

---

## 6. The Stream API

### 6.1 Types

```rust
#[derive(Debug, PartialEq)]
pub enum StreamState {
    Error(String), Unconnected, Connecting, Paused, Streaming,
}                                                                 // stream/mod.rs:25-32

#[repr(transparent)]
pub struct Stream(pw_sys::pw_stream);                             // stream/mod.rs:150-151
```

`Stream` is the **non-owning** wrapper (0.8's `StreamRef`). Owners:

```rust
pub struct StreamBox<'c> { ptr: NonNull<pw_sys::pw_stream>, core: PhantomData<&'c Core> }
                                                                  // stream/box_.rs:18-21
impl<'c> StreamBox<'c> {
    pub fn new(core: &'c Core, name: &str, properties: PropertiesBox)
        -> Result<StreamBox<'c>, Error>;                          // stream/box_.rs:27-31
    pub fn new_cstr(core: &'c Core, name: &CStr, properties: PropertiesBox)
        -> Result<StreamBox<'c>, Error>;                          // stream/box_.rs:39-43
    pub unsafe fn from_raw(raw: NonNull<pw_sys::pw_stream>) -> StreamBox<'c>;  // box_.rs:63
    pub fn into_raw(self) -> NonNull<pw_sys::pw_stream>;                       // box_.rs:70
}
// Deref<Target = Stream> box_.rs:75 ; Debug box_.rs:83 ; Drop -> pw_stream_destroy box_.rs:94-97

#[derive(Clone, Debug)]
pub struct StreamRc { inner: Rc<StreamRcInner> }                  // stream/rc.rs:33-36
impl StreamRc {
    pub fn new(core: CoreRc, name: &str, properties: PropertiesBox) -> Result<StreamRc, Error>;
                                                                  // stream/rc.rs:39
    pub fn new_cstr(core: CoreRc, name: &CStr, properties: PropertiesBox)
        -> Result<StreamRc, Error>;                               // stream/rc.rs:47
    pub fn downgrade(&self) -> StreamWeak;                        // stream/rc.rs:68
}
pub struct StreamWeak { weak: Weak<StreamRcInner> }               // stream/rc.rs:91
impl StreamWeak { pub fn upgrade(&self) -> Option<StreamRc>; }    // stream/rc.rs:96
```

`StreamRc::new` takes the `CoreRc` **by value** (`stream/rc.rs:39`) and stashes it so the core
outlives the stream (`stream/rc.rs:18-25`). `StreamBox::new` takes `&'c Core` and ties the stream's
lifetime to it via `PhantomData`.

### 6.2 `Stream` methods (verbatim)

```rust
impl Stream {
    pub fn as_raw(&self) -> &pw_sys::pw_stream;                                 // stream/mod.rs:154
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_stream;                         // stream/mod.rs:158

    #[must_use = "Use the builder to register event callbacks"]
    pub fn add_local_listener_with_user_data<D>(
        &self,
        user_data: D,
    ) -> ListenerLocalBuilder<'_, D>;                                           // stream/mod.rs:164-167

    #[must_use = "Use the builder to register event callbacks"]
    pub fn add_local_listener<D: Default>(&self) -> ListenerLocalBuilder<'_, D>;// stream/mod.rs:179

    pub fn connect(
        &self,
        direction: spa::utils::Direction,
        id: Option<u32>,
        flags: StreamFlags,
        params: &mut [&spa::pod::Pod],
    ) -> Result<(), Error>;                                                     // stream/mod.rs:188-194

    pub fn update_params(&self, params: &mut [&spa::pod::Pod]) -> Result<(), Error>;
                                                                                // stream/mod.rs:217
    pub fn set_active(&self, active: bool) -> Result<(), Error>;                // stream/mod.rs:231
    pub unsafe fn dequeue_raw_buffer(&self) -> *mut pw_sys::pw_buffer;          // stream/mod.rs:248
    pub fn dequeue_buffer(&self) -> Option<Buffer<'_>>;                         // stream/mod.rs:252
    pub unsafe fn queue_raw_buffer(&self, buffer: *mut pw_sys::pw_buffer);      // stream/mod.rs:266
    pub fn disconnect(&self) -> Result<(), Error>;                              // stream/mod.rs:271
    pub fn set_error(&mut self, res: i32, error: &str);                         // stream/mod.rs:283
    pub fn set_error_cstr(&mut self, res: i32, error: &CStr);                   // stream/mod.rs:294
    pub fn flush(&self, drain: bool) -> Result<(), Error>;                      // stream/mod.rs:302
    pub fn set_control(&self, id: u32, values: &[f32]) -> Result<(), Error>;    // stream/mod.rs:309
    pub fn name(&self) -> String;                                               // stream/mod.rs:325
    pub fn state(&self) -> StreamState;                                         // stream/mod.rs:335
    pub fn properties(&self) -> &Properties;                                    // stream/mod.rs:344
    pub fn node_id(&self) -> u32;                                               // stream/mod.rs:353
    #[cfg(feature = "v0_3_34")] pub fn is_driving(&self) -> bool;               // stream/mod.rs:358
    #[cfg(feature = "v0_3_34")] pub fn trigger_process(&self) -> Result<(), Error>; // stream/mod.rs:363
    pub fn time(&self) -> Result<Time, Error>;                                  // stream/mod.rs:382
}
// TODO in-source: pw_stream_get_core(), pw_stream_get_nsec()   // stream/mod.rs:401-402
```

`connect` takes `id: Option<u32>` and internally does
`id.unwrap_or(crate::constants::ID_ANY)` (`stream/mod.rs:199`). **It is a numeric node id, not a
name** — to target by name, set the `target.object` property instead (§14).

`set_error`/`set_error_cstr` are the only `&mut self` methods; everything else is `&self`.

### 6.3 `Time`

```rust
#[repr(transparent)]
pub struct Time(pw_sys::pw_time);                                 // stream/mod.rs:63-64
impl Time {
    pub fn as_raw(&self) -> &pw_sys::pw_time;                     // stream/mod.rs:73
    pub fn now(&self) -> i64;                                     // stream/mod.rs:78
    pub fn rate(&self) -> spa::utils::Fraction;                   // stream/mod.rs:83
    pub fn ticks(&self) -> u64;                                   // stream/mod.rs:88
    pub fn delay(&self) -> i64;                                   // stream/mod.rs:96
    pub fn queued(&self) -> u64;                                  // stream/mod.rs:101
    #[cfg(feature = "v0_3_50")] pub fn buffered(&self) -> u64;        // stream/mod.rs:107
    #[cfg(feature = "v0_3_50")] pub fn queued_buffers(&self) -> u32;  // stream/mod.rs:113
    #[cfg(feature = "v0_3_50")] pub fn avail_buffers(&self) -> u32;   // stream/mod.rs:119
}
impl Clone for Time                                               // stream/mod.rs:66
```

`Stream::time()` is documented as **RT-safe** (`stream/mod.rs:381`).

### 6.4 Callbacks — `ListenerLocalBuilder<'a, D>`

```rust
type ParamChangedCB<D> = dyn FnMut(&Stream, &mut D, u32, Option<&spa::pod::Pod>); // stream/mod.rs:405
type ProcessCB<D>      = dyn FnMut(&Stream, &mut D);                              // stream/mod.rs:406

pub struct ListenerLocalCallbacks<D> {
    pub state_changed: Option<Box<dyn FnMut(&Stream, &mut D, StreamState, StreamState)>>,
    pub control_info:
        Option<Box<dyn FnMut(&Stream, &mut D, u32, *const pw_sys::pw_stream_control)>>,
    pub io_changed: Option<Box<dyn FnMut(&Stream, &mut D, u32, *mut os::raw::c_void, u32)>>,
    pub param_changed: Option<Box<ParamChangedCB<D>>>,
    pub add_buffer: Option<Box<dyn FnMut(&Stream, &mut D, *mut pw_sys::pw_buffer)>>,
    pub remove_buffer: Option<Box<dyn FnMut(&Stream, &mut D, *mut pw_sys::pw_buffer)>>,
    pub process: Option<Box<ProcessCB<D>>>,
    pub drained: Option<Box<dyn FnMut(&Stream, &mut D)>>,
    #[cfg(feature = "v0_3_39")]
    pub command: Option<Box<dyn FnMut(&Stream, &mut D, *const spa_sys::spa_command)>>,
    #[cfg(feature = "v0_3_40")]
    pub trigger_done: Option<Box<dyn FnMut(&Stream, &mut D)>>,
    pub user_data: D,
    stream: Option<ptr::NonNull<pw_sys::pw_stream>>,
}                                                                 // stream/mod.rs:408-425

pub struct ListenerLocalBuilder<'a, D> { stream: &'a Stream, callbacks: ListenerLocalCallbacks<D> }
                                                                  // stream/mod.rs:660-663
```

Builder methods (each `#[must_use = "Call `.register()` to start receiving events"]`):

```rust
pub fn state_changed<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D, StreamState, StreamState) + 'static;      // stream/mod.rs:686-688
pub fn control_info<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D, u32, *const pw_sys::pw_stream_control) + 'static;
                                                                              // stream/mod.rs:714-716
pub fn io_changed<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D, u32, *mut os::raw::c_void, u32) + 'static;// stream/mod.rs:743-745
pub fn param_changed<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D, u32, Option<&spa::pod::Pod>) + 'static;   // stream/mod.rs:773-775
pub fn add_buffer<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D, *mut pw_sys::pw_buffer) + 'static;        // stream/mod.rs:800-802
pub fn remove_buffer<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D, *mut pw_sys::pw_buffer) + 'static;        // stream/mod.rs:827-829
pub fn process<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D) + 'static;                                // stream/mod.rs:860-862
pub fn drained<F>(mut self, callback: F) -> Self
    where F: FnMut(&Stream, &mut D) + 'static;                                // stream/mod.rs:886-888
pub fn register(self) -> Result<StreamListener<D>, Error>;                    // stream/mod.rs:895
```

**Argument-count gotchas vs. older memory:**

* `param_changed` is **4 args**: `(&Stream, &mut D, id: u32, Option<&Pod>)`. The 4th is an `Option`
  — `None` means "clear the format" (`examples/audio-capture.rs:81-84`). It is **not** `&Pod`.
* `process` is **2 args**: `(&Stream, &mut D)`. There is no `spa_io_position` argument here (that is
  the `pw_filter` signature, `/usr/include/pipewire-0.3/pipewire/filter.h:87`).
* `state_changed` is **4 args**: `(&Stream, &mut D, old, new)`. No error string parameter — the
  error is folded into `StreamState::Error(String)` (`stream/mod.rs:35-51`).
* `command` and `trigger_done` exist only behind `v0_3_39` / `v0_3_40`, and they have **no builder
  methods** — the struct fields exist (`stream/mod.rs:419-422`) and are wired in `into_raw`
  (`:616-623`), but `ListenerLocalBuilder` exposes no setters for them in 0.10.1. You would have to
  build `ListenerLocalCallbacks` yourself; `into_raw` is `pub(crate)` (`stream/mod.rs:453`), so in
  practice **these two callbacks are unreachable from safe user code in 0.10.1.**

```rust
#[must_use = "Listeners unregister themselves when dropped. Keep the listener alive in order to receive events."]
pub struct StreamListener<D> {
    listener: Box<spa_sys::spa_hook>,
    _events: Pin<Box<pw_sys::pw_stream_events>>,
    _data: Box<ListenerLocalCallbacks<D>>,
}                                                                 // stream/mod.rs:921-927
impl<D> StreamListener<D> { pub fn unregister(self); }            // stream/mod.rs:933
impl<D> Drop for StreamListener<D>                                // stream/mod.rs:938 -> spa::utils::hook::remove
```

**The user data lives inside the listener, not the stream.** Drop the `StreamListener` and your
`D` is dropped with it and the callbacks stop. Keep it alive for the life of the stream.

### 6.5 `StreamFlags` (verbatim, `stream/mod.rs:944-960`)

```rust
bitflags! {
    /// Extra flags that can be used in [`Stream::connect()`]
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub struct StreamFlags: pw_sys::pw_stream_flags {
        const AUTOCONNECT = pw_sys::pw_stream_flags_PW_STREAM_FLAG_AUTOCONNECT;
        const INACTIVE = pw_sys::pw_stream_flags_PW_STREAM_FLAG_INACTIVE;
        const MAP_BUFFERS = pw_sys::pw_stream_flags_PW_STREAM_FLAG_MAP_BUFFERS;
        const DRIVER = pw_sys::pw_stream_flags_PW_STREAM_FLAG_DRIVER;
        const RT_PROCESS = pw_sys::pw_stream_flags_PW_STREAM_FLAG_RT_PROCESS;
        const NO_CONVERT = pw_sys::pw_stream_flags_PW_STREAM_FLAG_NO_CONVERT;
        const EXCLUSIVE = pw_sys::pw_stream_flags_PW_STREAM_FLAG_EXCLUSIVE;
        const DONT_RECONNECT = pw_sys::pw_stream_flags_PW_STREAM_FLAG_DONT_RECONNECT;
        const ALLOC_BUFFERS = pw_sys::pw_stream_flags_PW_STREAM_FLAG_ALLOC_BUFFERS;
        #[cfg(feature = "v0_3_41")]
        const TRIGGER = pw_sys::pw_stream_flags_PW_STREAM_FLAG_TRIGGER;
    }
}
```

There is **no `ASYNC`** variant in the Rust bindings (the C API gained
`PW_STREAM_FLAG_ASYNC`; it is not mirrored here). `TRIGGER` needs `v0_3_41`.

`RT_PROCESS` is what moves `process()` onto the data thread. Without it, `process()` runs on the
main loop.

### 6.6 `Direction` (verbatim, `libspa-0.10.1/src/utils/direction.rs`)

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Direction(spa_sys::spa_direction);                     // direction.rs:6-7

#[allow(non_upper_case_globals)]
impl Direction {
    pub const Input: Self  = Self(spa_sys::SPA_DIRECTION_INPUT);  // direction.rs:11
    pub const Output: Self = Self(spa_sys::SPA_DIRECTION_OUTPUT); // direction.rs:12
    pub fn from_raw(raw: spa_sys::spa_direction) -> Self;         // direction.rs:14
    pub fn as_raw(&self) -> spa_sys::spa_direction;               // direction.rs:18
    pub fn reverse(&self) -> Self;                                // direction.rs:25
}
```

**It is a newtype with associated consts, not an enum.** `Direction::Input` — no `::` variants, no
`match` exhaustiveness. Re-exported at `libspa::utils::Direction` (`utils/mod.rs:4-5`).

**Direction is from the node's point of view:**
* a **sink** (audio flows *into* you) → `Direction::Input` (`examples/audio-capture.rs:180`)
* a **playback stream** (audio flows *out of* you) → `Direction::Output` (`examples/tone.rs:96`)

---

## 7. Buffers — getting at the actual samples

```rust
pub struct Buffer<'s> {
    buf: NonNull<pw_sys::pw_buffer>,
    stream: &'s Stream,
}                                                                 // pipewire-0.10.1/src/buffer.rs:8-14

impl Buffer<'_> {
    pub fn datas_mut(&mut self) -> &mut [Data];                   // buffer.rs:24
    pub fn find_meta<T>(&self) -> Option<&T> where T: Metadata;   // buffer.rs:41-44
    #[cfg(feature = "v0_3_49")] pub fn requested(&self) -> u64;   // buffer.rs:73
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        unsafe { self.stream.queue_raw_buffer(self.buf.as_ptr()); }
    }
}                                                                 // buffer.rs:78-84
```

**There is no `queue_buffer` method.** The buffer is returned to the stream by its `Drop` impl.
This is confirmed by the doc comment on the builder example: *"The buffer is enqueued back to the
stream when it's dropped"* (`stream/mod.rs:654`). Therefore:

* **Never `mem::forget` a `Buffer`.** You will leak a graph buffer.
* `dequeue_buffer()` returning `None` means the pool is empty — this is a normal underrun, handle it
  (`examples/audio-capture.rs:112`).

### `spa::buffer::Data` and `Chunk`

```rust
#[repr(transparent)]
pub struct Data(spa_sys::spa_data);                               // libspa/src/buffer/mod.rs:62-63
impl Data {
    pub fn as_raw(&self) -> &spa_sys::spa_data;                   // buffer/mod.rs:66
    pub fn type_(&self) -> DataType;                              // buffer/mod.rs:70
    pub fn flags(&self) -> DataFlags;                             // buffer/mod.rs:74
    pub fn fd(&self) -> RawFd;                                    // buffer/mod.rs:78
    pub fn data(&mut self) -> Option<&mut [u8]>;                  // buffer/mod.rs:84
    pub fn chunk(&self) -> &Chunk;                                // buffer/mod.rs:98
    pub fn chunk_mut(&mut self) -> &mut Chunk;                    // buffer/mod.rs:106
}

#[repr(transparent)]
pub struct Chunk(spa_sys::spa_chunk);                             // buffer/mod.rs:135-136
impl Chunk {
    pub fn as_raw(&self) -> &spa_sys::spa_chunk;                  // buffer/mod.rs:139
    pub fn size(&self) -> u32;         pub fn size_mut(&mut self) -> &mut u32;    // :143, :147
    pub fn offset(&self) -> u32;       pub fn offset_mut(&mut self) -> &mut u32;  // :151, :155
    pub fn stride(&self) -> i32;       pub fn stride_mut(&mut self) -> &mut i32;  // :159, :163
    pub fn flags(&self) -> ChunkFlags;                                            // :167
}
```

```rust
#[allow(non_upper_case_globals)]
impl DataType {                                                   // buffer/mod.rs:12-29
    pub const Invalid: Self = Self(spa_sys::SPA_DATA_Invalid);
    pub const MemPtr:  Self = Self(spa_sys::SPA_DATA_MemPtr);
    pub const MemFd:   Self = Self(spa_sys::SPA_DATA_MemFd);
    pub const DmaBuf:  Self = Self(spa_sys::SPA_DATA_DmaBuf);
    pub const MemId:   Self = Self(spa_sys::SPA_DATA_MemId);
}
bitflags! { pub struct DataFlags: u32 {                           // buffer/mod.rs:49-60
    const READABLE = 1<<0; const WRITABLE = 1<<1; const DYNAMIC = 1<<2;
    const READWRITE = Self::READABLE.bits() | Self::WRITABLE.bits();
}}
bitflags! { pub struct ChunkFlags: i32 { const CORRUPTED = 1<<0; } } // buffer/mod.rs:127-133
```

### ⚠️ The single biggest correctness trap

**`Data::data()` returns `maxsize` bytes, not the valid bytes.** Verbatim (`buffer/mod.rs:84-96`):

```rust
pub fn data(&mut self) -> Option<&mut [u8]> {
    if self.0.data.is_null() { None } else {
        unsafe {
            Some(std::slice::from_raw_parts_mut(
                self.0.data as *mut u8,
                usize::try_from(self.0.maxsize).unwrap(),
            ))
        }
    }
}
```

So:

* **Capture (input) path:** the valid region is `chunk().offset() .. chunk().offset()+chunk().size()`.
  You must slice it yourself. `examples/audio-capture.rs:121` computes
  `n_samples = data.chunk().size() / size_of::<f32>()` and then indexes into `data.data()` — note
  the example never applies `chunk().offset()`; for a robust implementation, do.
* **Playback (output) path:** you may write up to `slice.len()` (== `maxsize`) and **must** then set
  the chunk fields yourself. `examples/tone.rs:64-67`:
  ```rust
  let chunk = data.chunk_mut();
  *chunk.offset_mut() = 0;
  *chunk.stride_mut() = stride as _;
  *chunk.size_mut()   = (stride * n_frames) as _;
  ```
  Forgetting `size_mut` yields silence; forgetting `stride_mut` yields garbage timing.
* `data()` takes `&mut self`, so you must hold `datas_mut()` mutably; you cannot call
  `data.chunk()` and `data.data()` simultaneously — read `chunk()` into locals first.
* Always check `datas.is_empty()` before indexing `datas[0]` (`examples/audio-capture.rs:115-117`).

### Reading interleaved f32 out of the byte slice — full working pattern

Taken from `examples/audio-capture.rs:111-152` (verbatim structure, trimmed of printing):

```rust
.process(|stream, user_data| match stream.dequeue_buffer() {
    None => println!("out of buffers"),
    Some(mut buffer) => {
        let datas = buffer.datas_mut();
        if datas.is_empty() {
            return;
        }

        let data = &mut datas[0];
        let n_channels = user_data.format.channels();
        let n_samples = data.chunk().size() / (mem::size_of::<f32>() as u32);

        if let Some(samples) = data.data() {
            for c in 0..n_channels {
                for n in (c..n_samples).step_by(n_channels as usize) {
                    let start = n as usize * mem::size_of::<f32>();
                    let end = start + mem::size_of::<f32>();
                    let chan = &samples[start..end];
                    let f = f32::from_le_bytes(chan.try_into().unwrap());
                    // ... use f
                }
            }
        }
    }
})
```

For an in-place DSP you will want a real `&mut [f32]` view. There is **no safe API for that** in
0.10.1 — do it once, carefully, at the top of `process()`:

```rust
// SAFETY: PipeWire guarantees MemPtr buffers are 8-byte aligned (spa_data), and we negotiated
// SPA_AUDIO_FORMAT_F32 so the bytes are f32. Slice to the *valid* region only.
let bytes: &mut [u8] = data.data().expect("mapped buffer");
let off  = chunk_offset as usize;
let len  = chunk_size as usize;           // bytes
debug_assert_eq!(len % std::mem::size_of::<f32>(), 0);
let floats: &mut [f32] = unsafe {
    let p = bytes.as_mut_ptr().add(off).cast::<f32>();
    debug_assert!(p.is_aligned());
    std::slice::from_raw_parts_mut(p, len / std::mem::size_of::<f32>())
};
```

`MAP_BUFFERS` in `StreamFlags` is what makes `data.data()` non-`None` for `MemFd` buffers — without
it you get a raw fd and must `mmap` yourself.

---

## 8. Building the `SPA_TYPE_OBJECT_Format` audio/raw pod — the error-prone part

There are **two** sanctioned ways in 0.10.1. Both end in `Pod::from_bytes`.

### 8.1 `AudioInfoRaw` → `Vec<Property>` (the audio shortcut) — RECOMMENDED

```rust
#[repr(transparent)]
#[derive(PartialEq, Eq, Clone, Copy)]
pub struct AudioInfoRaw(spa_sys::spa_audio_info_raw);        // libspa/src/param/audio/raw.rs:21-23

impl AudioInfoRaw {
    pub fn new() -> Self;                                                       // raw.rs:26
    pub fn set_format(&mut self, format: AudioFormat);                          // raw.rs:36
    pub fn format(&self) -> AudioFormat;                                        // raw.rs:40
    pub fn set_flags(&mut self, flags: AudioInfoRawFlags);                      // raw.rs:44
    pub fn flags(&self) -> AudioInfoRawFlags;                                   // raw.rs:48
    pub fn set_rate(&mut self, rate: u32);                                      // raw.rs:52
    pub fn rate(&self) -> u32;                                                  // raw.rs:56
    pub fn set_channels(&mut self, channels: u32);                              // raw.rs:60
    pub fn channels(&self) -> u32;                                              // raw.rs:64
    pub fn set_position(&mut self, position: [u32; spa_sys::SPA_AUDIO_MAX_CHANNELS as usize]);
                                                                                // raw.rs:68
    pub fn position(&self) -> [u32; spa_sys::SPA_AUDIO_MAX_CHANNELS as usize];  // raw.rs:77
    pub fn parse(&mut self, format: &crate::pod::Pod) -> Result<SpaSuccess, Error>; // raw.rs:82
    pub fn from_raw(raw: spa_sys::spa_audio_info_raw) -> Self;                  // raw.rs:88
    pub fn as_raw(&self) -> spa_sys::spa_audio_info_raw;                        // raw.rs:93
}
impl Default for AudioInfoRaw { fn default() -> Self { Self::new() } }          // raw.rs:98-102
impl From<AudioInfoRaw> for Vec<Property>                                       // raw.rs:104
```

```rust
bitflags::bitflags! {
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub struct AudioInfoRawFlags: u32 {
        /// the position array explicitly contains unpositioned channels.
        const UNPOSITIONED = 1<<0;
    }
}                                                                 // param/audio/raw.rs:12-18

pub const MAX_CHANNELS: usize = spa_sys::SPA_AUDIO_MAX_CHANNELS as usize;  // param/audio/mod.rs:11
```

`SPA_AUDIO_MAX_CHANNELS` is **64** (`/usr/include/spa-0.2/spa/param/audio/raw.h:24`), so
`set_position` wants a `[u32; 64]`.

**Semantic traps in this type:**

* `AudioInfoRaw::new()` starts with `flags: AudioInfoRawFlags::UNPOSITIONED.bits()`
  (`raw.rs:29`), i.e. **unpositioned by default**.
* `set_position` clears `UNPOSITIONED` **only if `position[0] != 0`** (`raw.rs:70-74`). Since
  `SPA_AUDIO_CHANNEL_UNKNOWN == 0` (`/usr/include/spa-0.2/spa/param/audio/raw.h:154`), an all-zero
  array silently stays unpositioned. Set `position[0] = SPA_AUDIO_CHANNEL_FL`.
* The `From<AudioInfoRaw> for Vec<Property>` impl (`raw.rs:104-150`) **omits** properties that are
  at their zero/unknown value — `AudioFormat` is skipped if `Unknown` (`:116`), `rate` if `0`
  (`:123`), `channels` if `0` (`:130`), and `audio.position` only emitted if `channels != 0` **and**
  `UNPOSITIONED` is clear (`:135`). This is how the "leave rate/channels empty to accept the native
  graph rate" idiom works (`examples/audio-capture.rs:156-159`).
* The emitted keys are `SPA_FORMAT_mediaType`(Id audio), `SPA_FORMAT_mediaSubtype`(Id raw),
  `SPA_FORMAT_AUDIO_format`(Id), `SPA_FORMAT_AUDIO_rate`(**Int**, not Id),
  `SPA_FORMAT_AUDIO_channels`(Int), `SPA_FORMAT_AUDIO_position`(ValueArray of Id) — `raw.rs:107-146`.

#### Full working example — output stream, fixed S16LE 44100 stereo (`examples/tone.rs:72-102`, verbatim)

```rust
let mut audio_info = spa::param::audio::AudioInfoRaw::new();
audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
audio_info.set_rate(DEFAULT_RATE);
audio_info.set_channels(DEFAULT_CHANNELS);
let mut position = [0; spa::param::audio::MAX_CHANNELS];
position[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
position[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
audio_info.set_position(position);

let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
    std::io::Cursor::new(Vec::new()),
    &pw::spa::pod::Value::Object(pw::spa::pod::Object {
        type_: spa_sys::SPA_TYPE_OBJECT_Format,
        id: spa_sys::SPA_PARAM_EnumFormat,
        properties: audio_info.into(),
    }),
)
.unwrap()
.0
.into_inner();

let mut params = [Pod::from_bytes(&values).unwrap()];

stream.connect(
    spa::utils::Direction::Output,
    None,
    pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS,
    &mut params,
)?;
```

> In this workspace replace bare `spa_sys::` with `libspa::sys::` (see §0).

#### Same thing with the typed wrappers (`examples/audio-capture.rs:160-186`, verbatim)

```rust
let mut audio_info = spa::param::audio::AudioInfoRaw::new();
audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
let obj = pw::spa::pod::Object {
    type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
    id: pw::spa::param::ParamType::EnumFormat.as_raw(),
    properties: audio_info.into(),
};
let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
    std::io::Cursor::new(Vec::new()),
    &pw::spa::pod::Value::Object(obj),
)
.unwrap()
.0
.into_inner();

let mut params = [Pod::from_bytes(&values).unwrap()];

stream.connect(
    spa::utils::Direction::Input,
    None,
    pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS,
    &mut params,
)?;
```

Key facts you must get right:

1. `Object.type_` and `Object.id` are **`u32`**, not typed newtypes (`pod/mod.rs:1403-1410`). You
   must call `.as_raw()` on `SpaTypes::ObjectParamFormat` / `ParamType::EnumFormat`.
2. `type_` is `SPA_TYPE_OBJECT_Format` == `SpaTypes::ObjectParamFormat` (`utils/mod.rs:147`).
   **Not** `SpaTypes::Object`.
3. `id` is `SPA_PARAM_EnumFormat` == `ParamType::EnumFormat` (`param/mod.rs:27`) when *advertising*,
   and `SPA_PARAM_Format` == `ParamType::Format` (`param/mod.rs:29`) when the server tells you what
   it chose.
4. `PodSerializer::serialize` returns `Result<(O, u64), GenError>` — you want `.0.into_inner()`
   from the `Cursor`.
5. `values` must **outlive** the `params` array and the `connect` call. `Pod::from_bytes` borrows
   (`pod/mod.rs:111`). Keep the `Vec<u8>` in scope.
6. `params` must be `mut` because `connect` takes `&mut [&Pod]` (`stream/mod.rs:193`).

### 8.2 `object!` / `property!` macros (general pods)

```rust
#[macro_export]
macro_rules! __object__ {
    ($type_:expr, $id:expr, $($properties:expr),* $(,)?) => {
        $crate::pod::Object {
            type_: $type_.as_raw(),
            id: $id.as_raw(),
            properties: [ $( $properties, )* ].to_vec(),
        }
    };
}
pub use __object__ as object;                                     // libspa/src/pod/mod.rs:1438-1448
```

The macro calls `.as_raw()` for you, so pass the **typed** constants.
`property!` arms (`pod/mod.rs:1514-1608`):

| Arm | Shape |
| --- | --- |
| `property!(key, value_expr)` | raw `Value` |
| `property!(key, Id, v)` | `Value::Id(Id(v.as_raw()))` — note the extra `.as_raw()` |
| `property!(key, Int/Long/Float/.., v)` | `Value::<Ty>(v)` |
| `property!(key, Choice, Enum, Id, default, alt...)` | Id-enum choice |
| `property!(key, Choice, Enum, Ty, default, alt...)` | typed enum choice |
| `property!(key, Choice, Flags, Ty, default, flags...)` | flags choice |
| `property!(key, Choice, Step, Ty, default, min, max, step)` | step choice |
| `property!(key, Choice, Range, Ty, default, min, max)` | range choice |

Worked example (`examples/streams.rs:123-179`, video, but the structure is what matters):

```rust
let obj = pw::spa::pod::object!(
    pw::spa::utils::SpaTypes::ObjectParamFormat,
    pw::spa::param::ParamType::EnumFormat,
    pw::spa::pod::property!(
        pw::spa::param::format::FormatProperties::MediaType,
        Id,
        pw::spa::param::format::MediaType::Video
    ),
    pw::spa::pod::property!(
        pw::spa::param::format::FormatProperties::MediaSubtype,
        Id,
        pw::spa::param::format::MediaSubtype::Raw
    ),
    pw::spa::pod::property!(
        pw::spa::param::format::FormatProperties::VideoFormat,
        Choice, Enum, Id,
        pw::spa::param::video::VideoFormat::RGB,
        pw::spa::param::video::VideoFormat::RGB,
        pw::spa::param::video::VideoFormat::RGBA,
    ),
);
```

#### 🔴 The `Choice` arms only work for four types

Look again at the expansion (`pod/mod.rs:1593-1607`):

```rust
($key:expr, Choice, Range, $type_:ident, $default:expr, $min:expr, $max:expr) => {
    $crate::pod::property!(
        $key,
        $crate::pod::Value::Choice($crate::pod::ChoiceValue::$type_(
            $crate::utils::Choice::<$crate::utils::$type_>(   // <-- $crate::utils::$type_
```

`$type_` is pasted into **type position** as `libspa::utils::$type_`. `libspa::utils` exports
exactly four types usable there — `Id` (`utils/mod.rs:42`), `Fd` (`:47`), `Fraction` (`:33`) and
`Rectangle` (`:35`), plus `Point`/`Region` which no `ChoiceValue` variant accepts. There is **no
`libspa::utils::Int`, `Long`, `Float`, `Double` or `Bool`** — `SpaTypes::Int` (`utils/mod.rs:113`)
is an associated *const*, not a type.

So `ChoiceValue::Int`, `Long`, `Float`, `Double` and `Bool` are **unreachable through the macro**:

```rust
property!(FormatProperties::AudioRate, Choice, Range, Int, 48000, 44100, 192000)
// error[E0425]: cannot find type `Int` in module `$crate::utils`   <-- verified with cargo build
```

Only `Choice, Enum, Id` (its own dedicated arm, `pod/mod.rs:1531`), `Choice, …, Rectangle`,
`Choice, …, Fraction` and `Choice, …, Fd` compile. That is exactly why `examples/streams.rs` only
ever uses `Id`, `Rectangle` and `Fraction` (`:139,:153,:171`) — the video params happen to dodge it.

**Build integer choices as a plain `Property` instead.** This version is compile- and run-verified:

```rust
use libspa::pod::{object, property, ChoiceValue, Property, PropertyFlags, Value, Pod};
use libspa::param::{ParamType, format::{FormatProperties, MediaType, MediaSubtype}};
use libspa::param::audio::AudioFormat;
use libspa::utils::{Choice, ChoiceEnum, ChoiceFlags, SpaTypes};

/// The `Int` choice the `property!` macro cannot express.
fn int_range(key: FormatProperties, default: i32, min: i32, max: i32) -> Property {
    Property {
        key: key.as_raw(),                       // Property.key is a plain u32 (pod/mod.rs:1454)
        flags: PropertyFlags::empty(),
        value: Value::Choice(ChoiceValue::Int(Choice(
            ChoiceFlags::empty(),                // Choice is a 2-tuple struct (utils/mod.rs:51)
            ChoiceEnum::Range { default, min, max },
        ))),
    }
}

let obj = object!(
    SpaTypes::ObjectParamFormat,
    ParamType::EnumFormat,
    property!(FormatProperties::MediaType,     Id,  MediaType::Audio),
    property!(FormatProperties::MediaSubtype,  Id,  MediaSubtype::Raw),
    property!(FormatProperties::AudioFormat,   Id,  AudioFormat::F32LE),
    int_range(FormatProperties::AudioRate,     48_000, 44_100, 192_000),
    int_range(FormatProperties::AudioChannels, 2,      2,      8),
);

let values: Vec<u8> = libspa::pod::serialize::PodSerializer::serialize(
    std::io::Cursor::new(Vec::new()),
    &Value::Object(obj),
).unwrap().0.into_inner();
let mut params = [Pod::from_bytes(&values).unwrap()];
```

Equivalently, use the **two-argument** `property!` arm (`pod/mod.rs:1515`), which takes a
ready-made `Value` and only does `$key.as_raw()` for you — it sidesteps the `$crate::utils::$type_`
paste entirely:

```rust
property!(
    FormatProperties::AudioRate,
    Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default: 48_000, min: 44_100, max: 192_000 },
    )))
)
```

`ChoiceEnum<T>` variants, verbatim (`libspa/src/utils/mod.rs:63-102`):

```rust
pub struct Choice<T: CanonicalFixedSizedPod>(pub ChoiceFlags, pub ChoiceEnum<T>);   // :51

pub enum ChoiceEnum<T: CanonicalFixedSizedPod> {
    None(T),                                                    // :67  — tuple, not a struct variant
    Range { default: T, min: T, max: T },                       // :69-76
    Step  { default: T, min: T, max: T, step: T },              // :78-87
    Enum  { default: T, alternatives: Vec<T> },                 // :89-94
    Flags { default: T, flags: Vec<T> },                        // :96-101
}
```

`ChoiceFlags` defines **no real flags** — only a `#[doc(hidden)] const _FAKE = 1` "to keep
bitflags! happy" (`utils/mod.rs:53-61`). Always pass `ChoiceFlags::empty()`.

`AudioRate`/`AudioChannels` are **`Int`**, and `AudioFormat`/`AudioPosition` are **`Id`** — mixing
these up produces a pod the server silently rejects (`param/audio/raw.rs:118-143` confirms the
types).

> For FxSound you almost certainly do not need any of this: `AudioInfoRaw` (§8.1) emits fixed
> `Int`s for rate and channels and omits them entirely when zero, which is the idiom for "accept
> the graph's native rate". Reach for hand-built `Property` only when you genuinely need a range.

### 8.3 Parsing the negotiated format in `param_changed`

```rust
/// helper function to parse format properties type
pub fn parse_format(format: &Pod) -> Result<(MediaType, MediaSubtype), Error>;
                                                     // libspa/src/param/format_utils.rs:13
```

Verbatim from `examples/audio-capture.rs:80-109`:

```rust
.param_changed(|_, user_data, id, param| {
    // NULL means to clear the format
    let Some(param) = param else {
        return;
    };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }

    let (media_type, media_subtype) = match format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    // only accept raw audio
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return;
    }

    // call a helper function to parse the format for us.
    user_data
        .format
        .parse(param)
        .expect("Failed to parse param changed to AudioInfoRaw");

    println!(
        "capturing rate:{} channels:{}",
        user_data.format.rate(),
        user_data.format.channels()
    );
})
```

Note: `id` is a raw `u32`, so compare against `ParamType::Format.as_raw()`.

---

## 9. `libspa` reference tables

### 9.1 `AudioFormat` (`libspa/src/param/audio/mod.rs:13-94`)

```rust
#[repr(transparent)]
#[derive(PartialEq, PartialOrd, Eq, Clone, Copy)]
pub struct AudioFormat(pub spa_sys::spa_audio_format);
```

Associated consts (all `#[allow(non_upper_case_globals)]`, i.e. CamelCase names, **not** an enum):

`Unknown :19`, `Encoded :20`, `S8 :21`, `U8 :22`, `S16LE :23`, `S16BE :24`, `U16LE :25`, `U16BE :26`,
`S24_32LE :27`, `S24_32BE :28`, `U24_32LE :29`, `U24_32BE :30`, `S32LE :31`, `S32BE :32`,
`U32LE :33`, `U32BE :34`, `S24LE :35`, `S24BE :36`, `U24LE :37`, `U24BE :38`, `S20LE :39`,
`S20BE :40`, `U20LE :41`, `U20BE :42`, `S18LE :43`, `S18BE :44`, `U18LE :45`, `U18BE :46`,
`F32LE :47`, `F32BE :48`, `F64LE :49`, `F64BE :50`, `ULAW :51`, `ALAW :52`,
native-endian aliases `S16 :54`, `U16 :55`, `S18 :56`, `U18 :57`, `S20 :58`, `U20 :59`, `S24 :60`,
`U24 :61`, `S32 :62`, `U32 :63`,
planar `U8P :65`, `S16P :66`, `S24_32P :67`, `S32P :68`, `S24P :69`, `F32P :70`, `F64P :71`,
`S8P :72`.

**There is no `AudioFormat::F32`** as a distinct const — the native-endian aliases stop at `U32`
(`:63`). For native-endian f32 use `F32LE` on LE targets (what both examples do), or
`AudioFormat::from_raw(libspa::sys::SPA_AUDIO_FORMAT_F32)`.

```rust
pub fn is_interleaved(&self) -> bool;    // param/audio/mod.rs:77
pub fn is_planar(&self) -> bool;         // param/audio/mod.rs:81
pub fn from_raw(raw: spa_sys::spa_audio_format) -> Self;   // :86
pub fn as_raw(&self) -> spa_sys::spa_audio_format;         // :91
```

### 9.2 `ParamType` (`libspa/src/param/mod.rs:14-64`)

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct ParamType(pub spa_sys::spa_param_type);
```

`Invalid :21`, `PropInfo :23`, `Props :25`, `EnumFormat :27`, `Format :29`, `Buffers :31`,
`Meta :33`, `IO :35`, `EnumProfile :37`, `Profile :39`, `EnumPortConfig :41`, `PortConfig :43`,
`EnumRoute :45`, `Route :47`, `Control :49`, `Latency :51`, `ProcessLatency :53`,
`from_raw :56`, `as_raw :61`.

```rust
bitflags::bitflags! {
    pub struct ParamInfoFlags: u32 {
        const SERIAL = 1<<0; const READ = 1<<1; const WRITE = 1<<2;
        const READWRITE = Self::READ.bits() | Self::WRITE.bits();
    }
}                                                                 // param/mod.rs:81-89
#[repr(transparent)] pub struct ParamInfo(spa_sys::spa_param_info); // param/mod.rs:92-93
    pub fn id(&self) -> ParamType;          // :96
    pub fn flags(&self) -> ParamInfoFlags;  // :100
```

### 9.3 `MediaType` / `MediaSubtype` / `FormatProperties` (`libspa/src/param/format.rs`)

```rust
pub struct MediaType(pub spa_sys::spa_media_type);                // format.rs:14
//   Unknown :18  Audio :19  Video :20  Image :21  Binary :22  Stream :23  Application :24
//   from_raw :27  as_raw :32

pub struct MediaSubtype(pub spa_sys::spa_media_subtype);          // format.rs:56
//   Unknown :60  Raw :61  Dsp :62  Iec958 :64  Dsd :65  Mp3 :67  Aac :68  ... Control :107
//   is_audio :118  is_video :122  is_image :126  is_binary :130  is_stream :134
//   is_application :138  from_raw :143  as_raw :148

pub struct FormatProperties(pub spa_sys::spa_format);             // format.rs:171
//   MediaType :176        MediaSubtype :178
//   AudioFormat :181      AudioFlags :183       AudioRate :185
//   AudioChannels :187    AudioPosition :189    AudioIec958Codec :192
//   AudioBitorder :195    AudioInterleave :197  AudioBitrate :200
//   AudioBlockAlign :203  AudioAacStreamFormat :207  AudioWmaProfile :211
//   AudioAmrBandMode :215
//   VideoFormat :218 ... VideoH264Alignment :254
//   from_raw :292  as_raw :297
```

### 9.4 `SpaTypes` (`libspa/src/utils/mod.rs:104-170`)

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct SpaTypes(pub c_uint);
```

Basic: `None :110`, `Bool :111`, `Id :112`, `Int :113`, `Long :114`, `Float :115`, `Double :116`,
`String :117`, `Bytes :118`, `Rectangle :119`, `Fraction :120`, `Bitmap :121`, `Array :122`,
`Struct :123`, `Object :124`, `Sequence :125`, `Pointer :126`, `Fd :127`, `Choice :128`, `Pod :129`.
Pointers: `PointerBuffer :132`, `PointerMeta :133`, `PointerDict :134`.
Events: `EventDevice :137`, `EventNode :138`. Commands: `CommandDevice :141`, `CommandNode :142`.
**Objects:** `ObjectParamPropInfo :145`, `ObjectParamProps :146`, **`ObjectParamFormat :147`**,
`ObjectParamBuffers :148`, `ObjectParamMeta :149`, `ObjectParamIO :150`, `ObjectParamProfile :151`,
`ObjectParamPortConfig :152`, `ObjectParamRoute :153`, `ObjectProfiler :154`,
`ObjectParamLatency :155`, `ObjectParamProcessLatency :156`.
Vendor: `VendorPipeWire :159`, `VendorOther :161`. `from_raw :164`, `as_raw :169`.

### 9.5 Pod value model (`libspa/src/pod/mod.rs`)

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None, Bool(bool), Id(Id), Int(i32), Long(i64), Float(f32), Double(f64),
    String(String), Bytes(Vec<u8>), Rectangle(Rectangle), Fraction(Fraction), Fd(Fd),
    ValueArray(ValueArray), Struct(Vec<Value>), Object(Object), Choice(ChoiceValue),
    Pointer(u32, *const c_void),
}                                                                 // pod/mod.rs:1315-1351

#[derive(Debug, Clone, PartialEq)]
pub enum ValueArray {
    None(Vec<()>), Bool(Vec<bool>), Id(Vec<Id>), Int(Vec<i32>), Long(Vec<i64>),
    Float(Vec<f32>), Double(Vec<f64>), Rectangle(Vec<Rectangle>), Fraction(Vec<Fraction>),
    Fd(Vec<Fd>),
}                                                                 // pod/mod.rs:1354-1376

#[derive(Debug, Clone, PartialEq)]
pub enum ChoiceValue {
    Bool(Choice<bool>), Int(Choice<i32>), Long(Choice<i64>), Float(Choice<f32>),
    Double(Choice<f64>), Id(Choice<Id>), Rectangle(Choice<Rectangle>),
    Fraction(Choice<Fraction>), Fd(Choice<Fd>),
}                                                                 // pod/mod.rs:1379-1399

#[derive(Debug, Clone, PartialEq)]
pub struct Object { pub type_: u32, pub id: u32, pub properties: Vec<Property> }
                                                                  // pod/mod.rs:1402-1410

#[derive(Debug, Clone, PartialEq)]
pub struct Property { pub key: u32, pub flags: PropertyFlags, pub value: Value }
                                                                  // pod/mod.rs:1451-1459
impl Property { pub fn new(key: u32, value: Value) -> Self; }     // pod/mod.rs:1462

bitflags! { pub struct PropertyFlags: u32 {
    const READONLY  = spa_sys::SPA_POD_PROP_FLAG_READONLY;
    const HARDWARE  = spa_sys::SPA_POD_PROP_FLAG_HARDWARE;
    const HINT_DICT = spa_sys::SPA_POD_PROP_FLAG_HINT_DICT;
    const MANDATORY = spa_sys::SPA_POD_PROP_FLAG_MANDATORY;
    #[cfg(feature = "v0_3_33")]
    const DONT_FIXATE = spa_sys::SPA_POD_PROP_FLAG_DONT_FIXATE;
}}                                                                // pod/mod.rs:1471-1489
```

```rust
#[derive(Debug, Copy, Clone, Eq, PartialEq)] pub struct Id(pub u32);          // utils/mod.rs:42
#[derive(Debug, Copy, Clone, Eq, PartialEq)] pub struct Fd(pub i64);          // utils/mod.rs:47
pub struct Choice<T: CanonicalFixedSizedPod>(pub ChoiceFlags, pub ChoiceEnum<T>); // utils/mod.rs:51
bitflags! { pub struct ChoiceFlags: u32 { #[doc(hidden)] const _FAKE = 1; } } // utils/mod.rs:53-61
pub enum ChoiceEnum<T: CanonicalFixedSizedPod> {
    None(T),
    Range { default: T, min: T, max: T },
    Step  { default: T, min: T, max: T, step: T },
    Enum  { default: T, alternatives: Vec<T> },
    Flags { default: T, flags: Vec<T> },
}                                                                 // utils/mod.rs:63-102

pub use spa_sys::spa_fraction  as Fraction;   // utils/mod.rs:33
pub use spa_sys::spa_point     as Point;      // utils/mod.rs:34
pub use spa_sys::spa_rectangle as Rectangle;  // utils/mod.rs:35
pub use spa_sys::spa_region    as Region;     // utils/mod.rs:36
```

### 9.6 `Pod` (`libspa/src/pod/mod.rs:53-447`)

```rust
#[repr(transparent)] pub struct Pod(spa_sys::spa_pod);            // pod/mod.rs:54-55
pub unsafe fn from_raw(pod: *const spa_sys::spa_pod) -> &'static Self;     // pod/mod.rs:72
pub unsafe fn from_raw_mut(pod: *mut spa_sys::spa_pod) -> &'static mut Self; // pod/mod.rs:89
pub fn as_raw_ptr(&self) -> *mut spa_sys::spa_pod;                // pod/mod.rs:93
pub fn body(&self) -> *mut c_void;                                // pod/mod.rs:100
pub fn from_bytes(bytes: &[u8]) -> Option<&Self>;                 // pod/mod.rs:111
pub fn as_bytes(&self) -> &[u8];                                  // pod/mod.rs:138
pub fn type_(&self) -> SpaTypes;                                  // pod/mod.rs:146
pub fn size(&self) -> u32;                                        // pod/mod.rs:150
// is_*/get_* accessors :156-447 : none, bool, id, int, long, float, double, string, bytes,
//   pointer, fd, rectangle, fraction, bitmap, array, choice, struct, object, sequence
pub fn as_struct(&self) -> Result<&PodStruct, Errno>;             // pod/mod.rs:416
pub fn as_object(&self) -> Result<&PodObject, Errno>;             // pod/mod.rs:433
```

`PodObject` (`pod/mod.rs:556`): `as_raw_ptr :577`, `as_pod :581`, `type_ :586`, `id :590`,
`props :594`, `find_prop(&self, key: Id) -> Option<&PodProp> :598`, `fixate(&mut self) :610`,
`is_fixated :616`. `PodProp` (`:691`): `key :718`, `flags :722`, `value :726`.

### 9.7 Serialization (`libspa/src/pod/serialize.rs`)

```rust
pub struct PodSerializer<O: Write + Seek> { .. }                  // serialize.rs:251
impl<O: Write + Seek> PodSerializer<O> {
    pub fn serialize<P>(out: O, pod: &P) -> Result<(O, u64), GenError>
    where P: PodSerialize + ?Sized;                               // serialize.rs:268-271
    pub fn serialized_fixed_sized_pod<P>(self, pod: &P) -> Result<SerializeSuccess<O>, GenError>; // :343
    pub fn serialize_string(self, string: &str) -> Result<SerializeSuccess<O>, GenError>;         // :355
    pub fn serialize_bytes(self, bytes: &[u8]) -> Result<SerializeSuccess<O>, GenError>;          // :363
    pub fn serialize_array<P: FixedSizedPod>(..) -> ..;                                           // :368
    pub fn serialize_struct(mut self) -> Result<StructPodSerializer<O>, GenError>;                // :389
    pub fn serialize_object(..) -> ..;                                                            // :408
    pub fn serialize_choice<T: CanonicalFixedSizedPod>(..) -> ..;                                 // :432
    pub fn serialize_pointer<T>(..) -> ..;                                                        // :487
}
pub use cookie_factory::GenError;                                 // serialize.rs:19
```

`ObjectPodSerializer::serialize_property :665` / `.end() :690`;
`StructPodSerializer::serialize_field :593` / `.end() :608`;
`ArrayPodSerializer::serialize_element :533` / `.end() :546`.

### 9.8 Imperative pod `Builder` (`libspa/src/pod/builder.rs`)

An alternative to `PodSerializer` that writes into a `Vec<u8>` with an overflow callback.

```rust
pub struct Builder<'d> { .. }                                     // builder.rs:20
pub fn new(data: &'d mut Vec<u8>) -> Self;                        // builder.rs:49
pub fn as_raw(&self) -> &spa_sys::spa_pod_builder;                // builder.rs:76
pub fn as_raw_ptr(&self) -> *mut spa_sys::spa_pod_builder;        // builder.rs:80
pub unsafe fn state(&self) -> spa_sys::spa_pod_builder_state;     // builder.rs:88
pub unsafe fn reset(&mut self, state: *mut spa_sys::spa_pod_builder_state);  // builder.rs:100
pub unsafe fn push(..) -> ..;                                     // builder.rs:121
pub fn raw_padded(&mut self, data: &[u8]) -> Result<(), Errno>;   // builder.rs:132
pub unsafe fn pop(&mut self, frame: &mut spa_sys::spa_pod_frame); // builder.rs:151
pub fn add_none/add_bool/add_id/add_int/add_long/add_float/add_double(..) -> Result<(), Errno>;
                                                                  // builder.rs:159,173,185,197,209,221,233
pub fn add_string(&mut self, string: &str) -> Result<(), Errno>;  // builder.rs:251
pub fn add_bytes(&mut self, bytes: &[u8]) -> Result<(), Errno>;   // builder.rs:265
pub unsafe fn add_pointer(&mut self, type_: Id, val: *const c_void) -> Result<(), Errno>; // :286
pub fn add_fd(&mut self, val: RawFd) -> Result<(), Errno>;        // builder.rs:298
pub fn add_rectangle(&mut self, val: Rectangle) -> Result<(), Errno>;  // builder.rs:310
pub fn add_fraction(&mut self, val: Fraction) -> Result<(), Errno>;    // builder.rs:322
pub unsafe fn push_array/add_array/push_choice/push_struct/push_object/push_sequence(..);
                                                                  // :338, :355, :381, :405, :424, :460
pub fn add_prop(&mut self, key: u32, flags: u32) -> Result<(), Errno>;  // builder.rs:446
pub fn add_control(&mut self, offset: u32, type_: u32) -> c_int;        // builder.rs:475
```

Most of the composite ops are `unsafe` because you must pair `push_*` with `pop`. For declarative
format pods, prefer §8.1.

### 9.9 `SpaResult` / errors (`libspa/src/utils/result.rs`)

```rust
#[derive(Debug, Eq, PartialEq)] pub struct SpaResult(i32);        // result.rs:12-13
#[derive(PartialEq, Eq, Copy, Clone)] pub struct AsyncSeq(i32);   // result.rs:18-19
#[derive(Debug, Eq, PartialEq)] pub enum SpaSuccess { Sync(i32), Async(AsyncSeq) } // result.rs:22-28

impl AsyncSeq {
    pub fn seq(&self) -> i32;                    // result.rs:42
    pub fn raw(&self) -> i32;                    // result.rs:47
    pub fn from_seq(seq: i32) -> Self;           // result.rs:52
    pub fn from_raw(val: i32) -> Self;           // result.rs:60
}
impl SpaResult {
    pub fn from_c(res: i32) -> Self;                                  // result.rs:74
    pub fn new_return_async(seq: i32) -> Self;                        // result.rs:79
    pub fn into_result(self) -> Result<SpaSuccess, Error>;            // result.rs:89
    pub fn into_async_result(self) -> Result<AsyncSeq, Error>;        // result.rs:104  PANICS on Sync
    pub fn into_sync_result(self) -> Result<i32, Error>;              // result.rs:118  PANICS on Async
}
```

`into_async_result` **panics** if the result is a synchronous success (`result.rs:109`), and
`into_sync_result` panics on async. Use `into_result` when unsure.

```rust
#[derive(Debug)]
pub enum Error { CreationFailed, NoMemory, WrongProxyType, SpaError(spa::utils::result::Error) }
                                                                  // pipewire-0.10.1/src/error.rs:4-10
impl From<spa::utils::result::Error> for Error                     // error.rs:34
```

### 9.10 `IoFlags` (`libspa/src/support/system.rs:5-19`)

```rust
bitflags! {
    pub struct IoFlags: u32 {
        const IN  = spa_sys::SPA_IO_IN;
        const OUT = spa_sys::SPA_IO_OUT;
        const ERR = spa_sys::SPA_IO_ERR;
        const HUP = spa_sys::SPA_IO_HUP;
    }
}
```

### 9.11 `spa_interface_call_method!` (`libspa/src/utils/hook.rs:52-61`)

```rust
#[macro_export]
macro_rules! spa_interface_call_method {
    ($interface_ptr:expr, $methods_struct:ty, $method:ident, $( $arg:expr ),*) => {{
        let iface: *mut $crate::sys::spa_interface = $interface_ptr.cast();
        let funcs: *const $methods_struct = (*iface).cb.funcs.cast();
        let f = (*funcs).$method.unwrap();
        f((*iface).cb.data, $($arg),*)
    }};
}
pub fn remove(mut hook: spa_sys::spa_hook);                       // hook.rs:9
```

---

## 10. Driving the loop from a dedicated thread, and signalling it

### 10.1 The prescribed pattern

`pipewire-0.10.1/src/lib.rs:122-132` states plainly:

> The pipewire library is not really thread-safe, so pipewire objects do not implement `Send` or
> `Sync`. However, you can spawn a `MainLoop` in another thread and do bidirectional communication
> using two channels. To send messages to the main thread, we can easily use a `std::sync::mpsc`.
> Because we are stuck in the main loop in the pipewire thread and can't just block on receiving a
> message, we use a `pipewire::channel` instead.

### 10.2 `pipewire::channel` — the only sanctioned cross-thread wake-up

```rust
pub fn channel<T>() -> (Sender<T>, Receiver<T>) where T: 'static; // channel.rs:198-201

pub struct Receiver<T: 'static> { .. }                            // channel.rs:78
impl<T: 'static> Receiver<T> {
    #[must_use]
    pub fn attach<F>(self, loop_: &Loop, callback: F) -> AttachedReceiver<'_, T>
    where F: Fn(T) + 'static;                                     // channel.rs:86-90
}

pub struct AttachedReceiver<'l, T> { .. }                         // channel.rs:119
impl<'l, T> AttachedReceiver<'l, T> {
    #[must_use] pub fn deattach(self) -> Receiver<T>;             // channel.rs:134-135
}

pub struct Sender<T> { .. }                                       // channel.rs:143
impl<T> Clone for Sender<T>                                       // channel.rs:147
impl<T> Sender<T> {
    /// On any errors, this returns the message back to the caller.
    pub fn send(&self, t: T) -> Result<(), T>;                    // channel.rs:159
}
```

Implementation facts that matter:

* It is a `pipe(2)` + `Arc<Mutex<VecDeque<T>>>` (`channel.rs:202-208`, `:183-189`).
* `attach` registers the read end as an `IoSource` with `IoFlags::IN` (`channel.rs:100`), so the
  loop wakes on write.
* `Sender::send` only writes the wake byte **when the queue was empty** (`channel.rs:168-173`) —
  cheap for bursts.
* `Sender<T>` is `Send` as long as `T: Send` (it is an `Arc<Mutex<..>>`); the `Receiver` is not.
* Note the typo in the public API: **`deattach`**, not `detach` (`channel.rs:135`).

### 10.3 Full working thread pattern (from `channel.rs:14-63`, verbatim)

```rust
use std::{time::Duration, sync::mpsc, thread};
use pipewire::main_loop::MainLoopRc;

struct Terminate;

fn main() {
    let (main_sender, main_receiver) = mpsc::channel();
    let (pw_sender, pw_receiver) = pipewire::channel::channel();

    let pw_thread = thread::spawn(move || pw_thread(main_sender, pw_receiver));

    let mut n = 0;
    while n < 3 {
        println!("{}", main_receiver.recv().unwrap());
        n += 1;
    }

    pw_sender.send(Terminate);
    pw_thread.join();
}

fn pw_thread(
    main_sender: mpsc::Sender<String>,
    pw_receiver: pipewire::channel::Receiver<Terminate>
) {
    let mainloop = MainLoopRc::new(None).expect("Failed to create main loop");

    let _receiver = pw_receiver.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let timer = mainloop.loop_().add_timer(move |_| {
        main_sender.send(String::from("Hello"));
    });
    timer.update_timer(
        Some(Duration::from_millis(1)),
        Some(Duration::from_millis(100))
    );

    mainloop.run();
}
```

**Keep `_receiver` and `timer` alive.** Both `attach` and `add_timer` are `#[must_use]`; dropping
them detaches the source and your loop goes deaf.

### 10.4 `EventSource::signal` — the lighter wake-up

If you only need "wake up and run this closure" with no payload:

```rust
let ev = mainloop.loop_().add_event(move || { /* on loop thread */ });   // loop_/mod.rs:286
// later, from the loop thread only:
ev.signal();                                                            // loop_/mod.rs:529
```

`EventSource` is **not** `Send`, so this is an intra-thread wake-up, not a cross-thread one. For
cross-thread, use `pipewire::channel`.

### 10.5 `ThreadLoop` — PipeWire's own worker-thread wrapper

```rust
#[repr(transparent)] pub struct ThreadLoop(pw_sys::pw_thread_loop);  // thread_loop/mod.rs:16-17
impl ThreadLoop {
    pub fn as_raw(&self) -> &pw_sys::pw_thread_loop;                 // thread_loop/mod.rs:20
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_thread_loop;         // thread_loop/mod.rs:24
    pub fn loop_(&self) -> &Loop;                                    // thread_loop/mod.rs:28
    pub fn lock(&self) -> ThreadLoopLockGuard<'_>;                   // thread_loop/mod.rs:46
    pub fn start(&self);                                             // thread_loop/mod.rs:51
    pub fn stop(&self);                                              // thread_loop/mod.rs:60
    pub fn signal(&self, signal: bool);                              // thread_loop/mod.rs:67
    pub fn wait(&self);                                              // thread_loop/mod.rs:76
    pub fn timed_wait(&self, wait_max_sec: std::time::Duration);     // thread_loop/mod.rs:84
    pub fn get_time(&self, timeout: i64) -> rustix::time::Timespec;  // thread_loop/mod.rs:95
    pub fn timed_wait_full(&self, abstime: rustix::time::Timespec);  // thread_loop/mod.rs:113
    pub fn accept(&self);                                            // thread_loop/mod.rs:138
    pub fn in_thread(&self);                                         // thread_loop/mod.rs:145
}
pub struct ThreadLoopLockGuard<'a> { .. }                            // thread_loop/mod.rs:152
impl<'a> ThreadLoopLockGuard<'a> { pub fn unlock(self); }            // thread_loop/mod.rs:167
impl<'a> Drop for ThreadLoopLockGuard<'a>                            // thread_loop/mod.rs:172

pub struct ThreadLoopBox { .. }
    pub unsafe fn new(name: Option<&str>, properties: Option<&DictRef>) -> Result<Self, Error>;
                                                                     // thread_loop/box_.rs:24-27
    pub unsafe fn new_cstr(name: Option<&CStr>, properties: Option<&DictRef>) -> Result<Self, Error>;
                                                                     // thread_loop/box_.rs:38-41
pub struct ThreadLoopRc { .. }
    pub unsafe fn new(..) -> Result<Self, Error>;                    // thread_loop/rc.rs:33
    pub unsafe fn new_cstr(..) -> Result<Self, Error>;               // thread_loop/rc.rs:47
    pub unsafe fn from_box(thread_loop: ThreadLoopBox) -> Self;      // thread_loop/rc.rs:60
    pub unsafe fn from_raw(ptr: NonNull<pw_sys::pw_thread_loop>) -> Self; // thread_loop/rc.rs:72
    pub fn downgrade(&self) -> ThreadLoopWeak;                       // thread_loop/rc.rs:80
unsafe impl IsLoopRc for ThreadLoopRc {}                             // thread_loop/rc.rs:88
```

**Every `ThreadLoop*::new` is `unsafe`, with the doc comment literally `# Safety / TODO`**
(`thread_loop/box_.rs:22-23`, `thread_loop/rc.rs:31-32`). `ThreadLoopRc` is still `Rc`-based and not
`Send`, so you cannot construct it on one thread and hand it to another safely.

**Recommendation for this project: do not use `ThreadLoop`.** Spawn a plain
`std::thread`, build `MainLoopRc` *inside* it, and talk to it over `pipewire::channel`. That is what
the crate documents (`lib.rs:122-132`) and what §10.3 shows. The `pw_thread_loop` locking model
(`lock()` guards around every call from another thread, `thread_loop/mod.rs:36-48`) buys you nothing
here because none of the Rust wrappers are `Send` anyway.

---

## 11. `keys` — the property-name constants

```rust
macro_rules! key_constant {
    ($name:ident, $pw_symbol:ident, #[doc = $doc:expr]) => {
        #[doc = $doc]
        pub static $name: LazyLock<&'static str> = LazyLock::new(|| unsafe {
            CStr::from_bytes_with_nul_unchecked($crate::sys::$pw_symbol)
                .to_str()
                .unwrap()
        });
    };
}                                                                 // keys.rs:18-27
```

They are `LazyLock<&'static str>`, so **you must deref with `*`**: `*pw::keys::NODE_NAME`.

> 🔴 **Twenty of these constants are `#[cfg(feature = ...)]`-gated and do not exist with default
> features** — `NODE_LINK_GROUP`, `TARGET_OBJECT`, `NODE_WANT_DRIVER`, `NODE_RATE`, `AUDIO_RATE`,
> `OBJECT_SERIAL` and fourteen more. The complete gate list is in **§1.1**; read it before using
> this table. The *strings* below are always correct regardless of features — when in doubt, write
> the literal.

Constants defined in `pipewire-0.10.1/src/keys.rs` that matter for audio work, with their **actual
string values** (from `/usr/include/pipewire-0.3/pipewire/keys.h`, which is what bindgen reads):

| Rust constant | `keys.rs` | String | `keys.h` |
| --- | --- | --- | --- |
| `MEDIA_TYPE` | :454 | `media.type` | :343 |
| `MEDIA_CATEGORY` | :457 | `media.category` | :345 |
| `MEDIA_ROLE` | :460 | `media.role` | :347 |
| `MEDIA_CLASS` | :463 | `media.class` | :351 |
| `MEDIA_NAME` | :466 | `media.name` | :352 |
| `NODE_ID` | :180 | `node.id` | :144 |
| `NODE_NAME` | :183 | `node.name` | :145 |
| `NODE_NICK` | :186 | `node.nick` | :146 |
| `NODE_DESCRIPTION` | :189 | `node.description` | :147 |
| `NODE_GROUP` | :198 | `node.group` | :153 |
| `NODE_EXCLUSIVE` | :201 | `node.exclusive` | :163 |
| `NODE_AUTOCONNECT` | :204 | `node.autoconnect` | :164 |
| `NODE_LATENCY` | :207 | `node.latency` | :166 |
| `NODE_MAX_LATENCY` | :210 | `node.max-latency` | :168 |
| `NODE_LOCK_QUANTUM` | :214 | `node.lock-quantum` | :170 |
| `NODE_FORCE_QUANTUM` | :218 | `node.force-quantum` | :172 |
| `NODE_RATE` | :222 | `node.rate` | :174 |
| `NODE_LOCK_RATE` | :226 | `node.lock-rate` | :176 |
| `NODE_FORCE_RATE` | :230 | `node.force-rate` | :178 |
| `NODE_DONT_RECONNECT` | :233 | `node.dont-reconnect` | :182 |
| `NODE_ALWAYS_PROCESS` | :236 | `node.always-process` | :186 |
| `NODE_WANT_DRIVER` | :240 | `node.want-driver` | :187 |
| `NODE_PAUSE_ON_IDLE` | :243 | `node.pause-on-idle` | :189 |
| `NODE_SUSPEND_ON_IDLE` | :247 | `node.suspend-on-idle` | :190 |
| `NODE_DRIVER` | :257 | `node.driver` | :193 |
| `NODE_STREAM` | :260 | `node.stream` | :210 |
| `NODE_VIRTUAL` | :263 | `node.virtual` | :212 |
| `NODE_PASSIVE` | :266 | `node.passive` | :214 |
| `NODE_LINK_GROUP` | :270 | `node.link-group` | :217 |
| `NODE_NETWORK` | :274 | `node.network` | :220 |
| `NODE_TRIGGER` | :278 | `node.trigger` | :221 |
| `NODE_CHANNELNAMES` | :282 | `node.channel-names` | :224 |
| `PORT_MONITOR` | :306 | `port.monitor` | :246 |
| `STREAM_MONITOR` | :445 | `stream.monitor` | :332 |
| `STREAM_DONT_REMIX` | :448 | `stream.dont-remix` | :338 |
| `STREAM_CAPTURE_SINK` | :451 | `stream.capture.sink` | :339 |
| `STREAM_IS_LIVE` | :436 | `stream.is-live` | — |
| `STREAM_LATENCY_MIN` / `_MAX` | :439 / :442 | `stream.latency.min` / `.max` | — |
| `AUDIO_CHANNEL` | :505 | `audio.channel` | :372 |
| `AUDIO_RATE` | :509 | `audio.rate` | :373 |
| `AUDIO_CHANNELS` | :512 | `audio.channels` | :374 |
| `AUDIO_FORMAT` | :515 | `audio.format` | :375 |
| `AUDIO_ALLOWED_RATES` | :519 | `audio.allowed-rates` | :376 |
| `TARGET_OBJECT` | :532 | `target.object` | :384 |
| `OBJECT_SERIAL` | :66 | `object.serial` | :57 |
| `OBJECT_LINGER` | :69 | `object.linger` | :61 |
| `FACTORY_NAME` | :424 | `factory.name` | :323 |
| `APP_NAME` | :132 | `application.name` | :115 |
| `APP_ID` | :135 | `application.id` | :116 |
| `DEVICE_API` | :351 | `device.api` | :278 |
| `DEVICE_DESCRIPTION` | :354 | `device.description` | — |
| `DEVICE_FORM_FACTOR` | :378 | `device.form-factor` | — |
| `DEVICE_ICON_NAME` | :394 | `device.icon-name` | — |
| `DEVICE_BUS` | :381 | `device.bus` | — |
| `REMOTE_NAME` | (used at `examples/pw-mon.rs:80`) | `remote.name` | — |

### ⚠️ Keys with **no** Rust constant — you must use the literal string

Verified absent from `pipewire-0.10.1/src/keys.rs` (the AUDIO_* block is `:505-519` and stops at
`AUDIO_ALLOWED_RATES`; the NODE_* block is `:180-282`):

| String | Why you need it |
| --- | --- |
| `audio.position` | channel map, e.g. `"FL,FR"` — **no `PW_KEY_AUDIO_POSITION` exists in `keys.h` either**; it is an adapter/module argument. Confirmed in use at `/usr/share/pipewire/pipewire.conf:313,335` and `/usr/share/pipewire/minimal.conf:391`. |
| `device.class` | `"sound"` grouping hint |
| `priority.session` | policy preference for becoming default |
| `priority.driver` | driver election weight |
| `monitor.channel-volumes` | null-sink monitor behaviour |
| `monitor.passthrough` | `/usr/share/pipewire/pipewire.conf:314` |
| `node.sync-group` | `PW_KEY_NODE_SYNC_GROUP`, `keys.h:157` — present in C, **absent from Rust `keys.rs`** |
| `node.supports-lazy`, `node.supports-request`, `node.async`, `node.loop.name`, `node.loop.class`, `node.driver-id` | `keys.h:196-209` — all absent from Rust `keys.rs` |
| `node.hidden` | pavucontrol hiding |
| `media.icon-name`, `application.icon-name` | icon lookup |
| `resample.quality`, `resample.disable`, `channelmix.*` | adapter tuning, `/usr/share/pipewire/minimal.conf:392-400` |

Write these as plain `&str` — `properties!` accepts `impl Into<Vec<u8>>` for the key
(`properties/mod.rs:69`), so string literals work fine:

```rust
let props = properties! {
    *pw::keys::MEDIA_CLASS => "Audio/Sink",
    "audio.position"       => "FL,FR",      // no constant; literal is correct
    "priority.session"     => "1010",
};
```

### Channel-position ids

`audio.position` as a *property string* uses names (`"FL,FR"`). The *pod* `SPA_FORMAT_AUDIO_position`
uses `enum spa_audio_channel` ids. Verified ordering
(`/usr/include/spa-0.2/spa/param/audio/raw.h:153-194`, values are sequential from 0):

`UNKNOWN=0, NA=1, MONO=2, FL=3, FR=4, FC=5, LFE=6, SL=7, SR=8, FLC=9, FRC=10, RC=11, RL=12, RR=13,
TC=14, TFL=15, TFC=16, TFR=17, TRL=18, TRC=19, TRR=20, RLC=21, RRC=22, FLW=23, FRW=24, LFE2=25,
FLH=26, FCH=27, FRH=28, TFLC=29, TFRC=30, TSL=31, TSR=32, LLFE=33, RLFE=34, BC=35, BLC=36, BRC=37`,
then `AUX0 = SPA_AUDIO_CHANNEL_START_Aux = 0x1000` (`raw.h:196-197`).

Access from Rust as `libspa::sys::SPA_AUDIO_CHANNEL_FL` etc. — **there is no `libspa::param::audio::AudioChannel`
wrapper type in 0.10.1.** Only raw `u32`s.

---

## 12. The Filter API — **not in this crate**

**`pipewire-0.10.1` has no `filter` module.** Verified:

* `ls pipewire-0.10.1/src/` → no `filter.rs`, no `filter/`.
* `grep -ri filter pipewire-0.10.1/src/` returns only four unrelated comment hits
  (`port.rs:70`, `device.rs:68`, `node.rs:73` "FIXME: Add filter parameter" and
  `stream/mod.rs:92` "including all filters on the path").
* `pipewire-0.10.1/Cargo.toml:80-102` lists six examples; none is a filter example.

However, **the raw C API is reachable** through `pw_sys`: `pipewire-sys-0.10.1/wrapper.h` includes
`<pipewire/pipewire.h>`, which includes `<pipewire/filter.h>`
(`/usr/include/pipewire-0.3/pipewire/pipewire.h:34`), and `pipewire-sys-0.10.1/build.rs:29-32`
allowlists `pw_.*` functions, types and vars. So `pw_sys::pw_filter_new` etc. exist, entirely
unsafe and unwrapped.

The raw C surface, for reference (`/usr/include/pipewire-0.3/pipewire/filter.h`):

```c
struct pw_filter *pw_filter_new(struct pw_core *core, const char *name,
                                struct pw_properties *props);                       // :137-140
struct pw_filter *pw_filter_new_simple(struct pw_loop *loop, const char *name,
        struct pw_properties *props, const struct pw_filter_events *events, void *data); // :142-147
void pw_filter_destroy(struct pw_filter *filter);                                   // :150
void pw_filter_add_listener(struct pw_filter *filter, struct spa_hook *listener,
        const struct pw_filter_events *events, void *data);                         // :152-155
int pw_filter_connect(struct pw_filter *filter, enum pw_filter_flags flags,
        const struct spa_pod **params, uint32_t n_params);                          // :170-174
void *pw_filter_add_port(struct pw_filter *filter, enum pw_direction direction,
        enum pw_filter_port_flags flags, size_t port_data_size,
        struct pw_properties *props, const struct spa_pod **params, uint32_t n_params); // :185-192
int pw_filter_remove_port(void *port_data);                                         // :195
struct pw_buffer *pw_filter_dequeue_buffer(void *port_data);                        // :235
int pw_filter_queue_buffer(void *port_data, struct pw_buffer *buffer);              // :238
void *pw_filter_get_dsp_buffer(void *port_data, uint32_t n_samples);                // :241
uint32_t pw_filter_get_node_id(struct pw_filter *filter);                           // :178-179
int pw_filter_set_active(struct pw_filter *filter, bool active);                    // :244
bool pw_filter_is_driving(struct pw_filter *filter);                                // :258
int pw_filter_trigger_process(struct pw_filter *filter);                            // :266
struct pw_loop *pw_filter_get_data_loop(struct pw_filter *filter);                  // :231
```

```c
enum pw_filter_flags {                                                              // :100-124
    PW_FILTER_FLAG_NONE = 0,
    PW_FILTER_FLAG_INACTIVE        = (1 << 0),
    PW_FILTER_FLAG_DRIVER          = (1 << 1),
    PW_FILTER_FLAG_RT_PROCESS      = (1 << 2),
    PW_FILTER_FLAG_CUSTOM_LATENCY  = (1 << 3),
    PW_FILTER_FLAG_TRIGGER         = (1 << 4),
    PW_FILTER_FLAG_ASYNC           = (1 << 5),
};
enum pw_filter_port_flags {                                                         // :126-133
    PW_FILTER_PORT_FLAG_NONE          = 0,
    PW_FILTER_PORT_FLAG_MAP_BUFFERS   = (1 << 0),
    PW_FILTER_PORT_FLAG_ALLOC_BUFFERS = (1 << 1),
};
```

### How Filter differs from Stream (structurally)

| | `pw_stream` (bound in Rust) | `pw_filter` (C only) |
| --- | --- | --- |
| Ports | **One** implicit port; direction fixed at `connect()` | **N** ports, added individually with `pw_filter_add_port` (`filter.h:185`), each with its own direction, props and params |
| Buffer handle | `stream.dequeue_buffer()` → `Buffer` bound to the stream | `pw_filter_dequeue_buffer(port_data)` — keyed on the **port's** user data pointer, not the filter |
| Sample access | `Data::data()` → `&mut [u8]`, you deinterleave | `pw_filter_get_dsp_buffer(port_data, n_samples)` → `void*` — already one **planar DSP channel** |
| `process` signature | `FnMut(&Stream, &mut D)` (`stream/mod.rs:406`) | `void (*process)(void *data, struct spa_io_position *position)` (`filter.h:87`) — you get the position/quantum directly |
| `param_changed` | `(&Stream, &mut D, u32, Option<&Pod>)` (`stream/mod.rs:405`) | `(void *data, void *port_data, uint32_t id, const struct spa_pod *param)` (`filter.h:74`) — extra `port_data` to say *which port* |
| Format | Full `audio/raw` negotiation | Typically `SPA_MEDIA_SUBTYPE_dsp` + `SPA_AUDIO_FORMAT_DSP_F32` per port; no channel map |
| Connect | `direction` + `target id` + flags + params | flags + params only; targeting is via properties |

### Which should this project use?

**Two `pw_stream`s, not one `pw_filter`.** Reasons, all mechanical:

1. **`pw_filter` is unbound.** Using it means hand-writing `extern "C"` trampolines, a
   `pw_filter_events` vtable with the right `PW_VERSION_FILTER_EVENTS` (`filter.h:61`), manual
   `spa_hook` lifetime management, and `Box::into_raw`/`from_raw` for every port's user data —
   reproducing ~500 lines of `stream/mod.rs:433-630` with none of the safety. For a module
   `CLAUDE.md` flags as the highest-risk in the tree, that is the wrong trade.
2. **A filter node is one node, so it cannot be both an `Audio/Sink` and a targetable playback
   stream.** `module-filter-chain` gets around this by internally creating *two* nodes joined by
   `node.link-group` — exactly the topology `docs/spec/12-audio-io.md:1029-1075` already specifies.
   With `pw_stream` you build that topology explicitly and legibly.
3. **`pw_filter`'s DSP ports are planar mono `F32`.** FxSound's `DfxDsp::processAudio` takes
   interleaved (`docs/spec/12-audio-io.md:1240-ish`, "interleaved `F32` is the simpler port"), so a
   filter would add a deinterleave/reinterleave on every quantum.
4. **`Stream::connect` gives you `Direction` + target id + `AUTOCONNECT` in one call**
   (`stream/mod.rs:188`); `pw_filter_connect` has no direction and no target (`filter.h:170`), so
   targeting the user's chosen sink must go through properties anyway — losing the one thing a
   filter would have bought you.

The **only** thing you give up is `spa_io_position` in `process()`. Recover it via
`Stream::time()` (`stream/mod.rs:382`, documented RT-safe) or the `io_changed` callback
(`stream/mod.rs:743`) with `id == SPA_IO_Position`.

---

## 13. Concretely: virtual sink + capture + render, in one process

> **Both property sets in this section were compiled and run** against `pipewire`/`libspa` 0.10.1
> with `features = ["v0_3_65"]` (§1.1). At the workspace's current feature level they do **not**
> compile: `NODE_LINK_GROUP`, `NODE_WANT_DRIVER` and `TARGET_OBJECT` are all gated off.

### (a) Create a virtual sink other apps can play into

A `pw_stream` with `Direction::Input` and `media.class = "Audio/Sink"` **is** a sink node. You do
not need `module-null-sink`, `pactl`, or a factory call. The node disappears when your process
exits, because it is owned by your `pw_core` connection.

```rust
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

let sink_props = properties! {
    *pw::keys::MEDIA_CLASS      => "Audio/Sink",
    *pw::keys::NODE_NAME        => "fxsound_sink",
    *pw::keys::NODE_DESCRIPTION => "FxSound",
    *pw::keys::NODE_NICK        => "FxSound",
    *pw::keys::NODE_VIRTUAL     => "true",
    *pw::keys::NODE_LINK_GROUP  => "fxsound",     // MANDATORY, see (c)
    *pw::keys::MEDIA_TYPE       => "Audio",
    *pw::keys::AUDIO_CHANNELS   => "2",
    *pw::keys::NODE_WANT_DRIVER => "true",
    *pw::keys::NODE_LATENCY     => "1024/48000",
    "audio.position"            => "FL,FR",       // no constant exists
    "device.class"              => "sound",
    "priority.session"          => "1010",
    "monitor.channel-volumes"   => "false",
};

let sink = pw::stream::StreamBox::new(&core, "fxsound_sink", sink_props)?;
```

Then declare the format and connect as `Input` (§8.1 for the pod; `examples/audio-capture.rs:179-186`
for the call shape):

```rust
sink.connect(
    spa::utils::Direction::Input,           // a sink receives audio
    None,                                   // ID_ANY; targeting is via properties
    pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
    &mut params,
)?;
```

Note: **omit `AUTOCONNECT` on the sink.** A sink should not autoconnect anywhere; WirePlumber links
*clients* to it. `AUTOCONNECT` is for the output stream in (c).

Making it the *default* sink is a separate act — write `default.audio.sink` into the `default`
metadata object via `metadata::Metadata::set_property` (`metadata.rs:61`), obtained by
`registry.bind::<Metadata, _>(&global)` where `global.type_ == ObjectType::Metadata` and
`global.props.get("metadata.name") == Some("default")`. See `docs/spec/12-audio-io.md:1347`.

### (b) Capture what is written into it

There is nothing extra to do. Because the sink node is *your* `pw_stream`, the audio apps write
arrives directly in your `process()` callback — **there is no monitor hop, no second stream, no
`stream.capture.sink`.** That property (`keys.rs:451`, `stream.capture.sink`) is only needed when
you are a *separate* capture client wanting to read someone else's sink
(`examples/audio-capture.rs:73-74`).

```rust
struct SinkData {
    format: spa::param::audio::AudioInfoRaw,
    tx: rtrb::Producer<f32>,      // lock-free SPSC to the output stream
    dsp: DspHandle,
}

let _sink_listener = sink
    .add_local_listener_with_user_data(sink_data)
    .param_changed(|_stream, ud, id, param| {
        let Some(param) = param else { return };
        if id != spa::param::ParamType::Format.as_raw() { return; }
        let Ok((mt, ms)) = spa::param::format_utils::parse_format(param) else { return };
        if mt != spa::param::format::MediaType::Audio
            || ms != spa::param::format::MediaSubtype::Raw { return; }
        ud.format.parse(param).expect("bad Format pod");
    })
    .process(|stream, ud| {
        let Some(mut buffer) = stream.dequeue_buffer() else { return }; // underrun: bail
        let datas = buffer.datas_mut();
        if datas.is_empty() { return; }
        let d = &mut datas[0];

        let off = d.chunk().offset() as usize;
        let len = d.chunk().size()   as usize;   // BYTES, and NOT slice.len()
        let Some(bytes) = d.data() else { return };
        // ... reinterpret bytes[off..off+len] as &mut [f32] (see §7), run DSP in place,
        // ... push into ud.tx
        // buffer is queued back automatically on drop
    })
    .register()?;
```

### (c) Render to the user's chosen real output device

A second `pw_stream`, `Direction::Output`, targeted by name:

```rust
let mut out_props = properties! {
    *pw::keys::MEDIA_CLASS       => "Stream/Output/Audio",
    *pw::keys::MEDIA_TYPE        => "Audio",
    *pw::keys::MEDIA_CATEGORY    => "Playback",
    *pw::keys::MEDIA_ROLE        => "Production",
    *pw::keys::NODE_NAME         => "fxsound_output",
    *pw::keys::NODE_DESCRIPTION  => "FxSound output",
    *pw::keys::NODE_LINK_GROUP   => "fxsound",        // SAME group as the sink
    *pw::keys::NODE_AUTOCONNECT  => "true",
    *pw::keys::NODE_DONT_RECONNECT => "false",
    *pw::keys::NODE_PASSIVE      => "false",
    *pw::keys::NODE_LATENCY      => "1024/48000",
    *pw::keys::STREAM_DONT_REMIX => "false",
};
// Target the chosen sink by node.name (the modern replacement for the deprecated node.target)
out_props.insert(*pw::keys::TARGET_OBJECT, chosen_sink_node_name);

let out = pw::stream::StreamBox::new(&core, "fxsound_output", out_props)?;
out.connect(
    spa::utils::Direction::Output,
    None,
    pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS,
    &mut out_params,
)?;
```

Its `process()` pops from the ring and writes, setting the chunk (`examples/tone.rs:64-67`):

```rust
.process(|stream, ud| {
    let Some(mut buffer) = stream.dequeue_buffer() else { return };
    let datas = buffer.datas_mut();
    if datas.is_empty() { return; }
    let d = &mut datas[0];
    let stride = std::mem::size_of::<f32>() * ud.channels as usize;

    let n_frames = match d.data() {
        Some(slice) => {
            let n = slice.len() / stride;
            // pop n*channels f32 from the ring into slice; zero-fill on underrun
            n
        }
        None => 0,
    };

    let chunk = d.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as _;
    *chunk.size_mut()   = (stride * n_frames) as _;
})
```

**Discovering the user's real sinks** — registry walk, `examples/pw-mon.rs:105+` and
`examples/create-delete-remote-objects.rs:24-39` for the shape:

```rust
let _reg_listener = registry
    .add_listener_local()
    .global(move |global| {
        if global.type_ != pw::types::ObjectType::Node { return; }
        let Some(props) = global.props else { return };
        if props.get("media.class") != Some("Audio/Sink") { return; }
        let name  = props.get("node.name").unwrap_or_default();
        let descr = props.get("node.description").unwrap_or(name);
        // ... build SoundDevice { id: name.into(), object_id: global.id, .. }
    })
    .global_remove(move |id| { /* drop it from the list */ })
    .register();
```

`props` is `Option<&DictRef>` (`registry/mod.rs:294`), and `DictRef::get` returns `Option<&str>`
(`libspa/src/utils/dict.rs:90`). Use `GlobalObject::to_owned()` (`registry/mod.rs:321`) if you need
to keep the props past the callback.

### Two streams vs. one filter — the verdict

| | Two `pw_stream`s (RECOMMENDED) | One `pw_filter` |
| --- | --- | --- |
| Rust binding | Fully safe, `stream/mod.rs` | **None.** Raw FFI only, §12 |
| Node topology | 2 nodes, joined by `node.link-group` — same as `module-filter-chain` does internally | 1 node with N ports |
| Can be an `Audio/Sink` **and** target a chosen real device | Yes — that is what the two nodes are for | No; one node cannot be both |
| Sample layout | interleaved `F32` — matches `DfxDsp::processAudio` | planar DSP `F32` per port; needs de/re-interleave |
| Extra cost | one lock-free ring between the two `process()`es | none |
| `spa_io_position` in `process()` | not passed; use `Stream::time()` (`stream/mod.rs:382`) or `io_changed` (`:743`) | passed directly |

**Use two streams.** The `node.link-group = "fxsound"` on both is not optional: it is how
WirePlumber's policy learns the two nodes are one logical device and refuses to link
`fxsound_output` back into `fxsound_sink` — without it, the instant your sink becomes the default,
your own output stream autoconnects to your own sink and you get a feedback loop.

---

## 14. Node property strings — the complete list, correct spelling

All strings verified against `/usr/include/pipewire-0.3/pipewire/keys.h` and the shipped
`/usr/share/pipewire/*.conf`. "Const?" = whether `pipewire::keys` exposes it in 0.10.1.

> 🔴 A "Const?" entry means the constant exists **in the source**. Several of them are still
> `#[cfg]`-gated off at the workspace's current feature level — `NODE_LINK_GROUP`,
> `NODE_WANT_DRIVER`, `NODE_RATE`, `NODE_FORCE_QUANTUM`, `NODE_LOCK_QUANTUM`, `NODE_FORCE_RATE`,
> `NODE_SUSPEND_ON_IDLE`, `NODE_CHANNELNAMES`, `AUDIO_RATE`, `AUDIO_ALLOWED_RATES`,
> `TARGET_OBJECT`, `OBJECT_SERIAL`. Apply §1.1's feature line, or use the literal string in the
> first column. **The strings in column 1 are always valid.**

### Virtual sink (NODE 1)

| Property string | Const? | Value for FxSound | Cite |
| --- | --- | --- | --- |
| `media.class` | `*keys::MEDIA_CLASS` | `"Audio/Sink"` | keys.h:351; conf example minimal.conf:373 |
| `node.name` | `*keys::NODE_NAME` | `"fxsound_sink"` | keys.h:145 |
| `node.description` | `*keys::NODE_DESCRIPTION` | `"FxSound"` | keys.h:147 |
| `node.nick` | `*keys::NODE_NICK` | `"FxSound"` | keys.h:146 |
| `node.virtual` | `*keys::NODE_VIRTUAL` | `"true"` | keys.h:212 |
| `node.link-group` | `*keys::NODE_LINK_GROUP` | `"fxsound"` | keys.h:217 |
| `node.want-driver` | `*keys::NODE_WANT_DRIVER` | `"true"` | keys.h:187 |
| `node.always-process` | `*keys::NODE_ALWAYS_PROCESS` | `"false"` | keys.h:186 |
| `node.latency` | `*keys::NODE_LATENCY` | `"1024/48000"` (a **fraction**) | keys.h:166-167 |
| `node.rate` | `*keys::NODE_RATE` | `"1/48000"` (a **fraction**, `1/rate`) | keys.h:174-175 |
| `node.max-latency` | `*keys::NODE_MAX_LATENCY` | `"2048/48000"` | keys.h:168 |
| `node.force-quantum` | `*keys::NODE_FORCE_QUANTUM` | avoid unless pinned | keys.h:172 |
| `node.lock-quantum` | `*keys::NODE_LOCK_QUANTUM` | avoid | keys.h:170 |
| `node.force-rate` | `*keys::NODE_FORCE_RATE` | avoid | keys.h:178 |
| `node.suspend-on-idle` | `*keys::NODE_SUSPEND_ON_IDLE` | `"false"` | keys.h:190 |
| `node.pause-on-idle` | `*keys::NODE_PAUSE_ON_IDLE` | `"false"` | keys.h:189 |
| `audio.channels` | `*keys::AUDIO_CHANNELS` | `"2"` (clamp 2..=8) | keys.h:374 |
| `audio.rate` | `*keys::AUDIO_RATE` | `"48000"` | keys.h:373 |
| `audio.format` | `*keys::AUDIO_FORMAT` | `"F32"` | keys.h:375 |
| **`audio.position`** | **no const** | `"FL,FR"` / `"FL,FR,FC,LFE,RL,RR"` | pipewire.conf:313,335 |
| `media.type` | `*keys::MEDIA_TYPE` | `"Audio"` | keys.h:343 |
| **`device.class`** | no const | `"sound"` | — |
| **`priority.session`** | no const | `"1010"` | — |
| **`priority.driver`** | no const | `"0"` | conf uses `priority.driver` at pipewire.conf:286 |
| **`monitor.channel-volumes`** | no const | `"false"` | minimal.conf:327 |
| **`monitor.passthrough`** | no const | `"true"` (if you expose a monitor) | pipewire.conf:314 |
| **`media.icon-name`** / **`application.icon-name`** | no const | `"fxsound"` | — |
| **`node.hidden`** | no const | `"true"` (optional) | — |

### Output stream (NODE 2)

| Property string | Const? | Value | Cite |
| --- | --- | --- | --- |
| `media.class` | `*keys::MEDIA_CLASS` | `"Stream/Output/Audio"` | keys.h:351 |
| `media.category` | `*keys::MEDIA_CATEGORY` | `"Playback"` | keys.h:345 |
| `media.role` | `*keys::MEDIA_ROLE` | `"Production"` | keys.h:347-350 (valid roles listed there) |
| `media.name` | `*keys::MEDIA_NAME` | `"FxSound"` | keys.h:352 |
| `node.name` | `*keys::NODE_NAME` | `"fxsound_output"` | keys.h:145 |
| `node.description` | `*keys::NODE_DESCRIPTION` | `"FxSound output"` | keys.h:147 |
| `node.link-group` | `*keys::NODE_LINK_GROUP` | `"fxsound"` (same as NODE 1) | keys.h:217 |
| `node.autoconnect` | `*keys::NODE_AUTOCONNECT` | `"true"` | keys.h:164 |
| **`target.object`** | `*keys::TARGET_OBJECT` | chosen sink's `node.name`, or `object.serial` as a string | keys.h:384 |
| `node.dont-reconnect` | `*keys::NODE_DONT_RECONNECT` | `"false"` | keys.h:182 |
| `node.passive` | `*keys::NODE_PASSIVE` | `"false"` (`"out"`/`"in"`/`"true"` are the other legal values) | keys.h:214-216 |
| `stream.dont-remix` | `*keys::STREAM_DONT_REMIX` | `"false"` | keys.h:338 |
| `node.latency` | `*keys::NODE_LATENCY` | same fraction as NODE 1 | keys.h:166 |
| `application.name` | `*keys::APP_NAME` | `"FxSound"` | keys.h:115 |
| `application.id` | `*keys::APP_ID` | `"com.fxsound.FxSound"` | keys.h:116 |

### Capture-side keys (only if you ever add a monitor-capture fallback)

| Property string | Const? | Cite |
| --- | --- | --- |
| `stream.capture.sink` | `*keys::STREAM_CAPTURE_SINK` | keys.h:339 |
| `stream.monitor` | `*keys::STREAM_MONITOR` | keys.h:332 |
| `port.monitor` | `*keys::PORT_MONITOR` | keys.h:246 |
| `media.class = "Stream/Input/Audio"` | `*keys::MEDIA_CLASS` | keys.h:351 |

### Spelling traps

* `node.link-group` — hyphen, singular "group". Not `node.link_group`, not `node.link-groups`.
* `node.max-latency`, `node.force-quantum`, `node.lock-quantum`, `node.force-rate`,
  `node.lock-rate`, `node.dont-reconnect`, `node.always-process`, `node.want-driver`,
  `node.pause-on-idle`, `node.suspend-on-idle`, `node.channel-names` — **all hyphenated after the
  first dot.**
* `stream.capture.sink` — **two dots**, not `stream.capture-sink`.
* `stream.dont-remix` — hyphen.
* `audio.allowed-rates` — hyphen.
* `target.object` — the modern key. `node.target` is deprecated and has no Rust constant.
* `audio.position` values are comma-separated **without spaces** in the string form
  (`"FL,FR"`, pipewire.conf:335) but are a JSON array in conf files (`[ FL FR ]`,
  pipewire.conf:244). From `properties!` use the comma form.
* `media.class` values are case-sensitive and slash-separated:
  `"Audio/Sink"`, `"Audio/Source"`, `"Audio/Source/Virtual"`, `"Stream/Output/Audio"`,
  `"Stream/Input/Audio"` (pipewire.conf:312, minimal.conf:373, keys.h:351).

### If you ever need a server-side node instead

`Core::create_object` (`core/mod.rs:146`) against the `adapter` factory with
`factory.name = "support.null-audio-sink"` — the exact argument set is in
`/usr/share/pipewire/pipewire.conf:307-316`:

```
{ factory = adapter
    args = {
        factory.name     = support.null-audio-sink
        node.name        = "my-mic"
        node.description = "Microphone"
        media.class      = "Audio/Source/Virtual"
        audio.position   = "FL,FR"
        monitor.passthrough = true
    }
}
```

Add `object.linger = "1"` (`keys.h:61`, used at `examples/create-delete-remote-objects.rs:58`) only
if you *want* the node to survive your process — for FxSound you explicitly do **not**, so leave it
out. This path is a fallback only; see §13 for why the `pw_stream` sink is better.

---

## 15. The RT thread — what is forbidden inside `process()`

`process()` runs on PipeWire's **data thread** when you pass `StreamFlags::RT_PROCESS`
(`stream/mod.rs:952`). The C header states the contract precisely
(`/usr/include/pipewire-0.3/pipewire/filter.h:82-86`, same rule for streams):

> do processing. This is normally called from the mainloop but can also be called directly from the
> realtime data thread if the user is prepared to deal with this with the `PW_FILTER_FLAG_RT_PROCESS`.
> **Only call methods marked with RT safe from this event when called from the realtime thread.**

### Forbidden inside `process()`

| Forbidden | Why / what to do instead |
| --- | --- |
| **Any heap allocation or free** | `Vec::push`/`with_capacity`, `Box::new`, `String`, `format!`, `to_owned`, `collect`, any `Rc`/`Arc` *clone that allocates*. Preallocate everything in `param_changed` or at startup. |
| **Any `Mutex`/`RwLock` lock** | Priority inversion → xrun. Use `triple_buffer`, `arc_swap::ArcSwap::load` (wait-free read), or `rtrb` — all already in the workspace `Cargo.toml`. |
| **`println!` / `log::*` / `eprintln!`** | They lock stdout and allocate. The upstream examples print (`examples/audio-capture.rs:127`) — **that is example code, not a template.** Push a counter into an `AtomicU64` and report from the main loop. |
| **Any syscall that can block** | file/network I/O, `std::time::Instant::now` is fine (vDSO) but `SystemTime` may not be; no `sleep`, no `std::thread::yield_now`. |
| **`panic!` / `unwrap()` / `expect()` / array OOB / integer overflow in debug** | The unwind crosses the `extern "C"` trampoline (`stream/mod.rs:547-554`) → UB/abort. Use `let … else { return }` everywhere, as the examples do (`examples/audio-capture.rs:112-117`). |
| **`Stream::connect` / `disconnect` / `update_params` / `set_active`** | Not RT-safe. Post a message to the main loop instead. |
| **Dropping the `StreamListener`, `Buffer` misuse** | `Buffer::drop` calls `pw_stream_queue_buffer`, which *is* RT-safe (`filter.h:237-238` marks the analogous call RT safe). Dropping the listener is not. |
| **Reallocating the DSP state on a preset change** | Double-buffer the coefficient block and swap an `ArcSwap`/`triple_buffer` pointer. |
| **`pipewire::channel::Sender::send`** | It takes a `Mutex` (`channel.rs:161`) and may `write(2)` (`channel.rs:169`). **Not RT-safe.** Use an `rtrb` queue out of `process()` and have a main-loop timer drain it. |

### Allowed / documented RT-safe

* `Stream::dequeue_buffer()` / `Buffer` drop → `pw_stream_queue_buffer` (`stream/mod.rs:252,266`).
* `Stream::time()` — "This function is RT-safe" (`stream/mod.rs:381`).
* `Stream::flush(drain)` — the C doc marks `pw_filter_flush` RT safe (`filter.h:251`); the stream
  equivalent likewise.
* `Stream::trigger_process()` (v0_3_34, `stream/mod.rs:363`) — RT safe per `filter.h:265`.
* Reading an `ArcSwap` / `triple_buffer::Output::read()` / `rtrb` pop.
* Plain arithmetic on the sample slice.

### Getting RT priority

**Neither `pipewire` nor `libspa` 0.10.1 exposes any RT/RTKit API.** There is no `rt` module, no
`spa_thread_utils` binding — `libspa-0.10.1/src/support/` contains exactly `mod.rs` (4 lines) and
`system.rs` (16 lines, just `IoFlags`). Priority is **not** something you set from Rust here.

It is handled entirely by `libpipewire-module-rt`, loaded into **your client process** by
`client.conf`. Verbatim from `/usr/share/pipewire/client.conf:44-58`:

```
    # Uses realtime scheduling to boost the audio thread priorities
    { name = libpipewire-module-rt
        args = {
            #rt.prio      = 83
            #rt.time.soft = -1
            #rt.time.hard = -1
            #rlimits.enabled = true
            #rtportal.enabled = true
            #rtkit.enabled = true
            #uclamp.min = 0
            #uclamp.max = 1024
        }
        flags = [ ifexists nofail ]
        condition = [ { module.rt = !false } ]
    }
```

The daemon's own copy (`/usr/share/pipewire/pipewire.conf:112-126`) uses `nice.level = -11`,
`rt.prio = 88`, `rtportal.enabled = false`.

What this means for FxSound:

1. **Nothing to call.** `pw_context_new` loads the modules listed in `client.conf`, so
   `ContextBox::new` / `ContextRc::new` (`context/box_.rs:22`, `context/rc.rs:48`) already
   arranges RT for the data thread that runs your `process()`.
2. **Three escalation paths, tried in order by `module-rt`:**
   * direct `sched_setscheduler(SCHED_FIFO)` — needs `CAP_SYS_NICE` or a permissive
     `RLIMIT_RTPRIO` (`rlimits.enabled`);
   * the **XDG portal** (`rtportal.enabled`) — the sandboxed path, default `true` in `client.conf`;
   * **RTKit** over D-Bus (`rtkit.enabled`) — `org.freedesktop.RealtimeKit1`, the classic desktop path.
3. **The knobs you can turn without touching the user's config** — pass them as **context
   properties** at `ContextBox::new`/`ContextRc::new` time, since `pw_context_new`'s properties feed
   module args:
   * `rt.prio` — e.g. `"83"`; keep it *below* the daemon's `88` (pipewire.conf:115).
   * `nice.level`
   * `module.rt = false` to opt out entirely.
   * `loop.rt-prio`, `loop.class` per `pipewire.conf:26-27`.
4. **Verify, don't assume.** After the stream reaches `StreamState::Streaming`, read
   `/proc/self/task/*/stat` field 18 (RT priority) or call `sched_getscheduler` on the data thread
   to confirm `SCHED_FIFO`. If it is `SCHED_OTHER`, RTKit was refused (no D-Bus, no portal, no
   `CAP_SYS_NICE`) — log it once and raise your ring-buffer target fill rather than failing.
5. **Do not call `sched_setscheduler` yourself.** You will fight `module-rt` and, more importantly,
   an RT thread with no `RLIMIT_RTTIME` watchdog can wedge the machine — precisely the
   "affects system audio for all users" failure class `CLAUDE.md` warns about for `audiopassthru/`.
6. **`panic = "abort"` is already set** (`fxsound-linux/Cargo.toml`, `[profile.release]`), which is
   the right choice: a panic on the RT thread must not try to unwind through `extern "C"`.

---

## 16. Quick paste-in skeleton

**Prerequisite** (§1.1) — without this, `*pw::keys::NODE_LINK_GROUP` below does not compile:

```toml
pipewire = { version = "0.10.1", features = ["v0_3_65"] }
libspa   = { version = "0.10.1", features = ["v0_3_65"] }
```

```rust
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

fn pw_thread(rx: pw::channel::Receiver<Msg>) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context  = pw::context::ContextRc::new(&mainloop, None)?;
    let core     = context.connect_rc(None)?;

    let _rx = rx.attach(mainloop.loop_(), {
        let ml = mainloop.clone();
        move |msg| match msg { Msg::Quit => ml.quit(), /* … */ }
    });

    let stream = pw::stream::StreamBox::new(&core, "fxsound_sink", properties! {
        *pw::keys::MEDIA_CLASS     => "Audio/Sink",
        *pw::keys::NODE_NAME       => "fxsound_sink",
        *pw::keys::NODE_VIRTUAL    => "true",
        *pw::keys::NODE_LINK_GROUP => "fxsound",
        "audio.position"           => "FL,FR",
    })?;

    let _listener = stream
        .add_local_listener_with_user_data(UserData::default())
        .state_changed(|_s, _d, old, new| { /* no println on RT; this one is main-loop */ })
        .param_changed(|_s, d, id, param| { /* see §8.3 */ })
        .process(|s, d| { /* see §13(b) — RT SAFE ONLY */ })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(2);
    let mut pos = [0u32; spa::param::audio::MAX_CHANNELS];
    pos[0] = libspa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = libspa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);

    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id:    spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    ).unwrap().0.into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();          // blocks until quit()
    Ok(())                   // _listener, _rx, stream, core, context drop in the right order
}
```

Drop order at the end of that function is `params → stream → _listener → _rx → core → context →
mainloop` by declaration order — which is **wrong**: `stream` borrows `core` via `StreamBox<'c>`.
Either use `StreamRc::new(core.clone(), …)` (`stream/rc.rs:39`), which stashes the core internally
(`stream/rc.rs:18-25`), or keep `core` alive by declaring it last. **Prefer the `Rc` family
throughout this project** — it is what all six upstream examples use, and it removes the whole
class of drop-order bugs.
