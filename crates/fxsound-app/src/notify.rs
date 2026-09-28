//! Desktop notifications.
//!
//! `FxModel` has exactly **one** message slot: `pushMessage` overwrites `message_`/`message_link_`
//! and raises `Event::Notification` (`fxsound/Source/GUI/FxModel.h:170-175`), and the tray view
//! shows it unless notifications are hidden:
//!
//! ```cpp
//! if (!FxController::getInstance().isNotificationsHidden() && model_event == FxModel::Event::Notification)
//!     showNotification();
//! ```
//! `fxsound/Source/GUI/FxSystemTrayView.cpp:64-70`
//!
//! A second message pushed before the first is displayed silently replaces it. That semantic is
//! preserved here by reusing one `replaces_id`, so a new notification takes over the slot the old
//! one occupied instead of stacking (`docs/spec/07-startup-tray.md` §6.5).
//!
//! # What is not ported
//!
//! The shipped build always draws its *own* toast window — `custom_notification_` is hard-coded
//! `true` (`FxSystemTrayView.cpp:32`) — a 216 × (20·lines + 60) rounded panel with a 200 ms fade,
//! positioned next to the tray icon (`FxNotification.cpp`, geometry table in §6.2). None of that
//! survives: a Wayland client cannot position a surface in global coordinates and has no way to
//! learn where its StatusNotifierItem is drawn, and the notification daemon owns the look anyway.
//! The two timings that *are* behaviour rather than decoration — 7 s without a link, 8 s with one
//! (`FxNotification.cpp:185-192`) — are kept as [`TIMEOUT_MS`] and [`TIMEOUT_WITH_LINK_MS`].
//!
//! `SHQueryUserNotificationState`, which suppresses the toast outside
//! `QUNS_ACCEPTS_NOTIFICATIONS` (`FxSystemTrayView.cpp:394-398`), is not reimplemented either: Do
//! Not Disturb is the daemon's job, and every daemon that has one holds back normal and low
//! urgency alike — the analogue of `NIIF_RESPECT_QUIET_TIME` (`:411`).
//!
//! # Urgency, per kind of message
//!
//! 0.4.0 sent everything at `Urgency::Low`, and GNOME Shell never shows a banner for that
//! (`messageTray.js`, `_onNotificationRequestBanner`: `if (notification.urgency === Urgency.LOW)
//! return;`), so on GNOME not even "FxSound is still running, but this session has no tray icon"
//! was seen. Each message now has a [`Weight`]:
//!
//! | Message | Weight |
//! |---|---|
//! | [`Message::minimised_to_tray`], [`Message::hidden_with_no_tray`], [`Message::window_unavailable`] | [`Weight::Alert`]: where the window went, and how to get it back |
//! | [`Message::output_disconnected`] | [`Weight::Alert`]: the sound moved without being asked to |
//! | [`Message::preset_limit_reached`] | [`Weight::Alert`]: something asked for was refused |
//! | [`Message::power_toggled`] | [`Weight::Alert`]: sent only for a keybind or the command line, where it is the only answer |
//! | [`Message::preset_selected`], [`Message::output_selected`], [`Message::preset_saved`], [`Message::preset_overwritten`], [`Message::preset_deleted`], [`Message::presets_restored`] | [`Weight::Echo`], and [`Weight::Alert`] when no window is up to show the change ([`Message::raised`], [`crate::app::App`]) |
//!
//! # One connection, kept
//!
//! GNOME Shell files a notification under a source per application and watches the bus name of
//! the sender that made the source: when that name leaves the bus, the source and every
//! notification in it are destroyed (`notificationDaemon.js`, `FdoNotificationDaemonSource`,
//! `_onNameVanished`, for any source whose application it knows — ours, through `desktop-entry`).
//! `notify_rust::Notification::show()` opens a D-Bus connection per notification and closes it
//! with the handle, which for a message with no link was at once, so GNOME removed the notice
//! about 7 ms after showing it. [`DesktopSink`] sends every notification over one connection
//! that stays open while FxSound runs, so a notice stays in the message list until it is
//! dismissed or FxSound quits.
//!
//! # Threading
//!
//! Connecting and sending are blocking D-Bus round trips, so they must never happen on the egui
//! thread. [`Notifier`] owns a worker thread and the GUI only ever hands it a [`Message`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify_rust::{Hint, Notification, Timeout, Urgency};

use crate::tray::APP_ID;
use fxsound_core::i18n::{tr, tr_args};

/// `szInfoTitle` of the shell balloon (`fxsound/Source/GUI/FxSystemTrayView.cpp:413`), and the
/// `app_name` we introduce ourselves to the daemon with.
pub const APP_NAME: &str = "FxSound";

/// Auto-hide for a message with no link (`fxsound/Source/GUI/FxNotification.cpp:185-188`).
pub const TIMEOUT_MS: u32 = 7000;
/// Auto-hide for a message that carries a link (`FxNotification.cpp:189-192`).
pub const TIMEOUT_WITH_LINK_MS: u32 = 8000;

/// How long after the first hide-to-tray of a session the "FxSound in system tray" tip appears
/// (`Timer::callAfterDelay(2000, …)`, `FxController.cpp:920-926`). The scheduling and the
/// once-per-session flag (`minimize_tip_`, `:130`, `:922`) belong to the controller; the constant
/// lives here with the message it delays.
pub const TRAY_HINT_DELAY: Duration = Duration::from_millis(2000);

/// The clickable half of a message. On Windows it is an `FxHyperlink` drawn inside the toast; here
/// it becomes the notification's default action, so clicking the body follows it
/// (`docs/spec/07-startup-tray.md` §6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The link text.
    pub label: String,
    /// An `http`/`https` URL. Anything else is refused when the action fires.
    pub url: String,
}

/// How much a message asks of the desktop: the urgency it is sent with (see the module docs for
/// which message has which).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Says back a change the user has just made where they can see it made. `Urgency::Low`:
    /// GNOME files it in the message list without a banner; most other daemons show it as before.
    Echo,
    /// Something the user has to see. `Urgency::Normal`, so GNOME shows a banner too — unless Do
    /// Not Disturb is on, which holds it back as it does any other application's.
    Alert,
}

impl Weight {
    /// The urgency hint it is sent with.
    #[must_use]
    pub const fn urgency(self) -> Urgency {
        match self {
            Self::Echo => Urgency::Low,
            Self::Alert => Urgency::Normal,
        }
    }
}

/// One pushed message — the payload of `FxModel::pushMessage` (`FxModel.h:170-175`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The text. `\r\n` is normalised to `\n` when the notification is built.
    pub body: String,
    pub link: Option<Link>,
    pub weight: Weight,
}

impl Message {
    /// A message that echoes what the user did ([`Weight::Echo`]).
    #[must_use]
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            link: None,
            weight: Weight::Echo,
        }
    }

    /// A message the user has to see ([`Weight::Alert`]).
    #[must_use]
    pub fn alert(body: impl Into<String>) -> Self {
        Self {
            weight: Weight::Alert,
            ..Self::new(body)
        }
    }

    /// The same message as a [`Weight::Alert`]: what an echo becomes when there is no window on
    /// screen to show the change it echoes — a preset picked in the tray or by a keybind — and
    /// the notification is all the answer there is.
    #[must_use]
    pub fn raised(self) -> Self {
        Self {
            weight: Weight::Alert,
            ..self
        }
    }

    #[must_use]
    pub fn with_link(
        body: impl Into<String>,
        label: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            body: body.into(),
            link: Some(Link {
                label: label.into(),
                url: url.into(),
            }),
            weight: Weight::Alert,
        }
    }

    /// `expire_timeout` for this message (§6.2).
    #[must_use]
    pub const fn timeout_ms(&self) -> u32 {
        if self.link.is_some() {
            TIMEOUT_WITH_LINK_MS
        } else {
            TIMEOUT_MS
        }
    }

    // ---- the catalogue ---------------------------------------------------------------------
    // Every `pushMessage` call site in the Windows tree, in the order `docs/spec/07-startup-tray.md`
    // §6.4 tabulates them, but two. The English `TRANS` keys are reproduced verbatim;
    // `Resources/Strings/` is empty in this checkout, so they are the only authoritative strings
    // there are. Not ported: the "what's new" link to the upstream website after an update
    // (`FxController.cpp:719`) and the survey on the upstream developers' form
    // (`FxController.cpp:938-960`). This fork gathers nothing for upstream and sends nobody to
    // its site; the changelog is the bundled one, in Settings ▸ Help.

    /// First hide-to-tray of the session (`FxController.cpp:920-926`), after [`TRAY_HINT_DELAY`].
    ///
    /// This one matters more on Linux than on Windows: a user whose desktop has no
    /// StatusNotifierItem host — GNOME without the AppIndicator extension — has otherwise lost the
    /// app entirely (§6.5).
    #[must_use]
    pub fn minimised_to_tray() -> Self {
        Self::alert(tr("FxSound in system tray\nClick FxSound icon to reopen"))
    }

    /// The same moment, on a session that has no tray to hide into.
    ///
    /// A GNOME session without the AppIndicator extension registers no `StatusNotifierWatcher`,
    /// so hiding the window leaves a process with no window, no icon and — until this existed —
    /// nothing said about either. The user's audio keeps being processed by something they can
    /// neither see nor reach.
    ///
    /// English key, like every other string this port added: `tr` falls back to the key, so an
    /// untranslated build reads correctly.
    #[must_use]
    pub fn hidden_with_no_tray() -> Self {
        Self::alert(tr(
            "FxSound is still running, but this session has no tray icon.\nRun 'fxsound --show' \
             to bring the window back.",
        ))
    }

    /// The window was asked for and could not be opened, and FxSound stays in the tray instead of
    /// quitting: an instance the session bus started through the systemd user unit, on a desktop
    /// that never gave the systemd user manager its `WAYLAND_DISPLAY` or `DISPLAY`. The audio keeps
    /// being processed; only the window is out of reach, and a notification is the one way left to
    /// say so. It stays out of reach for as long as this instance runs — a process gets one try at
    /// its display connection (winit's event loop) — so the way back is a fresh start from the
    /// desktop. A port addition.
    #[must_use]
    pub fn window_unavailable() -> Self {
        Self::alert(tr(
            "FxSound could not open its window and keeps running in the system tray.\nQuit it \
             from the tray and start FxSound again to open the window.",
        ))
    }

    /// Preset changed with `notify == true` and power on (`FxController.cpp:1099-1102`).
    #[must_use]
    pub fn preset_selected(name: &str) -> Self {
        Self::new(format!("{}{name}", tr("Preset: ")))
    }

    /// Output device changed, optionally naming the preset that came with it
    /// (`FxController.cpp:1120`, `:1132`, `:1158`).
    #[must_use]
    pub fn output_selected(device: &str, preset: Option<&str>) -> Self {
        match preset {
            Some(preset) => Self::new(format!(
                "{}{device}\n{}{preset}",
                tr("Output: "),
                tr("Preset: ")
            )),
            None => Self::new(format!("{}{device}", tr("Output: "))),
        }
    }

    /// The selected output vanished (`FxController.cpp:1170`).
    #[must_use]
    pub fn output_disconnected() -> Self {
        Self::alert(tr("Output Disconnected"))
    }

    /// An existing user preset was overwritten (`FxController.cpp:1221`).
    #[must_use]
    pub fn preset_overwritten(name: &str) -> Self {
        Self::new(tr_args("Changes to preset %s are saved.", &[name]))
    }

    /// A new user preset was saved (`FxController.cpp:1234`).
    #[must_use]
    pub fn preset_saved(name: &str) -> Self {
        Self::new(tr_args("New preset %s is saved.", &[name]))
    }

    /// The user preset count hit `max_user_presets` (`FxController.cpp:1239`; the limit is clamped
    /// to `[10, 120]` at `:194-199`).
    #[must_use]
    pub fn preset_limit_reached() -> Self {
        Self::alert(tr("Reached the limit on new presets."))
    }

    /// A user preset was deleted (`FxController.cpp:1313`).
    #[must_use]
    pub fn preset_deleted(name: &str) -> Self {
        Self::new(tr_args("Preset %s is deleted.", &[name]))
    }

    /// Settings ▸ Reset Presets dropped every unsaved change. The original says "Presets are
    /// restored to factory defaults" (`FxController.cpp:1381`), because its reset deletes the user
    /// presets too; this one keeps them, and says what it did (0.4.0 audit #21).
    #[must_use]
    pub fn presets_restored() -> Self {
        Self::new(tr("Unsaved preset changes discarded"))
    }

    /// Power toggled from a hotkey rather than the tray or the window
    /// (`FxController.cpp:1933`) — on Linux, from a compositor keybinding running
    /// `fxsound --toggle-power`.
    #[must_use]
    pub fn power_toggled(on: bool) -> Self {
        Self::alert(tr_args(
            "FxSound is %s.",
            &[&tr(if on { "on" } else { "off" })],
        ))
    }
}

/// Build the `org.freedesktop.Notifications` call for a message.
///
/// Pure, so it can be checked without a daemon on the bus. The hint set is
/// `docs/spec/07-startup-tray.md` §6.5's, and each hint has a Windows ancestor:
/// `suppress-sound` is `NIIF_NOSOUND` (`FxSystemTrayView.cpp:411`), and `desktop-entry` is what
/// lets a shell attribute the notification to our window. The urgency is the message's
/// [`Weight`].
#[must_use]
pub fn build(message: &Message, replaces: Option<u32>) -> Notification {
    let mut notification = Notification::new();
    notification
        .appname(APP_NAME)
        .summary(APP_NAME)
        .body(&message.body.replace("\r\n", "\n"))
        .icon(APP_ID)
        .urgency(message.weight.urgency())
        .hint(Hint::SuppressSound(true))
        .hint(Hint::Category("device".to_owned()))
        .hint(Hint::DesktopEntry(APP_ID.to_owned()))
        .timeout(Timeout::Milliseconds(message.timeout_ms()));

    if let Some(link) = &message.link {
        // `"default"` is the action a click on the body invokes, which is how the in-toast
        // hyperlink behaved.
        notification.action("default", &link.label);
    }
    if let Some(id) = replaces {
        notification.id(id);
    }
    notification.finalize()
}

/// Where a built message goes. The real one is [`DesktopSink`]; tests substitute their own so that
/// nothing is sent to a live daemon.
pub trait Sink: Send + 'static {
    /// Deliver `message`, replacing the notification with id `replaces` if the server still has
    /// it. Returns the id to replace next time, or `None` if delivery failed.
    fn deliver(&mut self, message: &Message, replaces: Option<u32>) -> Option<u32>;
}

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// Delivery over `org.freedesktop.Notifications`, on one connection kept open from the first
/// message to the last (see "One connection, kept" in the module docs).
pub struct DesktopSink {
    /// The bus to connect to: the session's when `None`.
    address: Option<String>,
    connection: Option<zbus::blocking::Connection>,
    /// The notification whose link a click follows, and the link: the last one delivered, if it
    /// had a link. Shared with the thread that listens for the click.
    link: Arc<Mutex<Option<(u32, String)>>>,
    /// What following a link does: [`open_url`], or a test's recorder.
    open: Opener,
}

/// Follows a link a notification was clicked for.
type Opener = Arc<dyn Fn(&str) + Send + Sync>;

impl Default for DesktopSink {
    fn default() -> Self {
        Self {
            address: None,
            connection: None,
            link: Arc::default(),
            open: Arc::new(open_url),
        }
    }
}

impl std::fmt::Debug for DesktopSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopSink")
            .field("address", &self.address)
            .field("connected", &self.connection.is_some())
            .finish_non_exhaustive()
    }
}

impl DesktopSink {
    /// Delivery to the session bus's notification daemon.
    #[must_use]
    pub fn session() -> Self {
        Self::default()
    }

    /// Delivery to the daemon on the bus at `address` — a test's private bus.
    #[must_use]
    pub fn at_address(address: impl Into<String>) -> Self {
        let mut sink = Self::default();
        sink.address = Some(address.into());
        sink
    }

    /// The kept connection, made on the first call. A new one also starts the thread that follows
    /// links; it ends with the connection.
    fn connection(&mut self) -> zbus::Result<zbus::blocking::Connection> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let builder = match &self.address {
            Some(address) => zbus::blocking::connection::Builder::address(address.as_str())?,
            None => zbus::blocking::connection::Builder::session()?,
        };
        let connection = builder.build()?;
        follow_links(&connection, Arc::clone(&self.link), Arc::clone(&self.open));
        self.connection = Some(connection.clone());
        Ok(connection)
    }

    /// Close the kept connection, which also ends the thread that follows links on it: its
    /// signal stream holds a clone, so dropping ours alone would leave both behind.
    fn disconnect(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.close();
        }
    }

    fn send(&mut self, notification: &Notification, replaces: Option<u32>) -> zbus::Result<u32> {
        let connection = self.connection()?;
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> =
            notification.hints.iter().map(Into::into).collect();
        connection
            .call_method(
                Some(NOTIFICATIONS),
                NOTIFICATIONS_PATH,
                Some(NOTIFICATIONS),
                "Notify",
                &(
                    &notification.appname,
                    replaces.unwrap_or(0),
                    &notification.icon,
                    &notification.summary,
                    &notification.body,
                    &notification.actions,
                    hints,
                    i32::from(notification.timeout),
                ),
            )?
            .body()
            .deserialize()
    }
}

impl Sink for DesktopSink {
    fn deliver(&mut self, message: &Message, replaces: Option<u32>) -> Option<u32> {
        let notification = build(message, replaces);
        let kept = self.connection.is_some();
        let mut sent = self.send(&notification, replaces);
        if sent.is_err() && kept {
            // The kept connection may be the one that failed — the bus restarted under it — so
            // one more try, on a new one.
            self.disconnect();
            sent = self.send(&notification, replaces);
        }
        match sent {
            Ok(id) => {
                let link = message.link.as_ref().map(|link| (id, link.url.clone()));
                *self
                    .link
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = link;
                Some(id)
            }
            Err(e) => {
                // A session with no notification daemon is normal on a bare compositor; it is not
                // worth more than a line in the log.
                log::warn!("could not show a notification: {e}");
                self.disconnect();
                None
            }
        }
    }
}

/// The notifications go with the connection, on GNOME, when FxSound quits: they would otherwise
/// offer to open an application that is not running.
impl Drop for DesktopSink {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Listen on `connection` for a click on the notification in `link`, and follow its link. The
/// thread ends when the connection does.
fn follow_links(
    connection: &zbus::blocking::Connection,
    link: Arc<Mutex<Option<(u32, String)>>>,
    open: Opener,
) {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(NOTIFICATIONS)
        .and_then(|rule| rule.path(NOTIFICATIONS_PATH))
        .map(zbus::match_rule::Builder::build);
    let signals = rule.and_then(|rule| {
        zbus::blocking::MessageIterator::for_match_rule(rule, connection, Some(16))
    });
    let signals = match signals {
        Ok(signals) => signals,
        Err(e) => {
            log::warn!("notification links will not open: {e}");
            return;
        }
    };
    let spawned = thread::Builder::new()
        .name("fxsound-notify-links".to_owned())
        .spawn(move || {
            for signal in signals {
                let Ok(signal) = signal else { break };
                let header = signal.header();
                let member = header.member().map(zbus::names::MemberName::as_str);
                let mut link = link
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match member {
                    Some("ActionInvoked") => {
                        let Ok((id, _action)) = signal.body().deserialize::<(u32, String)>() else {
                            continue;
                        };
                        if link.as_ref().is_some_and(|(shown, _)| *shown == id)
                            && let Some((_, url)) = link.take()
                        {
                            open(&url);
                        }
                    }
                    Some("NotificationClosed") => {
                        let Ok((id, _reason)) = signal.body().deserialize::<(u32, u32)>() else {
                            continue;
                        };
                        if link.as_ref().is_some_and(|(shown, _)| *shown == id) {
                            *link = None;
                        }
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("notification links will not open: {e}");
    }
}

/// The GUI thread's end of the notification path.
///
/// Dropping it closes the queue and waits for the worker, so a message pushed on the way out is
/// still delivered.
pub struct Notifier {
    /// The persisted `hide_notifications` setting (`FxController.cpp:192`, `:2314-2323`), surfaced
    /// as the Settings dialog's "Hide notifications" toggle (`FxSettingsDialog.cpp:339`).
    hidden: bool,
    tx: Option<Sender<Message>>,
    worker: Option<JoinHandle<()>>,
}

impl Notifier {
    /// A notifier that talks to the session's notification daemon.
    #[must_use]
    pub fn new(hide_notifications: bool) -> Self {
        Self::with_sink(hide_notifications, DesktopSink::session())
    }

    /// A notifier that delivers through `sink`.
    #[must_use]
    pub fn with_sink(hide_notifications: bool, sink: impl Sink) -> Self {
        let (tx, rx) = unbounded();
        let worker = thread::Builder::new()
            .name("fxsound-notify".to_owned())
            .spawn(move || deliver_loop(&rx, sink))
            .ok();
        Self {
            hidden: hide_notifications,
            tx: Some(tx),
            worker,
        }
    }

    /// Whether notifications are suppressed.
    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Follow the "Hide notifications" toggle.
    pub const fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// Queue a message. Returns `false` when it was dropped because notifications are hidden —
    /// the check at `FxSystemTrayView.cpp:66`, which suppresses the toast entirely rather than
    /// queueing it for later.
    pub fn notify(&self, message: Message) -> bool {
        if self.hidden {
            return false;
        }
        match &self.tx {
            Some(tx) => tx.send(message).is_ok(),
            None => false,
        }
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("hidden", &self.hidden)
            .finish_non_exhaustive()
    }
}

/// The one-slot loop: every message replaces the previous one's id, so the daemon shows a single
/// FxSound notification that updates in place.
fn deliver_loop(rx: &Receiver<Message>, mut sink: impl Sink) {
    let mut slot: Option<u32> = None;
    while let Ok(message) = rx.recv() {
        if let Some(id) = sink.deliver(&message, slot) {
            slot = Some(id);
        }
    }
}

/// Open a link with `xdg-open`.
///
/// Spawned directly, never through a shell: `URL::launchInDefaultBrowser()`
/// (`FxSystemTrayView.cpp:280-283`) hands the string to the shell on Windows, and doing the
/// equivalent here would make any URL that reaches this function a command-injection path. The
/// scheme is checked for the same reason — every caller in the catalogue passes a constant, but
/// [`Message::with_link`] is public.
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        log::warn!("refusing to open a notification link that is not http(s)");
        return;
    }
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(_) => {}
        Err(e) => log::warn!("could not open {url}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A sink that reports what it was given instead of talking to a daemon, and hands back
    /// increasing ids the way a server would.
    struct Recorder {
        tx: Sender<(Message, Option<u32>)>,
        next_id: u32,
    }

    impl Sink for Recorder {
        fn deliver(&mut self, message: &Message, replaces: Option<u32>) -> Option<u32> {
            let _ = self.tx.send((message.clone(), replaces));
            self.next_id += 1;
            Some(self.next_id)
        }
    }

    fn recording(hidden: bool) -> (Notifier, Receiver<(Message, Option<u32>)>) {
        let (tx, rx) = unbounded();
        (Notifier::with_sink(hidden, Recorder { tx, next_id: 0 }), rx)
    }

    fn next(rx: &Receiver<(Message, Option<u32>)>) -> (Message, Option<u32>) {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the worker should have delivered a message")
    }

    fn hint(notification: &Notification, wanted: &Hint) -> bool {
        notification.hints.contains(wanted)
    }

    #[test]
    fn a_plain_message_becomes_a_silent_notification_at_its_weight_s_urgency() {
        let notification = build(&Message::preset_selected("Bass Booster"), None);

        assert_eq!(notification.appname, APP_NAME);
        assert_eq!(
            notification.summary, APP_NAME,
            "matches szInfoTitle at :413"
        );
        assert_eq!(notification.body, "Preset: Bass Booster");
        assert_eq!(notification.icon, APP_ID);
        assert_eq!(notification.timeout, Timeout::Milliseconds(TIMEOUT_MS));
        assert!(hint(&notification, &Hint::Urgency(Urgency::Low)), "an echo");
        assert!(hint(&notification, &Hint::SuppressSound(true)));
        assert!(hint(&notification, &Hint::Category("device".to_owned())));
        assert!(hint(&notification, &Hint::DesktopEntry(APP_ID.to_owned())));
        assert!(notification.actions.is_empty());
    }

    #[test]
    fn a_message_with_a_link_gets_the_longer_timeout_and_a_default_action() {
        // 8000 ms rather than 7000 (`FxNotification.cpp:185-192`).
        let message = Message::with_link("A page to read.", "Read it.", "https://example.org/");
        let notification = build(&message, None);
        assert_eq!(
            notification.timeout,
            Timeout::Milliseconds(TIMEOUT_WITH_LINK_MS)
        );
        assert_eq!(
            notification.actions,
            vec!["default".to_owned(), "Read it.".to_owned()],
            "the id must be `default` so a click on the body follows the link"
        );
    }

    #[test]
    fn windows_line_endings_are_normalised_for_the_daemon() {
        let notification = build(&Message::minimised_to_tray(), None);
        assert_eq!(
            notification.body,
            "FxSound in system tray\nClick FxSound icon to reopen"
        );
        assert!(!notification.body.contains('\r'));
        // The original's own texts break lines with `\r\n`, which the daemon would show as is.
        let crlf = build(&Message::new("Two\r\nlines"), None);
        assert_eq!(crlf.body, "Two\nlines");
    }

    #[test]
    fn an_id_to_replace_is_carried_into_the_call() {
        // `Notification::id` is write-only in notify-rust — the field is `pub(crate)`
        // (`notify-rust-4.18.0/src/notification.rs:61`) and `.id()` is the setter — so the derived
        // `Debug` is the only way to see what was stored.
        let replacing = format!("{:?}", build(&Message::output_disconnected(), Some(42)));
        assert!(replacing.contains("id: Some(42)"), "{replacing}");
        let fresh = format!("{:?}", build(&Message::output_disconnected(), None));
        assert!(fresh.contains("id: None"), "{fresh}");
    }

    #[test]
    fn the_catalogue_reproduces_the_windows_strings() {
        assert_eq!(Message::preset_selected("Rock").body, "Preset: Rock");
        assert_eq!(
            Message::output_selected("Speakers", None).body,
            "Output: Speakers"
        );
        assert_eq!(
            Message::output_selected("Speakers", Some("Rock")).body,
            "Output: Speakers\nPreset: Rock"
        );
        assert_eq!(Message::output_disconnected().body, "Output Disconnected");
        assert_eq!(
            Message::preset_overwritten("Rock").body,
            "Changes to preset Rock are saved."
        );
        assert_eq!(
            Message::preset_saved("Rock").body,
            "New preset Rock is saved."
        );
        assert_eq!(
            Message::preset_limit_reached().body,
            "Reached the limit on new presets."
        );
        assert_eq!(
            Message::preset_deleted("Rock").body,
            "Preset Rock is deleted."
        );
        assert_eq!(
            Message::presets_restored().body,
            "Unsaved preset changes discarded"
        );
        assert_eq!(Message::power_toggled(true).body, "FxSound is on.");
        assert_eq!(Message::power_toggled(false).body, "FxSound is off.");
    }

    #[test]
    fn no_message_sends_anyone_to_the_upstream_website_or_its_survey() {
        // The original's "what's new" link and its survey went to the upstream developers' site
        // and form. This fork gathers nothing for upstream, so no message in the catalogue carries
        // a link, and this file names neither address (upstream review §8).
        for message in [
            Message::minimised_to_tray(),
            Message::hidden_with_no_tray(),
            Message::preset_selected("Rock"),
            Message::output_selected("Speakers", Some("Rock")),
            Message::output_disconnected(),
            Message::preset_overwritten("Rock"),
            Message::preset_saved("Rock"),
            Message::preset_limit_reached(),
            Message::preset_deleted("Rock"),
            Message::presets_restored(),
            Message::power_toggled(true),
        ] {
            assert!(message.link.is_none(), "{message:?}");
        }
        let source = include_str!("notify.rs");
        for host in [concat!("fxsound", ".com"), concat!("forms", ".gle")] {
            assert!(!source.contains(host), "notify.rs names {host}");
        }
    }

    #[test]
    fn hiding_notifications_drops_them_instead_of_queueing_them() {
        // `FxSystemTrayView.cpp:64-70` never reaches `showNotification()` at all.
        let (mut notifier, rx) = recording(true);
        assert!(notifier.is_hidden());
        assert!(!notifier.notify(Message::preset_selected("Rock")));
        assert!(rx.try_recv().is_err());

        notifier.set_hidden(false);
        assert!(notifier.notify(Message::preset_selected("Rock")));
        assert_eq!(next(&rx).0, Message::preset_selected("Rock"));
    }

    #[test]
    fn each_message_replaces_the_one_before_it_in_the_single_slot() {
        // `FxModel::pushMessage` overwrites the slot (`FxModel.h:170-175`); on Linux that is the
        // `replaces_id` argument.
        let (notifier, rx) = recording(false);
        notifier.notify(Message::preset_selected("Rock"));
        notifier.notify(Message::preset_selected("Jazz"));
        notifier.notify(Message::output_disconnected());

        let (first, replaces) = next(&rx);
        assert_eq!(first, Message::preset_selected("Rock"));
        assert_eq!(replaces, None, "there is nothing to replace yet");

        let (second, replaces) = next(&rx);
        assert_eq!(second, Message::preset_selected("Jazz"));
        assert_eq!(replaces, Some(1));

        let (third, replaces) = next(&rx);
        assert_eq!(third, Message::output_disconnected());
        assert_eq!(replaces, Some(2));
    }

    #[test]
    fn dropping_the_notifier_flushes_what_is_still_queued() {
        let (notifier, rx) = recording(false);
        notifier.notify(Message::presets_restored());
        drop(notifier);

        let deadline = Instant::now() + Duration::from_secs(5);
        let delivered = loop {
            if let Ok(delivered) = rx.try_recv() {
                break delivered;
            }
            assert!(Instant::now() < deadline, "the queued message was lost");
        };
        assert_eq!(delivered.0, Message::presets_restored());
    }

    #[test]
    fn a_link_that_is_not_http_is_refused_rather_than_handed_to_a_process() {
        // `open_url` must not become a way to run a program; there is nothing to assert but the
        // absence of a spawn, so this only pins that the guard exists and does not panic.
        open_url("file:///etc/passwd");
        open_url("x-scheme-handler/evil");
    }

    #[test]
    fn what_the_user_has_to_see_is_an_alert_and_the_rest_an_echo() {
        // GNOME Shell shows no banner for low urgency, so an alert is what a user sees there.
        for message in [
            Message::minimised_to_tray(),
            Message::hidden_with_no_tray(),
            Message::window_unavailable(),
            Message::output_disconnected(),
            Message::preset_limit_reached(),
            Message::power_toggled(true),
            Message::power_toggled(false),
        ] {
            assert_eq!(message.weight, Weight::Alert, "{message:?}");
            assert!(
                hint(&build(&message, None), &Hint::Urgency(Urgency::Normal)),
                "{message:?}"
            );
        }
        for message in [
            Message::preset_selected("Rock"),
            Message::output_selected("Speakers", Some("Rock")),
            Message::preset_overwritten("Rock"),
            Message::preset_saved("Rock"),
            Message::preset_deleted("Rock"),
            Message::presets_restored(),
        ] {
            assert_eq!(message.weight, Weight::Echo, "{message:?}");
            assert!(
                hint(&build(&message, None), &Hint::Urgency(Urgency::Low)),
                "{message:?}"
            );
            let raised = message.clone().raised();
            assert_eq!(raised.weight, Weight::Alert);
            assert_eq!(
                raised.body, message.body,
                "raising changes the urgency alone"
            );
        }
    }

    /// What the notification daemon on a test's private bus was sent: the sender's unique name,
    /// the id to replace, and the urgency byte.
    type Received = (String, u32, u8);

    /// A notification daemon that records each call and answers with a new id, or the one it was
    /// asked to replace.
    struct Daemon {
        tx: Sender<Received>,
        next_id: std::sync::atomic::AtomicU32,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Daemon {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            #[zbus(header)] header: zbus::message::Header<'_>,
            _app_name: String,
            replaces_id: u32,
            _app_icon: String,
            _summary: String,
            _body: String,
            _actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _expire_timeout: i32,
        ) -> u32 {
            let sender = header.sender().map(ToString::to_string).unwrap_or_default();
            let urgency = hints
                .get("urgency")
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(u8::MAX);
            let _ = self.tx.send((sender, replaces_id, urgency));
            if replaces_id == 0 {
                self.next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1
            } else {
                replaces_id
            }
        }
    }

    #[test]
    fn every_notification_comes_from_one_connection_that_stays_until_the_notifier_goes() {
        // GNOME Shell destroys an application's notifications when the sender that made their
        // source leaves the bus; 0.4.0 opened a connection per notification and closed it at
        // once, and GNOME took the notice down about 7 ms after showing it.
        let Some(bus) = crate::private_bus::PrivateBus::start() else {
            return;
        };
        let (tx, rx) = unbounded();
        let _daemon = daemon_on(&bus, tx);
        let client = bus.client();

        let notifier = Notifier::with_sink(false, DesktopSink::at_address(bus.address.clone()));
        let received = |rx: &Receiver<Received>| {
            rx.recv_timeout(Duration::from_secs(10))
                .expect("the daemon was called")
        };
        assert!(notifier.notify(Message::hidden_with_no_tray()));
        let (first, replaces, urgency) = received(&rx);
        assert_eq!(replaces, 0);
        assert_eq!(urgency, 1, "an alert is sent at normal urgency");
        assert!(notifier.notify(Message::preset_selected("Rock")));
        let (second, replaces, urgency) = received(&rx);
        assert_eq!(second, first, "a second connection for the second notice");
        assert_eq!(replaces, 1, "it takes the first one's slot");
        assert_eq!(urgency, 0, "an echo is sent at low urgency");

        // Long after the last notice, its sender is still there to keep it in the list.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            bus.has_owner(&client, &first),
            "the sender left the bus after sending"
        );

        drop(notifier);
        let deadline = Instant::now() + Duration::from_secs(10);
        while bus.has_owner(&client, &first) {
            assert!(
                Instant::now() < deadline,
                "the connection outlived the notifier"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn with_no_daemon_on_the_bus_a_notice_is_lost_and_the_next_one_still_goes() {
        let Some(bus) = crate::private_bus::PrivateBus::start() else {
            return;
        };
        let mut sink = DesktopSink::at_address(bus.address.clone());
        assert_eq!(sink.deliver(&Message::output_disconnected(), None), None);

        let (tx, rx) = unbounded();
        let _daemon = daemon_on(&bus, tx);
        assert_eq!(sink.deliver(&Message::output_disconnected(), None), Some(1));
        assert!(rx.try_recv().is_ok());
    }

    /// The daemon of [`Daemon`] on `bus`, as a connection the test can signal from.
    fn daemon_on(
        bus: &crate::private_bus::PrivateBus,
        tx: Sender<Received>,
    ) -> zbus::blocking::Connection {
        zbus::blocking::connection::Builder::address(bus.address.as_str())
            .expect("an address")
            .name("org.freedesktop.Notifications")
            .expect("a well-known name")
            .serve_at(
                "/org/freedesktop/Notifications",
                Daemon {
                    tx,
                    next_id: std::sync::atomic::AtomicU32::new(0),
                },
            )
            .expect("a path")
            .build()
            .expect("the daemon is on the bus")
    }

    #[test]
    fn a_click_follows_the_link_of_the_notice_shown_and_of_no_notice_before_it() {
        let Some(bus) = crate::private_bus::PrivateBus::start() else {
            return;
        };
        let (tx, rx) = unbounded();
        let daemon = daemon_on(&bus, tx);
        let (opened_tx, opened) = unbounded::<String>();
        let mut sink = DesktopSink::at_address(bus.address.clone());
        sink.open = Arc::new(move |url: &str| {
            let _ = opened_tx.send(url.to_owned());
        });
        let emit = |member: &str, body: &(u32, &str)| {
            daemon
                .emit_signal(
                    None::<&str>,
                    NOTIFICATIONS_PATH,
                    NOTIFICATIONS,
                    member,
                    body,
                )
                .expect("the signal is sent");
        };
        let click = |id: u32| emit("ActionInvoked", &(id, "default"));
        let closed = |id: u32| {
            daemon
                .emit_signal(
                    None::<&str>,
                    NOTIFICATIONS_PATH,
                    NOTIFICATIONS,
                    "NotificationClosed",
                    &(id, 2_u32),
                )
                .expect("the signal is sent");
        };
        let next_opened = || {
            opened
                .recv_timeout(Duration::from_secs(10))
                .expect("a link was followed")
        };
        let link = |url: &str| Message::with_link("Read it.", "Open", url);

        let id = sink
            .deliver(&link("https://example.org/one"), None)
            .expect("delivered");
        click(id);
        assert_eq!(next_opened(), "https://example.org/one");

        // A notice without a link took the slot: a click on it follows nothing.
        let id = sink
            .deliver(&Message::presets_restored(), Some(id))
            .expect("delivered");
        click(id);
        // A notice closed before the click follows nothing either.
        let id = sink
            .deliver(&link("https://example.org/two"), Some(id))
            .expect("delivered");
        closed(id);
        click(id);
        // Signals arrive in order, so a link opened by either of those would come before this.
        let id = sink
            .deliver(&link("https://example.org/three"), Some(id))
            .expect("delivered");
        click(id);
        assert_eq!(next_opened(), "https://example.org/three");
        assert_eq!(rx.len(), 4, "four notices, four calls");
    }
}
