//! Suspend and resume: logind's `PrepareForSleep` on the system bus (U13).
//!
//! logind broadcasts `org.freedesktop.login1.Manager.PrepareForSleep(true)` as the machine goes to
//! sleep and `PrepareForSleep(false)` once it is back. A thread of its own, `fxsound-sleep`,
//! listens for the signal and hands each word to the GUI thread over a channel; the controller
//! does the rest ([`crate::App::system_sleeping`]): both lanes muted on the way down, their
//! filters cleared and unmuted on the way up, the engine told to wait out the devices coming
//! back. Upstream does the same from Windows' suspend notification (d62e024,
//! `FxController.cpp:2145-2159`).
//!
//! What it deliberately does not do is hold the suspend up. A delay inhibitor would give the mute
//! time to land before the machine stops, and upstream's maintainers turned exactly that down (its
//! PR #533): a sound program has no business delaying the user's suspend. The word may reach the
//! engine only after the resume, then; the mute and the unmute that follow it at once cost
//! nothing, and the clean start is what matters.
//!
//! No system bus — a container, a minimal session — is not an error: the watcher says so in the
//! log and ends, and FxSound runs on without it.
//!
//! # Threads and runtimes
//!
//! The watcher builds a current-thread tokio runtime on its own thread, as the D-Bus service does
//! on its own ([`crate::dbus`]), and never touches the global runtime zbus keeps for its blocking
//! API: a call parked in there for the whole session would be driving every other blocking zbus
//! call in the process, notify-rust's toasts among them.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::Poll;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::wake::WakingSender;
use tokio::sync::oneshot;
use zbus::export::futures_core::Stream;
use zbus::message::Type;
use zbus::{MatchRule, Message, MessageStream};

/// logind's well-known name on the system bus.
pub const LOGIND: &str = "org.freedesktop.login1";
/// Its manager object.
pub const LOGIND_PATH: &str = "/org/freedesktop/login1";
/// The manager's interface.
pub const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";
/// The signal, with one `b` argument: `true` going to sleep, `false` resumed.
pub const PREPARE_FOR_SLEEP: &str = "PrepareForSleep";

/// How long connecting and subscribing may take before the watcher gives up. zbus has no timeout
/// of its own for either, and a bus that accepts and then says nothing would leave the watcher
/// starting for ever with nothing in the log to say why.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Which bus to listen on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SleepBus {
    /// The system bus: `DBUS_SYSTEM_BUS_ADDRESS`, or `/run/dbus/system_bus_socket`.
    System,
    /// A bus at this address — a private `dbus-daemon`, for the tests. Never the real system bus.
    Address(String),
}

/// Where the watcher is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchState {
    /// Connecting and subscribing.
    Starting,
    /// Subscribed: a `PrepareForSleep` from logind reaches the channel.
    Listening,
    /// Not listening: no bus, the subscription refused, the connection gone, or stopped.
    Unavailable,
}

/// [`WatchState`], shared between the watcher's thread and its handle.
#[derive(Debug)]
struct StateCell {
    state: Mutex<WatchState>,
    settled: Condvar,
}

impl StateCell {
    fn get(&self) -> WatchState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, state: WatchState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
        self.settled.notify_all();
    }
}

/// The GUI thread's end of the watcher. Dropping it stops the watcher.
#[derive(Debug)]
pub struct SleepWatch {
    /// Dropped to stop the watcher: its receiver then wakes the thread, wherever it is waiting.
    stop: Option<oneshot::Sender<()>>,
    state: Arc<StateCell>,
    thread: Option<JoinHandle<()>>,
}

impl SleepWatch {
    /// Listen for logind's `PrepareForSleep` on the system bus, and send what each one says on
    /// `sleeping`. Never fails: without a system bus the watcher logs why and ends.
    ///
    /// A [`WakingSender`] wakes the GUI thread with each one (0.4.0 design §12): nothing holds
    /// the suspend up, so the mute has to happen now rather than at the pump's next keepalive.
    #[must_use]
    pub fn start(sleeping: impl Into<WakingSender<bool>>) -> Self {
        Self::start_on(SleepBus::System, sleeping)
    }

    /// [`SleepWatch::start`] on `bus`.
    #[must_use]
    pub fn start_on(bus: SleepBus, sleeping: impl Into<WakingSender<bool>>) -> Self {
        let sleeping = sleeping.into();
        let (stop, stopped) = oneshot::channel();
        let state = Arc::new(StateCell {
            state: Mutex::new(WatchState::Starting),
            settled: Condvar::new(),
        });
        let thread = {
            let state = Arc::clone(&state);
            thread::Builder::new()
                .name("fxsound-sleep".to_owned())
                .spawn(move || watch(&bus, &sleeping, stopped, &state))
        };
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(err) => {
                log::warn!("not watching for suspend: the thread did not start ({err})");
                state.set(WatchState::Unavailable);
                None
            }
        };
        Self {
            stop: Some(stop),
            state,
            thread,
        }
    }

    /// Where the watcher is.
    #[must_use]
    pub fn state(&self) -> WatchState {
        self.state.get()
    }

    /// Wait until the watcher is listening or has given up, for at most `timeout`; returns where
    /// it is then.
    #[must_use]
    pub fn wait_until_settled(&self, timeout: Duration) -> WatchState {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *state == WatchState::Starting {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .state
                .settled
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *state
    }
}

impl Drop for SleepWatch {
    fn drop(&mut self) {
        self.stop = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::warn!("the suspend watcher's thread panicked");
        }
    }
}

/// What a message says about sleep: `Some(true)` for logind's `PrepareForSleep(true)`,
/// `Some(false)` for `PrepareForSleep(false)`, `None` for anything else.
///
/// Only a broadcast counts. The bus delivers the watcher logind's broadcasts alone, by the match
/// rule's sender, but a signal addressed to the watcher's own connection reaches it whatever the
/// rule says, from anyone on the bus — and a mute is not something anyone else gets to ask for.
#[must_use]
pub fn prepare_for_sleep(message: &Message) -> Option<bool> {
    let header = message.header();
    let ours = header.message_type() == Type::Signal
        && header.destination().is_none()
        && header
            .path()
            .is_some_and(|path| path.as_str() == LOGIND_PATH)
        && header
            .interface()
            .is_some_and(|interface| interface.as_str() == LOGIND_MANAGER)
        && header
            .member()
            .is_some_and(|member| member.as_str() == PREPARE_FOR_SLEEP);
    if !ours {
        return None;
    }
    message.body().deserialize::<bool>().ok()
}

/// The watcher's thread: subscribe, pass on every `PrepareForSleep` until stopped, and say the
/// machine is awake if the connection goes while it was asleep — the resume would otherwise never
/// be heard, and both lanes would stay muted for the rest of the session.
fn watch(
    bus: &SleepBus,
    sleeping: &WakingSender<bool>,
    mut stopped: oneshot::Receiver<()>,
    state: &StateCell,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            log::warn!("not watching for suspend: its runtime did not start ({err})");
            state.set(WatchState::Unavailable);
            return;
        }
    };
    // Everything zbus is dropped inside `block_on`: a connection spawns its clean-up on the
    // runtime it was built on.
    runtime.block_on(async {
        let subscribing = async {
            tokio::time::timeout(CONNECT_TIMEOUT, subscribe(bus))
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "the bus did not answer within {} s",
                        CONNECT_TIMEOUT.as_secs()
                    ))
                })
        };
        let mut signals = match until_stopped(subscribing, &mut stopped).await {
            Some(Ok(signals)) => signals,
            Some(Err(why)) => {
                log::warn!("not watching for suspend: {why}; FxSound runs on without it");
                state.set(WatchState::Unavailable);
                return;
            }
            None => {
                state.set(WatchState::Unavailable);
                return;
            }
        };
        state.set(WatchState::Listening);
        log::info!("watching for suspend: logind's {PREPARE_FOR_SLEEP} on the system bus");

        let mut asleep = false;
        while let Some(next) = until_stopped(next_message(&mut signals), &mut stopped).await {
            match next {
                Some(Ok(message)) => {
                    let Some(now) = prepare_for_sleep(&message) else {
                        continue;
                    };
                    asleep = now;
                    // Nobody left to tell: the instance is on its way out.
                    if sleeping.send(now).is_err() {
                        break;
                    }
                }
                Some(Err(err)) => log::debug!("an unreadable message from the system bus: {err}"),
                None => {
                    log::warn!("the system bus connection closed; no longer watching for suspend");
                    if asleep {
                        let _ = sleeping.send(false);
                    }
                    break;
                }
            }
        }
        drop(signals);
    });
    state.set(WatchState::Unavailable);
}

/// Connect to `bus` and ask it for logind's `PrepareForSleep`.
async fn subscribe(bus: &SleepBus) -> Result<MessageStream, String> {
    let builder = match bus {
        SleepBus::System => zbus::connection::Builder::system()
            .map_err(|err| format!("there is no system bus ({err})"))?,
        SleepBus::Address(address) => zbus::connection::Builder::address(address.as_str())
            .map_err(|err| format!("{address} is not a bus address ({err})"))?,
    };
    let connection = builder
        .build()
        .await
        .map_err(|err| format!("the system bus could not be reached ({err})"))?;
    MessageStream::for_match_rule(rule().map_err(|err| err.to_string())?, &connection, None)
        .await
        .map_err(|err| format!("the bus refused the subscription ({err})"))
}

/// logind's `PrepareForSleep`, from logind only.
fn rule() -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(LOGIND)?
        .path(LOGIND_PATH)?
        .interface(LOGIND_MANAGER)?
        .member(PREPARE_FOR_SLEEP)?
        .build())
}

/// The next message on `stream`; `None` once the connection has gone.
async fn next_message(stream: &mut MessageStream) -> Option<zbus::Result<Message>> {
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// `work`, unless the handle goes away first — `None` then.
async fn until_stopped<F: Future>(
    work: F,
    stopped: &mut oneshot::Receiver<()>,
) -> Option<F::Output> {
    let mut work = pin!(work);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        // Nothing is ever sent; the sender being dropped is the whole message.
        if Pin::new(&mut *stopped).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_bus::PrivateBus;
    use crossbeam_channel::{Receiver, RecvTimeoutError};

    /// Long enough for a private bus on a loaded test machine; a pass takes milliseconds.
    const PATIENCE: Duration = Duration::from_secs(10);
    /// How long a signal that must not arrive is waited for.
    const QUIET: Duration = Duration::from_millis(300);

    fn signal(sleeping: bool) -> Message {
        Message::signal(LOGIND_PATH, LOGIND_MANAGER, PREPARE_FOR_SLEEP)
            .expect("a signal header")
            .build(&sleeping)
            .expect("a signal")
    }

    // ---- the signal, without a bus ------------------------------------------------------------

    #[test]
    fn logind_going_to_sleep_and_resuming_read_as_true_and_false() {
        assert_eq!(prepare_for_sleep(&signal(true)), Some(true));
        assert_eq!(prepare_for_sleep(&signal(false)), Some(false));
    }

    #[test]
    fn another_signal_of_logind_says_nothing_about_sleep() {
        let shutdown = Message::signal(LOGIND_PATH, LOGIND_MANAGER, "PrepareForShutdown")
            .expect("a header")
            .build(&true)
            .expect("a signal");
        assert_eq!(prepare_for_sleep(&shutdown), None);
        let elsewhere = Message::signal(
            "/org/freedesktop/login1/session/_31",
            LOGIND_MANAGER,
            PREPARE_FOR_SLEEP,
        )
        .expect("a header")
        .build(&true)
        .expect("a signal");
        assert_eq!(prepare_for_sleep(&elsewhere), None);
        let other_interface = Message::signal(
            LOGIND_PATH,
            "org.freedesktop.login1.Session",
            PREPARE_FOR_SLEEP,
        )
        .expect("a header")
        .build(&true)
        .expect("a signal");
        assert_eq!(prepare_for_sleep(&other_interface), None);
    }

    #[test]
    fn a_body_that_is_not_one_boolean_says_nothing() {
        let text = Message::signal(LOGIND_PATH, LOGIND_MANAGER, PREPARE_FOR_SLEEP)
            .expect("a header")
            .build(&"true")
            .expect("a signal");
        assert_eq!(prepare_for_sleep(&text), None);
        let empty = Message::signal(LOGIND_PATH, LOGIND_MANAGER, PREPARE_FOR_SLEEP)
            .expect("a header")
            .build(&())
            .expect("a signal");
        assert_eq!(prepare_for_sleep(&empty), None);
    }

    #[test]
    fn a_signal_addressed_to_fxsound_alone_is_not_logind_speaking() {
        let aimed = Message::signal(LOGIND_PATH, LOGIND_MANAGER, PREPARE_FOR_SLEEP)
            .expect("a header")
            .destination(":1.42")
            .expect("a destination")
            .build(&true)
            .expect("a signal");
        assert_eq!(prepare_for_sleep(&aimed), None);
    }

    #[test]
    fn the_subscription_asks_for_loginds_signal_from_logind_only() {
        let rule = rule().expect("a rule").to_string();
        for part in [
            "type='signal'",
            "sender='org.freedesktop.login1'",
            "path='/org/freedesktop/login1'",
            "interface='org.freedesktop.login1.Manager'",
            "member='PrepareForSleep'",
        ] {
            assert!(rule.contains(part), "{rule} lacks {part}");
        }
    }

    // ---- a private bus standing in for the system bus -----------------------------------------

    /// A watcher on `bus`, and the channel it reports on, once it is listening.
    fn listening(bus: &PrivateBus) -> (SleepWatch, Receiver<bool>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let watch = SleepWatch::start_on(SleepBus::Address(bus.address.clone()), tx);
        assert_eq!(watch.wait_until_settled(PATIENCE), WatchState::Listening);
        (watch, rx)
    }

    /// A connection to `bus` that owns logind's name, as logind's own does on the system bus.
    fn logind(bus: &PrivateBus) -> zbus::blocking::Connection {
        let connection = bus.client();
        connection.request_name(LOGIND).expect("the name is free");
        connection
    }

    fn say(connection: &zbus::blocking::Connection, sleeping: bool) {
        connection
            .emit_signal(
                None::<()>,
                LOGIND_PATH,
                LOGIND_MANAGER,
                PREPARE_FOR_SLEEP,
                &sleeping,
            )
            .expect("the signal is sent");
    }

    #[test]
    fn loginds_signal_on_the_bus_reaches_the_channel_going_down_and_coming_up() {
        let Some(bus) = PrivateBus::start_system_like() else {
            return;
        };
        let (watch, rx) = listening(&bus);
        let logind = logind(&bus);
        say(&logind, true);
        assert_eq!(rx.recv_timeout(PATIENCE), Ok(true));
        say(&logind, false);
        assert_eq!(rx.recv_timeout(PATIENCE), Ok(false));
        drop(watch);
    }

    #[test]
    fn the_same_signal_from_anyone_but_logind_is_not_heard() {
        let Some(bus) = PrivateBus::start_system_like() else {
            return;
        };
        let (_watch, rx) = listening(&bus);
        let impostor = bus.client();
        say(&impostor, true);
        assert_eq!(rx.recv_timeout(QUIET), Err(RecvTimeoutError::Timeout));
        // logind itself, afterwards, is: the watcher was listening all along.
        let logind = logind(&bus);
        say(&logind, true);
        assert_eq!(rx.recv_timeout(PATIENCE), Ok(true));
    }

    #[test]
    fn a_bus_that_goes_away_while_the_system_sleeps_wakes_the_lanes_up() {
        let Some(bus) = PrivateBus::start_system_like() else {
            return;
        };
        let (watch, rx) = listening(&bus);
        let logind = logind(&bus);
        say(&logind, true);
        assert_eq!(rx.recv_timeout(PATIENCE), Ok(true));
        drop(logind);
        drop(bus);
        assert_eq!(rx.recv_timeout(PATIENCE), Ok(false));
        let deadline = Instant::now() + PATIENCE;
        while watch.state() != WatchState::Unavailable && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(watch.state(), WatchState::Unavailable);
    }

    #[test]
    fn a_bus_that_goes_away_while_the_system_is_awake_says_nothing() {
        let Some(bus) = PrivateBus::start_system_like() else {
            return;
        };
        let (_watch, rx) = listening(&bus);
        drop(bus);
        assert_eq!(rx.recv_timeout(QUIET), Err(RecvTimeoutError::Disconnected));
    }

    #[test]
    fn no_bus_is_logged_and_nothing_else() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let address = format!("unix:path={}", dir.path().join("no-bus").display());
        let (tx, rx) = crossbeam_channel::unbounded();
        let watch = SleepWatch::start_on(SleepBus::Address(address), tx);
        assert_eq!(watch.wait_until_settled(PATIENCE), WatchState::Unavailable);
        // The thread has ended, and dropped its sender with it.
        assert_eq!(
            rx.recv_timeout(PATIENCE),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn dropping_the_handle_stops_the_watcher_at_once() {
        let Some(bus) = PrivateBus::start_system_like() else {
            return;
        };
        let (watch, rx) = listening(&bus);
        let started = Instant::now();
        drop(watch);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(rx.recv_timeout(QUIET), Err(RecvTimeoutError::Disconnected));
    }
}
