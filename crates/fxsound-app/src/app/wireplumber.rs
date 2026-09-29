//! Settings ▸ Experimental ▸ "Smooth moves in WirePlumber" (roadmap 0.5.0 §7, D5): FxSound's hook
//! in the user's WirePlumber (`fxsound_audio::wireplumber_hook`), ticked, unticked and restarted
//! from the pane.
//!
//! Not a setting of FxSound's: the tick is whether the hook's files are where WirePlumber reads
//! them, as "Launch on system startup" is whether the autostart entry is there. So a user who takes
//! the files away by hand finds the box unticked, and nothing in `settings.toml` can say otherwise.
//!
//! WirePlumber reads the files only when it starts, so the pane says when the WirePlumber that
//! runs started before the box was last changed, and offers to restart it — unless the box went
//! back to what that WirePlumber read (ticked and unticked again), which leaves it nothing new to
//! read. A restart asked for
//! and not made — WirePlumber not started by systemd — is said too; the next login brings the
//! change then.

use std::cell::{Cell, RefCell};
use std::sync::mpsc;
use std::time::SystemTime;

use fxsound_audio::wireplumber_hook::{self, Installed, Place, Version};
use fxsound_ui::dialogs::settings::{WirePlumberHook, WirePlumberRestart};

/// Where the hook's files go, and how WirePlumber is asked about and restarted.
///
/// [`WirePlumberHost::of_user`] for a run that may write the user's files, [`WirePlumberHost::none`]
/// for one that must not; the tests give theirs a scratch [`Place`] and stand-ins for WirePlumber,
/// so that no test ever reads the user's WirePlumber or restarts it.
pub(crate) struct WirePlumberHost {
    /// `None`: nothing is installed, taken away or offered.
    pub(crate) place: Option<Place>,
    /// The WirePlumber installed here (`wireplumber --version`).
    pub(crate) version: fn() -> Option<Version>,
    /// Whether a WirePlumber of this user has run since before a time
    /// ([`wireplumber_hook::running_since_before`]).
    pub(crate) running_since_before: fn(SystemTime) -> Option<bool>,
    /// Restart it ([`wireplumber_hook::restart`]). It waits for systemd to have stopped and started
    /// WirePlumber — up to systemd's stop timeout, 90 s by default, for one stuck on a device — so
    /// it runs on a thread of its own ([`Self::restart`]), never on the window's.
    pub(crate) restart: fn() -> std::io::Result<()>,
    /// When the box was last ticked or unticked in this run.
    changed: Option<SystemTime>,
    /// The files as a WirePlumber that started before this run's changes read them, when that is
    /// known ([`Self::set`]): the box ticked and unticked again leaves that WirePlumber with what
    /// it already has, and nothing to restart for.
    read_before: Option<ReadBefore>,
    /// A restart was asked for since, and the WirePlumber from before is still the one running.
    restart_failed: Cell<bool>,
    /// The restart under way, until its thread says how it went ([`Self::restart_settled`]).
    restarting: RefCell<Option<mpsc::Receiver<std::io::Result<()>>>>,
    /// The change the restart under way was asked for: the time the WirePlumber running must be
    /// newer than once it is over. A change made while it runs is not one it was asked for, so a
    /// restart that did its part leaves that change due, not failed.
    restart_asked_for: Cell<Option<SystemTime>>,
}

/// What a WirePlumber running since before `as_of` read of the hook's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadBefore {
    as_of: SystemTime,
    files: Installed,
}

impl WirePlumberHost {
    /// The user's WirePlumber, and the files where it reads them.
    pub(crate) fn of_user() -> Self {
        Self {
            place: Place::of_user(),
            version: wireplumber_hook::version,
            running_since_before: wireplumber_hook::running_since_before,
            restart: wireplumber_hook::restart,
            changed: None,
            read_before: None,
            restart_failed: Cell::new(false),
            restarting: RefCell::new(None),
            restart_asked_for: Cell::new(None),
        }
    }

    /// None at all: a run that must not touch the user's files. The option shows unavailable.
    pub(crate) fn none() -> Self {
        Self {
            place: None,
            version: || None,
            running_since_before: |_| None,
            restart: || Err(std::io::Error::other("no WirePlumber to restart")),
            changed: None,
            read_before: None,
            restart_failed: Cell::new(false),
            restarting: RefCell::new(None),
            restart_asked_for: Cell::new(None),
        }
    }

    /// A host on `place`, with stand-ins for WirePlumber: for the app's tests.
    #[cfg(test)]
    pub(crate) fn for_tests(
        place: Place,
        version: fn() -> Option<Version>,
        running_since_before: fn(SystemTime) -> Option<bool>,
        restart: fn() -> std::io::Result<()>,
    ) -> Self {
        Self {
            place: Some(place),
            version,
            running_since_before,
            restart,
            changed: None,
            read_before: None,
            restart_failed: Cell::new(false),
            restarting: RefCell::new(None),
            restart_asked_for: Cell::new(None),
        }
    }

    /// Whether the hook can run here: a place for its files and WirePlumber 0.5 or later.
    fn available(&self) -> bool {
        self.place.is_some() && (self.version)().is_some_and(wireplumber_hook::supported)
    }

    /// What the pane shows.
    pub(crate) fn state(&self) -> WirePlumberHook {
        let Some(place) = &self.place else {
            return WirePlumberHook::default();
        };
        let installed = place.installed();
        let on = installed.is_on();
        let due = !self.as_the_running_one_read_them(installed) && self.predates(self.since());
        // Due, not failed, while a restart is under way: it has not had its chance yet.
        let failed = self.restart_failed.get() && self.restarting.borrow().is_none();
        WirePlumberHook {
            available: self.available(),
            on,
            restart: match (due, failed) {
                (false, _) => WirePlumberRestart::NotNeeded,
                (true, false) => WirePlumberRestart::Due,
                (true, true) => WirePlumberRestart::Failed,
            },
        }
    }

    /// The time the WirePlumber running must have started after to have read the files as they
    /// are: the box's last change in this run, or the fragment's time for a hook an earlier run
    /// installed. `None`: nothing for WirePlumber to read.
    fn since(&self) -> Option<SystemTime> {
        let place = self.place.as_ref()?;
        self.changed.or_else(|| {
            place
                .installed()
                .is_on()
                .then(|| place.installed_at())
                .flatten()
        })
    }

    /// Whether the WirePlumber running started before `since`.
    fn predates(&self, since: Option<SystemTime>) -> bool {
        since.is_some_and(|since| (self.running_since_before)(since) == Some(true))
    }

    /// Whether the files, `installed`, are back to what the WirePlumber running read of them when
    /// it started: the box ticked and unticked again under it, or unticked and ticked again.
    fn as_the_running_one_read_them(&self, installed: Installed) -> bool {
        self.read_before
            .is_some_and(|read| read.files == installed && self.predates(Some(read.as_of)))
    }

    /// Tick (`on`) or untick the box: install the hook, or take it away. Ticking needs WirePlumber
    /// 0.5; unticking never does, so that a hook left from before can always go.
    ///
    /// # Errors
    ///
    /// No place for the files, no WirePlumber 0.5 to tick it for, or files that could not be
    /// written or removed. Nothing is changed then but what the error says.
    pub(crate) fn set(&mut self, on: bool) -> std::io::Result<()> {
        let place = self
            .place
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no home for WirePlumber's files"))?;
        if on && !self.available() {
            return Err(std::io::Error::other(
                "WirePlumber 0.5 or later is not installed",
            ));
        }
        // What the WirePlumber running read, while it is still the one this run first changed the
        // files under. Otherwise learnt anew: the files as they are, when it started after they
        // last changed; unknown when it started before that, since the files may have changed
        // between its start and this run's (and then any change is taken for due, as before).
        let now = SystemTime::now();
        if !self
            .read_before
            .is_some_and(|read| self.predates(Some(read.as_of)))
        {
            self.read_before = (!self.predates(self.since())).then(|| ReadBefore {
                as_of: now,
                files: place.installed(),
            });
        }
        let result = if on { place.install() } else { place.remove() };
        // Even a half-done change is a change WirePlumber has not read.
        self.changed = Some(now);
        self.restart_failed.set(false);
        result
    }

    /// Restart WirePlumber, once the user said yes: on a thread of its own, since systemd may take
    /// as long as its stop timeout to stop a WirePlumber stuck on a device, and the window would
    /// hang meanwhile. The pane learns how it went from [`Self::restart_settled`]. Asked again
    /// while one is under way, nothing more is done.
    pub(crate) fn restart(&self) {
        if self.restarting.borrow().is_some() {
            return;
        }
        let (done, outcome) = mpsc::channel();
        let restart = self.restart;
        let spawned = std::thread::Builder::new()
            .name("wireplumber-restart".to_owned())
            .spawn(move || {
                let _ = done.send(restart());
            });
        match spawned {
            Ok(_) => {
                *self.restarting.borrow_mut() = Some(outcome);
                self.restart_asked_for.set(self.since());
            }
            Err(error) => {
                log::warn!("WirePlumber could not be restarted: no thread to do it on: {error}");
                self.restart_failed.set(true);
            }
        }
    }

    /// Whether a restart asked for has finished since this was last asked, and what the pane
    /// shows now if it has: the WirePlumber that runs afterwards has read the hook's files as they
    /// are, or the pane says it could not be restarted. `None` while none is under way or it still
    /// is: asked on every frame the pane is drawn, it costs a look at a channel and nothing more.
    pub(crate) fn restart_settled(&self) -> Option<WirePlumberHook> {
        let outcome = match self.restarting.borrow().as_ref()?.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(std::io::Error::other(
                "the restart's thread ended without a word",
            )),
        };
        Some(self.settle(outcome))
    }

    /// The restart under way is over, with `outcome`: what the pane shows now. Failed only when
    /// the WirePlumber running still predates the change the restart was asked for; one made
    /// while it ran is still due, since the restart may have come before it.
    fn settle(&self, outcome: std::io::Result<()>) -> WirePlumberHook {
        self.restarting.replace(None);
        let asked_for = self.restart_asked_for.take();
        if let Err(error) = outcome {
            log::warn!("WirePlumber could not be restarted: {error}");
        }
        self.restart_failed.set(self.predates(asked_for));
        self.state()
    }

    /// Wait for the restart under way, if any, to finish, and say what the pane shows then: for the
    /// tests, which ask for a restart and then look.
    #[cfg(test)]
    pub(crate) fn finish_restart(&self) -> WirePlumberHook {
        let outcome = self
            .restarting
            .borrow()
            .as_ref()
            .map(|restarting| restarting.recv().unwrap_or(Ok(())));
        outcome.map_or_else(|| self.state(), |outcome| self.settle(outcome))
    }

    /// At start-up: bring a hook an earlier run installed up to this build — a newer FxSound's
    /// script over an older one's — and take it away from under a WirePlumber older than 0.5,
    /// which cannot run it. Nothing when it is not installed, or when no WirePlumber answers.
    pub(crate) fn bring_up_to_date(&self) {
        let Some(place) = &self.place else {
            return;
        };
        let installed = place.installed();
        if !installed.is_on() {
            return;
        }
        match (self.version)() {
            Some(version) if !wireplumber_hook::supported(version) => {
                log::warn!(
                    "WirePlumber {}.{}.{} cannot run FxSound's hook; taking it away",
                    version.0,
                    version.1,
                    version.2
                );
                if let Err(error) = place.remove() {
                    log::warn!("FxSound's hook in WirePlumber could not be taken away: {error}");
                }
            }
            Some(_) if installed == Installed::Stale => {
                log::info!("bringing FxSound's hook in WirePlumber up to this version");
                if let Err(error) = place.install() {
                    log::warn!("FxSound's hook in WirePlumber could not be written: {error}");
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A host on a scratch place, with WirePlumber `version`, none of it running, and a restart
    /// that does nothing.
    fn host(dir: &std::path::Path, version: fn() -> Option<Version>) -> WirePlumberHost {
        WirePlumberHost::for_tests(
            Place::under(&dir.join("config"), &dir.join("data")),
            version,
            |_| None,
            || Ok(()),
        )
    }

    fn wp_0_5() -> Option<Version> {
        Some((0, 5, 17))
    }

    fn wp_0_4() -> Option<Version> {
        Some((0, 4, 17))
    }

    #[test]
    fn ticking_installs_the_hook_and_unticking_takes_it_away() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        let place = host.place.clone().expect("a place");
        assert_eq!(
            host.state(),
            WirePlumberHook {
                available: true,
                on: false,
                restart: WirePlumberRestart::NotNeeded
            }
        );
        host.set(true).expect("the hook installs");
        assert_eq!(place.installed(), Installed::Current);
        assert!(host.state().on);
        host.set(false).expect("the hook goes");
        assert_eq!(place.installed(), Installed::No);
        assert!(!host.state().on);
    }

    #[test]
    fn without_wireplumber_0_5_the_box_cannot_be_ticked_but_a_hook_left_there_can_go() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_4);
        let place = host.place.clone().expect("a place");
        assert!(!host.state().available);
        assert!(host.set(true).is_err());
        assert_eq!(place.installed(), Installed::No);

        place.install().expect("a hook from before");
        assert!(host.state().on);
        host.set(false).expect("the hook goes");
        assert_eq!(place.installed(), Installed::No);

        // And with no WirePlumber at all.
        let mut host = WirePlumberHost {
            version: || None,
            ..host
        };
        assert!(!host.state().available);
        assert!(host.set(true).is_err());
    }

    #[test]
    fn a_run_that_must_not_touch_the_users_files_offers_nothing() {
        let mut host = WirePlumberHost::none();
        assert_eq!(host.state(), WirePlumberHook::default());
        assert!(host.set(true).is_err());
        assert!(host.set(false).is_err());
    }

    #[test]
    fn a_wireplumber_started_before_the_change_is_due_a_restart_and_one_started_after_is_not() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        // Nothing changed yet: nothing to restart for.
        host.running_since_before = |_| Some(true);
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);

        host.set(true).expect("the hook installs");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);

        // Restarted since, or none running: nothing is due.
        host.running_since_before = |_| Some(false);
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
        host.running_since_before = |_| None;
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
    }

    #[test]
    fn the_box_ticked_and_unticked_again_under_the_same_wireplumber_asks_for_no_restart() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        // Running since login, before anything this run did.
        host.running_since_before = |_| Some(true);
        host.set(true).expect("the hook installs");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
        // And once more, the same.
        host.set(true).expect("the hook installs");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
    }

    #[test]
    fn a_hook_the_running_wireplumber_read_unticked_and_ticked_again_asks_for_no_restart() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        let place = host.place.clone().expect("a place");
        place.install().expect("installed by an earlier run");
        // WirePlumber started after the earlier run's files, and before this run's changes.
        static FILES: std::sync::Mutex<Option<SystemTime>> = std::sync::Mutex::new(None);
        *FILES.lock().expect("the files' time") = place.installed_at();
        host.running_since_before = |since| {
            let files = FILES.lock().expect("the files' time").expect("a time");
            Some(since > files)
        };
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
        std::thread::sleep(std::time::Duration::from_millis(5));
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        host.set(true).expect("the hook is back");
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
    }

    #[test]
    fn a_wireplumber_restarted_between_two_changes_is_due_when_the_files_go_back() {
        // Started after the tick: it read the hook, so taking the hook away again is a change for
        // it, even though it gives back the files the WirePlumber from before had.
        static STARTED: std::sync::Mutex<Option<SystemTime>> = std::sync::Mutex::new(None);
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        host.running_since_before = |since| {
            let started = *STARTED.lock().expect("the start's lock");
            Some(started.is_none_or(|started| started < since))
        };
        host.set(true).expect("the hook installs");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        std::thread::sleep(std::time::Duration::from_millis(5));
        *STARTED.lock().expect("the start's lock") = Some(SystemTime::now());
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
        std::thread::sleep(std::time::Duration::from_millis(5));
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        // Back to what that WirePlumber read: nothing due.
        host.set(true).expect("the hook is back");
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
    }

    #[test]
    fn a_hook_installed_by_an_earlier_run_is_due_from_the_time_of_its_files() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let host = host(dir.path(), wp_0_5);
        host.place
            .as_ref()
            .expect("a place")
            .install()
            .expect("installed by an earlier run");
        let written = host
            .place
            .as_ref()
            .and_then(Place::installed_at)
            .expect("the time of its files");
        static ASKED: std::sync::Mutex<Option<SystemTime>> = std::sync::Mutex::new(None);
        let host = WirePlumberHost {
            running_since_before: |since| {
                *ASKED.lock().expect("the time asked") = Some(since);
                Some(true)
            },
            ..host
        };
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        assert_eq!(*ASKED.lock().expect("the time asked"), Some(written));
    }

    #[test]
    fn a_restart_that_leaves_the_old_wireplumber_running_is_said_to_have_failed() {
        static RESTARTS: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        host.restart = || {
            RESTARTS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        host.running_since_before = |_| Some(true);
        host.set(true).expect("the hook installs");
        host.restart();
        assert_eq!(host.finish_restart().restart, WirePlumberRestart::Failed);
        assert_eq!(RESTARTS.load(Ordering::SeqCst), 1);
        assert_eq!(host.state().restart, WirePlumberRestart::Failed);

        // Unticked, the files are what the old WirePlumber has; ticked again, asked again.
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
        host.set(true).expect("the hook installs");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);

        // A restart that takes: a WirePlumber started after the change.
        host.running_since_before = |_| Some(false);
        host.restart();
        assert_eq!(host.finish_restart().restart, WirePlumberRestart::NotNeeded);

        // systemctl failing, the old WirePlumber still there: failed.
        host.running_since_before = |_| Some(true);
        host.restart = || Err(std::io::Error::other("no systemd"));
        host.restart();
        assert_eq!(host.finish_restart().restart, WirePlumberRestart::Failed);
        assert_eq!(host.state().restart, WirePlumberRestart::Failed);
    }

    #[test]
    fn a_restart_that_systemd_takes_long_over_leaves_the_window_free_and_is_said_once_it_is_over() {
        // A WirePlumber stuck on a device: systemctl waits for systemd's stop timeout. Here, until
        // the test lets it go.
        static HELD: std::sync::Mutex<bool> = std::sync::Mutex::new(true);
        static LET_GO: std::sync::Condvar = std::sync::Condvar::new();
        static RESTARTS: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        host.restart = || {
            RESTARTS.fetch_add(1, Ordering::SeqCst);
            let mut held = HELD.lock().expect("the stand-in's lock");
            while *held {
                held = LET_GO.wait(held).expect("the stand-in's lock");
            }
            Ok(())
        };
        host.running_since_before = |_| Some(true);
        host.set(true).expect("the hook installs");

        let asked = std::time::Instant::now();
        host.restart();
        assert!(
            asked.elapsed() < std::time::Duration::from_secs(1),
            "the restart held the window for {:?}",
            asked.elapsed()
        );
        // Under way: still due, not failed, and nothing to say yet. Asked again, nothing more.
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        assert_eq!(host.restart_settled(), None);
        host.restart();

        *HELD.lock().expect("the stand-in's lock") = false;
        LET_GO.notify_all();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let settled = loop {
            if let Some(settled) = host.restart_settled() {
                break settled;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the restart never said it was over"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        // The WirePlumber from before still runs: the stand-in restarted nothing.
        assert_eq!(settled.restart, WirePlumberRestart::Failed);
        assert_eq!(RESTARTS.load(Ordering::SeqCst), 1);
        assert_eq!(host.restart_settled(), None);
    }

    #[test]
    fn a_change_made_while_a_restart_is_running_leaves_the_pane_at_due_not_failed() {
        // The restart starts a new WirePlumber after the first change, then systemd holds it until
        // the test lets it go; the box is unticked meanwhile, after the new WirePlumber started.
        static STARTED: std::sync::Mutex<Option<SystemTime>> = std::sync::Mutex::new(None);
        static HELD: std::sync::Mutex<bool> = std::sync::Mutex::new(true);
        static LET_GO: std::sync::Condvar = std::sync::Condvar::new();
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let mut host = host(dir.path(), wp_0_5);
        host.restart = || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            *STARTED.lock().expect("the start's lock") = Some(SystemTime::now());
            let mut held = HELD.lock().expect("the stand-in's lock");
            while *held {
                held = LET_GO.wait(held).expect("the stand-in's lock");
            }
            Ok(())
        };
        host.running_since_before = |since| {
            let started = *STARTED.lock().expect("the start's lock");
            Some(started.is_none_or(|started| started < since))
        };
        host.set(true).expect("the hook installs");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
        host.restart();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while STARTED.lock().expect("the start's lock").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the restart never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);

        *HELD.lock().expect("the stand-in's lock") = false;
        LET_GO.notify_all();
        // The restart did its part: the new WirePlumber read the files as they were when it was
        // asked for. The change after it is due another, not a restart that failed.
        assert_eq!(host.finish_restart().restart, WirePlumberRestart::Due);
        assert_eq!(host.state().restart, WirePlumberRestart::Due);
    }

    #[test]
    fn at_start_up_an_older_hook_is_brought_up_to_date_and_one_under_wireplumber_0_4_is_taken_away()
    {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let host = host(dir.path(), wp_0_5);
        let place = host.place.clone().expect("a place");

        // Not installed: nothing is written.
        host.bring_up_to_date();
        assert_eq!(place.installed(), Installed::No);

        place.install().expect("installed");
        std::fs::write(place.script(), "-- an older hook\n").expect("an older script");
        host.bring_up_to_date();
        assert_eq!(place.installed(), Installed::Current);

        let old = WirePlumberHost {
            version: wp_0_4,
            ..host
        };
        old.bring_up_to_date();
        assert_eq!(place.installed(), Installed::No);

        // No WirePlumber answering: left as it is.
        place.install().expect("installed");
        std::fs::write(place.script(), "-- an older hook\n").expect("an older script");
        let silent = WirePlumberHost {
            version: || None,
            ..old
        };
        silent.bring_up_to_date();
        assert_eq!(place.installed(), Installed::Stale);
    }
}
