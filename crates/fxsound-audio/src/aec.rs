//! Echo cancellation: PipeWire's `libpipewire-module-echo-cancel`, loaded into FxSound's own
//! context (`docs/0.4.0-design.md` §7).
//!
//! # What runs
//!
//! The module makes three streams, all named by us and all in the graph for as long as it is
//! loaded:
//!
//! ```text
//!  microphone ──► fxsound_aec_capture ─┐
//!                                      ├─ WebRTC ─► fxsound_aec_source ──► fxsound_capture (input lane)
//!  speakers' monitor ─► fxsound_aec_monitor ─┘
//! ```
//!
//! `monitor.mode` is what makes the far end the speakers' own monitor — what is actually played,
//! after FxSound's chain and after every other application's sound — rather than a sink of the
//! module's own that applications would have to be moved into. While the canceller's source is in
//! the graph, the input lane's capture stream records from it instead of from the microphone, and
//! everything after that — the voice chain, `fxsound_source`, the recorder — is unchanged.
//!
//! # Why a module, and why `unsafe`
//!
//! A canceller needs the far end and the near end on one clock, aligned to the sample, and the
//! module does all of that plumbing — its three streams share a `node.group` so the scheduler runs
//! them together. What `pipewire` 0.10.1 does not have is a way to load it: there is no
//! `pw_context_load_module` binding and no `pw_impl_module` type. So [`Canceller`] calls the C
//! functions through `pipewire-sys`, and its three functions are the only code in this crate that
//! `#[allow(unsafe_code)]` (`lib.rs` says why the crate denies rather than forbids it).
//!
//! # A connection of its own
//!
//! In a client, the module looks for a core in the context (`pw_context_get_object`) and makes a
//! connection of its own when there is none. There is none: nothing in libpipewire or in `pipewire`
//! puts one there, and we deliberately do not. The module's cleanup (`impl_destroy`, PipeWire 0.3.65
//! through 1.6.8) removes the listeners it hooks on its core only when that core is destroyed —
//! loaded onto *our* core and unloaded while it lives on, it would leave two hooks into freed
//! memory on the connection every other object of ours runs on, and the next event on it would
//! call through them. On a connection of its own, the module disconnects that connection itself as
//! it goes, and the hooks go with it.
//!
//! Its connection is made to the server ours is: [`module_args`] passes `remote.name` whenever the
//! engine was given one. Without it, the module would connect wherever `PIPEWIRE_REMOTE` or the
//! runtime directory's default socket pointed — which, for a test against a private daemon, is the
//! developer's own session.
//!
//! # Lifetime
//!
//! Created on the PipeWire thread and owned by the engine ([`EchoCancel`]). It is destroyed by us
//! when echo cancellation is switched off, when the input lane is detached, before a disconnect
//! closes the session, and at exit before the context goes (`engine::close_session` covers the last
//! two). It can also go by itself: the module schedules its own destroy when its connection breaks
//! or one of its streams is disconnected. So a destroy listener ([`Watch`]) marks the handle dead
//! the moment the module goes, whoever destroys it, and a dead handle is never destroyed again.

use std::cell::{Cell, UnsafeCell};
use std::ffi::{CString, c_void};
use std::fmt::Write as _;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use libspa::sys as spa_sys;
use pipewire as pw;
use pipewire_sys as pw_sys;

use crate::{AEC_CAPTURE_NODE_NAME, AEC_LINK_GROUP, AEC_MONITOR_NODE_NAME, AEC_SOURCE_NODE_NAME};

/// The module, as `pw_context_load_module` finds it in `PIPEWIRE_MODULE_DIR` or libpipewire's own
/// module directory.
pub(crate) const MODULE_NAME: &str = "libpipewire-module-echo-cancel";

/// The canceller FxSound runs: WebRTC's, the one every distribution that builds the module ships.
pub(crate) const WEBRTC_LIBRARY: &str = "aec/libspa-aec-webrtc";

/// A canceller that cancels nothing and passes the microphone through: only ever loaded by tests,
/// which care where the module's nodes go and not what they do to the sound.
#[cfg(test)]
pub(crate) const NULL_LIBRARY: &str = "aec/libspa-aec-null";

/// `priority.session` of the canceller's source: below `fxsound_source`'s 500, so that a session
/// manager left to choose a default source on its own never prefers the half of FxSound that no
/// application is meant to record from.
const SOURCE_PRIORITY_SESSION: u32 = 400;

const CAPTURE_DESCRIPTION: &str = "FxSound echo capture";
const MONITOR_DESCRIPTION: &str = "FxSound echo monitor";
const SOURCE_DESCRIPTION: &str = "FxSound echo-cancelled";

/// How long the speakers alone may hold the canceller up ([`held_up_by_speakers`]) before the
/// report names them: longer than an output pair takes to be rebuilt after a failure or two (a
/// lane's first retries come 200 and 400 ms apart), so that a wait the next few ticks end is not
/// shown as a fault — on and still starting is nothing to explain.
const SPEAKERS_GRACE: Duration = Duration::from_secs(1);

/// The report's detail once the speakers have held the canceller up for longer than
/// [`SPEAKERS_GRACE`].
const WAITING_FOR_SPEAKERS: &str = "waiting for the speakers";

/// What the canceller listens to: which microphone it takes the echo out of, and which speakers'
/// monitor it hears that echo on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Targets {
    /// `node.name` of the microphone the input lane is attached to.
    pub(crate) microphone: String,
    /// `node.name` of the device the output lane plays to — what is heard after FxSound. `None`
    /// while the output lane is detached: the monitor stream then follows the default sink, which
    /// is what is played when FxSound is not in the way.
    pub(crate) speakers: Option<String>,
}

/// Where a lane is, as far as the canceller is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side<'a> {
    /// Detached.
    Off,
    /// Enabled and between pairs: rebuilding, backing off, or waiting for a device.
    Between,
    /// Its pair is attached to this device.
    On(&'a str),
}

/// What the canceller should be listening to now, or `None` for no canceller at all.
///
/// Nothing unless echo cancellation is on and the input lane is enabled. A lane between pairs does
/// not change a canceller that is already running — a pair that fails and is rebuilt a moment
/// later is the same microphone and the same speakers — but neither does it start one: with the
/// microphone between pairs there is nothing to cancel for, and with the speakers between pairs
/// there is no telling yet what to listen to, only that it would have to be reloaded the moment
/// they settle. A wait for the speakers that outlasts a rebuild is not a silent one, though: the
/// report names them ([`held_up_by_speakers`]).
pub(crate) fn wanted(
    on: bool,
    microphone: Side<'_>,
    speakers: Side<'_>,
    loaded: Option<&Targets>,
) -> Option<Targets> {
    if !on {
        return None;
    }
    let microphone = match microphone {
        Side::Off => return None,
        Side::On(name) => name.to_owned(),
        Side::Between => loaded?.microphone.clone(),
    };
    let speakers = match speakers {
        Side::Off => None,
        Side::On(name) => Some(name.to_owned()),
        Side::Between => loaded?.speakers.clone(),
    };
    Some(Targets {
        microphone,
        speakers,
    })
}

/// Whether the speakers alone are what keeps a canceller from being loaded: echo cancellation on,
/// the microphone attached, nothing loaded, and the output lane enabled but between pairs — which
/// [`wanted`] waits out.
///
/// Only the speakers. A microphone between pairs is the input lane's own trouble, reported as its
/// status, and with nothing to cancel for there is nothing to say about the canceller; a detached
/// output lane is no wait at all, since the monitor then follows the default sink.
pub(crate) fn held_up_by_speakers(
    on: bool,
    microphone: Side<'_>,
    speakers: Side<'_>,
    loaded: Option<&Targets>,
) -> bool {
    on && loaded.is_none() && matches!(microphone, Side::On(_)) && speakers == Side::Between
}

/// What to do about the module, given what is [`wanted`], what is loaded and what last failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Nothing loaded and nothing to load — or a load that failed for these very devices, which
    /// would only fail again.
    Idle,
    /// The loaded module is the one wanted.
    Keep,
    /// A module is loaded and none is wanted.
    Unload,
    /// Nothing is loaded; load one for these devices.
    Load(Targets),
    /// The loaded module listens to the wrong devices. A module's targets are fixed when it is
    /// loaded, so it is unloaded and one is loaded for these.
    Reload(Targets),
}

/// The module's lifecycle, as a pure function of the three things it depends on.
///
/// A failed load is not retried for the devices it failed for: a missing `libspa-aec-webrtc` is
/// missing on every attempt, and the user has already been told. It is tried again once the
/// devices change, or when echo cancellation is asked for again ([`EchoCancel::set_on`]).
pub(crate) fn plan(
    want: Option<Targets>,
    loaded: Option<&Targets>,
    failed: Option<&Targets>,
) -> Plan {
    match (want, loaded) {
        (None, None) => Plan::Idle,
        (None, Some(_)) => Plan::Unload,
        (Some(want), Some(loaded)) if want == *loaded => Plan::Keep,
        (Some(want), Some(_)) => Plan::Reload(want),
        (Some(want), None) if failed == Some(&want) => Plan::Idle,
        (Some(want), None) => Plan::Load(want),
    }
}

/// Where the input lane's capture stream should record from, when that is not `microphone` itself:
/// the canceller's source, while a canceller listening to that same microphone is running.
///
/// Only once its source is in the registry, not as soon as the module loads. A capture stream
/// aimed at a node that does not exist yet waits for it at best; under a session manager it can be
/// linked to whatever the default source is meanwhile, which while the input lane holds the default
/// is FxSound's own. And only for the microphone the canceller hears: after a change of microphone
/// the lane records the new one directly until the canceller has been reloaded for it.
pub(crate) fn route(
    on: bool,
    running: bool,
    loaded: Option<&Targets>,
    microphone: &str,
) -> Option<&'static str> {
    (on && running && loaded.is_some_and(|targets| targets.microphone == microphone))
        .then_some(AEC_SOURCE_NODE_NAME)
}

// ---------------------------------------------------------------------------------------------
// The module's arguments
// ---------------------------------------------------------------------------------------------

/// One value in the module's arguments, which PipeWire reads as SPA JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arg {
    Text(String),
    Flag(bool),
    Number(u32),
    Object(Vec<(&'static str, Arg)>),
}

impl Arg {
    fn text(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

/// The module's arguments (`docs/0.4.0-design.md` §7), before they are written out.
///
/// # The canceller's link-group
///
/// Every one of the three streams gets `node.link-group = fxsound-aec` ([`AEC_LINK_GROUP`]): a
/// group of the canceller's own, rather than a lane's, and rather than the module's default
/// `echo-cancel-<pid>-<id>` — which would behave the same but carry a name nothing can check for.
///
/// * **Not `fxsound-input`.** The input lane's capture stream has to be linked to
///   `fxsound_aec_source`, and WirePlumber's `canLink` refuses outright a link into a node of the
///   linking node's own group — the very rule that stops `fxsound_capture` recording from
///   `fxsound_source`. In the input lane's group the canceller could never be used.
/// * **Not `fxsound`.** WirePlumber would allow it, but the server runs a group's members together
///   (`run_nodes`), and the canceller's capture stream runs whenever the microphone does: it would
///   keep `fxsound_sink`, `fxsound_output` and the speakers running for as long as the input lane
///   was on, the failure `graph_churn::a_microphone_being_captured_does_not_keep_the_speakers_awake`
///   guards against for the lanes.
/// * **`fxsound-aec`** passes `canLink` for every link the canceller needs, and still refuses the
///   loop that matters: `fxsound_aec_capture` falling back to `fxsound_source`, the default source
///   while the input lane holds it, is refused by the walk from `fxsound_source` through its
///   partner `fxsound_capture` to that stream's peer `fxsound_aec_source`, in `fxsound-aec` again.
///
/// That refusal only exists once `fxsound_capture` records from the canceller, so both of the
/// module's capture streams also carry `node.dont-fallback`: a microphone or speakers that are not
/// there yet are waited for, not replaced by whatever the default is at that moment.
///
/// The module's own `node.group` — one for all three streams, so the scheduler runs the near end
/// and the far end on one clock — is left as the module makes it. It is also why, while echo
/// cancellation runs, the speakers and the output pair run too (§7, "What echo cancellation costs
/// in idle").
///
/// # Names
///
/// Each stream has a `node.description` of ours as well as a `node.name`, so that a mixer or a
/// graph tool lists it as FxSound's rather than as the module's `Echo-Cancel Capture`,
/// `Echo-Cancel Sink` or `Echo-Cancel Source`. §7 names the capture stream and the source; the
/// monitor stream is named for the same reason, and more so: in `monitor.mode` it is no sink but a
/// stream recording the speakers, and the module's name for it would say otherwise.
///
/// # What WebRTC does not do
///
/// Its noise suppression, gain control, high-pass filter and voice detection are off: the voice
/// chain after it has a denoiser, a leveller, a high-pass and a gate of its own, each set by the
/// voice preset, and two of each would fight. The canceller only cancels.
pub(crate) fn module_args(
    library: &str,
    targets: &Targets,
    remote: Option<&str>,
) -> Vec<(&'static str, Arg)> {
    let mut args = vec![
        ("library.name", Arg::text(library)),
        ("monitor.mode", Arg::Flag(true)),
    ];
    if let Some(remote) = remote {
        args.push(("remote.name", Arg::text(remote)));
    }
    args.push((
        "aec.args",
        Arg::Object(vec![
            ("webrtc.noise_suppression", Arg::Flag(false)),
            ("webrtc.gain_control", Arg::Flag(false)),
            ("webrtc.high_pass_filter", Arg::Flag(false)),
            ("webrtc.voice_detection", Arg::Flag(false)),
        ]),
    ));
    args.push((
        "capture.props",
        Arg::Object(vec![
            ("node.name", Arg::text(AEC_CAPTURE_NODE_NAME)),
            ("node.description", Arg::text(CAPTURE_DESCRIPTION)),
            ("node.link-group", Arg::text(AEC_LINK_GROUP)),
            ("node.dont-fallback", Arg::Flag(true)),
            ("target.object", Arg::text(&targets.microphone)),
        ]),
    ));
    let mut sink = vec![
        ("node.name", Arg::text(AEC_MONITOR_NODE_NAME)),
        ("node.description", Arg::text(MONITOR_DESCRIPTION)),
        ("node.link-group", Arg::text(AEC_LINK_GROUP)),
        ("node.dont-fallback", Arg::Flag(true)),
        ("stream.capture.sink", Arg::Flag(true)),
    ];
    if let Some(speakers) = &targets.speakers {
        sink.push(("target.object", Arg::text(speakers)));
    }
    args.push(("sink.props", Arg::Object(sink)));
    args.push((
        "source.props",
        Arg::Object(vec![
            ("node.name", Arg::text(AEC_SOURCE_NODE_NAME)),
            ("node.description", Arg::text(SOURCE_DESCRIPTION)),
            ("node.link-group", Arg::text(AEC_LINK_GROUP)),
            ("priority.session", Arg::Number(SOURCE_PRIORITY_SESSION)),
        ]),
    ));
    args
}

/// Write arguments out as the SPA JSON object `pw_context_load_module` parses.
///
/// Every string is quoted, because a device's `node.name` is not a bare word SPA JSON would read
/// back whole — ALSA's contain colons as often as dots.
pub(crate) fn render(args: &[(&'static str, Arg)]) -> String {
    let mut out = String::new();
    render_object(&mut out, args);
    out
}

fn render_object(out: &mut String, entries: &[(&'static str, Arg)]) {
    out.push('{');
    for (key, value) in entries {
        out.push(' ');
        out.push_str(key);
        out.push_str(" = ");
        match value {
            Arg::Text(text) => render_text(out, text),
            Arg::Flag(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Arg::Number(number) => {
                let _ = write!(out, "{number}");
            }
            Arg::Object(inner) => render_object(out, inner),
        }
    }
    out.push_str(" }");
}

/// A quoted SPA JSON string: `"` and `\` escaped, control characters as `\uXXXX`.
fn render_text(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------------------------
// The module itself
// ---------------------------------------------------------------------------------------------

/// What the module's `destroy` event reaches: the hook it is delivered through, and the flag it
/// clears.
///
/// Pinned in a box of its own, because libpipewire keeps the hook's address in the module's
/// listener list and the flag's as the callback's data, and uses both until the module is gone.
/// The hook is an `UnsafeCell` because C writes it — `spa_hook_list_append` zeroes and links it,
/// and the module's destroy unlinks it — through a pointer made from a shared reference.
struct Watch {
    hook: UnsafeCell<spa_sys::spa_hook>,
    alive: Cell<bool>,
    _pinned: PhantomPinned,
}

/// The one event of the module's this crate listens to. Static, because libpipewire keeps a pointer
/// to it in the hook for as long as the hook is linked.
static MODULE_EVENTS: pw_sys::pw_impl_module_events = pw_sys::pw_impl_module_events {
    version: pw_sys::PW_VERSION_IMPL_MODULE_EVENTS,
    destroy: Some(on_module_destroy),
    free: None,
    initialized: None,
    registered: None,
};

/// `pw_impl_module_events::destroy`: the module is going, whoever destroys it. Emitted on the
/// thread that runs the context's loop — ours — from `pw_impl_module_destroy`, which is called by
/// [`Canceller`]'s `Drop`, by the module's own scheduled destroy, and by the context's teardown.
///
/// It only clears a flag: it may run in the middle of any turn of the loop, and must neither borrow
/// the engine's state nor panic across the FFI boundary.
#[allow(unsafe_code)]
unsafe extern "C" fn on_module_destroy(data: *mut c_void) {
    // SAFETY: `data` is the pointer `Canceller::load` registered: the `alive` field of a `Watch`
    // pinned in a box that `Canceller` drops only once this event has been delivered (its `Drop`
    // destroys a live module first) — so it points at a live `Cell<bool>`. The `Cell` is only ever
    // touched from this thread, the one the context's loop runs on.
    let alive = unsafe { &*data.cast::<Cell<bool>>() };
    alive.set(false);
}

/// One loaded echo-cancel module.
///
/// Neither `Send` nor `Sync` (the raw pointer sees to that): it is made, used and dropped on the
/// PipeWire thread, the one thread the module's context may be touched from.
pub(crate) struct Canceller {
    module: NonNull<pw_sys::pw_impl_module>,
    watch: Pin<Box<Watch>>,
}

impl Canceller {
    /// Load the module into `context` with `args` ([`render`]).
    ///
    /// The module does all of its setting up inside this call — it loads the canceller library,
    /// connects, creates and connects its three streams — so a library that is not installed, a
    /// module that is not, or arguments it cannot use all fail here, with the `errno` libpipewire
    /// leaves: `ENOENT` for a missing library or module.
    ///
    /// # Errors
    /// The error libpipewire reported.
    #[allow(unsafe_code)]
    pub(crate) fn load(context: &pw::context::Context, args: &str) -> std::io::Result<Self> {
        let name = CString::new(MODULE_NAME).map_err(std::io::Error::other)?;
        let args = CString::new(args).map_err(std::io::Error::other)?;
        // SAFETY: `context` is a live context, borrowed for the whole call, and this is the thread
        // its loop runs on — the only thread the engine touches it from. `name` and `args` are
        // NUL-terminated and outlive the call; the module copies what it keeps of them. The
        // properties may be null: libpipewire makes an empty set.
        let module = unsafe {
            pw_sys::pw_context_load_module(
                context.as_raw_ptr(),
                name.as_ptr(),
                args.as_ptr(),
                ptr::null_mut(),
            )
        };
        // Read before anything else can overwrite it: `pw_context_load_module` sets `errno` as
        // the last thing it does on every failure path, and returns null.
        let Some(module) = NonNull::new(module) else {
            return Err(std::io::Error::last_os_error());
        };

        let watch = Box::pin(Watch {
            hook: UnsafeCell::new(spa_sys::spa_hook {
                link: spa_sys::spa_list {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                },
                cb: spa_sys::spa_callbacks {
                    funcs: ptr::null(),
                    data: ptr::null_mut(),
                },
                removed: None,
                priv_: ptr::null_mut(),
            }),
            alive: Cell::new(true),
            _pinned: PhantomPinned,
        });
        // SAFETY: `module` was returned by `pw_context_load_module` just now, and nothing has run
        // the loop since, so its scheduled destroy cannot have run: it is live. The hook and the
        // flag are in a pinned box that `self` owns and drops only after the module has gone
        // (`Drop`), and the module's destroy unlinks the hook (`spa_hook_list_clean`), so neither
        // pointer outlives what it points at. `MODULE_EVENTS` is static.
        unsafe {
            pw_sys::pw_impl_module_add_listener(
                module.as_ptr(),
                watch.hook.get(),
                &raw const MODULE_EVENTS,
                ptr::from_ref(&watch.alive).cast_mut().cast::<c_void>(),
            );
        }
        Ok(Self { module, watch })
    }

    /// Whether the module is still loaded. `false` once it has been destroyed by anyone —
    /// including by itself, which it does when its connection breaks or a stream of its
    /// disconnects.
    pub(crate) fn is_alive(&self) -> bool {
        self.watch.alive.get()
    }
}

impl Drop for Canceller {
    /// Destroy the module, unless it is gone already. Its three streams go with it, and so does
    /// the connection they were made on.
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if !self.is_alive() {
            return;
        }
        // SAFETY: the destroy listener has not fired, and every path that frees a module emits
        // `destroy` first, so `module` is live — and destroying it once is what this is for:
        // afterwards `alive` is false and nothing touches the pointer again. Live is not the same
        // as nothing pending: a module whose connection broke, or one of whose streams was
        // disconnected, has queued its own destroy on the context's work queue
        // (`pw_impl_module_schedule_destroy`), and the loop may not have run it yet: when the
        // server goes, `close_session` can get here before it does. That queued destroy never
        // runs on the module freed here, because `pw_impl_module_destroy` cancels it: it calls
        // `pw_work_queue_cancel` for this module, which unsets the queued item's callback, before
        // it frees the module. So it does in 0.3.65, the floor this crate builds against, and in
        // every release checked since (1.0, 1.2, 1.4, 1.6.8). This is the thread the context's
        // loop runs on (the type is not `Send`). The module emits `destroy` to our hook and
        // unlinks it before it frees anything, so the box can go after this returns.
        unsafe { pw_sys::pw_impl_module_destroy(self.module.as_ptr()) };
        debug_assert!(
            !self.is_alive(),
            "a destroyed module always emits its destroy event"
        );
    }
}

impl std::fmt::Debug for Canceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canceller")
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// The engine's echo-cancellation state
// ---------------------------------------------------------------------------------------------

/// A module and the devices it was loaded for.
#[derive(Debug)]
struct Loaded {
    canceller: Canceller,
    targets: Targets,
}

/// Everything the engine knows about echo cancellation: whether it is wanted, the module while
/// one is loaded, what went wrong last, and what the GUI was last told.
#[derive(Debug)]
pub(crate) struct EchoCancel {
    /// [`fxsound_core::messages::UiToAudio::SetEchoCancel`]'s last word. Off at start.
    on: bool,
    /// The canceller library the module is asked to load.
    library: &'static str,
    loaded: Option<Loaded>,
    /// The devices a load last failed for, which are not tried again ([`plan`]).
    failed_for: Option<Targets>,
    /// Why echo cancellation is not running although it is on: the load error, or the module
    /// having gone by itself. Shown to the user as the report's detail.
    trouble: Option<String>,
    /// Since when the speakers alone have held the canceller up ([`held_up_by_speakers`]), for as
    /// long as they do.
    speakers_awaited_since: Option<Instant>,
    /// Whether that wait has outlasted [`SPEAKERS_GRACE`], as of the last [`Self::reconcile`]:
    /// then, with no trouble to report, the report names the speakers.
    speakers_overdue: bool,
    /// The registry ids of the nodes called `fxsound_aec_source` in the graph now, as far as they
    /// matter: forgotten the moment the module that made them goes ([`Self::unload`]) rather than
    /// when the registry gets round to saying so, because a source whose module is gone is gone,
    /// and nothing may be moved onto it or left recording from it meanwhile ([`route`]). A set
    /// rather than one id: the source of a module unloaded before it was announced can still be
    /// announced after its successor's.
    sources: Vec<u32>,
    /// Loads after the module went by itself, for the backoff before the next one.
    attempts: u32,
    next_attempt: Instant,
    /// What [`Self::running`] was when the input lane's rules were last asked to follow it.
    routed: bool,
    /// The last `(running, detail)` the GUI was told.
    reported: Option<(bool, String)>,
}

impl EchoCancel {
    pub(crate) fn new(library: &'static str) -> Self {
        Self {
            on: false,
            library,
            loaded: None,
            failed_for: None,
            trouble: None,
            speakers_awaited_since: None,
            speakers_overdue: false,
            sources: Vec::new(),
            attempts: 0,
            next_attempt: Instant::now(),
            routed: false,
            // Off, and nothing wrong: what the GUI assumes before it has asked for anything, so an
            // engine that is never asked says nothing about echo cancellation at all.
            reported: Some((false, String::new())),
        }
    }

    /// Echo cancellation on or off. Asked for again, it is a fresh start: a load that failed is
    /// tried again, and at once, and a wait for the speakers gets its grace again.
    pub(crate) fn set_on(&mut self, on: bool) {
        self.on = on;
        self.failed_for = None;
        self.trouble = None;
        self.speakers_awaited_since = None;
        self.speakers_overdue = false;
        self.attempts = 0;
        self.next_attempt = Instant::now();
    }

    fn loaded_targets(&self) -> Option<&Targets> {
        self.loaded.as_ref().map(|loaded| &loaded.targets)
    }

    /// Running: a module is loaded and its source is in the graph.
    pub(crate) fn running(&self) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|loaded| loaded.canceller.is_alive())
            && !self.sources.is_empty()
    }

    /// [`route`] for the input lane's microphone.
    pub(crate) fn route(&self, microphone: &str) -> Option<&'static str> {
        route(self.on, self.running(), self.loaded_targets(), microphone)
    }

    /// Whether [`Self::running`] changed since the input lane's rules were last asked to follow
    /// it — and, when it did, remember that they now have been.
    pub(crate) fn route_moved(&mut self) -> bool {
        let running = self.running();
        if running == self.routed {
            return false;
        }
        self.routed = running;
        true
    }

    /// A node called `fxsound_aec_source` joined the graph.
    pub(crate) fn source_appeared(&mut self, id: u32) {
        if !self.sources.contains(&id) {
            self.sources.push(id);
        }
    }

    /// A registry global went away. Whether it was one of the canceller's sources.
    pub(crate) fn source_removed(&mut self, id: u32) -> bool {
        let before = self.sources.len();
        self.sources.retain(|source| *source != id);
        self.sources.len() != before
    }

    /// The registry is gone with the connection it was on; the next one is read afresh.
    pub(crate) fn forget_sources(&mut self) {
        self.sources.clear();
    }

    /// Destroy the module, if one is loaded, and forget its source with it. Whether one was.
    pub(crate) fn unload(&mut self) -> bool {
        let Some(loaded) = self.loaded.take() else {
            return false;
        };
        self.sources.clear();
        if loaded.canceller.is_alive() {
            log::info!("unloading the echo canceller");
        }
        drop(loaded);
        true
    }

    /// Bring the module in line with the lanes: load, reload or unload it, as [`plan`] says.
    ///
    /// `context` is `None` where no load may be made — a control message, which cannot reach the
    /// context, or a connection that is not ready — and then only unloading happens; the next
    /// supervisor tick loads. A module that went by itself is let go first and loaded again on a
    /// backoff of `retry_after`. Speakers that hold a canceller up are timed against `now` as well,
    /// from whichever caller: the report names them once the wait outlasts [`SPEAKERS_GRACE`].
    pub(crate) fn reconcile(
        &mut self,
        microphone: Side<'_>,
        speakers: Side<'_>,
        context: Option<(&pw::context::Context, Option<&str>)>,
        retry_after: Duration,
        now: Instant,
    ) {
        if self
            .loaded
            .as_ref()
            .is_some_and(|l| !l.canceller.is_alive())
        {
            log::warn!("the echo canceller went away by itself");
            self.unload();
            self.trouble = Some("the echo canceller stopped unexpectedly".to_owned());
            self.attempts = self.attempts.saturating_add(1);
            self.next_attempt = now + retry_after;
        }
        // Timed before anything is loaded, and nothing can be: speakers that hold it up are
        // speakers [`wanted`] waits for.
        if held_up_by_speakers(self.on, microphone, speakers, self.loaded_targets()) {
            let since = *self.speakers_awaited_since.get_or_insert(now);
            self.speakers_overdue = now.saturating_duration_since(since) >= SPEAKERS_GRACE;
        } else {
            self.speakers_awaited_since = None;
            self.speakers_overdue = false;
        }
        let want = wanted(self.on, microphone, speakers, self.loaded_targets());
        let targets = match plan(want, self.loaded_targets(), self.failed_for.as_ref()) {
            Plan::Idle | Plan::Keep => return,
            Plan::Unload => {
                self.unload();
                return;
            }
            Plan::Reload(targets) => {
                self.unload();
                targets
            }
            Plan::Load(targets) => targets,
        };
        let Some((context, remote)) = context else {
            return;
        };
        if now < self.next_attempt {
            return;
        }
        let args = render(&module_args(self.library, &targets, remote));
        match Canceller::load(context, &args) {
            Ok(canceller) => {
                log::info!(
                    "echo canceller loaded ({}): microphone {}, speakers {}",
                    self.library,
                    targets.microphone,
                    targets.speakers.as_deref().unwrap_or("the default sink")
                );
                self.trouble = None;
                self.failed_for = None;
                self.loaded = Some(Loaded { canceller, targets });
            }
            Err(error) => {
                let detail = format!(
                    "could not load {MODULE_NAME} with {}: {error}",
                    self.library
                );
                log::warn!("{detail}");
                self.trouble = Some(detail);
                self.failed_for = Some(targets);
            }
        }
    }

    /// What to tell the GUI now: running, or not and why. The detail is empty unless something
    /// went wrong, or the speakers have held the canceller up for longer than a rebuild takes —
    /// off, or on and still starting, is nothing to explain. Something that went wrong comes
    /// first, as the more telling of the two: a missing library is still missing once the
    /// speakers are back.
    fn report(&self) -> (bool, String) {
        if self.running() {
            return (true, String::new());
        }
        if !self.on {
            return (false, String::new());
        }
        let detail = match &self.trouble {
            Some(trouble) => trouble.clone(),
            None if self.speakers_overdue => WAITING_FOR_SPEAKERS.to_owned(),
            None => String::new(),
        };
        (false, detail)
    }

    /// The report, if it differs from what the GUI was last told — and remember it as told.
    pub(crate) fn news(&mut self) -> Option<(bool, String)> {
        let report = self.report();
        if report.0 {
            // A module that got as far as running has proved itself: the next time it goes by
            // itself, the wait starts again from the shortest.
            self.attempts = 0;
        }
        if self.reported.as_ref() == Some(&report) {
            return None;
        }
        self.reported = Some(report.clone());
        Some(report)
    }

    /// The report, whether or not it is news: the answer to a `SetEchoCancel`, which the one who
    /// sent it is waiting for.
    pub(crate) fn answer(&mut self) -> (bool, String) {
        let report = self.report();
        self.reported = Some(report.clone());
        report
    }

    /// Loads after the module went by itself, for the backoff the caller computes.
    pub(crate) const fn attempts(&self) -> u32 {
        self.attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{INPUT_LINK_GROUP, LINK_GROUP};

    fn targets(microphone: &str, speakers: Option<&str>) -> Targets {
        Targets {
            microphone: microphone.to_owned(),
            speakers: speakers.map(str::to_owned),
        }
    }

    /// The value at `path` in a set of arguments: `["capture.props", "target.object"]`.
    fn at<'a>(args: &'a [(&'static str, Arg)], path: &[&str]) -> Option<&'a Arg> {
        let (first, rest) = path.split_first()?;
        let value = args
            .iter()
            .find_map(|(key, value)| (key == first).then_some(value))?;
        if rest.is_empty() {
            return Some(value);
        }
        match value {
            Arg::Object(inner) => at(inner, rest),
            _ => None,
        }
    }

    fn text(value: &str) -> Option<Arg> {
        Some(Arg::text(value))
    }

    fn keys(args: &[(&'static str, Arg)], path: &[&str]) -> Vec<&'static str> {
        let entries: &[(&'static str, Arg)] = if path.is_empty() {
            args
        } else {
            match at(args, path) {
                Some(Arg::Object(inner)) => inner,
                other => panic!("{path:?} is not an object: {other:?}"),
            }
        };
        entries.iter().map(|(key, _)| *key).collect()
    }

    #[test]
    fn the_module_is_asked_for_every_key_the_design_names() {
        let args = module_args(
            WEBRTC_LIBRARY,
            &targets("alsa_input.usb-mic", Some("alsa_output.pci-analog")),
            Some("/tmp/private/pipewire-0"),
        );
        assert_eq!(
            keys(&args, &[]),
            [
                "library.name",
                "monitor.mode",
                "remote.name",
                "aec.args",
                "capture.props",
                "sink.props",
                "source.props"
            ]
        );
        assert_eq!(at(&args, &["library.name"]).cloned(), text(WEBRTC_LIBRARY));
        assert_eq!(
            at(&args, &["monitor.mode"]),
            Some(&Arg::Flag(true)),
            "the far end is the speakers' monitor, not a sink of the module's own"
        );
        assert_eq!(
            at(&args, &["remote.name"]).cloned(),
            text("/tmp/private/pipewire-0"),
            "the module's own connection goes to the server the engine's does"
        );

        // WebRTC only cancels: the voice chain after it does the rest.
        for key in [
            "webrtc.noise_suppression",
            "webrtc.gain_control",
            "webrtc.high_pass_filter",
            "webrtc.voice_detection",
        ] {
            assert_eq!(
                at(&args, &["aec.args", key]),
                Some(&Arg::Flag(false)),
                "{key}"
            );
        }

        assert_eq!(
            keys(&args, &["capture.props"]),
            [
                "node.name",
                "node.description",
                "node.link-group",
                "node.dont-fallback",
                "target.object"
            ]
        );
        assert_eq!(
            at(&args, &["capture.props", "node.name"]).cloned(),
            text("fxsound_aec_capture")
        );
        assert_eq!(
            at(&args, &["capture.props", "node.description"]).cloned(),
            text("FxSound echo capture")
        );
        assert_eq!(
            at(&args, &["capture.props", "target.object"]).cloned(),
            text("alsa_input.usb-mic")
        );

        assert_eq!(
            keys(&args, &["sink.props"]),
            [
                "node.name",
                "node.description",
                "node.link-group",
                "node.dont-fallback",
                "stream.capture.sink",
                "target.object"
            ]
        );
        assert_eq!(
            at(&args, &["sink.props", "node.name"]).cloned(),
            text("fxsound_aec_monitor")
        );
        assert_eq!(
            at(&args, &["sink.props", "node.description"]).cloned(),
            text("FxSound echo monitor"),
            "named like the other two, not the module's `Echo-Cancel Sink`, which it is not"
        );
        assert_eq!(
            at(&args, &["sink.props", "stream.capture.sink"]),
            Some(&Arg::Flag(true))
        );
        assert_eq!(
            at(&args, &["sink.props", "target.object"]).cloned(),
            text("alsa_output.pci-analog"),
            "the monitor hears the device the output lane plays to, after FxSound"
        );

        assert_eq!(
            keys(&args, &["source.props"]),
            [
                "node.name",
                "node.description",
                "node.link-group",
                "priority.session"
            ]
        );
        assert_eq!(
            at(&args, &["source.props", "node.name"]).cloned(),
            text("fxsound_aec_source")
        );
        assert_eq!(
            at(&args, &["source.props", "node.description"]).cloned(),
            text("FxSound echo-cancelled")
        );
        assert_eq!(
            at(&args, &["source.props", "priority.session"]),
            Some(&Arg::Number(400))
        );
    }

    #[test]
    fn with_the_output_lane_detached_the_monitor_follows_the_default_sink() {
        let args = module_args(WEBRTC_LIBRARY, &targets("alsa_input.usb-mic", None), None);
        assert_eq!(
            at(&args, &["sink.props", "target.object"]),
            None,
            "no target: the monitor stream follows the default sink, which is what is played"
        );
        // Everything else about the monitor stream stays.
        assert_eq!(
            at(&args, &["sink.props", "stream.capture.sink"]),
            Some(&Arg::Flag(true))
        );
        assert_eq!(
            at(&args, &["remote.name"]),
            None,
            "with no remote named, the module finds the server the engine found"
        );
    }

    #[test]
    fn the_canceller_has_a_link_group_of_its_own_and_its_capture_streams_never_fall_back() {
        // `fxsound-input` would make WirePlumber refuse the link the input lane needs into the
        // canceller's source; `fxsound` would keep the speakers' pair running with the microphone.
        assert_ne!(AEC_LINK_GROUP, INPUT_LINK_GROUP);
        assert_ne!(AEC_LINK_GROUP, LINK_GROUP);
        assert_eq!(AEC_LINK_GROUP, "fxsound-aec");

        let args = module_args(
            NULL_LIBRARY,
            &targets("alsa_input.usb-mic", Some("alsa_output.pci")),
            None,
        );
        for stream in ["capture.props", "sink.props", "source.props"] {
            assert_eq!(
                at(&args, &[stream, "node.link-group"]).cloned(),
                text(AEC_LINK_GROUP),
                "{stream} must be in the canceller's own group"
            );
        }
        // A microphone or speakers not there yet are waited for, not replaced by the default —
        // which while the input lane holds it is FxSound's own source.
        for stream in ["capture.props", "sink.props"] {
            assert_eq!(
                at(&args, &[stream, "node.dont-fallback"]),
                Some(&Arg::Flag(true)),
                "{stream}"
            );
        }
        // The module's own `node.group` is what keeps the near and far ends on one clock.
        assert_eq!(at(&args, &["node.group"]), None);
        for stream in ["capture.props", "sink.props", "source.props"] {
            assert_eq!(at(&args, &[stream, "node.group"]), None, "{stream}");
        }
    }

    #[test]
    fn every_string_is_quoted_so_device_names_come_back_whole() {
        let rendered = render(&module_args(
            WEBRTC_LIBRARY,
            &targets(
                "alsa_input.pci-0000:00:1f.3.analog-stereo",
                Some("say \"hi\"\\"),
            ),
            None,
        ));
        assert!(rendered.starts_with("{ library.name = \"aec/libspa-aec-webrtc\""));
        assert!(rendered.ends_with(" }"));
        assert!(
            rendered.contains("target.object = \"alsa_input.pci-0000:00:1f.3.analog-stereo\""),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"target.object = "say \"hi\"\\""#),
            "{rendered}"
        );
        assert!(rendered.contains("monitor.mode = true"));
        assert!(rendered.contains("priority.session = 400"));
        assert!(rendered.contains(
            "aec.args = { webrtc.noise_suppression = false webrtc.gain_control = false \
             webrtc.high_pass_filter = false webrtc.voice_detection = false }"
        ));
        let mut control = String::new();
        render_text(&mut control, "a\nb");
        assert_eq!(control, "\"a\\u000ab\"");
    }

    #[test]
    fn a_canceller_is_wanted_only_while_it_is_on_and_the_microphone_lane_is_enabled() {
        let mic = Side::On("mic");
        let speakers = Side::On("speakers");
        assert_eq!(wanted(false, mic, speakers, None), None);
        assert_eq!(
            wanted(true, mic, speakers, None),
            Some(targets("mic", Some("speakers")))
        );
        assert_eq!(
            wanted(true, Side::Off, speakers, None),
            None,
            "a detached microphone lane has nothing to cancel"
        );
        assert_eq!(
            wanted(true, mic, Side::Off, None),
            Some(targets("mic", None)),
            "a detached output lane leaves the monitor on the default sink"
        );
    }

    #[test]
    fn a_lane_between_pairs_keeps_the_canceller_it_has_and_starts_none() {
        let loaded = targets("mic", Some("speakers"));
        assert_eq!(
            wanted(true, Side::Between, Side::On("speakers"), Some(&loaded)),
            Some(loaded.clone()),
            "a microphone pair being rebuilt keeps the canceller"
        );
        assert_eq!(
            wanted(true, Side::On("mic"), Side::Between, Some(&loaded)),
            Some(loaded.clone()),
            "a speakers' pair being rebuilt keeps the canceller"
        );
        assert_eq!(
            wanted(true, Side::Between, Side::On("speakers"), None),
            None
        );
        assert_eq!(
            wanted(true, Side::On("mic"), Side::Between, None),
            None,
            "the speakers are waited for rather than loaded without and reloaded a moment later"
        );
        assert_eq!(
            wanted(false, Side::Between, Side::Between, Some(&loaded)),
            None,
            "off is off, however the lanes are"
        );
    }

    #[test]
    fn only_speakers_between_pairs_with_the_microphone_attached_and_nothing_loaded_hold_the_canceller_up()
     {
        let mic = Side::On("mic");
        let loaded = targets("mic", Some("speakers"));
        assert!(held_up_by_speakers(true, mic, Side::Between, None));
        assert!(
            !held_up_by_speakers(false, mic, Side::Between, None),
            "off holds nothing up"
        );
        assert!(
            !held_up_by_speakers(true, Side::Between, Side::Between, None),
            "a microphone between pairs is the input lane's own trouble, and nothing to cancel for"
        );
        assert!(!held_up_by_speakers(true, Side::Off, Side::Between, None));
        assert!(
            !held_up_by_speakers(true, mic, Side::Off, None),
            "a detached output lane is no wait: the monitor follows the default sink"
        );
        assert!(!held_up_by_speakers(true, mic, Side::On("speakers"), None));
        assert!(
            !held_up_by_speakers(true, mic, Side::Between, Some(&loaded)),
            "a canceller already running keeps its speakers while their pair is rebuilt"
        );
    }

    #[test]
    fn speakers_that_hold_the_canceller_up_for_longer_than_a_rebuild_are_named_as_the_reason() {
        let mut aec = EchoCancel::new(NULL_LIBRARY);
        aec.set_on(true);
        assert_eq!(aec.answer(), (false, String::new()));
        let start = Instant::now();
        let reconcile = |aec: &mut EchoCancel, speakers: Side<'_>, at: Instant| {
            aec.reconcile(Side::On("mic"), speakers, None, Duration::ZERO, at);
        };

        reconcile(&mut aec, Side::Between, start);
        reconcile(&mut aec, Side::Between, start + SPEAKERS_GRACE / 2);
        assert_eq!(
            aec.news(),
            None,
            "a pair rebuilt within a few ticks is on and still starting, not a fault"
        );
        reconcile(&mut aec, Side::Between, start + SPEAKERS_GRACE);
        assert_eq!(
            aec.news(),
            Some((false, WAITING_FOR_SPEAKERS.to_owned())),
            "a wait that outlasts a rebuild is not left blank"
        );
        assert!(
            aec.loaded.is_none() && aec.failed_for.is_none(),
            "still waited for, not loaded without them"
        );
        assert_eq!(aec.news(), None, "told once");

        // The speakers settle: nothing holds the canceller up, and it is starting again.
        reconcile(&mut aec, Side::On("speakers"), start + SPEAKERS_GRACE * 2);
        assert_eq!(aec.news(), Some((false, String::new())));

        // A new wait gets its own grace, not what is left of the last one's.
        reconcile(&mut aec, Side::Between, start + SPEAKERS_GRACE * 3);
        assert_eq!(aec.news(), None);
        reconcile(&mut aec, Side::Between, start + SPEAKERS_GRACE * 4);
        assert_eq!(aec.news(), Some((false, WAITING_FOR_SPEAKERS.to_owned())));

        // Something that went wrong is the more telling reason.
        aec.trouble = Some("could not load it".to_owned());
        assert_eq!(aec.news(), Some((false, "could not load it".to_owned())));

        // Asked for again, a fresh start, and the wait gets its grace again.
        aec.set_on(true);
        assert_eq!(aec.answer(), (false, String::new()));
        reconcile(&mut aec, Side::Between, start + SPEAKERS_GRACE * 5);
        assert_eq!(aec.news(), None);

        // Switched off, there is nothing to wait for.
        aec.set_on(false);
        reconcile(&mut aec, Side::Between, start + SPEAKERS_GRACE * 7);
        assert_eq!(aec.news(), None);
        assert_eq!(aec.answer(), (false, String::new()));
    }

    #[test]
    fn the_module_is_reloaded_when_the_microphone_or_the_speakers_change() {
        let loaded = targets("mic", Some("speakers"));
        assert_eq!(plan(Some(loaded.clone()), Some(&loaded), None), Plan::Keep);
        assert_eq!(
            plan(
                Some(targets("other mic", Some("speakers"))),
                Some(&loaded),
                None
            ),
            Plan::Reload(targets("other mic", Some("speakers")))
        );
        assert_eq!(
            plan(
                Some(targets("mic", Some("headphones"))),
                Some(&loaded),
                None
            ),
            Plan::Reload(targets("mic", Some("headphones")))
        );
        assert_eq!(
            plan(Some(targets("mic", None)), Some(&loaded), None),
            Plan::Reload(targets("mic", None)),
            "detaching the output lane moves the monitor to the default sink"
        );
        assert_eq!(plan(None, Some(&loaded), None), Plan::Unload);
        assert_eq!(plan(None, None, None), Plan::Idle);
        assert_eq!(
            plan(Some(loaded.clone()), None, None),
            Plan::Load(loaded.clone())
        );
    }

    #[test]
    fn a_load_that_failed_is_not_retried_until_the_devices_change() {
        let failed = targets("mic", Some("speakers"));
        assert_eq!(
            plan(Some(failed.clone()), None, Some(&failed)),
            Plan::Idle,
            "a missing libspa-aec-webrtc is missing on every attempt"
        );
        assert_eq!(
            plan(
                Some(targets("other mic", Some("speakers"))),
                None,
                Some(&failed)
            ),
            Plan::Load(targets("other mic", Some("speakers")))
        );

        // And asking again is a fresh start.
        let mut aec = EchoCancel::new(NULL_LIBRARY);
        aec.failed_for = Some(failed.clone());
        aec.trouble = Some("no such library".to_owned());
        aec.set_on(true);
        assert_eq!(aec.failed_for, None);
        assert_eq!(
            aec.answer(),
            (false, String::new()),
            "starting again, with the old failure forgotten"
        );
    }

    #[test]
    fn the_capture_stream_goes_through_the_canceller_only_while_its_source_is_there_for_that_microphone()
     {
        let loaded = targets("mic", Some("speakers"));
        assert_eq!(
            route(true, true, Some(&loaded), "mic"),
            Some(AEC_SOURCE_NODE_NAME)
        );
        assert_eq!(
            route(true, false, Some(&loaded), "mic"),
            None,
            "loaded is not enough: the source has to be in the graph"
        );
        assert_eq!(
            route(true, true, Some(&loaded), "other mic"),
            None,
            "a canceller for the old microphone is not recorded from"
        );
        assert_eq!(
            route(false, true, Some(&loaded), "mic"),
            None,
            "switched off, the microphone is recorded directly, whatever is still loaded"
        );
        assert_eq!(route(true, true, None, "mic"), None);
    }

    #[test]
    fn the_report_says_why_only_when_something_went_wrong_and_each_change_once() {
        let mut aec = EchoCancel::new(NULL_LIBRARY);
        assert_eq!(
            aec.news(),
            None,
            "off at start is what the GUI assumes: an engine never asked says nothing"
        );

        aec.set_on(true);
        assert_eq!(
            aec.news(),
            None,
            "on and starting is still not running, and not a fault"
        );

        aec.trouble = Some("could not load it".to_owned());
        assert_eq!(aec.news(), Some((false, "could not load it".to_owned())));
        assert_eq!(aec.answer(), (false, "could not load it".to_owned()));

        aec.set_on(false);
        assert_eq!(
            aec.news(),
            Some((false, String::new())),
            "switched off, the old failure is nothing to show"
        );
        assert_eq!(aec.news(), None, "told once");

        // A source with no module behind it is not a running canceller.
        aec.source_appeared(7);
        assert!(!aec.running());
        assert!(aec.source_removed(7));
        assert!(!aec.source_removed(7), "removed once");
    }

    /// The module against a private daemon of `graph_churn`'s, on a loop of the test's own.
    #[test]
    fn a_module_that_goes_by_itself_is_known_to_be_gone_and_is_never_destroyed_again() {
        use crate::graph_churn::{PATIENCE, PrivateGraph, canceller_missing, skip};

        if let Some(missing) = canceller_missing() {
            skip(&format!(
                "{missing}, so the canceller's own destroy was not checked"
            ));
            return;
        }
        let Some(mut graph) = PrivateGraph::start("aecgone") else {
            return;
        };
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).expect("a main loop");
        let context = pw::context::ContextRc::new(&mainloop, None).expect("a context");
        let turn = |until: Instant, done: &dyn Fn() -> bool| {
            while !done() {
                let Some(left) = until.checked_duration_since(Instant::now()) else {
                    return;
                };
                mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                    left.min(std::time::Duration::from_millis(10)),
                ));
            }
        };

        let remote = graph.remote();
        let args = render(&module_args(
            NULL_LIBRARY,
            &targets("t_mic", Some("t_stereo")),
            Some(&remote),
        ));
        let canceller = Canceller::load(&context, &args).expect("the null canceller should load");
        assert!(canceller.is_alive());
        // Let its connection come up and its streams be made, so that what breaks next is a
        // module that was running.
        turn(
            Instant::now() + std::time::Duration::from_millis(300),
            &|| false,
        );
        assert!(canceller.is_alive(), "nothing has destroyed it yet");

        // The server goes. The module's connection breaks, and the module schedules its own
        // destroy, which runs on this loop — not through us.
        graph.kill();
        turn(Instant::now() + PATIENCE, &|| !canceller.is_alive());
        assert!(
            !canceller.is_alive(),
            "the destroy listener should have seen the module go by itself"
        );
        // And dropping the handle now must not destroy it a second time: freed memory, if it did.
        drop(canceller);
    }

    /// A module against a private daemon of `graph_churn`'s. The registry is played by hand: what
    /// is checked is what the handle believes between the module going and the registry saying so.
    #[test]
    fn a_reloaded_canceller_is_not_recorded_from_however_late_its_old_source_is_said_to_go() {
        use crate::graph_churn::{PrivateGraph, canceller_missing, skip};

        if let Some(missing) = canceller_missing() {
            skip(&format!(
                "{missing}, so forgetting an unloaded canceller's source was not checked"
            ));
            return;
        }
        let Some(graph) = PrivateGraph::start("aecforget") else {
            return;
        };
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).expect("a main loop");
        let context = pw::context::ContextRc::new(&mainloop, None).expect("a context");
        let remote = graph.remote();
        let connection = Some((&*context, Some(remote.as_str())));

        let mut aec = EchoCancel::new(NULL_LIBRARY);
        aec.set_on(true);
        aec.reconcile(
            Side::On("t_mic"),
            Side::On("t_stereo"),
            connection,
            std::time::Duration::ZERO,
            Instant::now(),
        );
        assert!(aec.loaded.is_some(), "the null canceller should load");
        aec.source_appeared(70);
        assert_eq!(aec.route("t_mic"), Some(AEC_SOURCE_NODE_NAME));

        // Other speakers: the module is reloaded, and the one just loaded has no source in the
        // registry yet. The old one's is gone with its module, whatever the registry still says.
        aec.reconcile(
            Side::On("t_mic"),
            Side::On("t_71"),
            connection,
            std::time::Duration::ZERO,
            Instant::now(),
        );
        assert_eq!(
            aec.loaded_targets(),
            Some(&targets("t_mic", Some("t_71"))),
            "reloaded for the new speakers"
        );
        assert!(!aec.running());
        assert_eq!(
            aec.route("t_mic"),
            None,
            "a capture stream left on the old canceller's source would be aimed at nothing"
        );
        assert!(
            !aec.source_removed(70),
            "its removal, when the registry gets round to it, has nothing left to take away"
        );

        aec.source_appeared(71);
        assert_eq!(
            aec.route("t_mic"),
            Some(AEC_SOURCE_NODE_NAME),
            "the new canceller's source is recorded from once it is there"
        );

        // Switched off from a control message, which cannot reach the context: unloaded all the
        // same, and its source forgotten with it.
        aec.set_on(false);
        aec.reconcile(
            Side::On("t_mic"),
            Side::On("t_71"),
            None,
            std::time::Duration::ZERO,
            Instant::now(),
        );
        assert!(aec.loaded.is_none());
        assert!(aec.sources.is_empty());
    }

    #[test]
    fn nothing_is_loaded_without_a_context_but_what_is_not_wanted_is_let_go() {
        let mut aec = EchoCancel::new(NULL_LIBRARY);
        aec.set_on(true);
        let now = Instant::now();
        aec.reconcile(
            Side::On("mic"),
            Side::On("speakers"),
            None,
            std::time::Duration::ZERO,
            now,
        );
        assert!(aec.loaded.is_none(), "a control message cannot load");
        assert!(aec.failed_for.is_none(), "and has not failed either");
        assert!(!aec.unload(), "nothing to unload");
        assert!(!aec.route_moved());
    }
}
