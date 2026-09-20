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

use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clap::Parser as _;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, unbounded};
use serde::{Deserialize, Serialize};

use crate::cli::{Cli, Command};

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
/// rather than its own timeout.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(4);

/// A request frame: one line of JSON, one per connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// [`PROTOCOL_VERSION`].
    pub v: u32,
    /// The forwarding process's real `argv`, `argv[0]` included.
    pub argv: Vec<String>,
    /// The forwarding process's working directory, so a relative path in an argument can still be
    /// resolved. The Windows build cannot do this at all — it `chdir`s to the exe directory at
    /// startup (`Main.cpp:308-321`), which this port does not port (§2.3).
    pub cwd: String,
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
    /// # Errors
    ///
    /// If the socket cannot be put into the blocking mode the accept loop needs.
    pub fn serve(self) -> io::Result<Server> {
        let Self {
            listener,
            lock,
            path,
        } = self;
        listener.set_nonblocking(false)?;

        let (tx, rx) = unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = {
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("fxsound-ipc".to_owned())
                .spawn(move || accept_loop(&listener, &tx, &shutdown))?
        };

        Ok(Server {
            rx,
            path,
            shutdown,
            worker: Some(worker),
            _lock: lock,
        })
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

    /// Where the socket lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The accept loop is parked inside `accept()`; connecting to ourselves wakes it so it can
        // see the flag. The connection is closed immediately and the loop discards it.
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
pub struct Forwarded {
    commands: Vec<Command>,
    cwd: PathBuf,
    reply: Option<Sender<Response>>,
}

impl Forwarded {
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
            let _ = reply.send(response);
        }
    }
}

impl Drop for Forwarded {
    fn drop(&mut self) {
        self.send(Response::ok());
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
    };
    write_frame(&stream, &request)?;

    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the running instance closed the control socket without answering",
        ));
    }
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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

fn accept_loop(listener: &UnixListener, tx: &Sender<Forwarded>, shutdown: &AtomicBool) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match stream {
            Ok(stream) => handle_connection(&stream, tx),
            Err(e) => {
                log::warn!("control socket accept failed: {e}");
                return;
            }
        }
    }
}

fn handle_connection(stream: &UnixStream, tx: &Sender<Forwarded>) {
    if let Err(e) = stream.set_read_timeout(Some(REQUEST_TIMEOUT)) {
        log::warn!("could not arm the control socket read timeout: {e}");
        return;
    }
    let response = match read_request(stream) {
        Ok(request) => dispatch(request, tx),
        Err(e) => Response::failed(e),
    };
    if let Err(e) = write_frame(stream, &response) {
        log::warn!("could not answer a forwarded command line: {e}");
    }
}

fn read_request(stream: &UnixStream) -> Result<Request, String> {
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("could not read the request: {e}"))?;
    let request: Request =
        serde_json::from_str(&line).map_err(|e| format!("malformed request: {e}"))?;
    if request.v != PROTOCOL_VERSION {
        return Err(format!(
            "this FxSound speaks control protocol v{PROTOCOL_VERSION}, the caller speaks v{}",
            request.v
        ));
    }
    Ok(request)
}

/// Parse the forwarded argv and hand the commands to the GUI thread.
///
/// Parsing happens *here*, on the primary, so that the forwarding process gets the same error text
/// it would have got had it parsed the line itself — which is the whole reason the frame carries
/// argv rather than a pre-parsed command list.
fn dispatch(request: Request, tx: &Sender<Forwarded>) -> Response {
    let cli = match Cli::try_parse_from(&request.argv) {
        Ok(cli) => cli,
        Err(e) => return Response::failed(e.render().to_string()),
    };

    let (reply_tx, reply_rx) = bounded(1);
    let forwarded = Forwarded {
        commands: cli.commands(),
        cwd: PathBuf::from(request.cwd),
        reply: Some(reply_tx),
    };

    match tx.try_send(forwarded) {
        Ok(()) => {}
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            return Response::failed("FxSound is shutting down and cannot take that command");
        }
    }

    match reply_rx.recv_timeout(HANDLER_TIMEOUT) {
        Ok(response) => response,
        Err(RecvTimeoutError::Timeout) => {
            Response::failed("FxSound did not answer in time; the command may still be running")
        }
        // The handler dropped the `Forwarded` without answering and without the `Drop` impl
        // running, which can only mean the process is going away.
        Err(RecvTimeoutError::Disconnected) => Response::ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{PowerCommand, PresetCommand, WindowCommand};
    use std::time::Instant;

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
}
