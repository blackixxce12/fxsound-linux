//! Settings ▸ Experimental ▸ "Smooth moves in WirePlumber" (roadmap 0.5.0 §7, D5): FxSound's hook
//! in the user's WirePlumber (`fxsound_audio::wireplumber_hook`), ticked, unticked and restarted
//! from the pane.
//!
//! Not a setting of FxSound's: the tick is whether the hook's files are where WirePlumber reads
//! them, as "Launch on system startup" is whether the autostart entry is there. So a user who takes
//! the files away by hand finds the box unticked, and nothing in `settings.toml` can say otherwise.
//!
//! WirePlumber reads the files only when it starts, so the pane says when the WirePlumber that
//! runs started before the box was last changed, and offers to restart it. A restart asked for
//! and not made — WirePlumber not started by systemd — is said too; the next login brings the
//! change then.

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
    /// Restart it ([`wireplumber_hook::restart`]).
    pub(crate) restart: fn() -> std::io::Result<()>,
    /// When the box was last ticked or unticked in this run.
    changed: Option<SystemTime>,
    /// A restart was asked for since, and the WirePlumber from before is still the one running.
    restart_failed: bool,
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
            restart_failed: false,
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
            restart_failed: false,
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
            restart_failed: false,
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
        let on = place.installed().is_on();
        // Changed in this run, or installed by an earlier one: the fragment's time.
        let since = self
            .changed
            .or_else(|| on.then(|| place.installed_at()).flatten());
        let due = since.is_some_and(|since| (self.running_since_before)(since) == Some(true));
        WirePlumberHook {
            available: self.available(),
            on,
            restart: match (due, self.restart_failed) {
                (false, _) => WirePlumberRestart::NotNeeded,
                (true, false) => WirePlumberRestart::Due,
                (true, true) => WirePlumberRestart::Failed,
            },
        }
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
        let result = if on { place.install() } else { place.remove() };
        // Even a half-done change is a change WirePlumber has not read.
        self.changed = Some(SystemTime::now());
        self.restart_failed = false;
        result
    }

    /// Restart WirePlumber, once the user said yes. Whether the WirePlumber that runs afterwards
    /// has read the hook's files as they are; if not, the pane says it could not be restarted.
    pub(crate) fn restart(&mut self) -> bool {
        if let Err(error) = (self.restart)() {
            log::warn!("WirePlumber could not be restarted: {error}");
        }
        self.restart_failed = self.state().restart != WirePlumberRestart::NotNeeded;
        !self.restart_failed
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
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);

        // Restarted since, or none running: nothing is due.
        host.running_since_before = |_| Some(false);
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);
        host.running_since_before = |_| None;
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
        assert!(!host.restart());
        assert_eq!(RESTARTS.load(Ordering::SeqCst), 1);
        assert_eq!(host.state().restart, WirePlumberRestart::Failed);

        // A change after it asks again.
        host.set(false).expect("the hook goes");
        assert_eq!(host.state().restart, WirePlumberRestart::Due);

        // A restart that takes: a WirePlumber started after the change.
        host.running_since_before = |_| Some(false);
        assert!(host.restart());
        assert_eq!(host.state().restart, WirePlumberRestart::NotNeeded);

        // systemctl failing, the old WirePlumber still there: failed.
        host.running_since_before = |_| Some(true);
        host.restart = || Err(std::io::Error::other("no systemd"));
        assert!(!host.restart());
        assert_eq!(host.state().restart, WirePlumberRestart::Failed);
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
