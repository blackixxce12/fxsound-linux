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
//! Not Disturb is the daemon's job. Sending at `Urgency::Low` is what asks it to apply that policy,
//! and it is also the analogue of `NIIF_RESPECT_QUIET_TIME` (`:411`).
//!
//! # Threading
//!
//! `notify_rust::Notification::show()` is `zbus::block_on` over a whole connect-and-send round trip
//! (`notify-rust-4.18.0/src/xdg/mod.rs:8,413`), so it must never be called from the egui thread.
//! [`Notifier`] owns a worker thread and the GUI only ever hands it a [`Message`].

use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify_rust::{Hint, Notification, NotificationResponse, Timeout, Urgency};

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

/// Where the "what's new" link goes (`FxController.cpp:719`).
pub const CHANGELOG_URL: &str = "https://www.fxsound.com/changelog";
/// Where the survey link goes (`FxController.cpp:938-960`).
pub const SURVEY_URL: &str = "https://forms.gle/ATx1ayXDWRaMdiR59";

/// The clickable half of a message. On Windows it is an `FxHyperlink` drawn inside the toast; here
/// it becomes the notification's default action, so clicking the body follows it
/// (`docs/spec/07-startup-tray.md` §6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The link text, e.g. `"Take the survey."`.
    pub label: String,
    /// An `http`/`https` URL. Anything else is refused when the action fires.
    pub url: String,
}

/// One pushed message — the payload of `FxModel::pushMessage` (`FxModel.h:170-175`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The text. `\r\n` is normalised to `\n` when the notification is built.
    pub body: String,
    pub link: Option<Link>,
}

impl Message {
    #[must_use]
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            link: None,
        }
    }

    #[must_use]
    pub fn with_link(body: impl Into<String>, label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            link: Some(Link {
                label: label.into(),
                url: url.into(),
            }),
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
    // §6.4 tabulates them. The English `TRANS` keys are reproduced verbatim; `Resources/Strings/`
    // is empty in this checkout, so they are the only authoritative strings there are.

    /// First run after a version change (`FxController.cpp:719`). The body really is a single
    /// space: the message is the link.
    #[must_use]
    pub fn whats_new() -> Self {
        Self::with_link(
            " ",
            "Click here to see what's new on this version!",
            CHANGELOG_URL,
        )
    }

    /// First hide-to-tray of the session (`FxController.cpp:920-926`), after [`TRAY_HINT_DELAY`].
    ///
    /// This one matters more on Linux than on Windows: a user whose desktop has no
    /// StatusNotifierItem host — GNOME without the AppIndicator extension — has otherwise lost the
    /// app entirely (§6.5).
    #[must_use]
    pub fn minimised_to_tray() -> Self {
        Self::new(tr("FxSound in system tray\nClick FxSound icon to reopen"))
    }

    /// Shown from `showMainWindow` once the 7-day `survey_timer` has elapsed
    /// (`FxController.cpp:938-960`).
    #[must_use]
    pub fn survey() -> Self {
        Self::with_link(
            "Thanks for using FxSound! Would you be\r\ninterested in helping us by taking a quick \
             4 minute\r\nsurvey so we can make FxSound better?",
            "Take the survey.",
            SURVEY_URL,
        )
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
        Self::new(tr("Output Disconnected"))
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
        Self::new(tr("Reached the limit on new presets."))
    }

    /// A user preset was deleted (`FxController.cpp:1313`).
    #[must_use]
    pub fn preset_deleted(name: &str) -> Self {
        Self::new(tr_args("Preset %s is deleted.", &[name]))
    }

    /// The preset list was reset (`FxController.cpp:1381`). Note the original has no full stop.
    #[must_use]
    pub fn presets_restored() -> Self {
        Self::new(tr("Presets are restored to factory defaults"))
    }

    /// Power toggled from a hotkey rather than the tray or the window
    /// (`FxController.cpp:1933`) — on Linux, from a compositor keybinding running
    /// `fxsound --toggle-power`.
    #[must_use]
    pub fn power_toggled(on: bool) -> Self {
        Self::new(tr_args("FxSound is %s.", &[&tr(if on { "on" } else { "off" })]))
    }
}

/// Build the `org.freedesktop.Notifications` call for a message.
///
/// Pure, so it can be checked without a daemon on the bus. The hint set is
/// `docs/spec/07-startup-tray.md` §6.5's, and each hint has a Windows ancestor:
/// `suppress-sound` is `NIIF_NOSOUND` (`FxSystemTrayView.cpp:411`), the low urgency is
/// `NIIF_RESPECT_QUIET_TIME` (same line), and `desktop-entry` is what lets a shell attribute the
/// notification to our window.
#[must_use]
pub fn build(message: &Message, replaces: Option<u32>) -> Notification {
    let mut notification = Notification::new();
    notification
        .appname(APP_NAME)
        .summary(APP_NAME)
        .body(&message.body.replace("\r\n", "\n"))
        .icon(APP_ID)
        .urgency(Urgency::Low)
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

/// Delivery over `org.freedesktop.Notifications`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DesktopSink;

impl Sink for DesktopSink {
    fn deliver(&mut self, message: &Message, replaces: Option<u32>) -> Option<u32> {
        match build(message, replaces).show() {
            Ok(handle) => {
                let id = handle.id();
                match message.link.clone() {
                    // The handle keeps the D-Bus connection alive, and dropping it can stop the
                    // action from ever firing (`notify-rust-4.18.0/src/xdg/mod.rs:64-70`), so it
                    // moves to a thread of its own that outlives this call.
                    Some(link) => {
                        thread::spawn(move || {
                            let _ = handle.wait_for_response(
                                move |response: &NotificationResponse| match response {
                                    NotificationResponse::Default
                                    | NotificationResponse::Action(_) => open_url(&link.url),
                                    // `Reply` is macOS-only and never arrives here
                                    // (`notify-rust-4.18.0/src/response.rs:75-78`).
                                    NotificationResponse::Reply(_)
                                    | NotificationResponse::Closed(_) => {}
                                },
                            );
                        });
                    }
                    None => drop(handle),
                }
                Some(id)
            }
            Err(e) => {
                // A session with no notification daemon is normal on a bare compositor; it is not
                // worth more than a line in the log.
                log::warn!("could not show a notification: {e}");
                None
            }
        }
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
        Self::with_sink(hide_notifications, DesktopSink)
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
        (
            Notifier::with_sink(hidden, Recorder { tx, next_id: 0 }),
            rx,
        )
    }

    fn next(rx: &Receiver<(Message, Option<u32>)>) -> (Message, Option<u32>) {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the worker should have delivered a message")
    }

    fn hint(notification: &Notification, wanted: &Hint) -> bool {
        notification.hints.contains(wanted)
    }

    #[test]
    fn a_plain_message_becomes_a_low_urgency_silent_notification() {
        let notification = build(&Message::preset_selected("Bass Booster"), None);

        assert_eq!(notification.appname, APP_NAME);
        assert_eq!(notification.summary, APP_NAME, "matches szInfoTitle at :413");
        assert_eq!(notification.body, "Preset: Bass Booster");
        assert_eq!(notification.icon, APP_ID);
        assert_eq!(notification.timeout, Timeout::Milliseconds(TIMEOUT_MS));
        assert!(hint(&notification, &Hint::Urgency(Urgency::Low)));
        assert!(hint(&notification, &Hint::SuppressSound(true)));
        assert!(hint(&notification, &Hint::Category("device".to_owned())));
        assert!(hint(&notification, &Hint::DesktopEntry(APP_ID.to_owned())));
        assert!(notification.actions.is_empty());
    }

    #[test]
    fn a_message_with_a_link_gets_the_longer_timeout_and_a_default_action() {
        // 8000 ms rather than 7000 (`FxNotification.cpp:185-192`).
        let notification = build(&Message::survey(), None);
        assert_eq!(
            notification.timeout,
            Timeout::Milliseconds(TIMEOUT_WITH_LINK_MS)
        );
        assert_eq!(
            notification.actions,
            vec!["default".to_owned(), "Take the survey.".to_owned()],
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
        assert_eq!(Message::whats_new().body, " ");
        assert_eq!(
            Message::whats_new().link.unwrap().url,
            "https://www.fxsound.com/changelog"
        );
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
        assert_eq!(Message::preset_saved("Rock").body, "New preset Rock is saved.");
        assert_eq!(
            Message::preset_limit_reached().body,
            "Reached the limit on new presets."
        );
        assert_eq!(Message::preset_deleted("Rock").body, "Preset Rock is deleted.");
        assert_eq!(
            Message::presets_restored().body,
            "Presets are restored to factory defaults"
        );
        assert_eq!(Message::power_toggled(true).body, "FxSound is on.");
        assert_eq!(Message::power_toggled(false).body, "FxSound is off.");
    }

    #[test]
    fn the_survey_message_keeps_its_three_lines() {
        // `FxNotification.cpp:53` drops anything past the third line; the daemon wraps instead, so
        // the text is passed through untouched apart from the line endings.
        let body = build(&Message::survey(), None).body;
        assert_eq!(body.lines().count(), 3);
        assert!(body.starts_with("Thanks for using FxSound!"));
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
}
