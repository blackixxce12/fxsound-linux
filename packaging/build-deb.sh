#!/bin/sh
#
# Build a .deb from an already-built fxsound binary using only ar, tar and gzip.
#
# A .deb is not a dpkg-specific container. It is an ar archive with exactly
# three members, in this order:
#
#     debian-binary     uncompressed, the four bytes "2.0\n"
#     control.tar.gz    the package metadata: ./control, ./md5sums, maintainer
#                       scripts
#     data.tar.gz       the filesystem tree, rooted at ./
#
# That is the whole format, and it is reproducible without any part of dpkg
# being installed. This script exists because the machine this packaging was
# written on has neither dpkg-deb nor debhelper, and because a CI runner or a
# developer on Arch or Fedora should still be able to hand someone a .deb.
#
# What this is NOT: a substitute for dpkg-buildpackage. In particular it runs
# no dpkg-shlibdeps, so the library dependencies it writes carry no version
# bounds, and it produces no source package and no dbgsym package. For an
# upload, or for anything that has to satisfy Debian policy in full, use
# debian/rules the ordinary way — see packaging/debian/README.Debian.
#
# The metadata is not duplicated here: the version comes from
# debian/changelog and the dependency fields come from debian/control, so the
# two cannot drift apart without this script noticing.
#
# Copyright (C) 2026 FxSound Linux port contributors
# Licensed under the GNU Affero General Public License, version 3 or later.

set -eu

PROG="build-deb.sh"

# ---------------------------------------------------------------------------
# Where things are
# ---------------------------------------------------------------------------

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$script_dir/.." && pwd)
DEBIAN_DIR="$REPO/packaging/debian"

BINARY="$REPO/target/release/fxsound"
OUTDIR="$REPO/dist"
ARCH=""
STRIP=1
STRICT=0
KEEP=0

usage() {
    cat <<USAGE
Usage: $PROG [options]

Assemble a .deb from a built fxsound binary using ar, tar and gzip only.

Options:
  --binary PATH     the fxsound executable to package
                    (default: target/release/fxsound)
  --output DIR      where to write the .deb (default: dist/)
  --arch ARCH       Debian architecture name; guessed from the machine when
                    dpkg is absent (amd64, arm64, ...)
  --no-strip        keep the binary's symbols; the package is much larger
  --strict          treat a version disagreement between debian/changelog and
                    Cargo.toml as an error instead of a warning
  --keep-staging    leave the staging tree behind for inspection
  -h, --help        this text

The version, the dependencies and the description all come from
packaging/debian/changelog and packaging/debian/control. Edit those, not this
script.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BINARY=$2; shift 2 ;;
        --binary=*) BINARY=${1#*=}; shift ;;
        --output) OUTDIR=$2; shift 2 ;;
        --output=*) OUTDIR=${1#*=}; shift ;;
        --arch) ARCH=$2; shift 2 ;;
        --arch=*) ARCH=${1#*=}; shift ;;
        --no-strip) STRIP=0; shift ;;
        --strict) STRICT=1; shift ;;
        --keep-staging) KEEP=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "$PROG: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

die() { echo "$PROG: $*" >&2; exit 1; }
warn() { echo "$PROG: warning: $*" >&2; }
note() { echo "$PROG: $*"; }

for tool in ar tar gzip; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required and was not found"
done

[ -f "$DEBIAN_DIR/control" ] || die "$DEBIAN_DIR/control is missing"
[ -f "$DEBIAN_DIR/changelog" ] || die "$DEBIAN_DIR/changelog is missing"
[ -f "$BINARY" ] || die "no binary at $BINARY — run: cargo build --release --bin fxsound"
[ -x "$BINARY" ] || die "$BINARY is not executable"

# ---------------------------------------------------------------------------
# Reading debian/control
#
# deb822 is simple enough to parse here and worth parsing rather than
# duplicating: a field is "Name: value" followed by continuation lines that
# begin with whitespace, blank lines separate stanzas, and (in debian/control
# specifically, per Policy 5.1) a line starting with # at column zero is a
# comment and may appear anywhere, including inside a field.
# ---------------------------------------------------------------------------

# control_field <stanza-number> <field-name> [raw]
#   Prints the field's value. Continuation lines are printed one per line with
#   their leading whitespace stripped, unless "raw" is given, in which case
#   they are printed verbatim — which is what Description needs, since a
#   leading space there is part of the format.
control_field() {
    awk -v want_stanza="$1" -v want_field="$2" -v raw="${3:-}" '
        BEGIN { stanza = 1; infield = 0 }
        /^#/ { next }
        /^[ \t]*$/ { stanza++; infield = 0; next }
        stanza != want_stanza { next }
        /^[A-Za-z0-9][A-Za-z0-9-]*:/ {
            name = $0
            sub(/:.*/, "", name)
            if (tolower(name) == tolower(want_field)) {
                infield = 1
                val = $0
                sub(/^[^:]*:[ \t]*/, "", val)
                if (val != "") print val
            } else {
                infield = 0
            }
            next
        }
        infield && /^[ \t]/ {
            if (raw != "") { print; next }
            line = $0
            sub(/^[ \t]+/, "", line)
            print line
        }
    ' "$DEBIAN_DIR/control"
}

# Fold a field that debian/control writes one dependency per line back into the
# single comma-separated line a binary package's control file wants, dropping
# the ${...} substitution variables, which only dpkg-gencontrol can expand.
fold_relations() {
    control_field "$1" "$2" \
        | sed -e 's/,[[:space:]]*$//' -e 's/^[[:space:]]*//' \
        | grep -v '^\${.*}$' \
        | grep -v '^$' \
        | paste -sd ',' - \
        | sed -e 's/,/, /g'
}

SOURCE_PKG=$(control_field 1 Source | head -1)
MAINTAINER=$(control_field 1 Maintainer | head -1)
SECTION=$(control_field 1 Section | head -1)
PRIORITY=$(control_field 1 Priority | head -1)
HOMEPAGE=$(control_field 1 Homepage | head -1)

PACKAGE=$(control_field 2 Package | head -1)
DEPENDS=$(fold_relations 2 Depends)
RECOMMENDS=$(fold_relations 2 Recommends)
SUGGESTS=$(fold_relations 2 Suggests)
PROVIDES=$(fold_relations 2 Provides)
CONFLICTS=$(fold_relations 2 Conflicts)
REPLACES=$(fold_relations 2 Replaces)

[ -n "$PACKAGE" ] || die "could not read Package from debian/control"
[ -n "$MAINTAINER" ] || die "could not read Maintainer from debian/control"

# ---------------------------------------------------------------------------
# Version, from debian/changelog, cross-checked against Cargo.toml
#
# The release workflow refuses a tag that disagrees with Cargo.toml for exactly
# this reason: a version mismatch is silent, and the package ends up named
# after one number while reporting another when asked.
# ---------------------------------------------------------------------------

changelog_head=$(sed -n '1p' "$DEBIAN_DIR/changelog")
FULL_VERSION=$(printf '%s\n' "$changelog_head" | sed -n 's/^[^ ]* *(\([^)]*\)).*/\1/p')
[ -n "$FULL_VERSION" ] || die "could not parse a version out of the first line of debian/changelog"

CHANGELOG_PKG=$(printf '%s\n' "$changelog_head" | sed -n 's/^\([^ ]*\) .*/\1/p')
[ "$CHANGELOG_PKG" = "$SOURCE_PKG" ] \
    || warn "debian/changelog names '$CHANGELOG_PKG' but debian/control names '$SOURCE_PKG'"

# "0.3.0-1" -> "0.3.0"; a native version with no revision is left alone.
UPSTREAM_VERSION=${FULL_VERSION%-*}

cargo_version=$(sed -n '/^\[workspace\.package\]/,/^\[/ {
    s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p
}' "$REPO/Cargo.toml" | head -1)

if [ -n "$cargo_version" ] && [ "$cargo_version" != "$UPSTREAM_VERSION" ]; then
    msg="debian/changelog says $UPSTREAM_VERSION, Cargo.toml says $cargo_version"
    if [ "$STRICT" -eq 1 ]; then
        die "$msg (--strict)"
    fi
    warn "$msg — the .deb will be named after the changelog"
fi

# ---------------------------------------------------------------------------
# Architecture
# ---------------------------------------------------------------------------

if [ -z "$ARCH" ]; then
    if command -v dpkg >/dev/null 2>&1; then
        ARCH=$(dpkg --print-architecture)
    else
        machine=$(uname -m)
        case "$machine" in
            x86_64|amd64) ARCH=amd64 ;;
            aarch64|arm64) ARCH=arm64 ;;
            armv7l|armv7*) ARCH=armhf ;;
            armv6l) ARCH=armel ;;
            i386|i486|i586|i686) ARCH=i386 ;;
            riscv64) ARCH=riscv64 ;;
            ppc64le) ARCH=ppc64el ;;
            s390x) ARCH=s390x ;;
            loongarch64) ARCH=loong64 ;;
            *) die "cannot map machine '$machine' to a Debian architecture; pass --arch" ;;
        esac
    fi
fi

# ---------------------------------------------------------------------------
# Shared-library dependencies
#
# dpkg-shlibdeps would compute these with version bounds from the shlibs and
# symbols files of the packages that own the libraries. None of that exists
# here, so the best available answer is: read the DT_NEEDED entries out of the
# binary and map each soname to the package that owns it, with no version
# bound. Anything unmapped is reported rather than silently dropped.
#
# Note that this only covers what the binary is LINKED against. Almost the whole
# graphics stack is dlopen()ed instead and is invisible to objdump; those
# packages are named by hand in debian/control's Depends and arrive through
# $DEPENDS above.
# ---------------------------------------------------------------------------

soname_to_package() {
    case "$1" in
        libc.so.*|libm.so.*|libdl.so.*|libpthread.so.*|librt.so.*|ld-linux*) echo libc6 ;;
        libgcc_s.so.*) echo libgcc-s1 ;;
        libstdc++.so.*) echo libstdc++6 ;;
        libpipewire-0.3.so.*) echo libpipewire-0.3-0 ;;
        *) echo "" ;;
    esac
}

SHLIB_DEPENDS=""
if command -v objdump >/dev/null 2>&1; then
    needed=$(objdump -p "$BINARY" 2>/dev/null | awk '/NEEDED/ { print $2 }' | sort -u)
    for so in $needed; do
        pkg=$(soname_to_package "$so")
        if [ -z "$pkg" ]; then
            warn "no package mapping for NEEDED soname '$so'; add one to soname_to_package()"
            continue
        fi
        case ",$SHLIB_DEPENDS," in
            *",$pkg,"*) ;;
            *) SHLIB_DEPENDS="${SHLIB_DEPENDS:+$SHLIB_DEPENDS,}$pkg" ;;
        esac
    done
else
    warn "objdump not found; falling back to a fixed shared-library dependency list"
    SHLIB_DEPENDS="libc6,libgcc-s1,libpipewire-0.3-0"
fi
SHLIB_DEPENDS=$(printf '%s\n' "$SHLIB_DEPENDS" | sed 's/,/, /g')

if [ -n "$SHLIB_DEPENDS" ]; then
    DEPENDS="${SHLIB_DEPENDS}${DEPENDS:+, $DEPENDS}"
fi

# ---------------------------------------------------------------------------
# Reproducibility
#
# gzip -n and tar's --mtime/--owner/--group/--sort together make the two
# tarballs, and therefore the .deb, a function of the inputs alone. The
# timestamp comes from debian/changelog so it moves only when the changelog
# does.
# ---------------------------------------------------------------------------

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    cl_date=$(grep -m1 '^ -- ' "$DEBIAN_DIR/changelog" | sed 's/^.*>  //')
    SOURCE_DATE_EPOCH=$(date -u -d "$cl_date" +%s 2>/dev/null || echo 0)
fi
export SOURCE_DATE_EPOCH

TAR_REPRO="--owner=root --group=root --numeric-owner --mtime=@$SOURCE_DATE_EPOCH --format=gnu"
# --sort=name needs GNU tar 1.28; do without it rather than fail on an older one.
if tar --sort=name -cf /dev/null --files-from /dev/null 2>/dev/null; then
    TAR_REPRO="$TAR_REPRO --sort=name"
fi

# ---------------------------------------------------------------------------
# Staging
#
# The layout below is packaging/PKGBUILD's and debian/rules', and the one part
# of it that is not a matter of taste is /usr/share/fxsound/presets: that path
# is hardcoded in crates/fxsound-preset/src/store.rs:65 as one of two search
# prefixes. Put the presets anywhere else and the application starts with an
# empty preset list.
# ---------------------------------------------------------------------------

WORK=$(mktemp -d "${TMPDIR:-/tmp}/fxsound-deb.XXXXXX")
cleanup() {
    if [ "$KEEP" -eq 1 ]; then
        echo "$PROG: staging left at $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT HUP INT TERM

DATA="$WORK/data"
CTRL="$WORK/control"
mkdir -p "$DATA" "$CTRL"

install -D -m 0755 "$BINARY" "$DATA/usr/bin/fxsound"
if [ "$STRIP" -eq 1 ]; then
    if command -v strip >/dev/null 2>&1; then
        strip --strip-unneeded "$DATA/usr/bin/fxsound"
    else
        warn "strip not found; the package will carry the unstripped binary"
    fi
fi

# `*.fac` and not the whole directory: assets/presets/BonusPresets also carries
# a zip of the same presets and a stray text file called MeaningfulPresets, and
# neither belongs in a package.
for dir in Factsoft BonusPresets; do
    mkdir -p "$DATA/usr/share/fxsound/presets/$dir"
    for f in "$REPO/assets/presets/$dir"/*.fac; do
        [ -f "$f" ] || continue
        install -m 0644 "$f" "$DATA/usr/share/fxsound/presets/$dir/"
    done
done

# The voice presets are TOML rather than .fac: the .fac format is a
# byte-for-byte contract with the Windows build and has nowhere to put a gate
# threshold.
mkdir -p "$DATA/usr/share/fxsound/presets/Input"
for f in "$REPO/assets/presets/Input"/*.toml; do
    [ -f "$f" ] || continue
    install -m 0644 "$f" "$DATA/usr/share/fxsound/presets/Input/"
done

install -D -m 0644 "$REPO/packaging/fxsound.desktop" \
    "$DATA/usr/share/applications/com.fxsound.FxSound.desktop"
install -D -m 0644 "$REPO/packaging/fxsound.service" \
    "$DATA/usr/lib/systemd/user/fxsound.service"
install -D -m 0644 "$REPO/assets/images/fxsound_large.png" \
    "$DATA/usr/share/icons/hicolor/256x256/apps/fxsound.png"
install -D -m 0644 "$REPO/assets/images/fxsound.png" \
    "$DATA/usr/share/icons/hicolor/32x32/apps/fxsound.png"

# The tray icon's three states. A StatusNotifier host resolves an icon *name*
# through the theme, so without these under hicolor's status context the tray
# shows a blank where the icon should be.
mkdir -p "$DATA/usr/share/icons/hicolor/scalable/status"
for f in "$REPO/assets/icons/status"/com.fxsound.FxSound-*.svg; do
    [ -f "$f" ] || die "no status icons under assets/icons/status/"
    install -m 0644 "$f" "$DATA/usr/share/icons/hicolor/scalable/status/"
done

# AppStream metadata, so GNOME Software and Discover list the package.
install -D -m 0644 "$REPO/packaging/com.fxsound.FxSound.metainfo.xml" \
    "$DATA/usr/share/metainfo/com.fxsound.FxSound.metainfo.xml"

DOCDIR="$DATA/usr/share/doc/$PACKAGE"
mkdir -p "$DOCDIR/examples"
# copyright is the one file in /usr/share/doc that is never compressed, however
# big it gets — and with the whole AGPL-3 inlined it does get big. Everything
# else over 4 KiB is gzipped, which is what dh_compress would have done.
# Files under examples/ are left alone, also as dh_compress leaves them.
install -m 0644 "$DEBIAN_DIR/copyright" "$DOCDIR/copyright"
install -m 0644 "$REPO/packaging/hyprland.conf.example" "$DOCDIR/examples/"
install -m 0644 "$REPO/packaging/fxsound-autostart.desktop" "$DOCDIR/examples/"

# Policy wants the upstream changelog at changelog.gz and the Debian one at
# changelog.Debian.gz. -n keeps the source filename and timestamp out of the
# gzip header, which is what makes the result a function of the input alone.
gzip -9nc "$REPO/CHANGELOG.md" > "$DOCDIR/changelog.gz"
gzip -9nc "$DEBIAN_DIR/changelog" > "$DOCDIR/changelog.Debian.gz"
gzip -9nc "$REPO/README.md" > "$DOCDIR/README.md.gz"
chmod 0644 "$DOCDIR/changelog.gz" "$DOCDIR/changelog.Debian.gz" "$DOCDIR/README.md.gz"
if [ -f "$DEBIAN_DIR/README.Debian" ]; then
    gzip -9nc "$DEBIAN_DIR/README.Debian" > "$DOCDIR/README.Debian.gz"
    chmod 0644 "$DOCDIR/README.Debian.gz"
fi

# The page lives beside the other packaging files, not under debian/, because
# every package ships it now; the unit's Documentation= line depends on it.
[ -f "$REPO/packaging/fxsound.1" ] || die "packaging/fxsound.1 is missing"
mkdir -p "$DATA/usr/share/man/man1"
gzip -9nc "$REPO/packaging/fxsound.1" > "$DATA/usr/share/man/man1/fxsound.1.gz"
chmod 0644 "$DATA/usr/share/man/man1/fxsound.1.gz"

if [ -f "$DEBIAN_DIR/$PACKAGE.lintian-overrides" ]; then
    install -D -m 0644 "$DEBIAN_DIR/$PACKAGE.lintian-overrides" \
        "$DATA/usr/share/lintian/overrides/$PACKAGE"
fi

find "$DATA" -type d -exec chmod 0755 {} +

# ---------------------------------------------------------------------------
# control.tar.gz
# ---------------------------------------------------------------------------

# Installed-Size is in KiB and is what apt shows before installing.
INSTALLED_SIZE=$(du -s -k "$DATA" | awk '{ print $1 }')

{
    printf 'Package: %s\n' "$PACKAGE"
    printf 'Version: %s\n' "$FULL_VERSION"
    printf 'Architecture: %s\n' "$ARCH"
    printf 'Maintainer: %s\n' "$MAINTAINER"
    printf 'Installed-Size: %s\n' "$INSTALLED_SIZE"
    [ -n "$DEPENDS" ] && printf 'Depends: %s\n' "$DEPENDS"
    [ -n "$RECOMMENDS" ] && printf 'Recommends: %s\n' "$RECOMMENDS"
    [ -n "$SUGGESTS" ] && printf 'Suggests: %s\n' "$SUGGESTS"
    [ -n "$PROVIDES" ] && printf 'Provides: %s\n' "$PROVIDES"
    [ -n "$CONFLICTS" ] && printf 'Conflicts: %s\n' "$CONFLICTS"
    [ -n "$REPLACES" ] && printf 'Replaces: %s\n' "$REPLACES"
    [ -n "$SECTION" ] && printf 'Section: %s\n' "$SECTION"
    [ -n "$PRIORITY" ] && printf 'Priority: %s\n' "$PRIORITY"
    [ -n "$HOMEPAGE" ] && printf 'Homepage: %s\n' "$HOMEPAGE"
    printf 'Description: '
    control_field 2 Description raw
} > "$CTRL/control"
chmod 0644 "$CTRL/control"

# md5sums: one "<hash>  <path>" line per regular file, paths relative to / with
# no leading slash and no ./.
if command -v md5sum >/dev/null 2>&1; then
    ( cd "$DATA" && find . -type f | sed 's|^\./||' | LC_ALL=C sort \
        | while IFS= read -r f; do md5sum "$f"; done ) > "$CTRL/md5sums"
    chmod 0644 "$CTRL/md5sums"
else
    warn "md5sum not found; the package will have no md5sums file"
fi

# The postrm, with the debhelper token removed — there is no debhelper here to
# expand it, and dpkg would run the literal line as a shell comment, which is
# harmless but misleading in a script someone may read while debugging a
# failed removal.
if [ -f "$DEBIAN_DIR/$PACKAGE.postrm" ]; then
    grep -v '^#DEBHELPER#$' "$DEBIAN_DIR/$PACKAGE.postrm" > "$CTRL/postrm"
    chmod 0755 "$CTRL/postrm"
fi

# No triggers file and no postinst on purpose: the icon cache and the desktop
# database are refreshed by triggers that hicolor-icon-theme and
# desktop-file-utils declare on /usr/share/icons/hicolor and
# /usr/share/applications. Shipping files under those paths activates them; a
# package does not declare anything to take part.

# ---------------------------------------------------------------------------
# Assemble
# ---------------------------------------------------------------------------

printf '2.0\n' > "$WORK/debian-binary"

# shellcheck disable=SC2086
tar $TAR_REPRO -czf "$WORK/control.tar.gz" -C "$CTRL" .
# shellcheck disable=SC2086
tar $TAR_REPRO -czf "$WORK/data.tar.gz" -C "$DATA" .

DEB="$OUTDIR/${PACKAGE}_${FULL_VERSION}_${ARCH}.deb"
mkdir -p "$OUTDIR"
rm -f "$DEB"

# The member order is part of the format, and `D` asks ar for deterministic
# mode: zeroed mtime, uid and gid in the member headers. An ar too old for `D`
# still produces a valid .deb, just not a bit-identical one.
#
# One thing that looks wrong in a hexdump and is not: GNU ar writes the member
# names with a trailing slash ("debian-binary/"), because the name field is
# fixed at 16 bytes and the slash is where the name ends. dpkg strips trailing
# slashes and spaces when it reads them, so this is the same archive dpkg-deb
# would have produced. There is also no symbol index, since ar only builds one
# for object files and none of these are.
if ! ( cd "$WORK" && ar rcD "$DEB" debian-binary control.tar.gz data.tar.gz ) 2>/dev/null
then
    # Start from nothing, or the second `ar` would append to whatever the first
    # one managed to write and the archive would carry each member twice.
    rm -f "$DEB"
    ( cd "$WORK" && ar rc "$DEB" debian-binary control.tar.gz data.tar.gz )
fi

# ---------------------------------------------------------------------------
# Verify what was produced, rather than assuming it
# ---------------------------------------------------------------------------

members=$(ar t "$DEB" | tr '\n' ' ')
expected="debian-binary control.tar.gz data.tar.gz "
[ "$members" = "$expected" ] \
    || die "member list is '$members', expected '$expected' — the archive is not a valid .deb"

ar p "$DEB" debian-binary | grep -qx '2.0' \
    || die "debian-binary does not contain 2.0"

ar p "$DEB" control.tar.gz | tar tz 2>/dev/null | grep -qx './control' \
    || die "control.tar.gz has no ./control"

ar p "$DEB" data.tar.gz | tar tz 2>/dev/null | grep -qx './usr/bin/fxsound' \
    || die "data.tar.gz has no ./usr/bin/fxsound"

file_count=$(ar p "$DEB" data.tar.gz | tar tz 2>/dev/null | grep -cv '/$' || true)

note "built $DEB"
note "  version      $FULL_VERSION"
note "  architecture $ARCH"
note "  files        $file_count"
note "  installed    ${INSTALLED_SIZE} KiB"
note "  size         $(du -h "$DEB" | awk '{ print $1 }')"
echo
note "Inspect it with:"
note "  ar t $DEB"
note "  ar p $DEB control.tar.gz | tar tzvf -"
note "  ar p $DEB data.tar.gz | tar tzvf -"
note "and, on a Debian machine, check it with 'lintian' and install it with"
note "'apt install ./$(basename "$DEB")'."
