# Fedora packaging for FxSound for Linux (Fedora 43 and newer, and rawhide).
#
# Not older: Cargo.toml sets edition = "2024" and rust-version = "1.98.1", and Fedora 43 is the
# oldest release whose `rust` package is 1.98.1.
#
# Build it with mock or rpmbuild; see packaging/fedora/README.md for the two commands and for
# how to feed this spec to COPR. The vendor tarball referenced as Source1 is not in the git
# tree — the README says how to produce it.
#
#
# WHY VENDORED DEPENDENCIES, AND WHY THE rust-packaging MACROS
#
# A Fedora reviewer will ask both questions, so: the dependency tree this application links is
# not available as packaged crates. eframe/egui 0.36, pipewire 0.10, ksni, nnnoiseless, rfd and
# resvg have no rust-*-devel packages in Fedora, so %%cargo_generate_buildrequires against
# %%{cargo_registry} cannot resolve and the package simply would not build. The guidelines
# allow an *application* to bundle its Rust dependencies, so this spec does that: `cargo vendor`
# output as Source1, %%cargo_prep -v vendor to point crates-io at it, and %%cargo_vendor_manifest
# to emit the `Provides: bundled(crate(...))` the guidelines require for bundling. (The spec
# stays ready for the other model: drop Source1, change %%cargo_prep, and add back
# %%generate_buildrequires when the crates land in Fedora.)
#
# The rust-packaging macros are used rather than a bare `cargo build` because they are what
# carries Fedora's build flags into the compiler: %%__cargo exports RUSTFLAGS=%%{build_rustflags}
# and CARGO_HOME=.cargo, and %%cargo_prep writes a `rpm` cargo profile with `strip = "none"` and
# debuginfo on, which is what lets the debuginfo and debugsource subpackages be generated at
# all. Reimplementing that by hand is how a package ends up shipping an unhardened, unstrippable
# binary.
#
# The one macro deliberately NOT used is %%cargo_install. It runs `cargo install --path .`, and
# the root of this repository is a *virtual* workspace manifest ([workspace] with six members
# and no [package]), so that call fails with
#
#     error: found a virtual manifest at `.../Cargo.toml` instead of a package manifest
#
# The binary is therefore installed by hand out of the build directory, which this package has
# to have a manual %%install section for anyway: presets, icons, the desktop entry and the
# systemd user unit are not things %%cargo_install knows about.

# Run the workspace test suite during the build. Nothing in it touches the live PipeWire graph,
# the session bus or a user settings file, so it is safe inside a mock chroot. Disable with
# `--without check` for a faster local rebuild; a submission should leave it on.
%bcond check 1

# Upstream calls the project "fxsound-linux": that is the repository name, the Arch package name
# and the directory the release tarballs and /usr/share/doc already use, so the RPM keeps it
# instead of inventing a second name. The executable is plain `fxsound`, and the virtual Provides
# below means `dnf install fxsound` finds this package. Note that the file is checked in as
# fxsound.spec: for a local rpmbuild, mock or COPR build the filename is irrelevant, but Fedora
# dist-git wants <name>.spec, so a submission renames this to fxsound-linux.spec.
Name:           fxsound-linux
Version:        0.3.0
Release:        1%{?dist}
Summary:        System-wide audio enhancement: EQ, ambience, surround, bass and dynamic boost

# The application itself is AGPL-3.0-or-later (see LICENSE). Its Rust dependencies are statically
# linked into the executable, so the effective license of the binary package is the conjunction
# of theirs with ours. The list below is the verbatim output of %%{cargo_license_summary} for the
# 0.3.0 Cargo.lock, that is of
#
#   cargo2rpm --path Cargo.toml license-summary
#
# It has not been re-run since. The 0.4.0 dependency bump (egui/eframe 0.36.2, signal-hook 0.4,
# clap 4.6.7 and the transitive refreshes that came with them) was checked against this list by
# reading the `license` field of every crate in the new Cargo.lock out of the registry cache: no
# crate changed its expression, and the two that dropped out (getrandom 0.2, tinyvec_macros)
# only held expressions other crates still hold. That is a reading, not the macro. Before the
# 0.4.0 tag is cut, re-run the command above and paste its output over the list below; if that
# step is skipped, the %%build printout is what shows the drift.
#
# Running the `cargo tree` underneath it by hand gives three extra lines: cargo2rpm rewrites the
# deprecated "MIT/Apache-2.0" slash form to "MIT OR Apache-2.0" before sorting, and the crates
# that still use it (bitflags 1.x, pollster, signal-hook, siphasher and friends) then collapse
# into entries already in the list. Compare against the macro, not against cargo tree.
#
# # 0BSD OR MIT OR Apache-2.0
# # AGPL-3.0-or-later
# # Apache-2.0
# # Apache-2.0 AND MIT
# # Apache-2.0 OR GPL-2.0-only
# # Apache-2.0 OR MIT
# # Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
# # BlueOak-1.0.0 OR MIT OR Apache-2.0
# # BSD-2-Clause
# # BSD-2-Clause OR Apache-2.0 OR MIT
# # BSD-3-Clause
# # BSD-3-Clause OR Apache-2.0
# # BSD-3-Clause OR MIT OR Apache-2.0
# # BSL-1.0
# # ISC
# # MIT
# # MIT OR Apache-2.0
# # (MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0
# # (MIT OR Apache-2.0) AND Unicode-3.0
# # MIT OR Apache-2.0 OR LGPL-2.1-or-later
# # MIT OR Apache-2.0 OR Zlib
# # MIT OR Zlib OR Apache-2.0
# # MPL-2.0
# # Unlicense
# # Unlicense OR MIT
# # Zlib
# # Zlib OR Apache-2.0 OR MIT
#
# %%build re-runs %%{cargo_license_summary} on every build and prints it to the build log: if it
# stops matching this comment, the dependency tree moved and this tag has to be updated.
# LICENSE.dependencies, shipped in the package, carries the per-crate breakdown.
License:        %{shrink:
    AGPL-3.0-or-later AND
    (0BSD OR Apache-2.0 OR MIT) AND
    Apache-2.0 AND
    (Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT) AND
    (Apache-2.0 OR BlueOak-1.0.0 OR MIT) AND
    (Apache-2.0 OR BSD-2-Clause OR MIT) AND
    (Apache-2.0 OR BSD-3-Clause) AND
    (Apache-2.0 OR BSD-3-Clause OR MIT) AND
    (Apache-2.0 OR GPL-2.0-only) AND
    (Apache-2.0 OR LGPL-2.1-or-later OR MIT) AND
    (Apache-2.0 OR MIT) AND
    (Apache-2.0 OR MIT OR Zlib) AND
    BSD-2-Clause AND
    BSD-3-Clause AND
    BSL-1.0 AND
    ISC AND
    MIT AND
    (MIT OR Unlicense) AND
    MPL-2.0 AND
    OFL-1.1 AND
    Ubuntu-font-1.0 AND
    Unicode-3.0 AND
    Unlicense AND
    Zlib
    }
URL:            https://github.com/blackixxce12/fxsound-linux
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
# `cargo vendor` output for the exact Cargo.lock in Source0. Not generated during the build,
# because an RPM build has no network; the maintainer produces it once per release and uploads
# it alongside the source tarball. packaging/fedora/README.md has the command.
Source1:        %{name}-%{version}-vendor.tar.xz

# %%cargo_prep -v, %%cargo_build and %%cargo_vendor_manifest all need at least version 24.
BuildRequires:  cargo-rpm-macros >= 24
# Cargo.toml declares `edition = "2024"` and `rust-version = "1.98.1"`. Naming the floor here
# turns "this Fedora's rustc is older than the workspace asks for" into a readable dependency
# error instead of a wall of compiler output halfway through the build.
BuildRequires:  rust >= 1.98.1
BuildRequires:  cargo >= 1.98.1
# libpipewire is the only C library the binary actually links (see the Requires comment below).
BuildRequires:  pkgconfig(libpipewire-0.3)
# Not optional and not transitively pulled in by anything else: the pipewire-sys build script
# runs bindgen, which fails with "Unable to find libclang" without it. Builds that appear to
# work without clang-devel are builds on a machine that happens to have clang for other reasons.
BuildRequires:  clang-devel
# %%{_userunitdir} and the %%systemd_user_* scriptlet macros.
BuildRequires:  systemd-rpm-macros
BuildRequires:  desktop-file-utils
# For `appstreamcli validate` in %%check, which the guidelines ask of every package that ships
# metainfo.
BuildRequires:  appstream

# No ExclusiveArch: rust_arches. rpm itself now depends on Rust (rpm-sequoia), so every Fedora
# architecture is a Rust architecture and rust-srpm-macros documents the tag as no longer needed.

# PipeWire is the audio backend, and this is a dependency on the *daemon*: the soname dependency
# on libpipewire-0.3.so.0 that rpm generates automatically only pulls in pipewire-libs.
Requires:       pipewire

# Everything below is opened with dlopen() at run time — by winit for the Wayland and X11
# backends, by glutin for EGL, by rfd for the portal file dialogs, and by fontdb for system
# faces. rpm's automatic dependency generator only sees DT_NEEDED entries, and `ldd` on the
# release binary lists nothing but libc, libm, libgcc_s and libpipewire-0.3, so these have to be
# spelled out by hand or the package installs cleanly and then dies at startup with a dlopen
# error. The list is not guesswork: it is the set of sonames the executable itself names, read
# back out of the built binary with `strings -a target/release/fxsound` and grepped for
# "lib*.so*". Re-run that after a dependency bump; it is the only thing that keeps this block
# honest.
Requires:       libwayland-client
Requires:       libwayland-egl
Requires:       libxkbcommon
Requires:       libglvnd-egl
# No libwayland-cursor: wayland-cursor is a pure-Rust reimplementation of it and the binary
# never names that soname. Same reason there is no libfontconfig and no libdecor.
#
# libdbus-1.so.3, dlopen'd by rfd's XDG desktop portal backend — the native file dialogs behind
# preset import and export. (ksni's tray and notify-rust's notifications go over zbus, which is
# pure Rust and speaks to the bus socket directly, so they need no library here.) Without it rfd
# falls back to shelling out to zenity, which Fedora does not install by default either.
Requires:       dbus-libs
# Not libfontconfig.so — nothing links or dlopens it. This is for /etc/fonts, which the pure-Rust
# fontconfig-parser under fontdb reads to find the faces the SVG icons and labels are drawn with.
Requires:       fontconfig
Requires:       hicolor-icon-theme

# X11 is the fallback backend; the port targets Wayland, so these are weak. libX11-xcb,
# libXrender and libxkbcommon-x11 are named by x11-dl and xkbcommon-dl next to libX11 itself and
# are just as dlopen'd. No libXrandr: winit drives RandR over XCB and the binary never names it.
Recommends:     libX11
Recommends:     libX11-xcb
Recommends:     libXcursor
Recommends:     libXi
Recommends:     libXrender
Recommends:     libxkbcommon-x11
Recommends:     libglvnd-glx
# Claiming the default sink goes through the session manager rather than PipeWire itself.
Recommends:     wireplumber
# Preset import and export use native file dialogs, which are an XDG portal call. The portal
# itself only routes the request: a *backend* has to implement the file chooser, and wlroots' own
# backend does not, so a bare compositor with only xdg-desktop-portal installed shows no dialog
# at all. Any of the three desktop backends will do; the GTK one is the smallest and works under
# every compositor. debian/control and the PKGBUILD name the same three.
Recommends:     xdg-desktop-portal
Recommends:     (xdg-desktop-portal-gtk or xdg-desktop-portal-kde or xdg-desktop-portal-gnome)

Provides:       fxsound = %{version}-%{release}

%description
FxSound for Linux is a Rust/egui port of the Windows FxSound audio enhancer. It inserts itself
into the PipeWire graph as a virtual sink, so everything the system plays passes through its
DSP chain: a 10-band equalizer plus ambience, surround, dynamic boost and bass effects, with
the factory and bonus presets of the original. A second, input-side chain does the same for a
microphone, with a noise gate and RNNoise denoising.

It runs as a tray application, can claim the default sink for each direction on its own, and
ships a systemd user unit for starting with the session.


%prep
%autosetup -n %{name}-%{version} -p1 -a1
# -v vendor points [source.crates-io] at the tree Source1 just unpacked and puts cargo in
# offline mode. It also leaves Cargo.lock alone: %%cargo_prep only deletes the lock file in its
# system-registry mode, and a vendored build needs it to resolve the same versions that were
# vendored.
%cargo_prep -v vendor


%build
%cargo_build
# Printed into the build log so the License tag above can be checked against reality, and
# written out per crate so the package can ship the breakdown.
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies
# Writes cargo-vendor.txt. Shipping that file under %%{_licensedir} (see %%files) is what makes
# rpm's cargo_vendor file attribute turn it into Provides: bundled(crate(...)) entries.
%cargo_vendor_manifest


%install
# See the header for why this is not %%cargo_install. %%cargo_build builds with `--profile rpm`,
# so the executable is target/rpm/fxsound; %%cargo_prep also leaves target/release as a symlink
# to target/rpm, and either path is the same file.
install -Dpm0755 target/rpm/fxsound %{buildroot}%{_bindir}/fxsound

# Factory and bonus presets. PresetStore::with_default_dirs
# (crates/fxsound-preset/src/store.rs) looks under %%{_datadir}/fxsound/presets when the binary
# is installed rather than run out of a build tree. Only the .fac files: the upstream
# BonusPresets folder also carries a zip of the same presets and a stray text file called
# MeaningfulPresets, and neither belongs in a package.
for dir in Factsoft BonusPresets; do
    install -Dpm0644 assets/presets/${dir}/*.fac \
        -t %{buildroot}%{_datadir}/fxsound/presets/${dir}
done

# The microphone presets are TOML rather than .fac, because they describe the input chain and a
# .fac has nowhere to put a gate threshold.
install -Dpm0644 assets/presets/Input/*.toml \
    -t %{buildroot}%{_datadir}/fxsound/presets/Input

# Installed under the reverse-DNS name the window's app_id also uses, so the compositor matches
# the window to this entry (StartupWMClass=com.fxsound.FxSound).
install -Dpm0644 packaging/fxsound.desktop \
    %{buildroot}%{_datadir}/applications/com.fxsound.FxSound.desktop

# So that `systemctl --user enable --now fxsound` works on an installed package instead of
# failing with "Unit not found".
install -Dpm0644 packaging/fxsound.service \
    %{buildroot}%{_userunitdir}/fxsound.service

# 256x256 and 32x32 respectively; both verified against the files, not assumed from the names.
install -Dpm0644 assets/images/fxsound_large.png \
    %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/fxsound.png
install -Dpm0644 assets/images/fxsound.png \
    %{buildroot}%{_datadir}/icons/hicolor/32x32/apps/fxsound.png

# The tray icon's three states. A StatusNotifier host resolves an icon *name* through the theme,
# so without these under hicolor's status context the tray shows a blank where the icon should be.
install -Dpm0644 assets/icons/status/com.fxsound.FxSound-*.svg \
    -t %{buildroot}%{_datadir}/icons/hicolor/scalable/status

# The unit says Documentation=man:fxsound(1); shipping the page is what makes that line true.
# rpm compresses it, hence the glob in %%files.
install -Dpm0644 packaging/fxsound.1 %{buildroot}%{_mandir}/man1/fxsound.1

# AppStream metadata, validated in %%check.
install -Dpm0644 packaging/com.fxsound.FxSound.metainfo.xml \
    %{buildroot}%{_metainfodir}/com.fxsound.FxSound.metainfo.xml


%check
desktop-file-validate %{buildroot}%{_datadir}/applications/com.fxsound.FxSound.desktop
appstreamcli validate --no-net %{buildroot}%{_metainfodir}/com.fxsound.FxSound.metainfo.xml
%if %{with check}
%cargo_test
%endif


# The unit is a *user* unit, so the user-scoped scriptlets. No %%systemd_requires: the macros
# guard on /usr/lib/systemd/systemd-update-helper being executable, and systemd's own file
# triggers handle the daemon reload.
#
# Deliberately no %%systemd_user_postun_with_restart: a running instance owns the virtual sink
# and the session's default-sink assignment, and restarting it mid-session to pick up a package
# upgrade would cut audio out from under whatever is playing. The user restarts it when they
# choose to.
%post
%systemd_user_post fxsound.service

%preun
%systemd_user_preun fxsound.service


%files
%license LICENSE
%license LICENSE.dependencies
%license cargo-vendor.txt
%doc README.md
%doc CHANGELOG.md
# A Wayland client cannot grab global shortcuts, so the keybindings live in the compositor
# configuration and call back into the running instance; these two are the worked examples.
%doc packaging/hyprland.conf.example
%doc packaging/fxsound-autostart.desktop

%{_bindir}/fxsound
%{_datadir}/applications/com.fxsound.FxSound.desktop
%dir %{_datadir}/fxsound
%dir %{_datadir}/fxsound/presets
%{_datadir}/fxsound/presets/Factsoft/
%{_datadir}/fxsound/presets/BonusPresets/
%{_datadir}/fxsound/presets/Input/
%{_datadir}/icons/hicolor/256x256/apps/fxsound.png
%{_datadir}/icons/hicolor/32x32/apps/fxsound.png
%{_datadir}/icons/hicolor/scalable/status/com.fxsound.FxSound-off.svg
%{_datadir}/icons/hicolor/scalable/status/com.fxsound.FxSound-on.svg
%{_datadir}/icons/hicolor/scalable/status/com.fxsound.FxSound-processing.svg
%{_mandir}/man1/fxsound.1*
%{_metainfodir}/com.fxsound.FxSound.metainfo.xml
%{_userunitdir}/fxsound.service


%changelog
* Mon Sep 21 2026 FxSound Linux port contributors <blackixxce12@users.noreply.github.com> - 0.3.0-1
- Initial Fedora packaging, for the 0.3.0 release
