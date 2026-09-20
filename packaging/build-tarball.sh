#!/usr/bin/env bash
# A distribution-agnostic binary tarball: everything an install needs, and a script to place it.
#
# For someone whose distribution is not Arch, Debian or Fedora, or who does not want a package
# manager involved. It installs under /usr/local by default, which is where a hand-installed
# program belongs and which no package manager will fight over.
#
#   packaging/build-tarball.sh [version]
#
# Produces dist/fxsound-<version>-x86_64.tar.gz beside the repository.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "${root}/Cargo.toml" | head -1)}"
arch="$(uname -m)"
stage="$(mktemp -d)"
name="fxsound-linux-${version}-${arch}"
trap 'rm -rf "${stage}"' EXIT

cd "${root}"
[ -x target/release/fxsound ] || cargo build --release --bin fxsound

pkg="${stage}/${name}"
install -Dm755 target/release/fxsound "${pkg}/bin/fxsound"
install -Dm644 packaging/fxsound.desktop "${pkg}/share/applications/fxsound.desktop"
install -Dm644 packaging/fxsound.service "${pkg}/lib/systemd/user/fxsound.service"
# The two icon sizes the tree actually carries, at the paths packaging/PKGBUILD uses.
install -Dm644 assets/images/fxsound_large.png \
  "${pkg}/share/icons/hicolor/256x256/apps/fxsound.png"
install -Dm644 assets/images/fxsound.png \
  "${pkg}/share/icons/hicolor/32x32/apps/fxsound.png"
# Both preset trees: the .fac files the Windows build shares, and the TOML voice presets.
for dir in Factsoft BonusPresets; do
  install -d "${pkg}/share/fxsound/presets/${dir}"
  install -Dm644 assets/presets/"${dir}"/*.fac -t "${pkg}/share/fxsound/presets/${dir}/"
done
install -d "${pkg}/share/fxsound/presets/Input"
install -Dm644 assets/presets/Input/*.toml -t "${pkg}/share/fxsound/presets/Input/"
install -Dm644 README.md CHANGELOG.md LICENSE -t "${pkg}/share/doc/fxsound/"

cat > "${pkg}/install.sh" <<'INNER'
#!/usr/bin/env sh
# Copy this tree into a prefix. Default /usr/local; pass another as the first argument.
set -eu
prefix="${1:-/usr/local}"
here="$(cd "$(dirname "$0")" && pwd)"
echo "installing FxSound into ${prefix}"
mkdir -p "${prefix}"
cp -a "${here}/bin" "${here}/share" "${prefix}/"
[ -d "${here}/lib" ] && cp -a "${here}/lib" "${prefix}/"
echo "done. Run 'fxsound', or 'systemctl --user enable --now fxsound' to start it at login."
echo "FxSound needs a running PipeWire session; it does not replace one."
INNER
chmod 755 "${pkg}/install.sh"

mkdir -p "${root}/dist"
tar -C "${stage}" -czf "${root}/dist/${name}.tar.gz" "${name}"
printf '%s\n' "${root}/dist/${name}.tar.gz"
