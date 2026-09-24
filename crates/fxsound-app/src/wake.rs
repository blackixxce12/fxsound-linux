//! Waking the GUI thread when something has arrived for it (0.4.0 design §12).
//!
//! The pump used to look at everything ten times a second, with or without a window, whether or
//! not anything had happened. Now whatever hands the GUI thread work wakes it: a connection on the
//! control socket, a D-Bus call, a tray click, the suspend watcher, a termination signal, the audio
//! thread's notifications. There is one [`Waker`] per process and every producer holds a clone.
//!
//! A wake-up goes two ways at once, because the GUI thread is in one of two states (see `main.rs`):
//!
//! * While a window exists, its [`egui::Context`] is attached, and a wake-up is
//!   [`egui::Context::request_repaint`] — eframe's event loop runs a frame, and the frame's
//!   `logic` drains what arrived.
//! * While there is none, the headless pump waits on the producers' channels themselves and on
//!   [`Waker::pending`], the one channel a producer that has none of its own (a signal) can make
//!   ready.
//!
//! What no producer announces — a notice running out, the per-device volume's delayed save, the
//! `--watch --meters` stream — the pump schedules itself; everything else waits for its wake-up.

use crossbeam_channel::{Receiver, SendError, Sender, TrySendError, bounded, unbounded};
use eframe::egui;
use std::sync::{Arc, Mutex, PoisonError};

/// What wakes the GUI thread. Cheap to clone; every clone wakes the same thread.
#[derive(Clone)]
pub struct Waker {
    inner: Arc<Inner>,
}

struct Inner {
    /// The window's context while there is a window.
    window: Mutex<Option<egui::Context>>,
    /// One pending wake-up at most: a token says "look", and two say nothing more.
    tx: Sender<()>,
    rx: Receiver<()>,
}

impl Default for Waker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Waker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waker")
            .field("window", &self.has_window())
            .field("pending", &!self.inner.rx.is_empty())
            .finish()
    }
}

impl Waker {
    /// A waker with no window attached and nothing pending.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = bounded(1);
        Self {
            inner: Arc::new(Inner {
                window: Mutex::new(None),
                tx,
                rx,
            }),
        }
    }

    /// Wake the GUI thread: a frame of the window, when there is one, and a pending wake-up for
    /// the headless pump either way. Callable from any thread; never blocks.
    pub fn wake(&self) {
        // Cloned out of the lock, so eframe's repaint callback runs with nothing of ours held.
        let window = self.window().clone();
        if let Some(ctx) = window {
            ctx.request_repaint();
        }
        // Full means a wake-up is pending already, which is all a token says.
        let _ = self.inner.tx.try_send(());
    }

    /// Send wake-ups to this window from now on. Called when a window is created.
    pub fn attach(&self, ctx: &egui::Context) {
        *self.window() = Some(ctx.clone());
    }

    /// Stop sending wake-ups to the window, which is going away. Its context would take them
    /// and do nothing: the event loop they reach is not running between two windows.
    pub fn detach(&self) {
        *self.window() = None;
    }

    /// Whether a window is attached.
    #[must_use]
    pub fn has_window(&self) -> bool {
        self.window().is_some()
    }

    /// The headless pump's end: ready while a wake-up is pending.
    #[must_use]
    pub fn pending(&self) -> &Receiver<()> {
        &self.inner.rx
    }

    /// Forget a pending wake-up: the pump is about to look at everything anyway.
    pub fn clear(&self) {
        while self.inner.rx.try_recv().is_ok() {}
    }

    fn window(&self) -> std::sync::MutexGuard<'_, Option<egui::Context>> {
        // Nothing is ever left half-written under this lock.
        self.inner
            .window
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A channel's sending end that wakes the GUI thread once it has handed a message over.
///
/// What the producers hold instead of a bare [`Sender`]: the control socket's connection threads
/// and the D-Bus service (`crate::ipc`), the tray's callbacks (`crate::tray`), the suspend watcher
/// (`crate::sleep`) and the thread that carries the audio thread's notifications ([`forward`]).
/// A message that could not be handed over wakes nobody.
pub struct WakingSender<T> {
    tx: Sender<T>,
    waker: Waker,
}

impl<T> Clone for WakingSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            waker: self.waker.clone(),
        }
    }
}

impl<T> std::fmt::Debug for WakingSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakingSender")
            .field("waker", &self.waker)
            .finish_non_exhaustive()
    }
}

/// A sender that wakes a [`Waker`] nothing listens to — for a test that reads the channel itself.
impl<T> From<Sender<T>> for WakingSender<T> {
    fn from(tx: Sender<T>) -> Self {
        Self::new(tx, Waker::new())
    }
}

impl<T> WakingSender<T> {
    #[must_use]
    pub const fn new(tx: Sender<T>, waker: Waker) -> Self {
        Self { tx, waker }
    }

    /// [`Sender::send`], then wake.
    ///
    /// # Errors
    ///
    /// When the receiving end is gone, as [`Sender::send`].
    pub fn send(&self, message: T) -> Result<(), SendError<T>> {
        self.tx.send(message)?;
        self.waker.wake();
        Ok(())
    }

    /// [`Sender::try_send`], then wake.
    ///
    /// # Errors
    ///
    /// When the channel is full or its receiving end is gone, as [`Sender::try_send`].
    pub fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        self.tx.try_send(message)?;
        self.waker.wake();
        Ok(())
    }
}

/// Carry what arrives on `source` to a channel of the GUI thread's, waking it for each message:
/// the audio thread's notifications, which it sends on a plain channel of the engine's
/// ([`fxsound_audio::EngineHandle::notifications`]).
///
/// The returned channel is the one to read from then; `source` is left to the carrier. The carrier
/// is a thread of its own, named `name`, that ends when `source`'s senders are all gone — the
/// audio thread has stopped — or when the returned end has been dropped and one more message
/// comes. Should the thread not start, `source` itself comes back, and is read at the pump's own
/// pace, unannounced, as before.
#[must_use]
pub fn forward<T: Send + 'static>(name: &str, source: Receiver<T>, waker: Waker) -> Receiver<T> {
    let (tx, rx) = unbounded();
    let tx = WakingSender::new(tx, waker);
    let carried = source.clone();
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            for message in carried.iter() {
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
    match spawned {
        Ok(_) => rx,
        Err(err) => {
            log::warn!("{name} did not start ({err}); its messages are polled instead");
            source
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A context that counts the repaints asked of it.
    fn counted() -> (egui::Context, Arc<AtomicUsize>) {
        let ctx = egui::Context::default();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        ctx.set_request_repaint_callback(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        (ctx, count)
    }

    #[test]
    fn a_wake_up_with_no_window_leaves_one_pending_for_the_headless_pump() {
        let waker = Waker::new();
        assert!(waker.pending().is_empty());
        waker.wake();
        waker.wake();
        waker.clone().wake();
        assert_eq!(
            waker.pending().len(),
            1,
            "three wake-ups are one thing to look at"
        );
        waker.clear();
        assert!(waker.pending().is_empty());
    }

    #[test]
    fn a_wake_up_from_another_thread_repaints_the_attached_window_and_nothing_after_it_went() {
        let waker = Waker::new();
        let (ctx, repaints) = counted();
        waker.attach(&ctx);
        assert!(waker.has_window());
        let producer = waker.clone();
        std::thread::spawn(move || producer.wake())
            .join()
            .expect("the producer");
        assert!(
            repaints.load(Ordering::SeqCst) >= 1,
            "the window was asked for a frame"
        );
        assert_eq!(
            waker.pending().len(),
            1,
            "and the headless pump would look too"
        );

        waker.detach();
        assert!(!waker.has_window());
        let before = repaints.load(Ordering::SeqCst);
        waker.wake();
        assert_eq!(
            repaints.load(Ordering::SeqCst),
            before,
            "a closed window is left alone"
        );
    }

    #[test]
    fn a_waking_sender_wakes_after_each_message_it_hands_over_and_not_for_one_it_could_not() {
        let waker = Waker::new();
        let (tx, rx) = bounded(1);
        let sender = WakingSender::new(tx, waker.clone());
        sender.try_send(1).expect("room for one");
        assert_eq!(waker.pending().len(), 1);
        waker.clear();

        assert!(sender.try_send(2).is_err(), "the channel is full");
        assert!(
            waker.pending().is_empty(),
            "nothing was handed over, so nobody is woken"
        );

        assert_eq!(rx.recv(), Ok(1));
        drop(rx);
        assert!(sender.send(3).is_err(), "nobody reads the channel any more");
        assert!(waker.pending().is_empty());
    }

    #[test]
    fn forwarded_messages_arrive_in_order_each_with_a_wake_up_and_the_carrier_ends_with_its_source()
    {
        let waker = Waker::new();
        let (ctx, repaints) = counted();
        waker.attach(&ctx);
        let (tx, source) = unbounded();
        let carried = forward("fxsound-test-carrier", source, waker.clone());
        for n in 0..5 {
            tx.send(n).expect("the carrier is listening");
        }
        let arrived: Vec<i32> = (0..5)
            .map(|_| {
                carried
                    .recv_timeout(Duration::from_secs(5))
                    .expect("carried")
            })
            .collect();
        assert_eq!(arrived, [0, 1, 2, 3, 4]);
        assert!(repaints.load(Ordering::SeqCst) >= 1);
        assert_eq!(waker.pending().len(), 1);

        // The audio thread stops: the carrier sees its source close, and so does the GUI thread.
        drop(tx);
        assert_eq!(
            carried.recv_timeout(Duration::from_secs(5)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        );
    }
}
