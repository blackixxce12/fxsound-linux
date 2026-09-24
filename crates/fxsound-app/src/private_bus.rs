//! A `dbus-daemon` of the tests' own, for the D-Bus service ([`crate::dbus`]) and the suspend
//! watcher ([`crate::sleep`]). Never the session's bus, never the system's: a test that reached
//! either would be driving — or muting — the FxSound the user is listening to.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, ChildStdout, Stdio};
use std::time::Duration;

/// A private bus, killed when dropped.
pub(crate) struct PrivateBus {
    daemon: Child,
    /// What a client connects to.
    pub(crate) address: String,
    /// Kept open: the daemon has nowhere to write otherwise.
    _stdout: BufReader<ChildStdout>,
    /// The bus's configuration and socket, removed with it.
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    /// A bus with the session bus's rules and none of its service files. `None`, after saying
    /// so, where `dbus-daemon` is not installed.
    ///
    /// Not `dbus-daemon --session`: that configuration reads `/usr/share/dbus-1/services`, where a
    /// FxSound package installs `org.fxsound.FxSound.service`, and a call to the name after the
    /// test's own service let go of it would start the installed FxSound — with this process's
    /// environment, on the session's PipeWire. Here a call nobody owns the name for just fails.
    pub(crate) fn start() -> Option<Self> {
        Self::typed("session")
    }

    /// A bus of the system's type, as logind speaks on — `<type>system</type>` — listening in a
    /// scratch directory rather than on `/run/dbus/system_bus_socket`, and with a policy that lets
    /// a test own `org.freedesktop.login1`, which the real system bus reserves for root. `None`,
    /// after saying so, where `dbus-daemon` is not installed.
    pub(crate) fn start_system_like() -> Option<Self> {
        Self::typed("system")
    }

    /// A bus of type `kind` in a scratch directory, anyone allowed to own any name, and no
    /// service directory: nothing is ever activated.
    fn typed(kind: &str) -> Option<Self> {
        let dir = scratch_dir();
        let config = dir.path().join(format!("{kind}-like.conf"));
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>{kind}</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().display()
            ),
        )
        .expect("write the bus configuration");
        let flag = format!("--config-file={}", config.display());
        Self::spawn(&[flag.as_str()], dir)
    }

    fn spawn(config: &[&str], dir: tempfile::TempDir) -> Option<Self> {
        let spawned = std::process::Command::new("dbus-daemon")
            .args(config)
            .args(["--print-address=1", "--nofork"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut daemon = match spawned {
            Ok(daemon) => daemon,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping: dbus-daemon is not installed");
                return None;
            }
            Err(err) => panic!("dbus-daemon did not start: {err}"),
        };
        let mut stdout = BufReader::new(daemon.stdout.take().expect("piped"));
        let mut address = String::new();
        stdout.read_line(&mut address).expect("the bus address");
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Some(Self {
            daemon,
            address,
            _stdout: stdout,
            _dir: dir,
        })
    }

    /// A blocking connection to the bus, for a test to call or signal from.
    pub(crate) fn client(&self) -> zbus::blocking::Connection {
        zbus::blocking::connection::Builder::address(self.address.as_str())
            .expect("an address")
            .method_timeout(Duration::from_secs(10))
            .build()
            .expect("a client connection to the private bus")
    }

    /// Whether `name` is owned on the bus, asked through `client`.
    pub(crate) fn has_owner(&self, client: &zbus::blocking::Connection, name: &str) -> bool {
        let _ = self;
        zbus::blocking::fdo::DBusProxy::new(client)
            .expect("the bus's own proxy")
            .name_has_owner(name.try_into().expect("a bus name"))
            .expect("NameHasOwner")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// A scratch directory short enough to hold a socket: a Unix socket's path has room for about
/// a hundred bytes, which a deep `TMPDIR` can use up on its own.
fn scratch_dir() -> tempfile::TempDir {
    let make = |parent: &std::path::Path| {
        tempfile::Builder::new()
            .prefix("fxs-bus-")
            .tempdir_in(parent)
    };
    match make(&std::env::temp_dir()) {
        Ok(dir) if dir.path().as_os_str().len() <= 64 => dir,
        _ => make(std::path::Path::new("/tmp")).expect("a scratch directory for the bus"),
    }
}
