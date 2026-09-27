//! What the README and the manual tell a script to type has to do what they say it does.
//!
//! The one case held here was found by the 0.4.0 live check on a private session bus: both told
//! a status bar to read FxSound's `Power` property with `busctl --user --auto-start=no
//! get-property …` so that polling would not start FxSound again after the user quit it. On
//! systemd 262 `busctl` honours `--auto-start=no` for `call` only; `get-property` and `introspect`
//! send their message without `NO_AUTO_START`, and the bus started FxSound on the first poll. The
//! documented read is a `call` of `org.freedesktop.DBus.Properties.Get`, which does carry the flag.

use std::fs;
use std::path::{Path, PathBuf};

/// The repository this test binary was built from.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root exists")
}

/// A document as a reader sees its commands: the manual's `\-` read as `-`, a line continued with
/// a backslash joined to the next.
fn commands_of(relative: &str) -> String {
    let path = repo().join(relative);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
    text.replace("\\-", "-")
        .replace("\\e\n", " ")
        .replace("\\\n", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_documented_way_to_read_a_property_never_starts_fxsound() {
    for document in ["README.md", "packaging/fxsound.1"] {
        let commands = commands_of(document);
        for ignores_the_flag in ["--auto-start=no get-property", "--auto-start=no introspect"] {
            assert!(
                !commands.contains(ignores_the_flag),
                "{document} tells a poller to run `busctl {ignores_the_flag}`, which starts FxSound"
            );
        }
        assert!(
            commands.contains(
                "busctl --user --auto-start=no call org.fxsound.FxSound /org/fxsound/FxSound \
                 org.freedesktop.DBus.Properties Get ss org.fxsound.FxSound Power"
            ),
            "{document} no longer shows how to read a property without starting FxSound"
        );
    }
}
