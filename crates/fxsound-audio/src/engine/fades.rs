//! The engine's side of the smooth handover (`crate::stream_handover`): the connection it reads and
//! writes application streams' volume through, what it learns there, the clock that wakes it for
//! a handover's next step, and the journal on disk.
//!
//! [`hand_over`] is the primitive; the moves that use it — the power button, the applications'
//! routes, the recorder's rescue — take it up one at a time. Everything around it runs whether or
//! not anything does: the connection, the streams' volumes read, and the journal put right, so a
//! volume a handover left behind is found by the first run that can find it.
//!
//! # A third connection
//!
//! Writing a parameter to a node another client owns makes the server stop reading the writer
//! until the owner has answered (`node_set_param` in `src/pipewire/impl-node.c`,
//! `pw_impl_client_set_busy`) — which is why a lane's own volume already goes through a second
//! connection ([`Session::_volume_core`]). An application's node is the application's, and an
//! application can stop answering. Written on the session's connection, one that did would stop
//! the engine hearing the graph; written on the volume connection, it would stop the lanes
//! telling desktops their volume. So application streams get a connection of their own
//! ([`FadeLine`]), with a `sync` behind every write: one not answered within [`LINE_WATCHDOG`]
//! means somebody is holding it, and the connection is made again ([`watch_line`]). What was held
//! is then whatever it is; the handover goes on without its echo ([`stream_handover::ECHO_WAIT`]).
//!
//! What the handover reads about a stream — whether it plays, the key WirePlumber keeps it under,
//! its `Props` — it reads on the same connection, from a proxy it binds to every application
//! stream there ([`StreamProbe`]). Reading holds nobody: the server answers from what the owner
//! last published. But a stream can go between its announcement and the request that subscribes
//! to its `Props`, and a request to an object the server no longer has is answered with an error
//! on the connection's core — which on the session's connection restarts everything
//! (`a_stream_and_a_client_gone_before_the_main_loop_bound_them_leave_the_connection_alone`), and
//! here is a line in the log.
//!
//! # A clock
//!
//! The handover's steps are tens of milliseconds apart, and the supervisor ticks every 200. So a
//! handover in progress has a thread of its own sleep until its next step and wake the main loop
//! for it ([`attach_clock`]); with none in progress, the thread sleeps until asked, and the engine
//! costs nothing more at rest. The tests, which turn the loop by hand, call [`drive`] instead.
//!
//! # Moves are never lost
//!
//! A handover's move is the caller's: a default written, a target set, a chain switched. Only its
//! fade is the handover's. So a move asked for while another handover is in progress waits its
//! turn, and one the connection went down in the middle of is made there and then, without its
//! fade ([`session_closed`]) — the engine's own state is what the move changes, and it outlives the
//! connection. Only on the way out is a move not made: the exit hands everything back itself
//! ([`restore_before_exit`]).

use std::collections::{HashMap, VecDeque};

use super::*;
use crate::stream_handover::{self, Handover, Journal, Repair, Step, Watched};

/// A handover's move, made once every stream it fades is silent ([`hand_over`]).
pub(super) type Move = Box<dyn FnOnce(&mut Shared)>;

/// How long a write on the [`FadeLine`] may go unanswered before the connection is taken for held
/// by an application that has stopped answering, and made again.
pub(super) const LINE_WATCHDOG: Duration = Duration::from_millis(500);

/// How long the way out waits for the streams a handover left silent to get their volume back.
pub(super) const EXIT_WAIT: Duration = Duration::from_millis(400);

/// The third connection: application streams' volume is read and written through it and nothing
/// else (module docs). Every proxy on it is declared before the core, and so dropped first, and
/// each listener before what it listens to.
pub(super) struct FadeLine {
    /// Every application stream, bound on this connection, by registry id.
    streams: HashMap<u32, StreamProbe>,
    _registry_listener: pw::registry::Listener,
    _listener: pw::core::Listener,
    registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
}

/// One application stream, bound on the [`FadeLine`]: its state, its properties and its `Props`
/// arrive on the listener, and its volume is written through the proxy.
struct StreamProbe {
    _listener: Option<pw::node::NodeListener>,
    node: pw::node::Node,
}

/// Everything the handover keeps between two events. Kept across connections, like the journal it
/// holds; what belongs to one connection is emptied when it closes ([`session_closed`]).
pub(super) struct Fades {
    handover: Handover,
    /// What is known about each application stream's volume, by registry id.
    watched: HashMap<u32, Watched>,
    /// The move of the handover in progress, until it is made.
    then: Option<Move>,
    /// Handovers asked for while one was in progress, in order: their streams and their moves.
    queued: VecDeque<(Vec<u32>, Move)>,
    journal: Journal,
    /// Where the journal is kept; `None` to keep it in memory only, as the tests do unless they
    /// say otherwise.
    journal_path: Option<std::path::PathBuf>,
    /// Streams the journal said were left at 0, whose volume has been written back, until they are
    /// heard at a volume again ([`Repaired`]).
    repairs: HashMap<u32, Repaired>,
    /// The latest `sync` sent on the [`FadeLine`], and when the oldest one not answered yet was
    /// sent: the connection is busy until it comes back.
    pending: Option<(AsyncSeq, Instant)>,
    /// Asks the clock's thread to wake the main loop at an instant ([`attach_clock`]); `None` in
    /// the tests, which call [`drive`] themselves.
    clock: Option<crossbeam_channel::Sender<Instant>>,
    /// The instant the clock was last asked for.
    armed: Option<Instant>,
    /// How many times [`watch_line`] has made the [`FadeLine`] again.
    lines_remade: u32,
    /// Every volume written to an application stream, in order: its id, the level and the ramp.
    #[cfg(test)]
    writes: Vec<(u32, f32, i32)>,
}

/// A stream at 0 whose volume [`repair`] has written back from the journal.
struct Repaired {
    /// The application's key: its line in the journal.
    key: String,
    /// Whether the line comes out once the stream is heard at a volume. Not when a handover was
    /// holding another stream of the same application at 0 as the volume was written: the line is
    /// that stream's too, and only the handover's own [`Step::Forget`] takes it out.
    forget: bool,
}

impl Fades {
    /// No handover, nothing watched, and the journal at `journal_path`, read now.
    pub(super) fn new(journal_path: Option<std::path::PathBuf>) -> Self {
        let journal = journal_path
            .as_deref()
            .map(Journal::load)
            .unwrap_or_default();
        if !journal.is_empty()
            && let Some(path) = &journal_path
        {
            log::info!(
                "{} lists streams a handover left silent; they get their volume back when they \
                 are met",
                path.display()
            );
        }
        Self {
            handover: Handover::default(),
            watched: HashMap::new(),
            then: None,
            queued: VecDeque::new(),
            journal,
            journal_path,
            repairs: HashMap::new(),
            pending: None,
            clock: None,
            armed: None,
            lines_remade: 0,
            #[cfg(test)]
            writes: Vec::new(),
        }
    }

    /// How many times [`watch_line`] has made the [`FadeLine`] again.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn lines_remade(&self) -> u32 {
        self.lines_remade
    }

    /// Every volume written to an application stream, in order: its id, the level and the ramp in
    /// milliseconds.
    #[cfg(test)]
    pub(super) fn writes(&self) -> &[(u32, f32, i32)] {
        &self.writes
    }

    /// Whether a handover is in progress.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) const fn busy(&self) -> bool {
        self.handover.busy()
    }

    /// What is known about the stream under `id`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn watched(&self, id: u32) -> Option<&Watched> {
        self.watched.get(&id)
    }

    /// Every master volume known, in no order.
    #[cfg(test)]
    pub(super) fn levels(&self) -> impl Iterator<Item = f32> + '_ {
        self.watched.values().filter_map(|watched| watched.level)
    }

    /// Whether the journal is empty.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn journal_is_empty(&self) -> bool {
        self.journal.is_empty()
    }

    fn save_journal(&self) {
        let Some(path) = &self.journal_path else {
            return;
        };
        if let Err(error) = self.journal.save(path) {
            log::warn!("could not write {}: {error}", path.display());
        }
    }

    /// Ask the clock for the handover's next step, if it has one and it is not asked for already.
    fn arm(&mut self) {
        let next = self.handover.next_deadline();
        if next == self.armed {
            return;
        }
        self.armed = next;
        if let (Some(next), Some(clock)) = (next, &self.clock) {
            let _ = clock.send(next);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------------------------

/// Make the [`FadeLine`]: a connection to the same server, with a registry to bind streams on and a
/// listener for the `done` of its `sync`s.
///
/// Whether the server ramps a volume is set in `volume_ramps` ([`Shared::volume_ramps`]) from this
/// connection's own core info too. The session's says the same, but on another socket, in no set
/// order with this one; this one's comes ahead of its registry, and so ahead of every stream a
/// repair ([`repair`]) may have to ramp.
pub(super) fn fade_line(
    shared: &Rc<RefCell<Shared>>,
    context: &pw::context::ContextRc,
    remote: Option<String>,
    volume_ramps: Rc<Cell<bool>>,
) -> Result<FadeLine, pw::Error> {
    let props = remote.map(|name| {
        properties! {
            *pw::keys::REMOTE_NAME => name,
        }
    });
    let core = context.connect_rc(props)?;
    let listener = core
        .add_listener_local()
        .info(move |info| volume_ramps.set(ramps_volume(info.version())))
        .done({
            let shared = Rc::clone(shared);
            move |id, seq| {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                if let Ok(mut guard) = shared.try_borrow_mut()
                    && guard
                        .fades
                        .pending
                        .is_some_and(|(pending, _)| pending == seq)
                {
                    guard.fades.pending = None;
                }
            }
        })
        .error(|id, _seq, res, message| {
            // A stream that went before its proxy did, most likely: nothing to do about it.
            log::debug!("the handover's connection: error on object {id}: {message} ({res})");
        })
        .register();
    let registry = core.get_registry_rc()?;
    let registry_listener = registry
        .add_listener_local()
        .global({
            let shared = Rc::clone(shared);
            let registry = registry.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                if StreamNode::from_props(global.id, &|key: &str| props.get(key)).is_none() {
                    return;
                }
                let probe = probe_stream(&shared, &registry, global);
                if let Ok(mut guard) = shared.try_borrow_mut()
                    && let Some(probe) = probe
                    && let Some(line) = guard
                        .session
                        .as_mut()
                        .and_then(|session| session.fade_line.as_mut())
                {
                    line.streams.insert(global.id, probe);
                    // Watched from now, before anything of it is in: until its properties and
                    // volume are, it may be the stream a handover left at 0, and a line in the
                    // journal waits for it ([`Journal::outlived`]).
                    guard.fades.watched.entry(global.id).or_default();
                }
            }
        })
        .global_remove({
            let shared = Rc::clone(shared);
            move |id| {
                // The session's registry says the same, and ends a handover's part in the stream
                // ([`stream_removed`]); this one lets go of the proxy, and of what its listener
                // learned — which may have come in after the session's registry let go of it.
                if let Ok(mut guard) = shared.try_borrow_mut()
                    && let Some(line) = guard
                        .session
                        .as_mut()
                        .and_then(|session| session.fade_line.as_mut())
                {
                    line.streams.remove(&id);
                    if guard.fades.watched.remove(&id).is_some() {
                        drop_outlived(&mut guard);
                    }
                }
            }
        })
        .register();
    Ok(FadeLine {
        streams: HashMap::new(),
        _registry_listener: registry_listener,
        _listener: listener,
        registry,
        core,
    })
}

/// Bind an application stream on the [`FadeLine`] and listen for what the handover reads of it:
/// whether it plays, its properties, and its `Props`, subscribed to.
fn probe_stream(
    shared: &Rc<RefCell<Shared>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&libspa::utils::dict::DictRef>,
) -> Option<StreamProbe> {
    let id = global.id;
    let node = registry
        .bind::<pw::node::Node, _>(global)
        .inspect_err(|error| log::debug!("could not bind stream {id} to fade it: {error}"))
        .ok()?;
    let listener = node
        .add_listener_local()
        .info({
            let shared = Rc::clone(shared);
            move |info| {
                let Ok(mut guard) = shared.try_borrow_mut() else {
                    return;
                };
                if info.change_mask().contains(pw::node::NodeChangeMask::STATE) {
                    let running = matches!(info.state(), pw::node::NodeState::Running);
                    stream_state(&mut guard, id, running);
                }
                if info.change_mask().contains(pw::node::NodeChangeMask::PROPS)
                    && let Some(props) = info.props()
                {
                    stream_props(&mut guard, id, &|key: &str| props.get(key));
                }
            }
        })
        .param({
            let shared = Rc::clone(shared);
            move |_seq, param_type, _index, _next, param| {
                if param_type != libspa::param::ParamType::Props {
                    return;
                }
                let Some(update) = param.and_then(PropsUpdate::from_pod) else {
                    return;
                };
                if let Ok(mut guard) = shared.try_borrow_mut() {
                    stream_volume(
                        &mut guard,
                        id,
                        update.volume,
                        update.locked.unwrap_or(false),
                    );
                }
            }
        })
        .register();
    node.subscribe_params(&[libspa::param::ParamType::Props]);
    Some(StreamProbe {
        _listener: Some(listener),
        node,
    })
}

/// Write `level` to the master volume of the application stream under `id`, over `ramp_ms` (at
/// once for 0), through the [`FadeLine`], and put a `sync` behind it. Whether the write was sent.
fn write_volume(shared: &mut Shared, id: u32, level: f32, ramp_ms: i32) -> bool {
    let Some(line) = shared
        .session
        .as_mut()
        .and_then(|session| session.fade_line.as_mut())
    else {
        return false;
    };
    let probe = match line.streams.entry(id) {
        std::collections::hash_map::Entry::Occupied(bound) => bound.into_mut(),
        std::collections::hash_map::Entry::Vacant(slot) => {
            // Not announced on this connection yet — made again a moment ago by [`watch_line`] —
            // so bound by id: the server finds the object whether or not this connection's
            // registry has announced it, and the session's registry has.
            let global = pw::registry::GlobalObject::<&libspa::utils::dict::DictRef> {
                id,
                permissions: pw::permissions::PermissionFlags::empty(),
                type_: pw::types::ObjectType::Node,
                version: 0,
                props: None,
            };
            match line.registry.bind::<pw::node::Node, _>(&global) {
                Ok(node) => slot.insert(StreamProbe {
                    _listener: None,
                    node,
                }),
                Err(error) => {
                    log::warn!("could not reach stream {id} to fade it: {error}");
                    return false;
                }
            }
        }
    };
    let node = &probe.node;
    let bytes = volume::master_volume_pod(level, ramp_ms);
    let Some(pod) = Pod::from_bytes(&bytes) else {
        log::warn!("could not build a Props pod for a master volume of {level}");
        return false;
    };
    node.set_param(libspa::param::ParamType::Props, 0, pod);
    #[cfg(test)]
    shared.fades.writes.push((id, level, ramp_ms));
    match line.core.sync(0) {
        Ok(seq) => {
            let since = shared
                .fades
                .pending
                .map_or_else(Instant::now, |(_, since)| since);
            shared.fades.pending = Some((seq, since));
        }
        Err(error) => log::debug!("could not follow the handover's write with a sync: {error}"),
    }
    true
}

/// The supervisor's check on the [`FadeLine`]: one whose writes have gone unanswered for
/// [`LINE_WATCHDOG`] is held by an application that stopped answering, and is made again.
pub(super) fn watch_line(
    shared: &mut Shared,
    handle: &Rc<RefCell<Shared>>,
    context: &pw::context::ContextRc,
    now: Instant,
) {
    let Some((_, since)) = shared.fades.pending else {
        return;
    };
    if now < since + LINE_WATCHDOG || shared.session.is_none() {
        return;
    }
    log::warn!(
        "an application has not answered a volume the handover wrote to it for {LINE_WATCHDOG:?}; \
         connecting again for the next"
    );
    shared.fades.pending = None;
    shared.fades.lines_remade += 1;
    let line = fade_line(
        handle,
        context,
        shared.remote.clone(),
        Rc::clone(&shared.volume_ramps),
    )
    .inspect_err(|error| log::warn!("could not connect again for the handover: {error}"))
    .ok();
    if let Some(session) = shared.session.as_mut() {
        session.fade_line = line;
    }
}

// ---------------------------------------------------------------------------------------------
// What the engine hears
// ---------------------------------------------------------------------------------------------

/// An application stream's properties arrived: the key WirePlumber keeps its volume under, and its
/// serial.
fn stream_props<'a>(shared: &mut Shared, id: u32, get: &impl Fn(&str) -> Option<&'a str>) {
    let watched = shared.fades.watched.entry(id).or_default();
    watched.key = stream_handover::state_key(get);
    watched.serial = get("object.serial").and_then(|serial| serial.parse().ok());
    watched.known = true;
    repair(shared, id);
    // Until now the stream could have been the one another application's line waits for.
    drop_outlived(shared);
}

/// An application stream's state changed: whether it is `running`.
fn stream_state(shared: &mut Shared, id: u32, running: bool) {
    shared.fades.watched.entry(id).or_default().running = running;
}

/// An application stream's `Props` arrived — its master volume, and whether its converter locks
/// it — on subscription, and again on every change: a write of ours echoed, or anybody's.
fn stream_volume(shared: &mut Shared, id: u32, level: Option<f32>, locked: bool) {
    let now = Instant::now();
    let watched = shared.fades.watched.entry(id).or_default();
    watched.level = level;
    watched.locked = locked;
    if shared.fades.handover.holds(id) {
        let steps = shared.fades.handover.volume(id, level, now);
        apply(shared, steps);
        return;
    }
    if shared.fades.repairs.contains_key(&id) {
        if level.is_some_and(|level| !stream_handover::silent(level))
            && let Some(Repaired { key, forget }) = shared.fades.repairs.remove(&id)
        {
            log::info!("{key} plays again at its volume from before the handover");
            // Not while another stream of the application may still be at 0: one a handover since
            // holds there, one repaired a moment ago whose echo is still on its way, one not known
            // yet.
            if forget {
                drop_if_outlived(shared, &key);
            }
        }
        return;
    }
    repair(shared, id);
}

/// A link appeared from the node `output` to the node `input`: a new link of a stream a handover
/// has moved ends its move.
pub(super) fn link_appeared(shared: &mut Shared, output: u32, input: u32) {
    let now = Instant::now();
    for id in [output, input] {
        if shared.fades.handover.holds(id) {
            let steps = shared.fades.handover.linked(id, now);
            apply(shared, steps);
        }
    }
}

/// An application stream left the graph.
pub(super) fn stream_removed(shared: &mut Shared, id: u32) {
    if shared.fades.watched.remove(&id).is_some() {
        // Gone, it is no longer a stream a line may be waiting for.
        drop_outlived(shared);
    }
    shared.fades.repairs.remove(&id);
    if let Some(line) = shared
        .session
        .as_mut()
        .and_then(|session| session.fade_line.as_mut())
    {
        line.streams.remove(&id);
    }
    if shared.fades.handover.holds(id) {
        let steps = shared.fades.handover.removed(id, Instant::now());
        apply(shared, steps);
    }
}

/// What the journal says about the stream under `id`, now that more is known of it: a volume a
/// handover left at 0 goes back — over the ramp if the stream is playing, at once if not — and a
/// line that has outlived its stream goes ([`drop_if_outlived`]).
///
/// A stream met at a volume of its own takes the line out only once no stream of the application
/// can still be the one left at 0. A handover fades only the streams that play: a browser's paused
/// tab, at its own volume, says nothing of the tab faded beside it — which a run started after a
/// killed one may meet a moment later, in no set order with the paused one.
///
/// While a handover holds a stream of the same application at 0, the line is that stream's: the
/// stream met here is another one of the application — a second tab, one WirePlumber restored —
/// and what it plays at says nothing of the faded one. Found at 0 (WirePlumber gave it the 0 it
/// kept for the faded one), it still gets the volume back; the line stays for the handover to take
/// out ([`Repaired::forget`]).
fn repair(shared: &mut Shared, id: u32) {
    if shared.fades.handover.holds(id) || shared.fades.repairs.contains_key(&id) {
        return;
    }
    let Some(watched) = shared.fades.watched.get(&id) else {
        return;
    };
    let Some(key) = watched.key.clone() else {
        return;
    };
    let held = shared.fades.handover.holds_key(&key);
    match shared.fades.journal.repair(&key, watched.level) {
        Repair::Nothing => {}
        Repair::Obsolete => drop_if_outlived(shared, &key),
        Repair::Restore(level) => {
            // Not the handover's word on the ramp: that is learned when a handover begins, and a
            // repair comes before any — on the very first connection of a run after a killed one.
            // A write with no line to go through is not sent at all ([`write_volume`]).
            let ramp = if shared.volume_ramps.get() && watched.running {
                stream_handover::RAMP_MS
            } else {
                0
            };
            let serial = shared.fades.journal.serial(&key);
            if write_volume(shared, id, level, ramp) {
                log::info!(
                    "{key} is at 0, where a handover left {}; putting {level} back",
                    serial.map_or_else(
                        || "one of its streams".to_owned(),
                        |serial| format!("stream {serial}")
                    )
                );
                shared
                    .fades
                    .repairs
                    .insert(id, Repaired { key, forget: !held });
            }
        }
    }
}

/// Take the line for `key` out of the journal if it has outlived the stream it was written for
/// ([`Journal::outlived`]). Never while a handover holds a stream of the application at 0: the line
/// is that stream's, and the handover takes it out itself.
fn drop_if_outlived(shared: &mut Shared, key: &str) {
    if shared.fades.handover.holds_key(key)
        || !shared
            .fades
            .journal
            .outlived(key, shared.fades.watched.values())
    {
        return;
    }
    log::info!("{key} plays at a volume of its own; its handover line is dropped");
    shared.fades.journal.forget(key);
    shared.fades.save_journal();
}

/// [`drop_if_outlived`] for every line in the journal: a stream that went, or became known, may
/// have been the last one a line waited for.
fn drop_outlived(shared: &mut Shared) {
    if shared.fades.journal.is_empty() {
        return;
    }
    let keys: Vec<String> = shared.fades.journal.keys().map(str::to_owned).collect();
    for key in keys {
        drop_if_outlived(shared, &key);
    }
}

// ---------------------------------------------------------------------------------------------
// The handover
// ---------------------------------------------------------------------------------------------

/// Hand the application streams under `streams` over: fade each one that plays to silence, make
/// `then` once they are all silent, and give each its volume back once it has its new link
/// (`crate::stream_handover`). With nothing to fade — no stream plays, the server has no ramp, no
/// connection to write through — `then` is made before this returns. While another handover is in
/// progress this one waits for it to finish.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn hand_over(shared: &mut Shared, streams: Vec<u32>, then: Move) {
    if shared.fades.handover.busy() {
        shared.fades.queued.push_back((streams, then));
        return;
    }
    begin(shared, &streams, then);
}

fn begin(shared: &mut Shared, streams: &[u32], then: Move) {
    let now = Instant::now();
    let writable = shared
        .session
        .as_ref()
        .is_some_and(|session| session.fade_line.is_some());
    shared
        .fades
        .handover
        .set_ramps(writable && shared.volume_ramps.get());
    let candidates: Vec<(u32, Watched)> = streams
        .iter()
        .filter_map(|&id| {
            shared
                .fades
                .watched
                .get(&id)
                .map(|watched| (id, watched.clone()))
        })
        .collect();
    let candidates: Vec<(u32, &Watched)> = candidates
        .iter()
        .map(|(id, watched)| (*id, watched))
        .collect();
    shared.fades.then = Some(then);
    let steps = shared.fades.handover.begin(&candidates, now);
    apply(shared, steps);
}

/// Do what the handover says, in order, and ask the clock for its next step.
fn apply(shared: &mut Shared, steps: Vec<Step>) {
    for step in steps {
        match step {
            Step::Volume { id, level } => {
                write_volume(shared, id, level, stream_handover::RAMP_MS);
            }
            Step::Remember { key, serial, level } => {
                if shared.fades.journal.remember(&key, serial, level) {
                    shared.fades.save_journal();
                }
            }
            Step::Forget { key } => {
                if shared.fades.journal.forget(&key) {
                    shared.fades.save_journal();
                }
            }
            Step::Move => {
                if let Some(then) = shared.fades.then.take() {
                    then(shared);
                }
            }
            Step::Finished => {
                // Left only by a handover the engine is leaving: the exit hands back instead.
                shared.fades.then = None;
                if let Some((streams, then)) = shared.fades.queued.pop_front() {
                    begin(shared, &streams, then);
                }
            }
        }
    }
    shared.fades.arm();
}

/// What time has made due in the handover: the clock's wake-up, the supervisor's tick, and the
/// tests' turns of the loop.
pub(super) fn drive(shared: &mut Shared) {
    let steps = shared.fades.handover.tick(Instant::now());
    apply(shared, steps);
}

/// Start the clock's thread (module docs, "A clock"), and have what it sends wake the main loop and
/// [`drive`] the handover. The thread ends when `shared` lets go of its end of the channel.
pub(super) fn attach_clock<'l>(
    loop_: &'l pw::loop_::Loop,
    shared: &Rc<RefCell<Shared>>,
) -> pw::channel::AttachedReceiver<'l, ()> {
    let (wake, woken) = pw::channel::channel::<()>();
    let attached = woken.attach(loop_, {
        let shared = Rc::clone(shared);
        move |()| {
            if let Ok(mut guard) = shared.try_borrow_mut() {
                guard.fades.armed = None;
                drive(&mut guard);
            }
        }
    });
    let (ask, asked) = crossbeam_channel::unbounded::<Instant>();
    let spawned = std::thread::Builder::new()
        .name("fxsound-fades".to_owned())
        .spawn(move || tick_at(&asked, || wake.send(()).is_ok()));
    match spawned {
        Ok(_) => shared.borrow_mut().fades.clock = Some(ask),
        Err(error) => log::warn!(
            "could not start the handover's clock ({error}); its steps wait for the supervisor"
        ),
    }
    attached
}

/// The clock's thread: sleep until the earliest instant asked for, then `wake` the main loop; and
/// until asked, when nothing is. An instant asked for while another is due replaces it if it is
/// sooner; a later one is left for the step woken for to ask again ([`Fades::arm`]). Returns when
/// nobody can ask any more, or `wake` says nobody is woken any more.
fn tick_at(asked: &crossbeam_channel::Receiver<Instant>, wake: impl Fn() -> bool) {
    let mut due: Option<Instant> = None;
    loop {
        let Some(at) = due else {
            match asked.recv() {
                Ok(at) => due = Some(at),
                Err(_) => return,
            }
            continue;
        };
        match asked.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Ok(sooner) => due = Some(sooner.min(at)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                due = None;
                if !wake() {
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Stop the clock's thread: the engine is done with it.
pub(super) fn stop_clock(shared: &mut Shared) {
    shared.fades.clock = None;
}

/// The connection is closing: its streams and its handover go with it. The moves not made yet are
/// made now, without their fades (module docs, "Moves are never lost"); the journal keeps its lines
/// for the next connection to find.
pub(super) fn session_closed(shared: &mut Shared) {
    // A move may ask for another handover; on this connection that one is abandoned too, and its
    // move made in the next round.
    loop {
        shared.fades.handover.abandon();
        let mut moves: Vec<Move> = shared.fades.then.take().into_iter().collect();
        moves.extend(shared.fades.queued.drain(..).map(|(_, then)| then));
        if moves.is_empty() {
            break;
        }
        for then in moves {
            then(shared);
        }
    }
    shared.fades.watched.clear();
    shared.fades.repairs.clear();
    shared.fades.pending = None;
    shared.fades.armed = None;
}

/// The way out: every stream a handover left silent gets its volume back, and the loop is turned
/// until the server has it or [`EXIT_WAIT`] is up — before the defaults are handed back and the
/// connection goes. The moves not made yet are not made: the exit hands everything back itself.
///
/// Called like [`release_defaults_before_exit`]: after `run()`, with nothing else attached.
pub(super) fn restore_before_exit(shared: &Rc<RefCell<Shared>>, loop_: &pw::loop_::Loop) {
    {
        let mut guard = shared.borrow_mut();
        stop_clock(&mut guard);
        guard.fades.queued.clear();
        if !guard.fades.handover.busy() {
            return;
        }
        let steps = guard.fades.handover.restore_all(Instant::now());
        apply(&mut guard, steps);
    }
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        {
            let mut guard = shared.borrow_mut();
            drive(&mut guard);
            if !guard.fades.handover.busy() && guard.fades.pending.is_none() {
                log::debug!("every stream the handover left silent has its volume back");
                break;
            }
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            log::warn!(
                "a stream the handover left silent did not get its volume back within \
                 {EXIT_WAIT:?}; the journal keeps it for the next start"
            );
            break;
        };
        loop_.iterate(pw::loop_::Timeout::Finite(
            remaining.min(Duration::from_millis(10)),
        ));
    }
    let mut guard = shared.borrow_mut();
    guard.fades.then = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a test waits for a wake-up that should come: far past any instant asked for here.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// The clock's thread, started on channels of the test's own: what is asked of it, the instant
    /// of each wake-up it gives, and the thread.
    fn clock() -> (
        crossbeam_channel::Sender<Instant>,
        crossbeam_channel::Receiver<Instant>,
        std::thread::JoinHandle<()>,
    ) {
        let (ask, asked) = crossbeam_channel::unbounded::<Instant>();
        let (woke, wakes) = crossbeam_channel::unbounded::<Instant>();
        let thread =
            std::thread::spawn(move || tick_at(&asked, || woke.send(Instant::now()).is_ok()));
        (ask, wakes, thread)
    }

    /// Wait for `thread` to end, for at most [`PATIENCE`]. Whether it did.
    fn ends(thread: &std::thread::JoinHandle<()>) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn the_clock_wakes_the_loop_once_at_the_instant_asked_for_and_not_before() {
        let (ask, wakes, thread) = clock();
        let at = Instant::now() + Duration::from_millis(40);
        ask.send(at).expect("the clock listens");
        let woke = wakes
            .recv_timeout(PATIENCE)
            .expect("the clock never woke the loop");
        assert!(woke >= at, "woken {:?} early", at - woke);
        assert!(
            wakes.recv_timeout(Duration::from_millis(100)).is_err(),
            "woken twice for one instant"
        );
        drop(ask);
        assert!(ends(&thread));
    }

    #[test]
    fn a_sooner_instant_asked_for_replaces_a_later_one_and_a_later_one_does_not_put_off_a_sooner() {
        let (ask, wakes, thread) = clock();
        let start = Instant::now();
        let later = start + Duration::from_secs(3);
        let sooner = start + Duration::from_millis(40);
        ask.send(later).expect("the clock listens");
        ask.send(sooner).expect("the clock listens");
        let woke = wakes
            .recv_timeout(PATIENCE)
            .expect("the clock never woke the loop");
        assert!(woke >= sooner, "woken {:?} early", sooner - woke);
        assert!(woke < later, "the sooner instant waited for the later one");

        let sooner = Instant::now() + Duration::from_millis(40);
        ask.send(sooner).expect("the clock listens");
        ask.send(sooner + Duration::from_secs(3))
            .expect("the clock listens");
        let woke = wakes
            .recv_timeout(PATIENCE)
            .expect("the clock never woke the loop");
        assert!(woke >= sooner, "woken {:?} early", sooner - woke);
        assert!(
            woke < sooner + Duration::from_secs(1),
            "a later instant put off the sooner one"
        );
        drop(ask);
        assert!(ends(&thread));
    }

    #[test]
    fn the_clock_ends_when_nobody_can_ask_it_any_more_even_with_an_instant_due() {
        let (ask, _wakes, thread) = clock();
        drop(ask);
        assert!(
            ends(&thread),
            "the clock outlived an engine that asked nothing"
        );

        let (ask, _wakes, thread) = clock();
        ask.send(Instant::now() + Duration::from_secs(60))
            .expect("the clock listens");
        std::thread::sleep(Duration::from_millis(20));
        drop(ask);
        assert!(
            ends(&thread),
            "the clock waited out an instant nobody needed"
        );
    }

    #[test]
    fn the_clock_ends_when_the_loop_it_wakes_is_gone() {
        let (ask, asked) = crossbeam_channel::unbounded::<Instant>();
        let thread = std::thread::spawn(move || tick_at(&asked, || false));
        ask.send(Instant::now()).expect("the clock listens");
        assert!(
            ends(&thread),
            "the clock kept going for a loop that is gone"
        );
    }
}
