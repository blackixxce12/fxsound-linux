# Fedora packaging

`fxsound.spec` builds FxSound for Linux on Fedora 43 and newer. It installs exactly what the
Arch package in `packaging/PKGBUILD` installs: the binary, the `.fac` factory and bonus presets,
the TOML input presets, the desktop entry, both icon sizes and the systemd **user** unit.

The Rust dependencies are vendored — `eframe`/`egui` 0.36, `pipewire`, `ksni`, `nnnoiseless`,
`rfd` and `resvg` are not packaged as crates in Fedora, so a system-registry build cannot
resolve. The spec header explains the choice in full; a reviewer will ask.

## Build

`Source1` is a `cargo vendor` tarball. It is not in git and an RPM build has no network, so
produce it once per release, then build:

```bash
VER=0.3.0
mkdir -p ~/rpmbuild/SOURCES ~/rpmbuild/SPECS

# Source0: the release tarball
curl -L -o ~/rpmbuild/SOURCES/fxsound-linux-$VER.tar.gz \
    https://github.com/blackixxce12/fxsound-linux/archive/v$VER/fxsound-linux-$VER.tar.gz

# Source1: the vendored crates, from that exact tarball's Cargo.lock
tar xf ~/rpmbuild/SOURCES/fxsound-linux-$VER.tar.gz -C /tmp
cd /tmp/fxsound-linux-$VER
cargo vendor --locked vendor
tar caf ~/rpmbuild/SOURCES/fxsound-linux-$VER-vendor.tar.xz vendor

# Build
cp packaging/fedora/fxsound.spec ~/rpmbuild/SPECS/
rpmbuild -ba ~/rpmbuild/SPECS/fxsound.spec
```

`--without check` skips the workspace test suite if you want a faster rebuild.

## Build in mock

Clean-chroot build, which is what COPR will do and the only build that proves the
`BuildRequires` are complete:

```bash
rpmbuild -bs ~/rpmbuild/SPECS/fxsound.spec
mock -r fedora-43-x86_64 ~/rpmbuild/SRPMS/fxsound-linux-$VER-1.fc*.src.rpm
```

The chroot needs `rust >= 1.98.1` (the workspace sets `edition = "2024"` and that
`rust-version`). Fedora 43 is the oldest release that ships it — 41 and 42 are too old, and the
spec's `BuildRequires: rust >= 1.98.1` will say so instead of failing halfway through a build.

## COPR

Simplest path, because the vendor tarball only exists locally — upload the SRPM:

```bash
copr-cli create fxsound-linux --chroot fedora-rawhide-x86_64 --chroot fedora-43-x86_64
copr-cli build fxsound-linux ~/rpmbuild/SRPMS/fxsound-linux-$VER-1.fc*.src.rpm
```

To let COPR rebuild from git instead, attach `fxsound-linux-$VER-vendor.tar.xz` to the GitHub
release and change `Source1` to
`%{url}/releases/download/v%{version}/%{name}-%{version}-vendor.tar.xz`; COPR's SCM method can
then fetch both sources itself.

## Status

This spec has never been fed to `rpmbuild`, `rpmspec` or `mock` — none of them exist on the
machine it was written on, so there is not even a `rpmspec --parse` behind it. What *has* been
checked, by running it rather than by reading:

- every path and glob `%install` touches (15 `Factsoft/*.fac`, 19 `BonusPresets/*.fac`,
  10 `Input/*.toml`, both icons at the sizes the hicolor paths claim, all six doc files);
- the macro behaviour, against `cargo-rpm-macros` 28.5 itself: `%cargo_prep -v <dir>` keeps
  `Cargo.lock`, writes `[net] offline`, creates `target/rpm` and symlinks `target/release` to
  it, and `%cargo_build` builds `--profile rpm` — hence `target/rpm/fxsound` in `%install`;
- `%cargo_license_summary`, `%cargo_license` and `%cargo_vendor_manifest`, by running
  `cargo2rpm` 0.4.0 against this workspace. The `License:` tag's comment block is that tool's
  output verbatim;
- that `%cargo_install` cannot be used here: `cargo install --path .` on this tree fails with
  `found a virtual manifest ... instead of a package manifest`;
- the dlopen dependency list, read out of the built binary's own soname strings.

The first real `mock` run is still the one that matters. The likeliest spot to fail is `%check`:
the workspace test suite has not been run headless in a chroot here.
