//! What the workspace's tests share, and nothing else does: built with the `test-support`
//! feature, which only the crates' dev-dependencies turn on, so none of it is in FxSound itself.
//!
//! Two kinds of leftover used to outlive the tests that made them. Scratch directories under
//! `/tmp`, removed on a test's last line — which a failed assertion never reaches — piled up by
//! the thousand. And the children of the tests that run PipeWire — a private daemon, `pw-record`,
//! `pw-cat`, a private `dbus-daemon` — were killed when their handles were dropped, which an
//! interrupted run never does: one such run left daemons and recorders writing raw audio into
//! `/tmp` for hours. [`ScratchDir`] answers the first, [`spawn`] the second.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{OnceLock, mpsc};

/// A directory of one test's own, removed when it is dropped — which unwinding from a panic
/// does too.
///
/// Always a new one: made under a name nothing else has, never an old directory found under the
/// same name. A test that reused one would find whatever an earlier run left in it, down to the
/// socket of a daemon that is still running.
pub struct ScratchDir(tempfile::TempDir);

impl ScratchDir {
    /// A new directory in the temporary directory, called `fxsound-<tag>-` and a random suffix.
    #[must_use]
    pub fn new(tag: &str) -> Self {
        Self::within(&std::env::temp_dir(), tag)
    }

    /// [`Self::new`], with room left in its path for a Unix socket a few levels down: a socket's
    /// path may not be longer than 108 bytes, which a deep `TMPDIR` can use up on its own. In
    /// `/tmp` itself when the temporary directory is that deep.
    #[must_use]
    pub fn for_sockets(tag: &str) -> Self {
        let parent = std::env::temp_dir();
        // `fxsound-`, the tag, `-` and the six random characters.
        let length = parent.as_os_str().len() + "/fxsound--XXXXXX".len() + tag.len();
        if length <= 48 {
            Self::within(&parent, tag)
        } else {
            Self::within(Path::new("/tmp"), tag)
        }
    }

    fn within(parent: &Path, tag: &str) -> Self {
        let made = tempfile::Builder::new()
            .prefix(&format!("fxsound-{tag}-"))
            .tempdir_in(parent)
            .unwrap_or_else(|error| {
                panic!(
                    "a scratch directory could not be made in {}: {error}",
                    parent.display()
                )
            });
        Self(made)
    }

    /// Where it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path()
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

/// A command for `program` that cannot outlive the test process: run through
/// `setpriv --pdeathsig KILL --` where util-linux's `setpriv` is installed, so that the kernel
/// kills it when the process dies — by `SIGKILL` too, which runs no destructor. Where it is not,
/// or `program` is not installed at all, the plain command, which [`spawn`] then watches.
///
/// The kernel's signal comes when the *thread* that started the child ends, not the process.
/// [`spawn`] starts such commands from a thread that lasts as long as the process; a command made
/// here and run straight away with `output` or `status`, which block the thread until it ends,
/// needs no such thread. Anything left running is handed to [`spawn`], never started with the
/// command's own `spawn`.
#[must_use]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    match setpriv() {
        Some(setpriv) if on_path(program).is_some() => {
            let mut command = Command::new(setpriv);
            command.args(["--pdeathsig", "KILL", "--"]).arg(program);
            command
        }
        _ => Command::new(program),
    }
}

/// [`command`], for a program that writes a file for as long as it runs, such as `pw-record`:
/// no file it writes can grow past `bytes`. It is run under `ulimit -f` — and dies of `SIGXFSZ`,
/// without a core file, when it tries — so a recorder that nobody stopped cannot fill `/tmp`.
#[must_use]
pub fn command_writing_at_most(program: impl AsRef<OsStr>, bytes: u64) -> Command {
    let program = program.as_ref();
    if on_path(program).is_none() {
        return Command::new(program);
    }
    let mut command = command("sh");
    command
        .arg("-c")
        .arg(format!(
            "ulimit -c 0; ulimit -f {} && exec \"$0\" \"$@\"",
            bytes.div_ceil(512)
        ))
        .arg(program);
    command
}

/// Start `command` — made by [`command`] or [`command_writing_at_most`] — as a child that goes
/// when the test does: killed and waited for when the [`Guarded`] is dropped, which a panicking
/// test's unwinding does as well, and killed when the test process dies, however it dies.
///
/// A command run through `setpriv` is started from a thread kept for the purpose, so the child
/// lives until it is dropped or the process ends, whichever thread it is handed to. Any other
/// command gets a watchdog: a shell that waits for the end of a pipe only this process holds,
/// and kills the child when it reads it — that is, when this process has gone.
///
/// # Errors
///
/// Whatever starting the program or its watchdog failed with: `NotFound` when the program is not
/// installed.
pub fn spawn(mut command: Command) -> io::Result<Guarded> {
    if setpriv().is_some_and(|setpriv| command.get_program() == setpriv.as_os_str()) {
        let child = spawned_by_the_keeper(command)?;
        return Ok(Guarded {
            child,
            watchdog: None,
        });
    }
    let mut child = command.spawn()?;
    match watchdog(child.id()) {
        Ok(watchdog) => Ok(Guarded {
            child,
            watchdog: Some(watchdog),
        }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// A child started by [`spawn`]. Dropped, it is killed and waited for.
pub struct Guarded {
    child: Child,
    /// The watchdog of a child `setpriv` could not start, killed first so that it can never kill
    /// a later process that happens to get the child's id.
    watchdog: Option<Child>,
}

impl Guarded {
    /// The child's process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// The child's standard input, when it was piped.
    pub fn stdin(&mut self) -> Option<&mut ChildStdin> {
        self.child.stdin.as_mut()
    }

    /// The child's standard output, when it was piped and has not been taken yet.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Kill the child with `SIGKILL`, as [`Child::kill`] does.
    ///
    /// # Errors
    ///
    /// As [`Child::kill`].
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// Wait for the child to end, as [`Child::wait`] does.
    ///
    /// # Errors
    ///
    /// As [`Child::wait`].
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.retire_watchdog();
        Ok(status)
    }

    /// Whether the child has ended, as [`Child::try_wait`] says.
    ///
    /// # Errors
    ///
    /// As [`Child::try_wait`].
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.retire_watchdog();
        }
        Ok(status)
    }

    /// What became of the child, for the message of a test that gave up on it: how it ended —
    /// waiting a moment for that — or that it is still running, and then the end of what it wrote
    /// to `stderr`, the file [`log_to`] sent its standard error to. A tool that "could not run"
    /// says why here, where a message that it never appeared says nothing.
    pub fn account(&mut self, stderr: Option<&Path>) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let ended = loop {
            match self.try_wait() {
                Ok(Some(status)) => break format!("it exited with {status}"),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Ok(None) => break "it is still running".to_owned(),
                Err(error) => break format!("its status could not be read: {error}"),
            }
        };
        let Some(stderr) = stderr else {
            return ended;
        };
        match std::fs::read(stderr) {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => {
                format!("{ended}, and wrote nothing to its standard error")
            }
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text.lines().collect();
                let tail = &lines[lines.len().saturating_sub(STDERR_LINES)..];
                format!(
                    "{ended}; its standard error{}:\n    {}",
                    if tail.len() < lines.len() {
                        format!(", the last {STDERR_LINES} lines")
                    } else {
                        String::new()
                    },
                    tail.join("\n    ")
                )
            }
            Err(error) => format!("{ended}; {} could not be read: {error}", stderr.display()),
        }
    }

    /// Kill the watchdog, once the child it watches has been waited for and its id is free.
    fn retire_watchdog(&mut self) {
        if let Some(mut watchdog) = self.watchdog.take() {
            let _ = watchdog.kill();
            let _ = watchdog.wait();
        }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.retire_watchdog();
    }
}

/// How much of a child's standard error [`Guarded::account`] quotes.
const STDERR_LINES: usize = 40;

/// A standard error for a child that goes to a new file at `path`, for [`Guarded::account`] to
/// quote; nowhere, when the file cannot be made.
#[must_use]
pub fn log_to(path: &Path) -> Stdio {
    std::fs::File::create(path).map_or_else(|_| Stdio::null(), Stdio::from)
}

/// `setpriv`, when it is installed and knows `--pdeathsig` (util-linux 2.33 and later). Asked once.
fn setpriv() -> Option<&'static Path> {
    static SETPRIV: OnceLock<Option<PathBuf>> = OnceLock::new();
    SETPRIV
        .get_or_init(|| {
            let setpriv = on_path(OsStr::new("setpriv"))?;
            Command::new(&setpriv)
                .args(["--pdeathsig", "KILL", "--", "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
                .then_some(setpriv)
        })
        .as_deref()
}

/// Where `program` is: itself when it names a path, the first executable file of that name on
/// `PATH` otherwise.
fn on_path(program: &OsStr) -> Option<PathBuf> {
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if program.as_encoded_bytes().contains(&b'/') {
        let path = PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| executable(path))
}

/// A command to start, and where to send what starting it gave.
type Request = (Command, mpsc::Sender<io::Result<Child>>);

/// Start `command` from the keeper: a thread that is never left, so the children it starts are
/// killed by the kernel when the process ends and not when the test's thread does.
fn spawned_by_the_keeper(command: Command) -> io::Result<Child> {
    static KEEPER: OnceLock<mpsc::Sender<Request>> = OnceLock::new();
    let keeper = KEEPER.get_or_init(|| {
        let (requests, incoming) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("fxsound-test-children".to_owned())
            .spawn(move || {
                for (mut command, reply) in incoming {
                    let _ = reply.send(command.spawn());
                }
            })
            .expect("the thread that starts the tests' children");
        requests
    });
    let gone = || io::Error::other("the thread that starts the tests' children is gone");
    let (reply, answer) = mpsc::channel();
    keeper.send((command, reply)).map_err(|_| gone())?;
    answer.recv().map_err(|_| gone())?
}

/// A shell that kills `pid` once its standard input ends. Nothing is ever written to it, so it
/// ends only when the write end closes, and that end is this process's alone — it is opened
/// close-on-exec, so no other child holds a copy — and closes when this process dies.
fn watchdog(pid: u32) -> io::Result<Child> {
    Command::new("sh")
        .arg("-c")
        .arg("read -r line; kill -KILL \"$1\" 2>/dev/null")
        .arg("fxsound-test-watchdog")
        .arg(pid.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::time::{Duration, Instant};

    /// Set, in a test process of this binary's that a test here starts, to what the child half
    /// ([`the_child_half_starts_its_children_and_then_panics_or_waits`]) does after starting its
    /// children: `panic` or `wait`.
    const CHILD_HALF: &str = "FXSOUND_TEST_SUPPORT_CHILD_HALF";

    /// Whether the process `pid` is still one of the `sleep 600`s started here. A zombie, an id
    /// nobody has, or an id a later process got is not.
    fn sleeping(pid: u32) -> bool {
        let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                // The state follows the command name, which is in parentheses.
                let state = stat.rsplit_once(") ")?.1.chars().next()?;
                Some(state != 'Z' && state != 'X')
            })
            .unwrap_or(false);
        alive
            && std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| cmdline == b"sleep\x00600\x00")
    }

    /// Wait until every one of `pids` is sleeping: `setpriv` is still becoming `sleep` for a moment
    /// after it has started. Whether they all were within the patience.
    fn all_sleeping_within(pids: &[u32], patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            if pids.iter().all(|&pid| sleeping(pid)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until none of `pids` is still sleeping; the ones that are, when the patience ran out.
    fn still_sleeping_after(pids: &[u32], patience: Duration) -> Vec<u32> {
        let deadline = Instant::now() + patience;
        loop {
            let left: Vec<u32> = pids.iter().copied().filter(|&pid| sleeping(pid)).collect();
            if left.is_empty() || Instant::now() >= deadline {
                return left;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Two `sleep`s, one started through [`command`] — `setpriv`'s, where it is installed — and
    /// one as a plain command, which always gets the watchdog.
    fn two_sleepers() -> [Guarded; 2] {
        let mut through_setpriv = command("sleep");
        through_setpriv.arg("600");
        let mut watched = Command::new("sleep");
        watched.arg("600");
        [
            spawn(through_setpriv).expect("sleep, through command()"),
            spawn(watched).expect("sleep, watched"),
        ]
    }

    #[test]
    fn a_scratch_directory_is_new_every_time_and_goes_when_dropped() {
        let first = ScratchDir::new("scratch-test");
        let second = ScratchDir::new("scratch-test");
        assert_ne!(first.path(), second.path());
        assert!(first.is_dir() && second.is_dir());
        let kept = first.to_path_buf();
        std::fs::write(first.join("left"), b"behind").expect("a file in it");
        drop(first);
        assert!(!kept.exists(), "{} is still there", kept.display());
    }

    #[test]
    fn a_scratch_directory_is_removed_by_a_panic_too() {
        let (sender, receiver) = mpsc::channel();
        let unwound = std::thread::spawn(move || {
            let dir = ScratchDir::new("scratch-panic");
            sender.send(dir.to_path_buf()).expect("the path");
            panic!("on purpose, with the directory still held");
        })
        .join();
        assert!(unwound.is_err());
        let dir = receiver.recv().expect("the path");
        assert!(!dir.exists(), "{} is still there", dir.display());
    }

    #[test]
    fn a_directory_for_sockets_leaves_room_for_one() {
        let dir = ScratchDir::for_sockets("sockets");
        assert!(dir.as_os_str().len() <= 64, "{}", dir.display());
        assert!(dir.is_dir());
    }

    #[test]
    fn a_program_that_is_not_installed_fails_to_start_as_not_found() {
        let error = spawn(command("fxsound-no-such-program-for-tests"))
            .err()
            .expect("nothing to start");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let error = spawn(command_writing_at_most("fxsound-no-such-program", 1024))
            .err()
            .expect("nothing to start");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_child_started_on_a_thread_that_ends_lives_on_until_it_is_dropped() {
        // `setpriv`'s signal follows the thread that started the child; the keeper is why a
        // child handed out of a short-lived thread is not killed with it.
        let [through_setpriv, watched] = std::thread::spawn(two_sleepers)
            .join()
            .expect("the thread started them");
        let pids = [through_setpriv.id(), watched.id()];
        assert!(
            all_sleeping_within(&pids, Duration::from_secs(10)),
            "{pids:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(all_sleeping_within(&pids, Duration::ZERO), "{pids:?}");
        drop((through_setpriv, watched));
        assert_eq!(still_sleeping_after(&pids, Duration::ZERO), []);
    }

    #[test]
    fn a_panicking_test_kills_the_children_it_started() {
        let (sender, receiver) = mpsc::channel();
        let unwound = std::thread::spawn(move || {
            let sleepers = two_sleepers();
            sender
                .send(sleepers.each_ref().map(Guarded::id))
                .expect("the ids");
            panic!("on purpose, with the children still held");
        })
        .join();
        assert!(unwound.is_err());
        let pids = receiver.recv().expect("the ids");
        assert_eq!(still_sleeping_after(&pids, Duration::ZERO), []);
    }

    #[test]
    fn a_child_that_failed_is_accounted_for_with_its_status_and_its_standard_error() {
        let dir = ScratchDir::new("account");
        let log = dir.join("stderr");
        let mut failing = command("sh");
        failing
            .args(["-c", "echo 'no such option: --raw' >&2; exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_to(&log));
        let mut failing = spawn(failing).expect("sh");
        let account = failing.account(Some(&log));
        assert!(account.contains("exit status: 3"), "{account}");
        assert!(account.contains("no such option: --raw"), "{account}");

        let mut quiet = command("sleep");
        quiet.arg("600").stderr(log_to(&dir.join("quiet")));
        let mut quiet = spawn(quiet).expect("sleep");
        let account = quiet.account(Some(&dir.join("quiet")));
        assert!(account.contains("still running"), "{account}");
        assert!(account.contains("nothing"), "{account}");
        assert!(quiet.account(None).contains("still running"));
    }

    #[test]
    fn a_recording_stops_at_its_size_limit() {
        let dir = ScratchDir::new("size-limit");
        let file = dir.join("recording");
        let mut writer = command_writing_at_most("sh", 64 * 1024);
        writer
            .arg("-c")
            .arg("exec cat /dev/zero > \"$0\"")
            .arg(&file)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        let mut writer = spawn(writer).expect("the writer");
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = writer.try_wait().expect("its status") {
                break status;
            }
            assert!(Instant::now() < deadline, "the writer never stopped");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
        let written = std::fs::metadata(&file).expect("the file").len();
        assert_eq!(written, 64 * 1024);
    }

    /// Not a test of its own: the half of the two below that runs in a test process of this
    /// binary's, which starts its children, says which they are and then does what
    /// [`CHILD_HALF`] says — panics, or waits a minute to be killed. Without it, it passes at once.
    #[test]
    fn the_child_half_starts_its_children_and_then_panics_or_waits() {
        let Some(then) = std::env::var_os(CHILD_HALF) else {
            return;
        };
        let sleepers = two_sleepers();
        println!("children {} {}", sleepers[0].id(), sleepers[1].id());
        std::io::stdout().flush().expect("the ids are out");
        assert_ne!(then, "panic", "on purpose, with the children still held");
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Start the child half with [`CHILD_HALF`] set to `then` — guarded itself, so a failing test
    /// here cannot leave it waiting — and read the ids of its children.
    fn child_half(then: &str) -> (Guarded, Vec<u32>) {
        let (_, module) = module_path!()
            .split_once("::")
            .expect("a module of the crate");
        let name = format!("{module}::the_child_half_starts_its_children_and_then_panics_or_waits");
        let mut process = command(std::env::current_exe().expect("this test binary"));
        process
            .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
            .env(CHILD_HALF, then)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = spawn(process).expect("the child half");
        let lines = BufReader::new(process.take_stdout().expect("piped")).lines();
        // The harness puts the test's name in front of what it prints, on the same line.
        let pids = lines
            .map_while(Result::ok)
            .find_map(|line| {
                let (_, ids) = line.rsplit_once("children ")?;
                ids.split(' ').map(|id| id.parse().ok()).collect()
            })
            .expect("the child half said which its children are");
        (process, pids)
    }

    #[test]
    fn a_test_process_that_panics_leaves_no_child_behind() {
        let (mut process, pids) = child_half("panic");
        let status = process.wait().expect("the child half ended");
        assert!(!status.success(), "its test panicked, so it failed");
        assert_eq!(still_sleeping_after(&pids, Duration::ZERO), []);
    }

    #[test]
    fn a_test_process_killed_outright_leaves_no_child_behind() {
        let (mut process, pids) = child_half("wait");
        assert!(
            all_sleeping_within(&pids, Duration::from_secs(10)),
            "{pids:?}"
        );
        process.kill().expect("SIGKILL");
        process.wait().expect("the child half ended");
        // What is left is the kernel's `setpriv` signal and the watchdog: no destructor ran.
        assert_eq!(still_sleeping_after(&pids, Duration::from_secs(10)), []);
    }
}
