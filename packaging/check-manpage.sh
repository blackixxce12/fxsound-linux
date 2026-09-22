#!/usr/bin/env bash
# Refuses a manual page that has drifted from the code it documents.
#
# packaging/fxsound.1 is written by hand: its OPTIONS follow crates/fxsound-app/src/cli.rs and
# its D-BUS section follows crates/fxsound-app/src/dbus.rs, and the page's own header says so.
# Nothing enforced it, and a page written to the design before the code landed is exactly the
# page that drifts. So, mechanically:
#
#   - every --option the OPTIONS section names must be a long name or an alias in cli.rs;
#   - every long name and visible alias cli.rs declares (what --help shows) must be in the
#     OPTIONS section;
#   - every bus name, object path and member (method, property, signal) the D-BUS section
#     names must appear in dbus.rs: a #[zbus(name = "...")] rename by its literal name, a plain
#     fn by the snake_case name zbus derives the member from.
#
# ci.yml runs it on every push and release.yml's verify job before anything is built. It reads
# the sources only, so it needs no toolchain. Exit 0 when the page and the code agree.
#
#   packaging/check-manpage.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
man="${root}/packaging/fxsound.1"
cli="${root}/crates/fxsound-app/src/cli.rs"
dbus="${root}/crates/fxsound-app/src/dbus.rs"
fail=0

ok()    { printf '  ok     %s\n' "$*"; }
wrong() { printf '  WRONG  %s\n' "$*"; fail=1; }

# ---------------------------------------------------------------- options
# The OPTIONS section only: EXAMPLES quotes busctl, jq and systemctl options that are not ours.
# The page escapes hyphens as \-; strip that and the leading -- to compare bare names. Every
# pipeline ends in `|| true` because a grep that finds nothing is an answer, not an error.
documented=$(sed -n '/^\.SH OPTIONS/,/^\.SH /p' "${man}" \
  | grep -o '\\-\\-[A-Za-z0-9_\\-]*' | sed 's/\\-/-/g; s/^--//' | sort -u || true)
# Everything clap accepts: long names, hidden and visible aliases, and its own -h/-V from
# #[command(version, ...)].
accepted=$({
  grep -oE '\b(long|alias|visible_alias) = "[^"]+"' "${cli}" | sed 's/.*"\(.*\)"/\1/' || true
  grep -oE '\baliases = \[[^]]*\]' "${cli}" | grep -o '"[^"]*"' | tr -d '"' || true
  printf 'help\nversion\n'
} | sort -u)
# What --help shows, which is what the page has to cover.
shown=$({
  grep -oE '\b(long|visible_alias) = "[^"]+"' "${cli}" | sed 's/.*"\(.*\)"/\1/' || true
  printf 'help\nversion\n'
} | sort -u)

for opt in ${documented}; do
  if printf '%s\n' "${accepted}" | grep -qx -- "${opt}"; then
    ok "--${opt}"
  else
    wrong "--${opt}: documented in fxsound.1, not declared in cli.rs"
  fi
done
for opt in ${shown}; do
  if ! printf '%s\n' "${documented}" | grep -qx -- "${opt}"; then
    wrong "--${opt}: declared in cli.rs, not documented in fxsound.1"
  fi
done

# ---------------------------------------------------------------- D-Bus
if [ ! -f "${dbus}" ]; then
  wrong "fxsound.1 documents a D-Bus interface; crates/fxsound-app/src/dbus.rs does not exist"
else
  section=$(sed -n '/^\.SH D\\-BUS/,/^\.SH /p' "${man}")
  for literal in org.fxsound.FxSound com.fxsound.FxSound /org/fxsound/FxSound; do
    if grep -qF -- "${literal}" "${dbus}"; then
      ok "${literal}"
    else
      wrong "${literal}: named in fxsound.1, absent from dbus.rs"
    fi
  done
  # Members are the PascalCase words the section follows with "(": methods in the .EX block as
  # `Name(`, signals as `.BR Name (sig)`, properties as `.BR Name " (type), "`.
  members=$(printf '%s\n' "${section}" \
    | grep -oE '\b[A-Z][A-Za-z0-9]+( " | )?\(' | sed 's/[ "(]*$//' | sort -u || true)
  for member in ${members}; do
    snake=$(printf '%s' "${member}" | sed 's/\([A-Z]\)/_\L\1/g; s/^_//')
    if grep -qE "fn ${snake}\b|\"${member}\"" "${dbus}"; then
      ok "${member}"
    else
      wrong "${member}: in fxsound.1, but dbus.rs has neither fn ${snake} nor \"${member}\""
    fi
  done
fi

if [ "${fail}" -ne 0 ]; then
  echo "packaging/fxsound.1 disagrees with the code; it is synced by hand, and the WRONG lines say where" >&2
  exit 1
fi
