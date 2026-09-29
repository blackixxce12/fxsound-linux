//! Smooth moves in WirePlumber: the hook that fades a stream WirePlumber moves, installed into the
//! user's WirePlumber when Settings ▸ Experimental asks for it (roadmap 0.5.0 §7, D5).
//!
//! # Why
//!
//! FxSound fades the streams it has moved itself — the power button's, an application's route,
//! the default it takes back (`crate::stream_handover`) — because it knows of those moves before
//! they happen. A device picked in the desktop's sound settings is moved by WirePlumber the moment
//! the default changes, before FxSound hears of it, unlinked and linked again in the middle of the
//! wave: −19 to −26 dBFS, as in plain Linux (roadmap §14 #16). Only WirePlumber can fade that
//! one, and WirePlumber 0.5 has the place for it: the chain of hooks a `select-target` event runs
//! through, where an asynchronous hook ahead of `linking/prepare-link` can fade the stream and hold
//! the move until the fade has been played, and one after `linking/link-target` can give the
//! volume back once the new link is up. That is [`SCRIPT`]; measured on a private graph with
//! WirePlumber 0.5.17, every move of 24 went below −70 dBFS with it.
//!
//! A stream moved off FxSound's own nodes — the desktop's pick with FxSound on — is held silent for
//! longer after its new link (`CLAIM_WAIT_MS` in the script): FxSound takes the default back a
//! moment later and has the stream moved back, and the hook must not be giving the volume back
//! then, or FxSound's fade and the hook's ramp meet on the stream and PipeWire's converter jumps.
//! Held, the stream is silent when FxSound's handover looks at it, which then leaves it to the hook
//! (`crate::stream_handover`, "not silent already").
//!
//! # The volume WirePlumber keeps
//!
//! WirePlumber keeps a stream's master volume for the application's next stream
//! (`node/state-stream.lua`: `store_stream_props_hook` saves it each time the stream's `Props`
//! change, `restore_stream_hook` gives it to the next stream). The 0 the hook holds a stream at is
//! not the stream's, and kept, a stream closed while it is held — a player stopped, a tab closed, in
//! that tenth of a second — would leave every later stream of the application silent while the
//! desktop's mixer says 100 %: the stuck zero `crate::stream_handover`'s journal guards against
//! for FxSound's own fades. The hook keeps it from being kept instead: a held stream's change to
//! silence stops at the hook, ahead of `node/store-stream-props`, and never reaches the state; the
//! volume given back is a change like any other. Writing the state from the hook afterwards would
//! not do: `state-stream.lua` keeps what it saved in memory, gives that to the next stream and
//! writes it over its file at its next save. Measured on a private graph (`graph_churn::hook`): a
//! PulseAudio player closed in the hold came back at 0 without this, at its own volume with it.
//!
//! # What it costs, and why it is an option
//!
//! It changes WirePlumber's policy for every application, not FxSound's alone; it needs
//! WirePlumber 0.5 or later ([`SINCE`]: Ubuntu 24.04 ships 0.4, which is configured in Lua); it
//! leans on the names of three of WirePlumber's own hooks; and WirePlumber reads it only when it
//! starts. So it is off unless the user ticks it, and the pane offers the restart
//! ([`restart`]), asking first.
//!
//! # How it is installed
//!
//! Two files, the only ones FxSound writes into WirePlumber's directories ([`Place`]): the script,
//! under `$XDG_DATA_HOME/wireplumber/scripts/fxsound/`, and a configuration fragment
//! ([`FRAGMENT`]), under `$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/`, that makes it a
//! component. The component is **optional**: a virtual feature that the `base` profile — which
//! every profile of WirePlumber 0.5 inherits — requires *wants* it. A component WirePlumber is
//! made to require stops WirePlumber, and with it the sound of the whole session, when it fails to
//! load: a script missing or failing on a WirePlumber that changed its API did exactly that on a
//! private graph (`failed to load components: … Lua runtime error`), where a wanted one is logged
//! and skipped. The script also needs `hooks.linking.target.link`, so an instance of WirePlumber
//! that links nothing leaves it out.
//!
//! Unticked, both files go. A newer FxSound finding the files of an older one writes its own
//! over them ([`Place::installed`], [`Installed::Stale`]), and one finding them under a
//! WirePlumber older than 0.5 takes them away.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// The hook itself, a WirePlumber 0.5 Lua script.
pub const SCRIPT: &str = include_str!("wireplumber_hook/fade-on-move.lua");

/// The configuration fragment that makes [`SCRIPT`] an optional component of every profile.
pub const FRAGMENT: &str = include_str!("wireplumber_hook/90-fxsound-fade-on-move.conf");

/// Where [`SCRIPT`] goes, under the user's data directory: where WirePlumber looks for the scripts
/// its components name (`fxsound/fade-on-move.lua` in [`FRAGMENT`]).
const SCRIPT_PATH: &str = "wireplumber/scripts/fxsound/fade-on-move.lua";

/// Where [`FRAGMENT`] goes, under the user's configuration directory.
const FRAGMENT_PATH: &str = "wireplumber/wireplumber.conf.d/90-fxsound-fade-on-move.conf";

/// The first WirePlumber with profiles, components configured in `wireplumber.conf.d` and the
/// event hooks the script is built from.
pub const SINCE: (u32, u32) = (0, 5);

/// A WirePlumber version: major, minor, micro.
pub type Version = (u32, u32, u32);

/// Whether the hook's files are where WirePlumber reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// No fragment: WirePlumber loads nothing of FxSound's.
    No,
    /// This build's script and fragment.
    Current,
    /// A fragment, with a script or a fragment other than this build's, or no script: another
    /// version of FxSound installed it, or someone changed it. Still ticked; FxSound writes its own
    /// over it.
    Stale,
}

impl Installed {
    /// Whether the option is ticked: the fragment is there.
    #[must_use]
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::No)
    }
}

/// The two files of the hook, for one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    script: PathBuf,
    fragment: PathBuf,
}

impl Place {
    /// The hook's files under a configuration directory and a data directory: what
    /// `$XDG_CONFIG_HOME` and `$XDG_DATA_HOME` are to WirePlumber.
    #[must_use]
    pub fn under(config_home: &Path, data_home: &Path) -> Self {
        Self {
            script: data_home.join(SCRIPT_PATH),
            fragment: config_home.join(FRAGMENT_PATH),
        }
    }

    /// The hook's files for the user FxSound runs as, where the user's WirePlumber reads them.
    /// `None` without a home to put them in.
    #[must_use]
    pub fn of_user() -> Option<Self> {
        Self::in_environment(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// [`Place::of_user`] given `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME` and `$HOME`: each XDG
    /// directory when it is set to an absolute path — the XDG spec ignores a relative one — and
    /// `.config` or `.local/share` in the home otherwise.
    fn in_environment(
        config_home: Option<&std::ffi::OsStr>,
        data_home: Option<&std::ffi::OsStr>,
        home: Option<&std::ffi::OsStr>,
    ) -> Option<Self> {
        let home = home.filter(|home| !home.is_empty()).map(PathBuf::from);
        let xdg = |value: Option<&std::ffi::OsStr>, fallback: &[&str]| {
            value
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
                .or_else(|| {
                    home.as_ref().map(|home| {
                        fallback
                            .iter()
                            .fold(home.clone(), |dir, part| dir.join(part))
                    })
                })
        };
        Some(Self::under(
            &xdg(config_home, &[".config"])?,
            &xdg(data_home, &[".local", "share"])?,
        ))
    }

    /// Where the script goes.
    #[must_use]
    pub fn script(&self) -> &Path {
        &self.script
    }

    /// Where the fragment goes.
    #[must_use]
    pub fn fragment(&self) -> &Path {
        &self.fragment
    }

    /// Whether the hook is installed, and whether it is this build's.
    #[must_use]
    pub fn installed(&self) -> Installed {
        let Ok(fragment) = std::fs::read_to_string(&self.fragment) else {
            return if self.fragment.exists() {
                Installed::Stale
            } else {
                Installed::No
            };
        };
        let script = std::fs::read_to_string(&self.script).ok();
        if fragment == FRAGMENT && script.as_deref() == Some(SCRIPT) {
            Installed::Current
        } else {
            Installed::Stale
        }
    }

    /// When the hook was last installed: the fragment's modification time. `None` when it is not
    /// installed.
    #[must_use]
    pub fn installed_at(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.fragment)
            .and_then(|meta| meta.modified())
            .ok()
    }

    /// Write this build's script and fragment, the script first: a fragment is what has
    /// WirePlumber look for the script. Each is written beside itself and renamed into place, so
    /// that a WirePlumber starting meanwhile reads a whole file or none.
    ///
    /// # Errors
    ///
    /// A directory that cannot be made or a file that cannot be written.
    pub fn install(&self) -> io::Result<()> {
        write_whole(&self.script, SCRIPT)?;
        write_whole(&self.fragment, FRAGMENT)
    }

    /// Take the hook away: the fragment first, then the script, then the script's `fxsound`
    /// directory if nothing else is in it. A file already gone is not an error.
    ///
    /// # Errors
    ///
    /// A file that is there and cannot be removed.
    pub fn remove(&self) -> io::Result<()> {
        for file in [&self.fragment, &self.script] {
            match std::fs::remove_file(file) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        if let Some(dir) = self.script.parent() {
            // Only when empty: `remove_dir` refuses a directory with anything in it.
            let _ = std::fs::remove_dir(dir);
        }
        Ok(())
    }
}

/// Write `text` to `path` whole: into a file beside it, then renamed over it.
fn write_whole(path: &Path, text: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no directory", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".fxsound-new");
    let beside = dir.join(name);
    std::fs::write(&beside, text)?;
    std::fs::rename(&beside, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&beside);
    })
}

/// The WirePlumber installed here, as `wireplumber --version` says: the library it is linked with.
/// `None` when there is none, or it says nothing this can read.
#[must_use]
pub fn version() -> Option<Version> {
    let output = Command::new("wireplumber")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

/// The version `wireplumber --version` prints: the library it is linked with, or the one it was
/// compiled with when it names only that.
///
/// ```text
/// wireplumber
/// Compiled with libwireplumber 0.5.17
/// Linked with libwireplumber 0.5.17
/// ```
#[must_use]
pub fn parse_version(text: &str) -> Option<Version> {
    let words: Vec<&str> = text.split_whitespace().collect();
    words
        .windows(2)
        .filter(|pair| pair[0] == "libwireplumber")
        .filter_map(|pair| {
            let mut parts = pair[1].split('.').map(str::parse::<u32>);
            Some((
                parts.next()?.ok()?,
                parts.next()?.ok()?,
                parts.next().and_then(Result::ok).unwrap_or(0),
            ))
        })
        .next_back()
}

/// Whether the hook can run on WirePlumber `version`: 0.5 or later ([`SINCE`]).
#[must_use]
pub fn supported(version: Version) -> bool {
    (version.0, version.1) >= SINCE
}

/// Restart the user's WirePlumber, so that it reads the hook's files again: `systemctl --user
/// try-restart wireplumber.service`, which restarts it only if systemd started it. A WirePlumber
/// the desktop started some other way is left running: starting a second one beside it would be
/// worse than the restart a new login brings.
///
/// # Errors
///
/// No `systemctl`, or one that did not succeed.
pub fn restart() -> io::Result<()> {
    let status = Command::new("systemctl")
        .args(["--user", "try-restart", "wireplumber.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("systemctl: {status}")))
    }
}

/// Whether the user's WirePlumber has run since before `when` — so that it read WirePlumber's
/// files as they were before then, and a change made at `when` is not in it yet. `None` when no
/// WirePlumber of this user runs, or its start cannot be read.
///
/// Read from `/proc`: each process of this user named `wireplumber`, its start in clock ticks after
/// boot (`stat`'s 22nd field), set against the time since boot now (`/proc/uptime`). One started
/// before `when` is enough: that one has not read it.
#[must_use]
pub fn running_since_before(when: SystemTime) -> Option<bool> {
    let proc = Path::new("/proc");
    let uptime = std::fs::read_to_string(proc.join("uptime")).ok()?;
    let now = SystemTime::now();
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(proc.join("self")).ok()?);
    let starts: Vec<SystemTime> = std::fs::read_dir(proc)
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .filter(|entry| {
            entry
                .metadata()
                .is_ok_and(|meta| std::os::unix::fs::MetadataExt::uid(&meta) == uid)
        })
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("comm"))
                .is_ok_and(|comm| comm.trim_end() == "wireplumber")
        })
        .filter_map(|entry| {
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            started_at(&stat, &uptime, now)
        })
        .collect();
    if starts.is_empty() {
        return None;
    }
    Some(starts.iter().any(|start| *start < when))
}

/// The clock ticks of `/proc/<pid>/stat`'s start time: `USER_HZ`, which the kernel keeps at 100 for
/// what it shows user space on every architecture a desktop runs (`sysconf(_SC_CLK_TCK)`).
const TICKS_PER_SECOND: f64 = 100.0;

/// When the process of `stat` (`/proc/<pid>/stat`) started, given `uptime` (`/proc/uptime`) read
/// at `now`. `None` when either cannot be read.
fn started_at(stat: &str, uptime: &str, now: SystemTime) -> Option<SystemTime> {
    // The command name, the second field, is in parentheses and may hold spaces and parentheses
    // itself: the fields are counted from the last `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the name: the state is field 3, the start time field 22.
    let ticks: f64 = rest.split_whitespace().nth(22 - 3)?.parse().ok()?;
    let since_boot: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let ago = since_boot - ticks / TICKS_PER_SECOND;
    if !ago.is_finite() || ago < 0.0 {
        return None;
    }
    now.checked_sub(Duration::from_secs_f64(ago))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_the_library_wireplumber_is_linked_with() {
        let text = "wireplumber\nCompiled with libwireplumber 0.5.17\nLinked with libwireplumber \
                    0.5.18\n";
        assert_eq!(parse_version(text), Some((0, 5, 18)));
        assert_eq!(
            parse_version("wireplumber\nCompiled with libwireplumber 0.4.17\n"),
            Some((0, 4, 17))
        );
        assert_eq!(parse_version("libwireplumber 0.5"), Some((0, 5, 0)));
        assert_eq!(parse_version("wireplumber: command not found"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn the_hook_needs_wireplumber_0_5_or_later() {
        assert!(!supported((0, 4, 17)));
        assert!(supported((0, 5, 0)));
        assert!(supported((0, 5, 17)));
        assert!(supported((0, 6, 0)));
        assert!(supported((1, 0, 0)));
    }

    #[test]
    fn the_files_go_where_wireplumber_reads_its_scripts_and_its_fragments() {
        let place = Place::under(Path::new("/c"), Path::new("/d"));
        assert_eq!(
            place.script(),
            Path::new("/d/wireplumber/scripts/fxsound/fade-on-move.lua")
        );
        assert_eq!(
            place.fragment(),
            Path::new("/c/wireplumber/wireplumber.conf.d/90-fxsound-fade-on-move.conf")
        );
        // The fragment names the script by its path under `scripts`.
        assert!(FRAGMENT.contains("name = fxsound/fade-on-move.lua"));
    }

    #[test]
    fn the_user_s_files_follow_the_xdg_directories_and_fall_back_to_the_home() {
        use std::ffi::OsStr;
        let place = Place::in_environment(
            Some(OsStr::new("/x/config")),
            Some(OsStr::new("/x/data")),
            Some(OsStr::new("/home/u")),
        );
        assert_eq!(
            place,
            Some(Place::under(Path::new("/x/config"), Path::new("/x/data")))
        );
        // A relative XDG directory is ignored, as the spec says.
        let place = Place::in_environment(
            Some(OsStr::new("relative")),
            None,
            Some(OsStr::new("/home/u")),
        );
        assert_eq!(
            place,
            Some(Place::under(
                Path::new("/home/u/.config"),
                Path::new("/home/u/.local/share")
            ))
        );
        assert_eq!(Place::in_environment(None, None, None), None);
        assert_eq!(
            Place::in_environment(None, None, Some(OsStr::new(""))),
            None
        );
    }

    #[test]
    fn installing_writes_both_files_and_removing_takes_both_and_their_directory_away() {
        let dir = fxsound_core::test_support::ScratchDir::new("wp-hook");
        let place = Place::under(&dir.path().join("config"), &dir.path().join("data"));
        assert_eq!(place.installed(), Installed::No);
        assert!(place.installed_at().is_none());

        place.install().expect("the hook installs");
        assert_eq!(place.installed(), Installed::Current);
        assert!(place.installed_at().is_some());
        assert_eq!(
            std::fs::read_to_string(place.script()).expect("the script"),
            SCRIPT
        );
        assert_eq!(
            std::fs::read_to_string(place.fragment()).expect("the fragment"),
            FRAGMENT
        );
        // Nothing is left beside them.
        for file in [place.script(), place.fragment()] {
            let names: Vec<_> = std::fs::read_dir(file.parent().expect("a directory"))
                .expect("the directory")
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            assert_eq!(names.len(), 1, "{names:?}");
        }

        place.remove().expect("the hook is taken away");
        assert_eq!(place.installed(), Installed::No);
        assert!(!place.script().exists() && !place.fragment().exists());
        assert!(!place.script().parent().expect("fxsound/").exists());
        // WirePlumber's own directories stay.
        assert!(dir.path().join("data/wireplumber/scripts").is_dir());
        assert!(
            dir.path()
                .join("config/wireplumber/wireplumber.conf.d")
                .is_dir()
        );
        // Twice is not an error.
        place.remove().expect("nothing to take away");
    }

    #[test]
    fn another_build_s_files_are_stale_and_are_written_over() {
        let dir = fxsound_core::test_support::ScratchDir::new("wp-hook");
        let place = Place::under(&dir.path().join("config"), &dir.path().join("data"));
        place.install().expect("the hook installs");
        std::fs::write(place.script(), "-- an older hook\n").expect("the script");
        assert_eq!(place.installed(), Installed::Stale);
        assert!(place.installed().is_on());

        std::fs::remove_file(place.script()).expect("the script");
        assert_eq!(
            place.installed(),
            Installed::Stale,
            "a fragment without its script"
        );

        place.install().expect("the hook installs over it");
        assert_eq!(place.installed(), Installed::Current);

        // A script alone loads nothing: the option is off.
        std::fs::remove_file(place.fragment()).expect("the fragment");
        assert_eq!(place.installed(), Installed::No);
        assert!(!Installed::No.is_on());
    }

    #[test]
    fn a_script_left_in_the_fxsound_directory_by_someone_else_keeps_it() {
        let dir = fxsound_core::test_support::ScratchDir::new("wp-hook");
        let place = Place::under(&dir.path().join("config"), &dir.path().join("data"));
        place.install().expect("the hook installs");
        let other = place.script().with_file_name("mine.lua");
        std::fs::write(&other, "-- the user's\n").expect("a script of the user's");
        place.remove().expect("the hook is taken away");
        assert!(other.exists());
    }

    #[test]
    fn a_process_start_is_read_from_its_stat_and_the_uptime() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Started 1234.56 s after boot, which was 2000.00 s ago: 765.44 s ago.
        let stat = "1958 (wire plumber) S 1 1958 1958 0 -1 4194560 1 0 0 0 1 1 0 0 9 -11 5 0 \
                    123456 1 2 3";
        let start = started_at(stat, "2000.00 7777.00\n", now).expect("a start");
        let ago = now
            .duration_since(start)
            .expect("in the past")
            .as_secs_f64();
        assert!((ago - 765.44).abs() < 0.001, "{ago}");
        assert_eq!(started_at("garbage", "2000.00 1.00", now), None);
        assert_eq!(started_at(stat, "", now), None);
        // A start after now is not a start.
        assert_eq!(started_at(stat, "1000.00 1.00", now), None);
    }

    #[test]
    fn the_script_fades_before_the_link_is_broken_and_gives_back_after_the_new_one() {
        // The names of WirePlumber 0.5's own hooks the script sits between, and what makes it
        // harmless where they are not.
        for needle in [
            "before = \"linking/prepare-link\"",
            "after = \"linking/link-target\"",
            "if not AsyncEventHook or not SimpleEventHook then",
            "volumeRampTime",
            "volumeRampStepSamples",
        ] {
            assert!(SCRIPT.contains(needle), "{needle}");
        }
        // Optional, never required: a component WirePlumber requires stops it when it fails.
        assert!(FRAGMENT.contains("wants = [ custom.fxsound.fade-on-move ]"));
        assert!(FRAGMENT.contains("provides = custom.fxsound.fade-on-move"));
        assert!(!FRAGMENT.contains("custom.fxsound.fade-on-move = required"));
        // Licensed as the rest of the tree, since it lands outside it.
        assert!(SCRIPT.contains("SPDX-License-Identifier: AGPL-3.0-or-later"));
    }
}
