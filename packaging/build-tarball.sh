#!/usr/bin/env bash
# A distribution-agnostic binary tarball: everything an install needs, and a script to place it.
#
# For someone whose distribution is not Arch, Debian or Fedora, or who does not want a package
# manager involved. It installs under /usr/local by default, which is where a hand-installed
# program belongs and which no package manager will fight over.
#
#   packaging/build-tarball.sh [version]
#
# Produces dist/fxsound-linux-<version>-<arch>.tar.gz beside the repository. This is also what
# the release workflow attaches: it used to assemble its own archive inline, with a different
# desktop-file name and doc directory, and the -bin AUR recipe was written against that one —
# so there is exactly one layout now, this script's, and it is the layout packaging/PKGBUILD
# installs, minus the /usr prefix:
#
#   bin/fxsound
#   lib/systemd/user/fxsound.service
#   share/applications/com.fxsound.FxSound.desktop
#   share/icons/hicolor/{256x256,32x32}/apps/fxsound.png
#   share/icons/hicolor/scalable/status/com.fxsound.FxSound-{off,on,processing}.svg
#   share/fxsound/presets/{Factsoft,BonusPresets}/*.fac  share/fxsound/presets/Input/*.toml
#   share/man/man1/fxsound.1
#   share/metainfo/com.fxsound.FxSound.metainfo.xml
#   share/doc/fxsound-linux/{README.md,CHANGELOG.md,LICENSE,hyprland.conf.example,fxsound-autostart.desktop}
#   install.sh
#
# The binary is linked against the glibc of the machine that builds it and runs on that release
# and newer. CI builds it on debian:bookworm for that reason; a local build is for local use.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The first `version =` line of Cargo.toml is [workspace.package]'s; no dependency line starts
# with the word, which is what keeps this one-liner honest.
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "${root}/Cargo.toml" | head -1)}"
arch="$(uname -m)"
stage="$(mktemp -d)"
name="fxsound-linux-${version}-${arch}"
trap 'rm -rf "${stage}"' EXIT

cd "${root}"
[ -x target/release/fxsound ] || cargo build --release --bin fxsound

pkg="${stage}/${name}"
install -Dm755 target/release/fxsound "${pkg}/bin/fxsound"
# Named after the window's app_id (StartupWMClass=com.fxsound.FxSound), so the compositor can
# match the window to the entry — the same rename every package does.
install -Dm644 packaging/fxsound.desktop "${pkg}/share/applications/com.fxsound.FxSound.desktop"
# Installed as written, with ExecStart=/usr/bin/fxsound; install.sh below rewrites that for the
# prefix it is given, because a unit that points at /usr/bin on a /usr/local install starts nothing.
install -Dm644 packaging/fxsound.service "${pkg}/lib/systemd/user/fxsound.service"
# The two icon sizes the tree actually carries, at the paths packaging/PKGBUILD uses.
install -Dm644 assets/images/fxsound_large.png \
  "${pkg}/share/icons/hicolor/256x256/apps/fxsound.png"
install -Dm644 assets/images/fxsound.png \
  "${pkg}/share/icons/hicolor/32x32/apps/fxsound.png"
# The tray icon's three states, resolved by name through the theme; without them the tray shows
# a blank where the icon should be.
install -Dm644 assets/icons/status/com.fxsound.FxSound-{off,on,processing}.svg \
  -t "${pkg}/share/icons/hicolor/scalable/status/"
# Both preset trees: the .fac files the Windows build shares, and the TOML voice presets.
for dir in Factsoft BonusPresets; do
  install -d "${pkg}/share/fxsound/presets/${dir}"
  install -Dm644 assets/presets/"${dir}"/*.fac -t "${pkg}/share/fxsound/presets/${dir}/"
done
install -d "${pkg}/share/fxsound/presets/Input"
install -Dm644 assets/presets/Input/*.toml -t "${pkg}/share/fxsound/presets/Input/"
# The unit says Documentation=man:fxsound(1); shipping the page is what makes that true.
install -Dm644 packaging/fxsound.1 "${pkg}/share/man/man1/fxsound.1"
install -Dm644 packaging/com.fxsound.FxSound.metainfo.xml \
  "${pkg}/share/metainfo/com.fxsound.FxSound.metainfo.xml"
# share/doc/fxsound-linux, not share/doc/fxsound: the package name, as on every distribution.
install -Dm644 README.md CHANGELOG.md LICENSE \
  packaging/hyprland.conf.example packaging/fxsound-autostart.desktop \
  -t "${pkg}/share/doc/fxsound-linux/"

cat > "${pkg}/install.sh" <<'INNER'
#!/usr/bin/env sh
# Copy this tree into a prefix. Default /usr/local; pass another as the first argument.
set -eu
prefix="${1:-/usr/local}"
here="$(cd "$(dirname "$0")" && pwd)"

case "${prefix}" in
  /usr|/usr/local) ;;
  *)
    # Not a refusal, because there are reasons to put a binary elsewhere — but the presets will
    # not be found there: the binary searches exactly /usr/share/fxsound and
    # /usr/local/share/fxsound (crates/fxsound-preset/src/store.rs), not $XDG_DATA_DIRS, and
    # systemd --user does not look for units under an arbitrary prefix either.
    echo "warning: FxSound only looks for its presets under /usr and /usr/local; installed" >&2
    echo "         under ${prefix} it will start with an empty preset list, and systemd will" >&2
    echo "         not find ${prefix}/lib/systemd/user/fxsound.service." >&2
    ;;
esac

echo "installing FxSound into ${prefix}"
mkdir -p "${prefix}"
cp -a "${here}/bin" "${here}/share" "${prefix}/"
[ -d "${here}/lib" ] && cp -a "${here}/lib" "${prefix}/"

# The unit ships with the path every package uses. Point it at the binary that was just
# installed; on a /usr prefix this changes nothing.
unit="${prefix}/lib/systemd/user/fxsound.service"
if [ -f "${unit}" ] && [ "${prefix}" != /usr ]; then
  sed -i "s|/usr/bin/fxsound|${prefix}/bin/fxsound|g" "${unit}"
fi

echo "done. Run 'fxsound', or 'systemctl --user enable --now fxsound' to start it at login."
echo "FxSound needs a running PipeWire session; it does not replace one."
INNER
chmod 755 "${pkg}/install.sh"

mkdir -p "${root}/dist"
# root-owned entries, so an unpack as root does not hand the tree to whatever uid built it.
tar -C "${stage}" --owner=0 --group=0 --numeric-owner -czf "${root}/dist/${name}.tar.gz" "${name}"
printf '%s\n' "${root}/dist/${name}.tar.gz"
