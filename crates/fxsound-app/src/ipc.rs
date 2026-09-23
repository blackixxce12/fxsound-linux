//! Single-instance enforcement and command forwarding.
//!
//! `FxSoundApplication::moreThanOneInstanceAllowed()` returns `false` (`fxsound/Source/Main.cpp:46`)
//! and JUCE therefore hands a second process's raw command line to the first one, which feeds it
//! straight to `FxController::applyConfig` (`Main.cpp:136-139`). The transport lives inside JUCE
//! and is not in this tree; the observable contract — the second process exits immediately, the
//! payload is the whole command line, there is no reply channel — is documented from the client
//! side by the in-tree Go client (`fxmcp/internal/fxsound/process.go:84-87`).
//!
//! # What this replaces it with
//!
//! A `SOCK_STREAM` unix socket plus an `flock`ed lock file, both in
//! `$XDG_RUNTIME_DIR/fxsound/` (`docs/spec/07-startup-tray.md` §3.3):
//!
//! ```text
//! $XDG_RUNTIME_DIR/fxsound/instance.lock   flock(LOCK_EX|LOCK_NB), holds "<pid>\n"
//! $XDG_RUNTIME_DIR/fxsound/instance.sock   one JSON line per request, one per reply
//! ```
//!
//! The lock, not the socket, is what decides who is primary. That is the whole answer to the
//! stale-socket problem a crash leaves behind: the kernel drops an `flock` when the owning process
//! dies, so whoever *takes* the lock knows by construction that any socket file still sitting
//! there is dead, and can unlink and rebind it without a liveness probe and without racing a
//! second starter doing the same.
//!
//! # Why argv and not a command line
//!
//! The request frame carries real `argv`. JUCE re-derives the tail of `GetCommandLineW()` and
//! re-tokenises it with a quote-toggling state machine that has no backslash escapes, which is why
//! the Go client has to build the raw command line itself and *reject* any value containing a `"`
//! as an argument-injection path (`fxmcp/internal/fxsound/config.go:31-45`). Passing argv deletes
//! that entire class of bug, and a preset name may contain a quote again.
//!
//! # Why there is a reply
//!
//! `--status` on Windows answers out of band: it writes `status.json` and then does
//! `AttachConsole(ATTACH_PARENT_PROCESS)` to print to the *running* instance's original console,
//! explicitly not the caller's (`FxController.cpp:686-699`), which is why the Go client polls the
//! file's mtime with a 2 s budget (`fxmcp/internal/fxsound/status.go:95-128`). Here the primary
//! answers on the socket and the forwarding process prints it on its own stdout.
//!
//! # Security
//!
//! `$XDG_RUNTIME_DIR` is created by the session manager mode `0700` and owned by the user, and the
//! `fxsound/` subdirectory is created `0700` as well, so the directory permissions are the access
//! control. `SO_PEERCRED`, which `docs/spec/07-startup-tray.md` §3.3 asks for, guards an *abstract*
//! socket — those live in the network namespace and are reachable by every uid on the machine.
//! This implementation deliberately does not use an abstract socket, and `UnixStream::peer_cred`
//! is still unstable (`peer_credentials_unix_socket`, rust-lang/rust#42839), so the check is left
//! to the filesystem.
//!
//! # One thread per caller
//!
//! Every accepted connection is answered on a short-lived thread of its own, and a request line
//! longer than [`MAX_REQUEST_BYTES`] is refused unread. Until 0.4.0 the accept loop read each
//! request itself, so a caller that connected and then said nothing held every compositor keybind
//! behind it for the whole five-second request timeout.
//!
//! At most `MAX_CONNECTIONS` (32) callers are answered at once; the next one is told to try again
//! before its request is read. The primary hangs up on a refused caller without reading what it
//! sent, so the client reads the answer even when writing its request fails, and prints the
//! reason rather than a broken pipe. A `--watch` stream gives its place back as soon as it is
//! subscribed, and counts against `MAX_SUBSCRIBERS` (64) instead.
//!
//! # Watching
//!
//! A request with `watch` set (`fxsound --watch`, 0.4.0 design §10) is not answered once. The
//! connection is handed to a broadcaster and stays open: it is sent the `--status --json` document
//! as a `status` event, and then one line per [`AppEvent`] that [`Server::publish`] is given, as
//! JSON or as `event key=value` depending on the caller's `--json`. The primary never reads from
//! it again; the stream ends when the instance quits, or when the caller stops reading — a write
//! that does not go through within [`WATCH_WRITE_TIMEOUT`] drops the subscriber, since half a line
//! cannot be taken back. `input_meters` goes only to callers that asked for `--meters`, and to
//! each at most every [`METER_INTERVAL`].
//!
//! The status document is fetched *after* the subscriber is registered, and whatever is published
//! meanwhile is held and written after it. Every event carries absolute values rather than
//! deltas, so an event from just before the document was taken repeats what the document already
//! says, and one from just after it is not lost.
//!
//! # The other caller
//!
//! The D-Bus service (`crate::dbus`, 0.4.0 design §9) does not go through the socket: it hands
//! its command lists to the same channel through a [`Control`], and they are drained, carried out
//! and answered exactly like a forwarded command line, under the same [`HANDLER_TIMEOUT`].
//!
//! # Waking the GUI thread
//!
//! Whatever is put on that channel — a forwarded command line, a D-Bus call, a new subscriber's
//! request for the status document — wakes the GUI thread through the [`crate::wake::Waker`] the
//! server was started with ([`Listener::serve_waking`]), so it is answered at once rather than at
//! the pump's next keepalive (0.4.0 design §12). The headless pump waits on the channel itself
//! ([`Server::arrivals`]).

use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser as _;
use crossbeam_channel::{Receiver, RecvTimeoutError, bounded, unbounded};
use serde::{Deserialize, Serialize};

use crate::cli::{Cli, Command};
use crate::events::{AppEvent, EventSink};
use crate::wake::{Waker, WakingSender};

/// Frame version. Bump when the shape of [`Request`] or [`Response`] changes incompatibly; a
/// primary rejects anything it does not recognise rather than guessing.
pub const PROTOCOL_VERSION: u32 = 1;

/// Name of the socket inside the runtime directory.
pub const SOCKET_NAME: &str = "instance.sock";
/// Name of the lock file inside the runtime directory.
pub const LOCK_NAME: &str = "instance.lock";

/// How long a forwarding process waits for the primary's reply.
///
/// `fxmcp/internal/fxsound/config.go:12-18` budgets 5 s for an apply, so a client written against
/// the Windows build already expects this much.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the primary waits for a connected client to finish sending its request before hanging
/// up on it, so that a wedged client cannot stall the accept loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the primary gives the GUI thread to answer a forwarded command before replying with a
/// bare acknowledgement. Deliberately shorter than [`REPLY_TIMEOUT`] so the client hears *us*
/// rather than its own timeout. A D-Bus method call waits exactly as long ([`Control::call`]).
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(4);

/// What a caller hears when the GUI thread no longer takes commands.
const SHUTTING_DOWN: &str = "FxSound is shutting down and cannot take that command";

/// What a caller hears when the GUI thread took a command and did not answer within
/// [`HANDLER_TIMEOUT`].
const NO_ANSWER_IN_TIME: &str = "FxSound did not answer in time; the command may still be running";

/// The longest request line the primary reads. An argv is a few hundred bytes; anything near this
/// is not a command line, and reading it without a bound is how a stray writer eats the heap.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// How many callers may be in the middle of being answered at once. Each has a thread for at most
/// `REQUEST_TIMEOUT` plus `HANDLER_TIMEOUT`; past this the caller is told to try again. The
/// D-Bus service holds its calls in flight to the same number (`crate::dbus`).
pub const MAX_CONNECTIONS: usize = 32;

/// What a caller past [`MAX_CONNECTIONS`] hears, on the socket and on the bus.
pub const TOO_MANY_CALLERS: &str = "FxSound is answering too many callers at once; try again";

/// What a caller hears when its request line does not arrive whole within [`REQUEST_TIMEOUT`].
const REQUEST_TOO_SLOW: &str = "the request did not arrive in time";

/// How many `--watch` streams may be open at once.
const MAX_SUBSCRIBERS: usize = 64;

/// How long [`Server::publish`] waits for one subscriber to take a line. It runs on the GUI
/// thread, and a subscriber whose socket buffer is full has not read for hundreds of events.
pub const WATCH_WRITE_TIMEOUT: Duration = Duration::from_millis(50);

/// `--meters`: at most four `input_meters` events a second to each subscriber that asked.
pub const METER_INTERVAL: Duration = Duration::from_millis(250);

/// Lines held for a subscriber while its status document is being fetched. More than a few
/// seconds' worth of every event but the meters.
const MAX_BACKLOG: usize = 256;

/// A request frame: one line of JSON, one per connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// [`PROTOCOL_VERSION`].
    pub v: u32,
    /// The forwarding process's real `argv`, `argv[0]` included.
    pub argv: Vec<String>,
    /// The forwarding process's working directory, so a relative path in an argument can still be
    /// resolved. The Windows build cannot do this at all — it `chdir`s to the exe directory at
    /// startup (`Main.cpp:308-321`), which this port does not port (§2.3).
    pub cwd: String,
    /// Subscribe to the event stream rather than be answered once. `argv` must then be a
    /// `--watch` line, whose `--json` picks the stream's spelling. Left out of every other frame,
    /// so a plain command line is byte for byte what 0.3.0 sent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub watch: bool,
    /// With `watch`: stream the microphone's meters too (`--meters`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub meters: bool,
}

/// `skip_serializing_if` hands the field over by reference.
const fn is_false(value: &bool) -> bool {
    !*value
}

/// A reply frame: one line of JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// [`PROTOCOL_VERSION`].
    pub v: u32,
    /// `false` when the command line did not parse or the primary refused it.
    pub ok: bool,
    /// What the forwarding process should print on stdout — the `--status` JSON, normally empty.
    pub stdout: String,
    /// What it should print on stderr, normally empty.
    pub stderr: String,
}

impl Response {
    /// An empty success.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// A success carrying output for the caller's stdout.
    #[must_use]
    pub fn output(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            ..Self::ok()
        }
    }

    /// A refusal carrying a diagnostic for the caller's stderr.
    #[must_use]
    pub fn failed(stderr: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// What a caller hears when the GUI thread took its command and did not answer within
    /// [`HANDLER_TIMEOUT`]: failed, but not refused — the command may still be running.
    #[must_use]
    pub fn unanswered() -> Self {
        Self::failed(NO_ANSWER_IN_TIME)
    }

    /// Whether this is [`Response::unanswered`] rather than an answer: D-Bus reports that as a
    /// failure, and every other `ok = false` as FxSound refusing the command.
    #[must_use]
    pub fn is_unanswered(&self) -> bool {
        !self.ok && self.stderr == NO_ANSWER_IN_TIME
    }

    /// The process exit code the forwarding process should use.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.ok { 0 } else { 1 }
    }
}

/// Which of the two roles this process has.
pub enum Instance {
    /// We took the lock: we are the application. Call [`Listener::serve`] to start accepting
    /// forwarded command lines.
    Primary(Listener),
    /// Somebody else holds the lock. Call [`Client::forward`] and exit, exactly as the second
    /// Windows process does (`fxmcp/internal/fxsound/process.go:84-87`).
    Secondary(Client),
}

impl Instance {
    /// Decide the role using `$XDG_RUNTIME_DIR/fxsound`.
    ///
    /// # Errors
    ///
    /// If the runtime directory cannot be created, or the lock file cannot be opened, or the
    /// socket is stale but cannot be replaced.
    pub fn acquire() -> io::Result<Self> {
        Self::acquire_in(&runtime_dir())
    }

    /// Decide the role using an explicit directory. Tests use this; so would a second profile.
    ///
    /// # Errors
    ///
    /// As [`Instance::acquire`].
    pub fn acquire_in(dir: &Path) -> io::Result<Self> {
        prepare_dir(dir)?;
        let socket = dir.join(SOCKET_NAME);
        let lock_path = dir.join(LOCK_NAME);

        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        match lock.try_lock() {
            Ok(()) => {
                // We hold the lock, so nothing is listening on that socket however alive the inode
                // looks: it is a leftover from a process that died without unlinking it.
                match fs::remove_file(&socket) {
                    Ok(()) => log::info!("removed a stale control socket at {}", socket.display()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                let listener = UnixListener::bind(&socket)?;
                record_pid(&lock);
                Ok(Self::Primary(Listener {
                    listener,
                    lock,
                    path: socket,
                }))
            }
            Err(TryLockError::WouldBlock) => Ok(Self::Secondary(Client { path: socket })),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

/// The bound socket of the primary instance, before the accept loop starts.
///
/// Kept as its own step so that a caller can bind early — the point of single-instance
/// enforcement is to lose the race before doing any expensive startup work — and only start
/// serving once there is something to serve.
pub struct Listener {
    listener: UnixListener,
    lock: File,
    path: PathBuf,
}

impl Listener {
    /// Where the socket lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start accepting forwarded command lines on a background thread.
    ///
    /// A new `--watch` subscriber's `status` event is the application's own answer to
    /// `--status --json`, asked for through [`Server::drain`] like any forwarded command line, so
    /// it is exactly what `fxsound --status --json` prints.
    ///
    /// # Errors
    ///
    /// If the socket cannot be put into the blocking mode the accept loop needs.
    pub fn serve(self) -> io::Result<Server> {
        self.start(None, Waker::new())
    }

    /// As [`Listener::serve`], waking the GUI thread through `waker` whenever a forwarded command
    /// line, a D-Bus call or a new subscriber's status request is waiting for it (0.4.0 design
    /// §12). The one the binary uses: without it, what arrives waits for the pump's keepalive.
    ///
    /// # Errors
    ///
    /// As [`Listener::serve`].
    pub fn serve_waking(self, waker: Waker) -> io::Result<Server> {
        self.start(None, waker)
    }

    /// As [`Listener::serve`], with the `status` event built by `status` instead — on the
    /// connection's own thread, so it must not wait for the GUI thread.
    ///
    /// # Errors
    ///
    /// As [`Listener::serve`].
    pub fn serve_with_status(self, status: StatusSource) -> io::Result<Server> {
        self.start(Some(status), Waker::new())
    }

    fn start(self, status: Option<StatusSource>, waker: Waker) -> io::Result<Server> {
        let Self {
            listener,
            lock,
            path,
        } = self;
        listener.set_nonblocking(false)?;

        let (tx, rx) = unbounded();
        let tx = WakingSender::new(tx, waker);
        let closing = Closing::default();
        let status = status.unwrap_or_else(|| status_from_application(tx.clone(), closing.clone()));
        let shared = Arc::new(Shared {
            tx,
            closing,
            status,
            broadcaster: Broadcaster::default(),
            connections: AtomicUsize::new(0),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = {
            let shared = Arc::clone(&shared);
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("fxsound-ipc".to_owned())
                .spawn(move || accept_loop(&listener, &shared, &shutdown))?
        };

        Ok(Server {
            rx,
            path,
            shutdown,
            worker: Some(worker),
            shared,
            _lock: lock,
        })
    }
}

/// Where a new subscriber's `status` event comes from: the `--status --json` document, or `None`
/// when there is none to be had in time, in which case the subscription is refused.
pub type StatusSource = Arc<dyn Fn() -> Option<serde_json::Value> + Send + Sync>;

/// The default [`StatusSource`]: ask the application, the way `fxsound --status --json` does —
/// through [`hand_over`], so an instance on its way out answers at once rather than never.
fn status_from_application(tx: WakingSender<Forwarded>, closing: Closing) -> StatusSource {
    Arc::new(move || {
        let response = hand_over(
            &tx,
            &closing,
            vec![Command::Status { json: true }],
            "/".into(),
        );
        if !response.ok {
            return None;
        }
        serde_json::from_str(&response.stdout).ok()
    })
}

/// Set once the instance has decided to quit ([`Server::refuse_pending`]), and never cleared.
///
/// The channel to the GUI thread stays open until the [`Server`] is dropped at the very end of the
/// way out, after the engine has handed the default devices back — and nothing drains it in
/// between. Every sender checks this before and after handing a command over, and a [`Forwarded`]
/// that is dropped unanswered while it is set says [`SHUTTING_DOWN`] rather than `ok`, so a
/// command that arrives in that window is refused, as the way out promises, instead of being
/// dropped with a bare acknowledgement or left to wait out [`HANDLER_TIMEOUT`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Closing(Arc<AtomicBool>);

impl Closing {
    pub(crate) fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Hand `commands` to the GUI thread and wait up to [`HANDLER_TIMEOUT`] for its answer, for a
/// caller on a thread of its own: a connection on the socket, a new subscriber's status request.
///
/// Refused with [`SHUTTING_DOWN`] once the way out has begun ([`Closing`]). A caller that stops
/// waiting marks the command abandoned, so the GUI thread does not carry it out later
/// ([`Forwarded::is_abandoned`]).
fn hand_over(
    tx: &WakingSender<Forwarded>,
    closing: &Closing,
    commands: Vec<Command>,
    cwd: PathBuf,
) -> Response {
    hand_over_within(tx, closing, commands, cwd, HANDLER_TIMEOUT)
}

/// [`hand_over`], waiting `timeout` for the answer.
fn hand_over_within(
    tx: &WakingSender<Forwarded>,
    closing: &Closing,
    commands: Vec<Command>,
    cwd: PathBuf,
    timeout: Duration,
) -> Response {
    if closing.is_set() {
        return Response::failed(SHUTTING_DOWN);
    }
    let (reply_tx, reply_rx) = bounded(1);
    let forwarded = Forwarded::new(commands, cwd, move |response| {
        let _ = reply_tx.send(response);
    })
    .closing_with(closing.clone());
    let abandoned = Arc::clone(&forwarded.abandoned);
    if tx.try_send(forwarded).is_err() {
        return Response::failed(SHUTTING_DOWN);
    }
    // Queued after the way out drained the channel: nothing will take it now. An answer the GUI
    // thread gave before it began the way out is already here, and is the one to give.
    if closing.is_set() {
        return reply_rx
            .try_recv()
            .unwrap_or_else(|_| Response::failed(SHUTTING_DOWN));
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(response) => response,
        Err(RecvTimeoutError::Timeout) => {
            abandoned.store(true, Ordering::SeqCst);
            Response::unanswered()
        }
        // The `Forwarded` was dropped without its `Drop` running, which can only mean the process
        // is going away.
        Err(RecvTimeoutError::Disconnected) => Response::failed(SHUTTING_DOWN),
    }
}

/// The running accept loop, and the GUI thread's end of it.
///
/// Dropping the server closes the socket, joins the thread and unlinks the socket file, which is
/// the analogue of `Shell_NotifyIcon(NIM_DELETE)` plus the removal of the hidden message window in
/// `FxSystemTrayView::~FxSystemTrayView` (`fxsound/Source/GUI/FxSystemTrayView.cpp:47-62`).
pub struct Server {
    rx: Receiver<Forwarded>,
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// What the connection threads share: the way to the GUI thread and the subscribers.
    shared: Arc<Shared>,
    /// Held for the lifetime of the process: releasing it would let a second instance start.
    _lock: File,
}

impl Server {
    /// Take one forwarded command line, or `None` if none is waiting.
    ///
    /// This never blocks, so it is safe to call once per egui frame — the equivalent of JUCE
    /// delivering `anotherInstanceStarted` on the message thread (`Main.cpp:136-139`).
    #[must_use]
    pub fn try_recv(&self) -> Option<Forwarded> {
        self.rx.try_recv().ok()
    }

    /// Take everything waiting. Convenience for the top of a frame.
    #[must_use]
    pub fn drain(&self) -> Vec<Forwarded> {
        self.rx.try_iter().collect()
    }

    /// The channel forwarded command lines and D-Bus calls arrive on, for the headless pump to
    /// wait on (0.4.0 design §12). Wait on it, do not take from it: [`Server::drain`] does that.
    #[must_use]
    pub const fn arrivals(&self) -> &Receiver<Forwarded> {
        &self.rx
    }

    /// Refuse everything forwarded and not yet drained, with the answer a caller gets from an
    /// instance that is going away — for the way out, so that nobody waits out
    /// [`HANDLER_TIMEOUT`] on a GUI thread that has stopped answering.
    ///
    /// Sticky: from here on every caller is refused at once, on the socket and on the bus, and a
    /// command that slips into the channel after this drain is refused when the server drops it
    /// (`Closing`).
    pub fn refuse_pending(&self) {
        self.shared.closing.set();
        for forwarded in self.rx.try_iter() {
            forwarded.reply(Response::failed(SHUTTING_DOWN));
        }
    }

    /// Where the socket lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A way into the GUI thread for callers that are not on the socket — the D-Bus service. What
    /// it sends arrives through [`Server::drain`] like any forwarded command line.
    #[must_use]
    pub fn control(&self) -> Control {
        Control {
            tx: self.shared.tx.clone(),
            closing: self.shared.closing.clone(),
        }
    }

    /// Send `event` to every `--watch` subscriber, and drop the ones that are gone or stuck.
    ///
    /// Costs one uncontended lock when nobody is watching. With subscribers, the event is
    /// serialised at most once per spelling, and each write waits at most
    /// [`WATCH_WRITE_TIMEOUT`].
    pub fn publish(&self, event: &AppEvent) {
        self.shared
            .broadcaster
            .publish(event, unix_millis(), Instant::now());
    }

    /// How many `--watch` streams are open.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.shared.broadcaster.lock().list.len()
    }

    /// Whether any subscriber asked for `--meters` — so the meters are only gathered for one.
    #[must_use]
    pub fn wants_meters(&self) -> bool {
        self.shared
            .broadcaster
            .lock()
            .list
            .iter()
            .any(|subscriber| subscriber.meters)
    }
}

/// The `--watch` streams are one of the consumers of the controller's events; the broadcaster
/// holds each subscriber that asked for meters to four a second.
impl EventSink for Server {
    fn publish(&self, event: &AppEvent) {
        Self::publish(self, event);
    }

    fn wants_meters(&self) -> bool {
        Self::wants_meters(self)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Whatever reached the channel since the way out began is answered before the channel
        // goes, rather than discarded with the receiver.
        self.refuse_pending();
        self.shutdown.store(true, Ordering::SeqCst);
        // The accept loop is parked inside `accept()`; connecting to ourselves wakes it so it can
        // see the flag. The connection is closed immediately and the loop discards it.
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Every `fxsound --watch` sees end-of-file. One that got the `quit` event, published
        // before the server goes (`Runtime::shutdown`), exits 0; any other — one still waiting for
        // its status document, say — exits 1, its stream cut off.
        self.shared.broadcaster.close();
        if let Err(e) = fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            log::warn!("could not remove {}: {e}", self.path.display());
        }
    }
}

/// One command line handed over by a second process, waiting for the application to act on it.
///
/// The reply is sent when this value is dropped, so a handler that forgets to answer still lets
/// the forwarding process exit; answer explicitly with [`Forwarded::reply`] when there is
/// output, which in practice means `--status`.
///
/// A second process on the control socket is where most of these come from; a D-Bus method call
/// is the other place (`crate::dbus`), through [`Control`]. Both are answered by the same
/// `commands::run` on the GUI thread, so there is one way a command is carried out.
pub struct Forwarded {
    commands: Vec<Command>,
    cwd: PathBuf,
    reply: Option<Reply>,
    /// Set by the caller when it stops waiting for the answer ([`HANDLER_TIMEOUT`]).
    abandoned: Arc<AtomicBool>,
    /// The server's [`Closing`], for a command handed over through it.
    closing: Option<Closing>,
}

/// Where a [`Forwarded`]'s answer goes: a channel back to a connection thread, or a D-Bus call's
/// oneshot. Called exactly once.
type Reply = Box<dyn FnOnce(Response) + Send>;

impl Forwarded {
    /// A command list to hand to the GUI thread, answered through `reply` — once, with whatever
    /// [`Forwarded::reply`] is given, or with a bare acknowledgement if the value is dropped
    /// unanswered.
    ///
    /// `cwd` is the directory a relative path in an argument is resolved against; a caller that
    /// has none, like a D-Bus client, passes `/`.
    #[must_use]
    pub fn new(
        commands: Vec<Command>,
        cwd: PathBuf,
        reply: impl FnOnce(Response) + Send + 'static,
    ) -> Self {
        Self {
            commands,
            cwd,
            reply: Some(Box::new(reply)),
            abandoned: Arc::new(AtomicBool::new(false)),
            closing: None,
        }
    }

    /// Answer [`SHUTTING_DOWN`] instead of `ok` when dropped unanswered after `closing` is set:
    /// then nothing carried it out, and nothing will.
    fn closing_with(mut self, closing: Closing) -> Self {
        self.closing = Some(closing);
        self
    }

    /// Whether the caller has stopped waiting for the answer — it was told that FxSound did not
    /// answer in time. Such a command is not carried out late: a keybind pressed a dozen times
    /// while the GUI thread was busy would otherwise replay all of them at once when it is free.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::SeqCst)
    }

    /// What the forwarding process asked for, in `applyConfig` order.
    #[must_use]
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// The forwarding process's working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Answer the forwarding process with whatever should land on its stdout — which in practice
    /// means the `--status` JSON, and otherwise the empty string.
    ///
    /// Answering is optional: dropping the value sends a bare acknowledgement, so a handler that
    /// forgets one still lets the caller exit.
    pub fn respond(self, stdout: impl Into<String>) {
        self.reply(Response::output(stdout));
    }

    /// Answer with everything running a command line produced: its output, its diagnostics, and
    /// whether the forwarding process should exit non-zero.
    pub fn respond_with(self, stdout: String, stderr: String, failed: bool) {
        self.reply(Response {
            v: PROTOCOL_VERSION,
            ok: !failed,
            stdout,
            stderr,
        });
    }

    /// Answer with a whole [`Response`] — the way to refuse with [`Response::failed`].
    pub fn reply(mut self, response: Response) {
        self.send(response);
    }

    fn send(&mut self, response: Response) {
        if let Some(reply) = self.reply.take() {
            reply(response);
        }
    }
}

impl Drop for Forwarded {
    fn drop(&mut self) {
        let closing = self.closing.as_ref().is_some_and(Closing::is_set);
        self.send(if closing {
            Response::failed(SHUTTING_DOWN)
        } else {
            Response::ok()
        });
    }
}

impl std::fmt::Debug for Forwarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwarded")
            .field("commands", &self.commands)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// The control channel, for a caller that is not a connection on the socket: the D-Bus service
/// (`crate::dbus`, 0.4.0 design §9). A command list sent here is a [`Forwarded`] like any other,
/// drained by the GUI thread and carried out by the same `commands::run`, and it is answered
/// under the same [`HANDLER_TIMEOUT`] and with the same refusals as a forwarded command line.
#[derive(Clone)]
pub struct Control {
    tx: WakingSender<Forwarded>,
    closing: Closing,
}

impl Control {
    /// A control channel whose other end is `tx` — for a test that plays the GUI thread itself.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_sender(tx: crossbeam_channel::Sender<Forwarded>) -> Self {
        Self {
            tx: tx.into(),
            closing: Closing::default(),
        }
    }

    /// [`Control::from_sender`], with the way out begun or not as `closing` says.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_sender_closing(
        tx: crossbeam_channel::Sender<Forwarded>,
        closing: Closing,
    ) -> Self {
        Self {
            tx: tx.into(),
            closing,
        }
    }

    /// Hand `commands` to the GUI thread and wait for its answer.
    ///
    /// Asynchronous, so a D-Bus method handler awaits it instead of blocking a worker of the
    /// runtime it is on; that runtime needs its timer enabled for [`HANDLER_TIMEOUT`]. Never
    /// fails as such: an instance that is going away, or a GUI thread that does not answer in
    /// time, comes back as a failed [`Response`] that says so, as `fxsound` would print it.
    pub async fn call(&self, commands: Vec<Command>) -> Response {
        // As [`hand_over`] does for the socket: once the way out has begun, nothing waits.
        if self.closing.is_set() {
            return Response::failed(SHUTTING_DOWN);
        }
        let (reply, mut answer) = tokio::sync::oneshot::channel();
        let forwarded = Forwarded::new(commands, PathBuf::from("/"), move |response| {
            let _ = reply.send(response);
        })
        .closing_with(self.closing.clone());
        let abandoned = Arc::clone(&forwarded.abandoned);
        if self.tx.try_send(forwarded).is_err() {
            return Response::failed(SHUTTING_DOWN);
        }
        if self.closing.is_set() {
            return answer
                .try_recv()
                .unwrap_or_else(|_| Response::failed(SHUTTING_DOWN));
        }
        match tokio::time::timeout(HANDLER_TIMEOUT, &mut answer).await {
            Ok(Ok(response)) => response,
            // Dropped without its `Drop` running: the process is going away, as in `hand_over`.
            Ok(Err(_)) => Response::failed(SHUTTING_DOWN),
            Err(_) => {
                abandoned.store(true, Ordering::SeqCst);
                Response::unanswered()
            }
        }
    }
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

/// The forwarding end: connect, hand over this process's argv, print the reply, exit.
///
/// This is the whole of a secondary instance's job, and it is what makes `fxsound --next-preset`
/// usable as a compositor keybinding.
pub struct Client {
    path: PathBuf,
}

impl Client {
    /// Where the primary's socket is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Forward this process's own command line.
    ///
    /// The frame carries the real `std::env::args()`, `argv[0]` included: the primary re-parses it
    /// with the same `clap` definition, so a typo comes back as the same message the caller would
    /// have seen had it parsed the line itself.
    ///
    /// # Errors
    ///
    /// If the primary cannot be reached, or does not answer within [`REPLY_TIMEOUT`].
    pub fn forward(&self) -> io::Result<Response> {
        let argv: Vec<String> = std::env::args().collect();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        forward_to(&self.path, &argv, &cwd, REPLY_TIMEOUT)
    }

    /// Forward an explicit argv.
    ///
    /// # Errors
    ///
    /// As [`Client::forward`].
    pub fn send(&self, argv: &[String], cwd: &Path, timeout: Duration) -> io::Result<Response> {
        forward_to(&self.path, argv, cwd, timeout)
    }

    /// `fxsound --watch`: subscribe with this process's own command line and copy every event to
    /// stdout, a line at a time, until the running instance quits. Returns the exit code.
    #[must_use]
    pub fn watch(&self, meters: bool) -> i32 {
        let argv: Vec<String> = std::env::args().collect();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        watch_to(
            &self.path,
            &argv,
            &cwd,
            meters,
            &mut io::stdout().lock(),
            &mut io::stderr().lock(),
        )
    }
}

/// What `fxsound --watch` says when its stream ends without the instance's `quit` event.
const STREAM_CUT_OFF: &str =
    "the event stream was cut off (the reader fell behind or FxSound stopped)";

/// Subscribe on `socket` and copy the stream to `out` until it ends. Returns the exit code:
///
/// * `0` when the instance hung up after its `quit` event — it quit — or when `out` stopped
///   taking lines, which is what `fxsound --watch | head -n 1` does on purpose;
/// * `1` with `STREAM_CUT_OFF` on `err` when the stream ended without that event: the instance
///   dropped a reader that fell behind, or it died. A supervisor that restarts `--watch` until it
///   exits 0 then keeps it running for as long as FxSound does, instead of stopping as though
///   FxSound had quit;
/// * `1` with `FxSound is not running` on `err` when there is nobody to subscribe to;
/// * the refusal's own code, with its message on `err`, when the instance said no — an older
///   instance that has never heard of `--watch` answers that way.
pub fn watch_to(
    socket: &Path,
    argv: &[String],
    cwd: &Path,
    meters: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> i32 {
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(e) => {
            log::debug!("could not connect to {}: {e}", socket.display());
            let _ = writeln!(err, "FxSound is not running");
            return 1;
        }
    };
    let request = Request {
        v: PROTOCOL_VERSION,
        argv: argv.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        watch: true,
        meters,
    };
    let mut unsent = match stream
        .set_write_timeout(Some(REPLY_TIMEOUT))
        .and_then(|()| write_frame(&stream, &request))
    {
        Ok(()) => None,
        // Turned away before the request was read; the reason is still there to print.
        Err(e) if hung_up(&e) => Some(e),
        Err(e) => {
            let _ = writeln!(err, "could not reach the running instance: {e}");
            return 1;
        }
    };

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    let mut first = true;
    // The server writes `quit` before it hangs up on its way out, and on no other occasion: the
    // only end of the stream that is the instance quitting.
    let mut quit = false;
    let ended = |quit: bool, err: &mut dyn Write| {
        if quit {
            0
        } else {
            let _ = writeln!(err, "{STREAM_CUT_OFF}");
            1
        }
    };
    loop {
        line.clear();
        let read = reader.read_line(&mut line);
        if let Some(e) = unsent.take()
            && !line.ends_with('\n')
        {
            let _ = writeln!(err, "could not reach the running instance: {e}");
            return 1;
        }
        match read {
            // End of the stream. A line without its newline is one a write abandoned half-way,
            // and is not printed.
            Ok(_) if !line.ends_with('\n') => return ended(quit, err),
            Ok(_) => {}
            // The instance died with the frame half-read; the stream is over all the same.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return ended(quit, err),
            Err(e) => {
                let _ = writeln!(err, "the event stream broke off: {e}");
                return 1;
            }
        }
        // Only the first line can be a refusal; an event never parses as one.
        if std::mem::take(&mut first)
            && let Ok(response) = serde_json::from_str::<Response>(&line)
        {
            if !response.stdout.is_empty() {
                let _ = writeln!(out, "{}", response.stdout);
            }
            if !response.stderr.is_empty() {
                let _ = writeln!(err, "{}", response.stderr);
            }
            return response.exit_code();
        }
        quit = is_quit_event(&line);
        if out
            .write_all(line.as_bytes())
            .and_then(|()| out.flush())
            .is_err()
        {
            return 0;
        }
    }
}

/// Whether `line` is the `quit` event, in either of the stream's two spellings.
fn is_quit_event(line: &str) -> bool {
    let line = line.trim_end();
    let quit = AppEvent::Quit.name();
    line == quit
        || (line.starts_with('{')
            && serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|event| event["event"] == quit))
}

/// Forward to a socket this process did not learn from [`Instance::acquire`] — a second profile,
/// or a test on a temporary directory.
///
/// # Errors
///
/// If the socket cannot be reached, the frame cannot be written, or no reply arrives in time.
pub fn forward_to(
    socket: &Path,
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> io::Result<Response> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let request = Request {
        v: PROTOCOL_VERSION,
        argv: argv.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Request::default()
    };
    let unsent = match write_frame(&stream, &request) {
        Ok(()) => None,
        Err(e) if hung_up(&e) => Some(e),
        Err(e) => return Err(e),
    };

    let mut line = String::new();
    if let Err(e) = BufReader::new(&stream).read_line(&mut line) {
        return Err(unsent.unwrap_or(e));
    }
    if line.trim().is_empty() {
        return Err(unsent.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the running instance closed the control socket without answering",
            )
        }));
    }
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Whether a request that could not be written may still have been answered: the instance
/// refuses a caller past [`MAX_CONNECTIONS`], or a request past [`MAX_REQUEST_BYTES`], without
/// reading the rest of it, and hangs up. Its reason is already waiting to be read.
fn hung_up(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

/// Where the primary's socket lives.
#[must_use]
pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET_NAME)
}

/// `$XDG_RUNTIME_DIR/fxsound`, or `/tmp/fxsound-<uid>` when the session has no runtime directory
/// — cron and `ssh` without `pam_systemd` are the usual cases
/// (`docs/spec/07-startup-tray.md` §4.7).
#[must_use]
pub fn runtime_dir() -> PathBuf {
    runtime_dir_from(dirs::runtime_dir(), current_uid())
}

/// The decision itself, with the session's answer passed in.
///
/// Split out so both branches can be tested on any machine. Reading `dirs::runtime_dir()` inside
/// the test instead means the fallback branch is exercised only where `XDG_RUNTIME_DIR` happens to
/// be unset — which on a developer's desktop is never, and on a CI runner is always.
fn runtime_dir_from(session: Option<PathBuf>, uid: u32) -> PathBuf {
    session.map_or_else(
        || PathBuf::from(format!("/tmp/fxsound-{uid}")),
        |dir| dir.join("fxsound"),
    )
}

/// Our own uid, read from `/proc/self` so that no libc binding is needed in a crate that is
/// otherwise `forbid(unsafe_code)`.
fn current_uid() -> u32 {
    fs::metadata("/proc/self").map_or(0, |m| m.uid())
}

/// Create the directory `0700` and refuse to use it if it is a symlink.
///
/// The spec asks for `O_NOFOLLOW` here (§4.7) to stop another user from aiming our `/tmp` fallback
/// somewhere else. `std` cannot open a directory with that flag, so we check afterwards with
/// `symlink_metadata`, which is a race in theory and adequate in practice: the parent
/// `$XDG_RUNTIME_DIR` is already `0700` and only the `/tmp` fallback is world-writable.
fn prepare_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symlink; refusing to use it", dir.display()),
        ));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Write our pid into the lock file, the way `$XDG_RUNTIME_DIR/…/instance.lock` is described in
/// §3.3. Purely informational — the lock itself is what enforces anything — so a failure is
/// logged and swallowed.
fn record_pid(lock: &File) {
    let pid = std::process::id();
    let mut handle: &File = lock;
    if let Err(e) = lock
        .set_len(0)
        .and_then(|()| handle.write_all(format!("{pid}\n").as_bytes()))
    {
        log::debug!("could not record the pid in the lock file: {e}");
    }
}

fn write_frame(mut stream: &UnixStream, frame: &impl Serialize) -> io::Result<()> {
    let mut line =
        serde_json::to_vec(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()
}

/// What the accept loop and the connection threads share.
struct Shared {
    /// The way to the GUI thread, which it wakes (0.4.0 design §12).
    tx: WakingSender<Forwarded>,
    /// Whether the way out has begun: every caller is refused from then on.
    closing: Closing,
    status: StatusSource,
    broadcaster: Broadcaster,
    /// Connections being answered right now, against [`MAX_CONNECTIONS`].
    connections: AtomicUsize,
}

fn accept_loop(listener: &UnixListener, shared: &Arc<Shared>, shutdown: &AtomicBool) {
    let mut failures = AcceptFailures::default();
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match stream {
            Ok(stream) => {
                let failed = failures.accepted();
                if failed > 1 {
                    log::info!("control socket accepting again after {failed} failed attempts");
                }
                spawn_handler(stream, shared);
            }
            // `EMFILE` and friends pass; giving up here would leave every later keybind talking
            // to a socket nobody reads. A listener that is broken for good costs a wake-up a
            // second and one warning, not twenty of each.
            Err(e) => {
                let (new, pause) = failures.failed(&e);
                if new {
                    log::warn!("control socket accept failed: {e}; retrying");
                } else {
                    log::debug!("control socket accept failed again: {e}");
                }
                thread::sleep(pause);
                // Quitting must not wait for an `accept()` that may not come back.
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
            }
        }
    }
}

/// The first pause after a failed `accept()`.
const ACCEPT_RETRY_MIN: Duration = Duration::from_millis(50);
/// The longest pause between failed `accept()`s.
const ACCEPT_RETRY_MAX: Duration = Duration::from_secs(1);

/// A run of failed `accept()`s: how long to pause before the next one, and whether a failure is
/// worth a warning. The pause doubles from [`ACCEPT_RETRY_MIN`] to [`ACCEPT_RETRY_MAX`]; a
/// failure is warned about when it differs from the one before, and only logged at debug level
/// when it repeats it.
#[derive(Debug, Default)]
struct AcceptFailures {
    /// Failures since the last connection that came in.
    run: u32,
    /// The last failure, as its kind and `errno`, to tell a repeat from something new.
    last: Option<(io::ErrorKind, Option<i32>)>,
}

impl AcceptFailures {
    /// Note `e`. Returns whether it is new, and how long to pause before trying again.
    fn failed(&mut self, e: &io::Error) -> (bool, Duration) {
        let this = (e.kind(), e.raw_os_error());
        let new = self.last != Some(this);
        self.last = Some(this);
        let pause = ACCEPT_RETRY_MIN
            .saturating_mul(1_u32 << self.run.min(5))
            .min(ACCEPT_RETRY_MAX);
        self.run = self.run.saturating_add(1);
        (new, pause)
    }

    /// A connection came in: the run is over. Returns how many failures it had.
    fn accepted(&mut self) -> u32 {
        self.last = None;
        std::mem::take(&mut self.run)
    }
}

/// Answer one caller on a thread of its own, so that the next one is accepted at once.
fn spawn_handler(stream: UnixStream, shared: &Arc<Shared>) {
    if shared.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
        shared.connections.fetch_sub(1, Ordering::SeqCst);
        let _ = stream.set_write_timeout(Some(WATCH_WRITE_TIMEOUT));
        refuse(&stream, TOO_MANY_CALLERS);
        return;
    }
    let slot = ConnectionSlot(Arc::clone(shared));
    let spawned = thread::Builder::new()
        .name("fxsound-ipc-client".to_owned())
        .spawn(move || handle_connection(stream, &slot.0));
    // A closure that never ran is dropped with its slot, which gives the count back.
    if let Err(e) = spawned {
        log::warn!("could not start a thread for a control connection: {e}");
    }
}

/// One of [`MAX_CONNECTIONS`], given back when the thread holding it ends.
struct ConnectionSlot(Arc<Shared>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_connection(stream: UnixStream, shared: &Shared) {
    if let Err(e) = stream.set_write_timeout(Some(REQUEST_TIMEOUT)) {
        log::warn!("could not arm the control socket timeouts: {e}");
        return;
    }
    let request = match read_request(&stream, REQUEST_TIMEOUT) {
        Ok(request) => request,
        Err(e) => {
            refuse(&stream, e);
            return;
        }
    };
    if shared.closing.is_set() {
        refuse(&stream, SHUTTING_DOWN);
        return;
    }
    if request.watch {
        match watch_flags(&request) {
            Ok((json, meters)) => {
                shared
                    .broadcaster
                    .subscribe(stream, json, meters, &shared.status);
            }
            Err(response) => answer(&stream, &response),
        }
        return;
    }
    answer(&stream, &dispatch(request, &shared.tx, &shared.closing));
}

fn answer(stream: &UnixStream, response: &Response) {
    if let Err(e) = write_frame(stream, response) {
        log::warn!("could not answer a forwarded command line: {e}");
    }
}

fn refuse(stream: &UnixStream, why: impl Into<String>) {
    answer(stream, &Response::failed(why));
}

/// Read one request line, all of it within `within`.
///
/// The deadline is for the whole line, not for each read: a socket's receive timeout restarts
/// with every byte, so a caller trickling one byte every few seconds would otherwise hold its
/// connection — one of [`MAX_CONNECTIONS`] — for as long as [`MAX_REQUEST_BYTES`] takes at that
/// pace, which is days.
fn read_request(stream: &UnixStream, within: Duration) -> Result<Request, String> {
    let mut line = Vec::new();
    let reader = Deadline {
        stream,
        until: Instant::now() + within,
    };
    // One byte past the cap, so that a line of exactly the cap still has room for its newline.
    BufReader::new(Read::take(reader, MAX_REQUEST_BYTES as u64 + 1))
        .read_until(b'\n', &mut line)
        .map_err(|e| match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => REQUEST_TOO_SLOW.to_owned(),
            _ => format!("could not read the request: {e}"),
        })?;
    if line.len() > MAX_REQUEST_BYTES && line.last() != Some(&b'\n') {
        return Err(format!(
            "the request is longer than {} KiB and was not read",
            MAX_REQUEST_BYTES / 1024
        ));
    }
    let request: Request =
        serde_json::from_slice(&line).map_err(|e| format!("malformed request: {e}"))?;
    if request.v != PROTOCOL_VERSION {
        return Err(format!(
            "this FxSound speaks control protocol v{PROTOCOL_VERSION}, the caller speaks v{}",
            request.v
        ));
    }
    Ok(request)
}

/// A stream read against one deadline: each read waits at most for what is left of it.
struct Deadline<'a> {
    stream: &'a UnixStream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut stream = self.stream;
        stream.read(buf)
    }
}

/// A watch request's `--json` and `--meters`, from the same parser every other request goes
/// through. The argv has to be a `--watch` line and nothing else: with `--status` on it too, the
/// caller asked a question that is answered once.
fn watch_flags(request: &Request) -> Result<(bool, bool), Response> {
    let cli =
        Cli::try_parse_from(&request.argv).map_err(|e| Response::failed(e.render().to_string()))?;
    match cli.commands().as_slice() {
        [Command::Watch { json, meters }] => Ok((*json, *meters || request.meters)),
        _ => Err(Response::failed(
            "a watch request has to ask for --watch and nothing else",
        )),
    }
}

/// Parse the forwarded argv and hand the commands to the GUI thread.
///
/// Parsing happens *here*, on the primary, so that the forwarding process gets the same error text
/// it would have got had it parsed the line itself — which is the whole reason the frame carries
/// argv rather than a pre-parsed command list.
fn dispatch(request: Request, tx: &WakingSender<Forwarded>, closing: &Closing) -> Response {
    let cli = match Cli::try_parse_from(&request.argv) {
        Ok(cli) => cli,
        Err(e) => return Response::failed(e.render().to_string()),
    };
    hand_over(tx, closing, cli.commands(), PathBuf::from(request.cwd))
}

// =============================================================================================
// The event stream
// =============================================================================================

/// The `--watch` subscribers.
#[derive(Default)]
struct Broadcaster {
    inner: Mutex<Subscribers>,
}

#[derive(Default)]
struct Subscribers {
    list: Vec<Subscriber>,
    next_id: u64,
    /// The server is going away: refuse newcomers rather than leave them hanging.
    closed: bool,
}

struct Subscriber {
    id: u64,
    stream: UnixStream,
    /// One JSON object per line rather than `event key=value`.
    json: bool,
    /// Asked for `input_meters`.
    meters: bool,
    /// When the last `input_meters` went out to this subscriber.
    last_meters: Option<Instant>,
    /// `Some` until the `status` event has been written: what was published meanwhile.
    backlog: Option<Vec<String>>,
}

impl Subscriber {
    /// Write `line`, or hold it while the status document is still on its way. `false` means
    /// the subscriber is gone or stuck and has to be dropped.
    fn deliver(&mut self, line: &str) -> bool {
        match &mut self.backlog {
            Some(backlog) if backlog.len() < MAX_BACKLOG => {
                backlog.push(line.to_owned());
                true
            }
            Some(_) => false,
            None => write_line(&self.stream, line),
        }
    }
}

impl Broadcaster {
    fn lock(&self) -> MutexGuard<'_, Subscribers> {
        // A panic while writing to a socket leaves nothing half-updated worth refusing over.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register `stream`, send it the status document, then whatever was published meanwhile.
    fn subscribe(&self, stream: UnixStream, json: bool, meters: bool, status: &StatusSource) {
        // The stream is only written from now on, and never for longer than a publish may wait.
        if let Err(e) = stream
            .set_read_timeout(None)
            .and_then(|()| stream.set_write_timeout(Some(WATCH_WRITE_TIMEOUT)))
        {
            log::warn!("could not arm a watch stream: {e}");
            return;
        }

        let id = {
            let mut subscribers = self.lock();
            if subscribers.closed {
                refuse(&stream, "FxSound is shutting down");
                return;
            }
            // Nothing is written to a quiet stream, so a caller that went away is only noticed
            // here or at the next event.
            subscribers.list.retain(|subscriber| {
                subscriber.backlog.is_some() || !peer_closed(&subscriber.stream)
            });
            if subscribers.list.len() >= MAX_SUBSCRIBERS {
                refuse(&stream, "FxSound has too many --watch streams open");
                return;
            }
            let id = subscribers.next_id;
            subscribers.next_id += 1;
            subscribers.list.push(Subscriber {
                id,
                stream,
                json,
                meters,
                last_meters: None,
                backlog: Some(Vec::new()),
            });
            id
        };

        // Not under the lock: the default source waits for the GUI thread, which publishes.
        let document = status();

        let mut subscribers = self.lock();
        // Gone already: it fell behind, or the server closed.
        let Some(index) = subscribers.list.iter().position(|s| s.id == id) else {
            return;
        };
        let Some(document) = document else {
            let subscriber = subscribers.list.remove(index);
            refuse(
                &subscriber.stream,
                "FxSound did not report its status in time",
            );
            return;
        };
        let subscriber = &mut subscribers.list[index];
        let event = AppEvent::Status(document);
        let first = if subscriber.json {
            event.to_json(unix_millis())
        } else {
            event.to_plain()
        } + "\n";
        let backlog = subscriber.backlog.take().unwrap_or_default();
        let delivered = std::iter::once(first.as_str())
            .chain(backlog.iter().map(String::as_str))
            .all(|line| write_line(&subscriber.stream, line));
        if !delivered {
            subscribers.list.remove(index);
        }
    }

    /// Hand `event` to every subscriber it is meant for; drop the ones a write fails on.
    fn publish(&self, event: &AppEvent, ts_ms: u64, now: Instant) {
        let mut subscribers = self.lock();
        if subscribers.list.is_empty() {
            return;
        }
        let is_meters = matches!(event, AppEvent::InputMeters(_));
        let mut json: Option<String> = None;
        let mut plain: Option<String> = None;
        subscribers.list.retain_mut(|subscriber| {
            if is_meters {
                if !subscriber.meters
                    || subscriber
                        .last_meters
                        .is_some_and(|last| now.saturating_duration_since(last) < METER_INTERVAL)
                {
                    return true;
                }
                subscriber.last_meters = Some(now);
            }
            let line = if subscriber.json {
                json.get_or_insert_with(|| event.to_json(ts_ms) + "\n")
            } else {
                plain.get_or_insert_with(|| event.to_plain() + "\n")
            };
            subscriber.deliver(line)
        });
    }

    /// Subscribers whose status document has gone out, so that events reach them directly.
    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.lock()
            .list
            .iter()
            .filter(|subscriber| subscriber.backlog.is_none())
            .count()
    }

    /// End every stream and refuse new ones.
    fn close(&self) {
        let mut subscribers = self.lock();
        subscribers.closed = true;
        for subscriber in subscribers.list.drain(..) {
            let _ = subscriber.stream.shutdown(Shutdown::Both);
        }
    }
}

/// Write one whole line; a stream that cannot take it within its timeout is done for, since the
/// half that went out cannot be taken back.
fn write_line(mut stream: &UnixStream, line: &str) -> bool {
    match stream.write_all(line.as_bytes()) {
        Ok(()) => true,
        Err(e) => {
            log::debug!("dropping a --watch subscriber: {e}");
            false
        }
    }
}

/// Whether the caller at the other end of a watch stream has hung up. Watchers never write after
/// their request, so end-of-file is the only thing a read can find.
fn peer_closed(stream: &UnixStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut reader = stream;
    let closed = match reader.read(&mut [0_u8; 64]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ),
    };
    closed || stream.set_nonblocking(false).is_err()
}

/// Unix time in milliseconds, the events' `ts` — on the socket and in D-Bus's `AudioStateChanged`.
pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{PowerCommand, PresetCommand, WindowCommand};
    use fxsound_core::DeviceDirection;
    use serde_json::{Value, json};

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("fxsound")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    /// Wait for the accept loop to hand something over. The loop is a real thread on a real
    /// socket, so the test has to wait for it rather than assume it has already run.
    fn wait_for(server: &Server) -> Forwarded {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(forwarded) = server.try_recv() {
                return forwarded;
            }
            assert!(
                Instant::now() < deadline,
                "the primary never received the forwarded command line"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_first_instance_binds_and_the_second_one_forwards() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("first acquire")
        else {
            panic!("the first instance in an empty directory must be the primary");
        };
        assert_eq!(listener.path(), dir.path().join(SOCKET_NAME));

        let server = listener.serve().expect("serve");
        assert!(
            matches!(
                Instance::acquire_in(dir.path()).expect("second acquire"),
                Instance::Secondary(_)
            ),
            "a second instance must not take the lock while the first holds it"
        );

        let socket = server.path().to_path_buf();
        let sender = thread::spawn(move || {
            forward_to(
                &socket,
                &argv(&["--power=1", "--preset=Bass Booster"]),
                Path::new("/tmp"),
                REPLY_TIMEOUT,
            )
            .expect("the primary should answer")
        });

        let forwarded = wait_for(&server);
        assert_eq!(
            forwarded.commands(),
            [
                Command::Power(PowerCommand::On),
                Command::Preset(PresetCommand::Select("Bass Booster".to_owned())),
                Command::Window(WindowCommand::Show),
            ]
        );
        assert_eq!(forwarded.cwd(), Path::new("/tmp"));
        drop(forwarded); // the implicit acknowledgement

        let response = sender.join().expect("client thread");
        assert!(response.ok, "{response:?}");
        assert_eq!(response.exit_code(), 0);
        assert!(response.stdout.is_empty());
    }

    #[test]
    fn a_status_request_comes_back_on_the_callers_own_stdout() {
        // The Windows build cannot do this: it prints to the *running* instance's console
        // (`FxController.cpp:686-699`).
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");
        let socket = server.path().to_path_buf();

        let sender = thread::spawn(move || {
            forward_to(&socket, &argv(&["--status"]), Path::new("/"), REPLY_TIMEOUT)
                .expect("the primary should answer")
        });

        let forwarded = wait_for(&server);
        assert_eq!(forwarded.commands(), [Command::Status { json: false }]);
        forwarded.reply(Response::output(r#"{"version":"0.1.0","power":true}"#));

        let response = sender.join().expect("client thread");
        assert!(response.ok);
        assert_eq!(response.stdout, r#"{"version":"0.1.0","power":true}"#);
    }

    #[test]
    fn a_command_line_the_primary_cannot_parse_is_refused_with_the_parser_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");

        let response = forward_to(
            server.path(),
            &argv(&["--num_bands=7"]),
            Path::new("/"),
            REPLY_TIMEOUT,
        )
        .expect("the primary should answer");

        assert!(!response.ok);
        assert_eq!(response.exit_code(), 1);
        assert!(
            response.stderr.contains("5, 10, 15, 20 or 31"),
            "the caller should get the same message it would have got locally: {}",
            response.stderr
        );
        assert!(
            server.try_recv().is_none(),
            "a line that does not parse must never reach the application"
        );
    }

    #[test]
    fn a_socket_left_behind_by_a_crash_is_replaced_rather_than_believed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join(SOCKET_NAME);

        // Exactly what a SIGKILLed primary leaves: the inode is there, nothing is listening.
        // Dropping a `UnixListener` does not unlink it.
        drop(UnixListener::bind(&socket).expect("bind a doomed listener"));
        assert!(socket.exists(), "the stale socket should still be on disk");

        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("a stale socket must not make us think an instance is running");
        };
        let server = listener.serve().expect("serve");

        // And the replacement really is live.
        let socket = server.path().to_path_buf();
        let sender = thread::spawn(move || {
            forward_to(
                &socket,
                &argv(&["--next-preset"]),
                Path::new("/"),
                REPLY_TIMEOUT,
            )
            .expect("the replacement primary should answer")
        });
        let forwarded = wait_for(&server);
        assert_eq!(forwarded.commands(), [Command::Preset(PresetCommand::Next)]);
        drop(forwarded);
        assert!(sender.join().expect("client thread").ok);
    }

    #[test]
    fn a_regular_file_squatting_on_the_socket_path_is_cleared_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(SOCKET_NAME), b"not a socket").expect("write the squatter");

        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        assert!(listener.path().exists());
    }

    #[test]
    fn dropping_the_server_unlinks_the_socket_and_frees_the_lock() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let path = listener.path().to_path_buf();
        let server = listener.serve().expect("serve");
        assert!(path.exists());

        drop(server);
        assert!(
            !path.exists(),
            "the socket file must not outlive the server"
        );

        // With the lock released, the next process is the primary again.
        assert!(matches!(
            Instance::acquire_in(dir.path()).expect("acquire"),
            Instance::Primary(_)
        ));
    }

    #[test]
    fn the_runtime_directory_is_private_to_the_user() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = dir.path().join("fxsound");
        prepare_dir(&nested).expect("prepare");
        let mode = fs::metadata(&nested)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "got {:o}", mode & 0o777);
    }

    #[test]
    fn the_runtime_directory_falls_back_to_tmp_when_the_session_has_none() {
        assert_eq!(
            runtime_dir_from(Some(PathBuf::from("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/fxsound")
        );
        // `Path::starts_with` compares whole components, so an assertion spelled
        // `starts_with("/tmp/fxsound-")` is false for `/tmp/fxsound-1000` and this test passed
        // only through its other branch — on a machine that has a session runtime directory.
        assert_eq!(
            runtime_dir_from(None, 1000),
            PathBuf::from("/tmp/fxsound-1000")
        );
    }

    #[test]
    fn a_frame_from_a_future_version_is_refused_instead_of_guessed_at() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let path = listener.path().to_path_buf();
        let server = listener.serve().expect("serve");

        let stream = UnixStream::connect(&path).expect("connect");
        stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .expect("timeout");
        write_frame(
            &stream,
            &Request {
                v: PROTOCOL_VERSION + 1,
                argv: argv(&["--status"]),
                cwd: "/".to_owned(),
                ..Request::default()
            },
        )
        .expect("write");

        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        let response: Response = serde_json::from_str(&line).expect("parse");
        assert!(!response.ok);
        assert!(response.stderr.contains("control protocol"), "{response:?}");
        assert!(server.try_recv().is_none());
    }

    // ---- one thread per caller ---------------------------------------------------------------

    #[test]
    fn a_caller_that_connects_and_says_nothing_does_not_hold_up_a_keybind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");

        // Connected, and silent for as long as the test runs. Before 0.4.0 the accept loop sat
        // on this caller for the whole `REQUEST_TIMEOUT`.
        let _silent = UnixStream::connect(server.path()).expect("connect");

        let started = Instant::now();
        let socket = server.path().to_path_buf();
        let sender = thread::spawn(move || {
            forward_to(
                &socket,
                &argv(&["--next-preset"]),
                Path::new("/"),
                REPLY_TIMEOUT,
            )
            .expect("the primary should answer")
        });
        let forwarded = wait_for(&server);
        assert!(
            started.elapsed() < REQUEST_TIMEOUT / 2,
            "the keybind waited {:?} behind a silent caller",
            started.elapsed()
        );
        assert_eq!(forwarded.commands(), [Command::Preset(PresetCommand::Next)]);
        drop(forwarded);
        assert!(sender.join().expect("client thread").ok);
    }

    #[test]
    fn a_request_longer_than_the_cap_is_refused_unread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");

        let mut stream = UnixStream::connect(server.path()).expect("connect");
        stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .expect("timeout");
        // No newline, and more than the cap: the primary must stop reading and say so. The
        // write may be cut short once the primary hangs up, which is the point.
        let _ = stream.write_all(&vec![b'x'; MAX_REQUEST_BYTES + 4096]);

        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        let response: Response = serde_json::from_str(&line).expect("parse");
        assert!(!response.ok);
        assert!(response.stderr.contains("64 KiB"), "{response:?}");
        assert!(server.try_recv().is_none());
    }

    #[test]
    fn a_long_request_under_the_cap_is_still_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");

        let name = "n".repeat(MAX_REQUEST_BYTES - 1024);
        let socket = server.path().to_path_buf();
        let preset = format!("--preset={name}");
        let sender = thread::spawn(move || {
            forward_to(&socket, &argv(&[&preset]), Path::new("/"), REPLY_TIMEOUT)
                .expect("the primary should answer")
        });
        let forwarded = wait_for(&server);
        assert_eq!(
            forwarded.commands()[0],
            Command::Preset(PresetCommand::Select(name))
        );
        drop(forwarded);
        assert!(sender.join().expect("client thread").ok);
    }

    #[test]
    fn a_caller_cut_off_mid_request_hears_why_instead_of_a_broken_pipe() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));

        // Far past the cap and past the socket buffer, so the primary hangs up while this caller
        // is still writing.
        let name = "n".repeat(16 * MAX_REQUEST_BYTES);
        let response = forward_to(
            server.path(),
            &argv(&[&format!("--preset={name}")]),
            Path::new("/"),
            REPLY_TIMEOUT,
        )
        .expect("the refusal, not the failed write");
        assert!(!response.ok);
        assert!(response.stderr.contains("64 KiB"), "{response:?}");
        assert!(server.try_recv().is_none());
    }

    // ---- how many callers at once ------------------------------------------------------------

    /// Callers being answered right now.
    fn connections(server: &Server) -> usize {
        server.shared.connections.load(Ordering::SeqCst)
    }

    /// Forward `--next-preset` the way a keybind does, and check that the application gets it.
    fn a_keybind_gets_through(server: &Server) {
        let socket = server.path().to_path_buf();
        let sender = thread::spawn(move || {
            forward_to(
                &socket,
                &argv(&["--next-preset"]),
                Path::new("/"),
                REPLY_TIMEOUT,
            )
            .expect("the primary should answer")
        });
        let forwarded = wait_for(server);
        assert_eq!(forwarded.commands(), [Command::Preset(PresetCommand::Next)]);
        drop(forwarded);
        let response = sender.join().expect("client thread");
        assert!(response.ok, "{response:?}");
    }

    #[test]
    fn a_caller_past_the_connection_limit_is_told_to_try_again_until_a_place_frees_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));

        // Connected and silent: each holds a thread, and a place, until its request times out.
        let mut silent: Vec<UnixStream> = (0..MAX_CONNECTIONS)
            .map(|_| UnixStream::connect(server.path()).expect("connect"))
            .collect();
        wait_until("every silent caller has a thread", || {
            connections(&server) == MAX_CONNECTIONS
        });

        let response = forward_to(
            server.path(),
            &argv(&["--next-preset"]),
            Path::new("/"),
            REPLY_TIMEOUT,
        )
        .expect("a caller turned away is still answered");
        assert!(!response.ok);
        assert_eq!(response.exit_code(), 1);
        assert!(response.stderr.contains("too many callers"), "{response:?}");
        assert!(
            server.try_recv().is_none(),
            "a caller turned away must not reach the application"
        );
        assert_eq!(
            connections(&server),
            MAX_CONNECTIONS,
            "turning a caller away neither takes a place nor gives one back"
        );

        // One of them gives up: its thread reads end-of-file, ends, and gives its place back.
        drop(silent.pop());
        wait_until("the place is given back", || {
            connections(&server) == MAX_CONNECTIONS - 1
        });
        a_keybind_gets_through(&server);
    }

    #[test]
    fn callers_answered_one_after_another_give_their_places_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));

        // One more than there are places: a place kept by an answered caller turns this one away.
        for _ in 0..=MAX_CONNECTIONS {
            a_keybind_gets_through(&server);
        }
        wait_until("every place is given back", || connections(&server) == 0);
    }

    // ---- a listener that fails ---------------------------------------------------------------

    /// Linux's numbers: `EMFILE` is what a process out of descriptors gets from `accept()`, and
    /// `ECONNABORTED` a caller that gave up while it waited.
    const EMFILE: i32 = 24;
    const ECONNABORTED: i32 = 103;

    fn failure(errno: i32) -> io::Error {
        io::Error::from_raw_os_error(errno)
    }

    #[test]
    fn the_first_failed_accept_is_warned_about_and_its_repeats_are_not() {
        let mut failures = AcceptFailures::default();
        assert!(failures.failed(&failure(EMFILE)).0);
        for _ in 0..100 {
            assert!(
                !failures.failed(&failure(EMFILE)).0,
                "a repeat of the same failure is only worth a debug line"
            );
        }
    }

    #[test]
    fn a_different_failure_in_the_same_run_is_warned_about_again() {
        let mut failures = AcceptFailures::default();
        assert!(failures.failed(&failure(EMFILE)).0);
        assert!(failures.failed(&failure(ECONNABORTED)).0);
        assert!(failures.failed(&failure(EMFILE)).0);
        assert!(!failures.failed(&failure(EMFILE)).0);
    }

    #[test]
    fn the_pause_after_a_failed_accept_doubles_up_to_a_second() {
        let mut failures = AcceptFailures::default();
        let pauses: Vec<u128> = (0..8)
            .map(|_| failures.failed(&failure(EMFILE)).1.as_millis())
            .collect();
        assert_eq!(pauses, [50, 100, 200, 400, 800, 1000, 1000, 1000]);

        // However long the listener stays broken: a wake-up a second.
        for _ in 0..10_000 {
            failures.failed(&failure(EMFILE));
        }
        assert_eq!(failures.failed(&failure(EMFILE)).1, ACCEPT_RETRY_MAX);
    }

    #[test]
    fn a_connection_that_comes_in_ends_the_run_and_says_how_long_it_was() {
        let mut failures = AcceptFailures::default();
        assert_eq!(failures.accepted(), 0, "no failures, nothing to report");
        for _ in 0..3 {
            failures.failed(&failure(EMFILE));
        }
        assert_eq!(failures.accepted(), 3);

        // The next failure starts a run of its own: warned about, and a short pause.
        assert_eq!(failures.failed(&failure(EMFILE)), (true, ACCEPT_RETRY_MIN));
    }

    #[test]
    fn a_listener_that_keeps_failing_still_answers_callers_and_still_shuts_down() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let twin = listener.listener.try_clone().expect("share the listener");
        let server = listener.serve().expect("serve");

        // The descriptor is shared with the accept loop, so from here on each `accept()` with
        // nobody waiting fails at once, the way one out of descriptors does.
        twin.set_nonblocking(true).expect("non-blocking");
        a_keybind_gets_through(&server);
        // Long enough for a run of failures and a pause that has doubled a few times.
        thread::sleep(ACCEPT_RETRY_MIN * 8);
        a_keybind_gets_through(&server);

        let started = Instant::now();
        drop(server);
        assert!(
            started.elapsed() < ACCEPT_RETRY_MAX * 2,
            "shutting down took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_plain_command_frame_is_byte_for_byte_what_0_3_0_sent() {
        let request = Request {
            v: PROTOCOL_VERSION,
            argv: argv(&["--next-preset"]),
            cwd: "/".to_owned(),
            ..Request::default()
        };
        let frame = serde_json::to_string(&request).expect("serialise");
        assert_eq!(
            frame,
            r#"{"v":1,"argv":["fxsound","--next-preset"],"cwd":"/"}"#
        );

        // And a 0.3.0 frame still parses, as a request that does not watch.
        let parsed: Request = serde_json::from_str(&frame).expect("parse");
        assert!(!parsed.watch && !parsed.meters);
    }

    // ---- the control channel -----------------------------------------------------------------

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime")
            .block_on(future)
    }

    #[test]
    fn a_command_list_from_the_control_channel_is_drained_like_a_forwarded_command_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");
        let control = server.control();
        let caller = thread::spawn(move || {
            block_on(control.call(vec![Command::Power(PowerCommand::Toggle)]))
        });

        let forwarded = wait_for(&server);
        assert_eq!(forwarded.commands(), [Command::Power(PowerCommand::Toggle)]);
        assert_eq!(forwarded.cwd(), Path::new("/"));
        forwarded.respond_with("on".to_owned(), "a note".to_owned(), false);

        let response = caller.join().expect("the caller");
        assert!(response.ok);
        assert_eq!(response.stdout, "on");
        assert_eq!(response.stderr, "a note");
    }

    #[test]
    fn a_refusal_on_the_control_channel_comes_back_failed_with_its_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");
        let control = server.control();
        let caller = thread::spawn(move || {
            block_on(control.call(vec![Command::Preset(PresetCommand::Select(
                "Nope".to_owned(),
            ))]))
        });
        wait_for(&server).reply(Response::failed("no output preset is called \"Nope\""));
        let response = caller.join().expect("the caller");
        assert!(!response.ok);
        assert_eq!(response.stderr, "no output preset is called \"Nope\"");
    }

    #[test]
    fn a_command_nobody_answered_in_time_is_told_apart_from_a_refusal() {
        let unanswered = Response::unanswered();
        assert!(!unanswered.ok);
        assert_eq!(unanswered.exit_code(), 1);
        assert!(unanswered.is_unanswered());
        assert!(!Response::failed("no output preset is called \"Nope\"").is_unanswered());
        assert!(!Response::failed(SHUTTING_DOWN).is_unanswered());
        assert!(!Response::ok().is_unanswered());
    }

    #[test]
    fn the_control_channel_of_a_server_that_is_gone_refuses_at_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");
        let control = server.control();
        drop(server);
        let started = Instant::now();
        let response = block_on(control.call(vec![Command::Quit]));
        assert!(!response.ok);
        assert_eq!(response.stderr, SHUTTING_DOWN);
        assert!(started.elapsed() < HANDLER_TIMEOUT);
    }

    #[test]
    fn a_socket_caller_forwarded_during_the_way_out_is_told_so_rather_than_acknowledged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");
        let socket = server.path().to_owned();
        let caller = thread::spawn(move || {
            forward_to(
                &socket,
                &argv(&["--next-preset"]),
                Path::new("/"),
                REPLY_TIMEOUT,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !caller.is_finished() {
            assert!(Instant::now() < deadline, "the caller was never answered");
            server.refuse_pending();
            thread::sleep(Duration::from_millis(5));
        }
        let response = caller.join().expect("the caller").expect("an answer");
        assert!(!response.ok);
        assert_eq!(response.stderr, SHUTTING_DOWN);
    }

    // ---- the event stream --------------------------------------------------------------------

    /// A primary whose `status` event is `document`.
    fn serve_with(dir: &Path, document: Value) -> Server {
        let Instance::Primary(listener) = Instance::acquire_in(dir).expect("acquire") else {
            panic!("expected to be primary");
        };
        listener
            .serve_with_status(Arc::new(move || Some(document.clone())))
            .expect("serve")
    }

    /// Connect and ask to watch with `args`, the way `fxsound --watch …` does.
    fn subscribe(socket: &Path, args: &[&str]) -> BufReader<UnixStream> {
        let stream = UnixStream::connect(socket).expect("connect");
        stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .expect("timeout");
        write_frame(
            &stream,
            &Request {
                v: PROTOCOL_VERSION,
                argv: argv(args),
                cwd: "/".to_owned(),
                watch: true,
                meters: false,
            },
        )
        .expect("write");
        BufReader::new(stream)
    }

    fn next_line(reader: &mut BufReader<UnixStream>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read an event");
        assert!(line.ends_with('\n'), "a whole line: {line:?}");
        line.trim_end().to_owned()
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_new_subscriber_is_sent_the_status_document_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({"power": true, "preset": "Rock"}));

        let mut watcher = subscribe(server.path(), &["--watch", "--json"]);
        let first: Value = serde_json::from_str(&next_line(&mut watcher)).expect("json");
        assert_eq!(first["v"], 1);
        assert_eq!(first["event"], "status");
        assert!(
            first["ts"]
                .as_u64()
                .is_some_and(|ts| ts > 1_600_000_000_000)
        );
        assert_eq!(first["status"], json!({"power": true, "preset": "Rock"}));
        assert_eq!(server.subscriber_count(), 1);
    }

    #[test]
    fn by_default_the_status_document_is_the_applications_own_answer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");

        let socket = server.path().to_path_buf();
        let watcher = thread::spawn(move || {
            let mut watcher = subscribe(&socket, &["--watch"]);
            next_line(&mut watcher)
        });
        // The same question `fxsound --status --json` asks, through the same queue.
        let forwarded = wait_for(&server);
        assert_eq!(forwarded.commands(), [Command::Status { json: true }]);
        forwarded.respond(r#"{"power":false,"view":"lite"}"#);

        assert_eq!(
            watcher.join().expect("watcher thread"),
            "status power=false view=lite"
        );
    }

    #[test]
    fn an_application_that_cannot_report_its_status_refuses_the_subscription() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener
            .serve_with_status(Arc::new(|| None))
            .expect("serve");

        let mut watcher = subscribe(server.path(), &["--watch"]);
        let response: Response = serde_json::from_str(&next_line(&mut watcher)).expect("parse");
        assert!(!response.ok);
        assert!(response.stderr.contains("status"), "{response:?}");
        assert_eq!(server.subscriber_count(), 0);
    }

    #[test]
    fn published_events_arrive_as_json_and_as_plain_lines() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let mut json_watcher = subscribe(server.path(), &["--watch", "--json"]);
        let mut plain_watcher = subscribe(server.path(), &["--watch"]);
        next_line(&mut json_watcher);
        next_line(&mut plain_watcher);

        server.publish(&AppEvent::PresetChanged {
            direction: DeviceDirection::Output,
            name: Some("Bass Booster".to_owned()),
            modified: false,
        });

        let json: Value = serde_json::from_str(&next_line(&mut json_watcher)).expect("json");
        assert_eq!(json["event"], "preset_changed");
        assert_eq!(json["direction"], "output");
        assert_eq!(json["name"], "Bass Booster");
        assert_eq!(json["modified"], false);
        assert_eq!(
            next_line(&mut plain_watcher),
            r#"preset_changed direction=output name="Bass Booster" modified=false"#
        );
    }

    #[test]
    fn what_is_published_while_the_status_is_fetched_follows_it_instead_of_being_lost() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let (asked_tx, asked_rx) = bounded::<()>(1);
        let (go_tx, go_rx) = bounded::<()>(1);
        let server = listener
            .serve_with_status(Arc::new(move || {
                let _ = asked_tx.send(());
                go_rx.recv().ok()?;
                Some(json!({"power": true}))
            }))
            .expect("serve");

        let socket = server.path().to_path_buf();
        let watcher = thread::spawn(move || {
            let mut watcher = subscribe(&socket, &["--watch"]);
            (next_line(&mut watcher), next_line(&mut watcher))
        });
        asked_rx
            .recv_timeout(REPLY_TIMEOUT)
            .expect("the status is asked for");
        server.publish(&AppEvent::Power { on: false });
        go_tx.send(()).expect("release the status");

        let (first, second) = watcher.join().expect("watcher thread");
        assert_eq!(first, "status power=true");
        assert_eq!(second, "power on=false");
    }

    #[test]
    fn a_subscriber_that_hung_up_is_dropped_at_the_next_event() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let mut watcher = subscribe(server.path(), &["--watch"]);
        next_line(&mut watcher);
        assert_eq!(server.subscriber_count(), 1);

        drop(watcher);
        server.publish(&AppEvent::Power { on: true });
        assert_eq!(server.subscriber_count(), 0);
    }

    #[test]
    fn a_subscriber_that_hung_up_is_noticed_when_the_next_one_arrives() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let mut first = subscribe(server.path(), &["--watch"]);
        next_line(&mut first);
        drop(first);

        // Nothing has been published in between: only the newcomer's arrival can notice.
        let mut second = subscribe(server.path(), &["--watch"]);
        next_line(&mut second);
        assert_eq!(server.subscriber_count(), 1);
    }

    #[test]
    fn a_subscriber_that_stops_reading_is_dropped_instead_of_stalling_the_publisher() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        // Subscribed, and never read from again.
        let mut stuck = subscribe(server.path(), &["--watch"]);
        next_line(&mut stuck);

        let notice = AppEvent::Notice {
            message: "x".repeat(16 * 1024),
        };
        let mut slowest = Duration::ZERO;
        for _ in 0..200 {
            let started = Instant::now();
            server.publish(&notice);
            slowest = slowest.max(started.elapsed());
            if server.subscriber_count() == 0 {
                break;
            }
        }
        assert_eq!(server.subscriber_count(), 0, "the socket never filled up");
        assert!(
            slowest < WATCH_WRITE_TIMEOUT * 10,
            "one publish waited {slowest:?}"
        );
    }

    #[test]
    fn meters_go_only_to_subscribers_that_asked_and_at_most_four_times_a_second() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let mut metered = subscribe(server.path(), &["--watch", "--meters"]);
        let mut unmetered = subscribe(server.path(), &["--watch"]);
        next_line(&mut metered);
        next_line(&mut unmetered);
        assert!(server.wants_meters());

        let meters = AppEvent::InputMeters(crate::commands::InputMeters {
            voice_probability: 0.5,
            noise_floor_db: Some(-42.0),
            denoise_reduction_db: 0.0,
            gate_reduction_db: 0.0,
            compressor_reduction_db: 0.0,
            deesser_reduction_db: 0.0,
            denoise_running: false,
            deesser_running: false,
        });
        let start = Instant::now();
        // Ten a second for 300 ms: 0, 100 and 200 ms fall in the first quarter second, 300 ms
        // in the second.
        for tick in 0..4 {
            server.shared.broadcaster.publish(
                &meters,
                0,
                start + Duration::from_millis(100 * tick),
            );
        }
        server.publish(&AppEvent::Power { on: true });

        assert!(next_line(&mut metered).starts_with("input_meters "));
        assert!(next_line(&mut metered).starts_with("input_meters "));
        assert_eq!(next_line(&mut metered), "power on=true");
        assert_eq!(
            next_line(&mut unmetered),
            "power on=true",
            "no meters for a subscriber that did not ask"
        );
    }

    #[test]
    fn the_request_flag_asks_for_meters_as_well_as_the_command_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let stream = UnixStream::connect(server.path()).expect("connect");
        write_frame(
            &stream,
            &Request {
                v: PROTOCOL_VERSION,
                argv: argv(&["--watch"]),
                cwd: "/".to_owned(),
                watch: true,
                meters: true,
            },
        )
        .expect("write");
        wait_until("the subscriber is registered", || {
            server.subscriber_count() == 1
        });
        assert!(server.wants_meters());
    }

    #[test]
    fn a_watch_request_that_asks_for_status_as_well_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let mut watcher = subscribe(server.path(), &["--watch", "--status"]);
        let response: Response = serde_json::from_str(&next_line(&mut watcher)).expect("parse");
        assert!(!response.ok);
        assert!(response.stderr.contains("--watch"), "{response:?}");
        assert_eq!(server.subscriber_count(), 0);
    }

    #[test]
    fn a_watch_line_sent_as_an_ordinary_command_is_refused_by_the_command_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Instance::Primary(listener) = Instance::acquire_in(dir.path()).expect("acquire") else {
            panic!("expected to be primary");
        };
        let server = listener.serve().expect("serve");
        // Without `watch` in the frame it goes to the application like any other line, and the
        // application is the one that says one answer is not a stream.
        let socket = server.path().to_path_buf();
        let sender = thread::spawn(move || {
            forward_to(&socket, &argv(&["--watch"]), Path::new("/"), REPLY_TIMEOUT)
                .expect("the primary should answer")
        });
        let forwarded = wait_for(&server);
        assert_eq!(
            forwarded.commands(),
            [Command::Watch {
                json: false,
                meters: false
            }]
        );
        forwarded.reply(Response::failed("one answer is not a stream"));
        assert!(!sender.join().expect("client thread").ok);
        assert_eq!(server.subscriber_count(), 0);
    }

    #[test]
    fn the_watch_client_prints_every_event_and_exits_zero_when_the_instance_quits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({"power": true}));

        let socket = server.path().to_path_buf();
        let client = thread::spawn(move || {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = watch_to(
                &socket,
                &argv(&["--watch"]),
                Path::new("/"),
                false,
                &mut out,
                &mut err,
            );
            (code, String::from_utf8(out).expect("utf-8"), err)
        });
        wait_until("the watcher has its status", || {
            server.shared.broadcaster.active_count() == 1
        });
        server.publish(&AppEvent::Power { on: false });
        server.publish(&AppEvent::Quit);
        drop(server);

        let (code, out, err) = client.join().expect("client thread");
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        assert_eq!(out, "status power=true\npower on=false\nquit\n");
        assert!(err.is_empty());
    }

    #[test]
    fn watching_with_nobody_to_watch_says_fxsound_is_not_running_and_exits_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = watch_to(
            &dir.path().join(SOCKET_NAME),
            &argv(&["--watch"]),
            Path::new("/"),
            false,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(String::from_utf8_lossy(&err), "FxSound is not running\n");
    }

    #[test]
    fn a_refused_watch_prints_the_instances_reason_and_exits_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = watch_to(
            server.path(),
            &argv(&["--watch", "--status"]),
            Path::new("/"),
            false,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(String::from_utf8_lossy(&err).contains("--watch"));
    }

    #[test]
    fn a_reader_that_stops_taking_lines_ends_the_watch_quietly() {
        /// `fxsound --watch | head -n 1`: the pipe closes after the first line.
        struct ClosesAfterOneLine(usize);
        impl Write for ClosesAfterOneLine {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                self.0 -= 1;
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let socket = server.path().to_path_buf();
        let client = thread::spawn(move || {
            watch_to(
                &socket,
                &argv(&["--watch"]),
                Path::new("/"),
                false,
                &mut ClosesAfterOneLine(1),
                &mut Vec::new(),
            )
        });
        wait_until("the watcher has its status", || {
            server.shared.broadcaster.active_count() == 1
        });
        server.publish(&AppEvent::Power { on: false });
        assert_eq!(client.join().expect("client thread"), 0);
    }

    #[test]
    fn a_new_watcher_after_the_server_closed_is_turned_away() {
        let broadcaster = Broadcaster::default();
        broadcaster.close();
        let (ours, theirs) = UnixStream::pair().expect("pair");
        theirs
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .expect("timeout");
        let status: StatusSource = Arc::new(|| Some(json!({})));
        broadcaster.subscribe(ours, false, false, &status);

        let mut line = String::new();
        BufReader::new(&theirs).read_line(&mut line).expect("read");
        let response: Response = serde_json::from_str(&line).expect("parse");
        assert!(!response.ok);
        assert!(broadcaster.lock().list.is_empty());
    }

    #[test]
    fn a_watcher_cut_off_mid_request_prints_why_and_exits_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));

        // Past the cap and the socket buffer: the primary hangs up while the request is going out.
        let name = "n".repeat(16 * MAX_REQUEST_BYTES);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = watch_to(
            server.path(),
            &argv(&["--watch", &format!("--preset={name}")]),
            Path::new("/"),
            false,
            &mut out,
            &mut err,
        );

        assert_eq!(code, 1);
        assert!(out.is_empty());
        let err = String::from_utf8(err).expect("utf-8");
        assert!(err.contains("64 KiB"), "{err}");
        assert_eq!(server.subscriber_count(), 0);
    }

    #[test]
    fn a_watcher_past_the_subscriber_limit_is_turned_away_until_one_hangs_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));
        let mut watchers: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_| {
                let mut watcher = subscribe(server.path(), &["--watch"]);
                assert_eq!(next_line(&mut watcher), "status");
                watcher
            })
            .collect();
        assert_eq!(server.subscriber_count(), MAX_SUBSCRIBERS);

        let mut refused = subscribe(server.path(), &["--watch"]);
        let response: Response =
            serde_json::from_str(&next_line(&mut refused)).expect("a refusal, not an event");
        assert!(!response.ok);
        assert!(
            response.stderr.contains("too many --watch streams"),
            "{response:?}"
        );
        assert_eq!(server.subscriber_count(), MAX_SUBSCRIBERS);

        // One hangs up: the next newcomer notices, and takes its place.
        drop(watchers.pop());
        let mut next = subscribe(server.path(), &["--watch"]);
        assert_eq!(next_line(&mut next), "status");
        assert_eq!(server.subscriber_count(), MAX_SUBSCRIBERS);
    }

    #[test]
    fn watch_streams_left_open_do_not_use_up_the_places_of_callers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({}));

        // More open streams than there are places for callers.
        let _watchers: Vec<_> = (0..=MAX_CONNECTIONS)
            .map(|_| {
                let mut watcher = subscribe(server.path(), &["--watch"]);
                next_line(&mut watcher);
                watcher
            })
            .collect();
        wait_until("every watch has given its place back", || {
            connections(&server) == 0
        });
        a_keybind_gets_through(&server);
    }

    // ---- the way out, the deadline and the stream's end (C15) --------------------------------

    fn primary(dir: &Path) -> Server {
        let Instance::Primary(listener) = Instance::acquire_in(dir).expect("acquire") else {
            panic!("expected to be primary");
        };
        listener.serve().expect("serve")
    }

    #[test]
    fn a_command_line_forwarded_after_the_way_out_began_is_refused_at_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = primary(dir.path());
        server.refuse_pending();
        let started = Instant::now();
        let response = forward_to(
            server.path(),
            &argv(&["--preset", "Rock"]),
            Path::new("/"),
            REPLY_TIMEOUT,
        )
        .expect("an answer");
        assert!(!response.ok, "never a bare acknowledgement");
        assert_eq!(response.stderr, SHUTTING_DOWN);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            server.try_recv().is_none(),
            "nothing reached the GUI thread"
        );
    }

    #[test]
    fn a_command_line_still_queued_when_the_server_goes_is_refused_not_acknowledged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = primary(dir.path());
        let socket = server.path().to_owned();
        let caller = thread::spawn(move || {
            forward_to(
                &socket,
                &argv(&["--status", "--json"]),
                Path::new("/"),
                REPLY_TIMEOUT,
            )
        });
        wait_until("the command is queued", || server.arrivals().len() == 1);
        // The way out with nothing draining the channel after it began: the GUI thread is busy
        // handing the default devices back, and then the server is dropped.
        drop(server);
        let response = caller.join().expect("the caller").expect("an answer");
        assert!(!response.ok);
        assert_eq!(response.stderr, SHUTTING_DOWN);
    }

    #[test]
    fn a_forwarded_command_dropped_unanswered_during_the_way_out_says_so() {
        let closing = Closing::default();
        let answer = |closing: &Closing| {
            let (tx, rx) = bounded(1);
            drop(
                Forwarded::new(Vec::new(), PathBuf::from("/"), move |response| {
                    let _ = tx.send(response);
                })
                .closing_with(closing.clone()),
            );
            rx.recv().expect("an answer")
        };
        assert!(answer(&closing).ok, "dropped by a handler: acknowledged");
        closing.set();
        let response = answer(&closing);
        assert!(!response.ok, "dropped with the channel: nothing ran it");
        assert_eq!(response.stderr, SHUTTING_DOWN);
    }

    #[test]
    fn the_control_channel_refuses_at_once_once_the_way_out_began() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = primary(dir.path());
        let control = server.control();
        server.refuse_pending();
        let started = Instant::now();
        let response = block_on(control.call(vec![Command::Status { json: true }]));
        assert!(!response.ok);
        assert_eq!(response.stderr, SHUTTING_DOWN);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(server.try_recv().is_none());
    }

    #[test]
    fn a_command_whose_caller_stopped_waiting_is_marked_so_and_one_answered_is_not() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let tx: WakingSender<Forwarded> = tx.into();
        let closing = Closing::default();
        let response = hand_over_within(
            &tx,
            &closing,
            vec![Command::Status { json: true }],
            PathBuf::from("/"),
            Duration::from_millis(20),
        );
        assert!(response.is_unanswered());
        let late = rx.try_recv().expect("still queued");
        assert!(late.is_abandoned(), "the GUI thread will not run it late");

        let gui = thread::spawn(move || {
            let forwarded = rx.recv().expect("a command");
            assert!(!forwarded.is_abandoned());
            forwarded.respond("done");
        });
        let response = hand_over_within(
            &tx,
            &closing,
            vec![Command::Status { json: true }],
            PathBuf::from("/"),
            REPLY_TIMEOUT,
        );
        gui.join().expect("the GUI thread");
        assert_eq!(response.stdout, "done");
    }

    #[test]
    fn a_request_trickled_a_byte_at_a_time_is_cut_off_at_one_deadline_for_the_whole_line() {
        let (ours, theirs) = UnixStream::pair().expect("pair");
        let trickle = thread::spawn(move || {
            let mut theirs = theirs;
            // A byte every 20 ms, each well inside any per-read timeout, and never a newline.
            for _ in 0..100 {
                if theirs.write_all(b"{").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let error = read_request(&ours, Duration::from_millis(200)).expect_err("cut off");
        let took = started.elapsed();
        assert_eq!(error, REQUEST_TOO_SLOW);
        assert!(took < Duration::from_secs(1), "{took:?}");
        drop(ours);
        trickle.join().expect("the trickle");
    }

    #[test]
    fn a_whole_request_inside_the_deadline_is_read() {
        let (ours, mut theirs) = UnixStream::pair().expect("pair");
        write_frame(
            &theirs,
            &Request {
                v: PROTOCOL_VERSION,
                argv: argv(&["--next-preset"]),
                ..Request::default()
            },
        )
        .expect("write");
        theirs.flush().expect("flush");
        let request = read_request(&ours, Duration::from_secs(1)).expect("read");
        assert_eq!(request.argv, argv(&["--next-preset"]));
    }

    #[test]
    fn the_quit_event_is_told_apart_in_both_spellings() {
        assert!(is_quit_event("quit\n"));
        assert!(is_quit_event(&(AppEvent::Quit.to_json(5) + "\n")));
        assert!(!is_quit_event("power on=false\n"));
        assert!(!is_quit_event(
            r#"{"v":1,"event":"power","ts":5,"on":true}"#
        ));
        assert!(!is_quit_event("quitting\n"));
    }

    /// A stand-in instance on `socket` that reads one request and writes `stream` before it
    /// hangs up.
    fn instance_that_writes(socket: &Path, stream: &'static str) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).expect("bind");
        thread::spawn(move || {
            let (connection, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&connection)
                .read_line(&mut line)
                .expect("the request");
            (&connection).write_all(stream.as_bytes()).expect("write");
        })
    }

    fn watch(socket: &Path) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = watch_to(
            socket,
            &argv(&["--watch"]),
            Path::new("/"),
            false,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    }

    #[test]
    fn a_watch_stream_cut_off_without_quit_exits_one_and_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join(SOCKET_NAME);
        // A reader that fell behind: the last write stopped half-way through a line.
        let instance = instance_that_writes(&socket, "status power=true\npower on=fal");
        let (code, out, err) = watch(&socket);
        instance.join().expect("the instance");
        assert_eq!(code, 1);
        assert_eq!(out, "status power=true\n", "the half line is not printed");
        assert!(err.contains("cut off"), "{err}");
    }

    #[test]
    fn a_watch_stream_that_ends_after_quit_exits_zero_quietly() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join(SOCKET_NAME);
        let instance = instance_that_writes(
            &socket,
            "{\"v\":1,\"event\":\"status\",\"ts\":1}\n{\"v\":1,\"event\":\"quit\",\"ts\":2}\n",
        );
        let (code, _out, err) = watch(&socket);
        instance.join().expect("the instance");
        assert_eq!(code, 0, "{err}");
        assert!(err.is_empty());
    }

    #[test]
    fn a_watcher_dropped_by_a_server_that_goes_without_quit_is_not_told_it_quit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let server = serve_with(dir.path(), json!({"power": true}));
        let socket = server.path().to_path_buf();
        let client = thread::spawn(move || watch(&socket));
        wait_until("the watcher has its status", || {
            server.shared.broadcaster.active_count() == 1
        });
        server.publish(&AppEvent::Power { on: false });
        drop(server);
        let (code, out, err) = client.join().expect("client thread");
        assert_eq!(code, 1);
        assert_eq!(out, "status power=true\npower on=false\n");
        assert!(err.contains("cut off"), "{err}");
    }
}
