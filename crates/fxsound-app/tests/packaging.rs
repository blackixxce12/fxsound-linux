//! What the packages ship, held by running the recipes themselves wherever that needs no package
//! manager: `packaging/build-tarball.sh` with a stand-in for cargo, and the `package()` function of
//! each Arch recipe in a plain bash, the way makepkg calls it. What cannot be run here — an RPM
//! build, dpkg — is read: the Fedora spec's `%files` and Debian's machine-readable `copyright`.
//!
//! Two things are held. The first is the licences. The binary carries two parts that are not AGPL:
//! the RNNoise code and model in `crates/fxsound-rnnoise` (BSD-3-Clause, whose second clause makes
//! reproducing the notice in the accompanying materials the condition of shipping a binary) and
//! the Noto fallback faces embedded in the window (SIL OFL 1.1, whose second condition asks the
//! same of the font's notice and licence). 0.4.0's first packages installed only `LICENSE`, and
//! Debian's `copyright` called every file in the tree AGPL, fonts included. The second is that the
//! tarball script packs the binary of the tree it sits in: it used to build only when there was no
//! binary at all, so a 0.3.0 left in `target/release` went out under the 0.4.0 name.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use fxsound_core::test_support::{ScratchDir, command};

/// The repository this test binary was built from.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root exists")
}

fn read(path: impl AsRef<Path>) -> Vec<u8> {
    let path = path.as_ref();
    fs::read(path).unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()))
}

fn text(path: impl AsRef<Path>) -> String {
    String::from_utf8(read(path)).expect("the file is UTF-8")
}

fn write_executable(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("a file has a parent")).expect("the parent is made");
    fs::write(path, contents).expect("the file is written");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("it is made executable");
}

/// What a run printed, for a failure message.
fn transcript(output: &Output) -> String {
    format!(
        "status {}\n--- stdout\n{}--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The two notices every package has to carry beside `LICENSE`, as (installed name, source).
fn notices() -> [(&'static str, PathBuf); 2] {
    [
        (
            "COPYING.rnnoise",
            repo().join("crates/fxsound-rnnoise/COPYING"),
        ),
        ("OFL.noto", repo().join("assets/fonts/OFL.txt")),
    ]
}

/// A copy of the source tree made of links to the real one, with a `Cargo.toml` of its own (for
/// the version) and a `target/`, `dist/` and temporary directory the runs may write into. Nothing
/// a run writes can reach the repository: every directory it writes in is a real one in here.
struct Tree {
    scratch: ScratchDir,
}

/// The version the tree's `Cargo.toml` is at.
const TREE_VERSION: &str = "9.8.7";

impl Tree {
    fn new(tag: &str) -> Self {
        let scratch = ScratchDir::new(&format!("packaging-{tag}"));
        let tree = Self { scratch };
        let root = tree.root();
        let repo = repo();
        // A real directory of linked files rather than a linked directory, so that the script's
        // `cd "$(dirname "$0")/.."` lands in this tree and not in the repository.
        fs::create_dir_all(root.join("packaging")).expect("packaging/ is made");
        for entry in fs::read_dir(repo.join("packaging")).expect("packaging/ is listed") {
            let entry = entry.expect("an entry of packaging/");
            symlink(entry.path(), root.join("packaging").join(entry.file_name()))
                .expect("a packaging file is linked");
        }
        for shared in ["assets", "crates", "README.md", "CHANGELOG.md", "LICENSE"] {
            symlink(repo.join(shared), root.join(shared)).expect("a source path is linked");
        }
        fs::write(
            root.join("Cargo.toml"),
            format!("[workspace.package]\nversion = \"{TREE_VERSION}\"\n"),
        )
        .expect("Cargo.toml is written");
        fs::create_dir_all(tree.scratch.join("tmp")).expect("a temporary directory is made");
        // Stands in for cargo: notes how it was called and, told a version, "builds" a binary that
        // reports it, where a real build would put it.
        write_executable(
            &tree.scratch.join("bin/cargo"),
            r#"#!/bin/sh
printf '%s | CARGO_TARGET_DIR=%s\n' "$*" "${CARGO_TARGET_DIR:-}" >> "$STUB_CARGO_LOG"
if [ -n "${STUB_BUILDS:-}" ]; then
  out="${CARGO_TARGET_DIR:-target}/release"
  mkdir -p "$out"
  printf '#!/bin/sh\necho "fxsound %s"\n# built by this run\n' "$STUB_BUILDS" > "$out/fxsound"
  chmod 755 "$out/fxsound"
fi
"#,
        );
        tree
    }

    fn root(&self) -> PathBuf {
        self.scratch.join("root")
    }

    /// Leave a binary in `target/release` that says it is `version`, as a build of another
    /// checkout would.
    fn leave_binary(&self, version: &str, marker: &str) {
        write_executable(
            &self.root().join("target/release/fxsound"),
            &format!("#!/bin/sh\necho \"fxsound {version}\"\n# {marker}\n"),
        );
    }

    /// Run `packaging/build-tarball.sh` in this tree. `builds` is the version the stand-in cargo
    /// builds, or `None` for a cargo that leaves `target/` alone; `target_dir` is a
    /// `CARGO_TARGET_DIR` the caller's shell has set.
    fn build_tarball(&self, builds: Option<&str>, target_dir: Option<&Path>) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.scratch.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut run = command("bash");
        run.arg(self.root().join("packaging/build-tarball.sh"))
            .current_dir(self.scratch.path())
            .env("PATH", std::env::join_paths(paths).expect("PATH is joined"))
            .env("TMPDIR", self.scratch.join("tmp"))
            .env("STUB_CARGO_LOG", self.cargo_log_path())
            .env_remove("STUB_BUILDS")
            .env_remove("CARGO_TARGET_DIR");
        if let Some(version) = builds {
            run.env("STUB_BUILDS", version);
        }
        if let Some(dir) = target_dir {
            run.env("CARGO_TARGET_DIR", dir);
        }
        run.output().expect("bash runs")
    }

    fn cargo_log_path(&self) -> PathBuf {
        self.scratch.join("cargo.log")
    }

    fn cargo_log(&self) -> String {
        fs::read_to_string(self.cargo_log_path()).unwrap_or_default()
    }

    /// The archives in `dist/`.
    fn tarballs(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.root().join("dist")) else {
            return Vec::new();
        };
        entries
            .map(|entry| entry.expect("an entry of dist/").path())
            .filter(|path| path.to_string_lossy().ends_with(".tar.gz"))
            .collect()
    }

    /// Unpack the one archive in `dist/` into `into`, without its top directory.
    fn unpack_tarball(&self, into: &Path) {
        let tarballs = self.tarballs();
        let [tarball] = tarballs.as_slice() else {
            panic!("dist/ should hold one archive, and holds {tarballs:?}");
        };
        fs::create_dir_all(into).expect("the directory to unpack into is made");
        let unpacked = command("tar")
            .arg("-xzf")
            .arg(tarball)
            .arg("--strip-components=1")
            .arg("-C")
            .arg(into)
            .output()
            .expect("tar runs");
        assert!(unpacked.status.success(), "{}", transcript(&unpacked));
    }

    /// Source `recipe` in bash and call its `package()`, as makepkg does, with `srcdir` and
    /// `startdir` as given and `pkgdir` a fresh directory, which is returned. `before` runs
    /// between the two, with the recipe's variables set and `$UNPACKED` naming `unpacked`. Also
    /// returns the recipe's `license` array.
    fn run_package(
        &self,
        recipe: &Path,
        srcdir: &Path,
        startdir: &Path,
        (before, unpacked): (&str, &Path),
    ) -> (PathBuf, Vec<String>) {
        let pkgdir = self.scratch.join("pkg");
        let script = format!(
            "set -e\nsource \"$RECIPE\"\n{before}\npackage\nprintf '%s\\n' \"${{license[@]}}\" \
             > \"$LICENSES\"\n"
        );
        let licenses = self.scratch.join("licenses.txt");
        let packaged = command("bash")
            .arg("-c")
            .arg(script)
            .current_dir(self.scratch.path())
            .env("RECIPE", recipe)
            .env("srcdir", srcdir)
            .env("startdir", startdir)
            .env("pkgdir", &pkgdir)
            .env("LICENSES", &licenses)
            .env("UNPACKED", unpacked)
            .output()
            .expect("bash runs");
        assert!(
            packaged.status.success(),
            "{}'s package() failed: {}",
            recipe.display(),
            transcript(&packaged)
        );
        let declared = text(&licenses).lines().map(str::to_owned).collect();
        (pkgdir, declared)
    }
}

/// `dir` holds `LICENSE` and both notices, each byte for byte what the tree has.
fn assert_notices_in(dir: &Path, package: &str) {
    assert_eq!(
        read(dir.join("LICENSE")),
        read(repo().join("LICENSE")),
        "{package}: {}/LICENSE is not the tree's LICENSE",
        dir.display()
    );
    for (name, source) in notices() {
        let installed = dir.join(name);
        assert!(
            installed.is_file(),
            "{package} does not install {name} ({}) into {}",
            source.display(),
            dir.display()
        );
        assert_eq!(
            read(&installed),
            read(&source),
            "{package}: {} is not {}",
            installed.display(),
            source.display()
        );
    }
}

/// An Arch recipe that installs the BSD and OFL texts says it ships those licences, too.
fn assert_declares_the_notices(declared: &[String], package: &str) {
    for licence in ["AGPL-3.0-or-later", "BSD-3-Clause", "OFL-1.1"] {
        assert!(
            declared.iter().any(|declared| declared == licence),
            "{package}'s license=() is {declared:?}, without {licence}"
        );
    }
}

#[test]
fn the_tarball_script_builds_the_tree_it_sits_in_even_when_a_binary_is_already_there() {
    let tree = Tree::new("tarball-rebuilds");
    // A binary from an older checkout, and a shell that sends cargo's output somewhere else: the
    // script has to build anyway, and into the target/ it packs from.
    tree.leave_binary("0.3.0", "left by an older checkout");
    let elsewhere = tree.scratch.join("elsewhere");

    let run = tree.build_tarball(Some(TREE_VERSION), Some(&elsewhere));

    assert!(run.status.success(), "{}", transcript(&run));
    let log = tree.cargo_log();
    assert!(
        log.lines().any(|call| call.starts_with("build ")
            && call.contains("--release")
            && call.contains("--bin fxsound")),
        "the script did not build the binary; cargo was called as:\n{log}"
    );
    let unpacked = tree.scratch.join("unpacked");
    tree.unpack_tarball(&unpacked);
    let packed = text(unpacked.join("bin/fxsound"));
    assert!(
        packed.contains("built by this run") && packed.contains(TREE_VERSION),
        "the archive holds the binary that was there before the build:\n{packed}"
    );
}

#[test]
fn the_tarball_script_refuses_a_binary_that_is_not_the_trees_version() {
    let tree = Tree::new("tarball-refuses");
    tree.leave_binary("0.3.0", "left by an older checkout");

    // A build that did not replace it.
    let run = tree.build_tarball(None, None);

    assert!(
        !run.status.success(),
        "a 0.3.0 binary was packed for a {TREE_VERSION} tree: {}",
        transcript(&run)
    );
    let said = String::from_utf8_lossy(&run.stderr);
    assert!(
        said.contains("0.3.0") && said.contains(TREE_VERSION),
        "the refusal should name both versions: {}",
        transcript(&run)
    );
    assert!(
        tree.tarballs().is_empty(),
        "an archive was written anyway: {:?}",
        tree.tarballs()
    );
}

#[test]
fn the_tarball_and_the_bin_package_made_from_it_carry_the_notices_of_rnnoise_and_noto() {
    let tree = Tree::new("tarball-notices");
    let run = tree.build_tarball(Some(TREE_VERSION), None);
    assert!(run.status.success(), "{}", transcript(&run));

    // The tarball, as install.sh copies it: share/doc/fxsound-linux.
    let unpacked = tree.scratch.join("unpacked");
    tree.unpack_tarball(&unpacked);
    assert_notices_in(&unpacked.join("share/doc/fxsound-linux"), "the tarball");

    // fxsound-linux-bin, which is that tarball as makepkg unpacks it into $srcdir.
    let srcdir = tree.scratch.join("src");
    let (pkgdir, declared) = tree.run_package(
        &repo().join("packaging/aur/fxsound-linux-bin/PKGBUILD"),
        &srcdir,
        &srcdir,
        (
            "mkdir -p \"$srcdir/${_pkgname}-${pkgver}-x86_64\"\n\
             cp -a \"$UNPACKED\"/. \"$srcdir/${_pkgname}-${pkgver}-x86_64\"/",
            &unpacked,
        ),
    );
    assert_notices_in(
        &pkgdir.join("usr/share/licenses/fxsound-linux"),
        "fxsound-linux-bin",
    );
    assert_declares_the_notices(&declared, "fxsound-linux-bin");
}

#[test]
fn the_arch_package_installs_the_notices_of_rnnoise_and_noto_beside_its_licence() {
    let tree = Tree::new("arch-notices");
    tree.leave_binary(TREE_VERSION, "built from this tree");
    let startdir = tree.root().join("packaging");

    let (pkgdir, declared) = tree.run_package(
        &startdir.join("PKGBUILD"),
        &tree.scratch.join("src"),
        &startdir,
        ("", Path::new("")),
    );

    assert_notices_in(
        &pkgdir.join("usr/share/licenses/fxsound-linux"),
        "packaging/PKGBUILD",
    );
    assert_declares_the_notices(&declared, "packaging/PKGBUILD");
}

/// The body of the spec section that starts with `header` (`%install`, `%files`): up to the next
/// line that starts a section.
fn spec_section<'a>(spec: &'a str, header: &str) -> Vec<&'a str> {
    const SECTIONS: [&str; 10] = [
        "%prep",
        "%build",
        "%install",
        "%check",
        "%post",
        "%preun",
        "%postun",
        "%files",
        "%changelog",
        "%description",
    ];
    spec.lines()
        .skip_while(|line| line.trim_end() != header)
        .skip(1)
        .take_while(|line| {
            !SECTIONS
                .iter()
                .any(|section| line.split_whitespace().next() == Some(section))
        })
        .collect()
}

#[test]
fn the_fedora_package_ships_the_notices_of_rnnoise_and_noto_as_licence_files() {
    let spec = text(repo().join("packaging/fedora/fxsound.spec"));
    let install = spec_section(&spec, "%install");
    let files = spec_section(&spec, "%files");
    for (name, source) in notices() {
        let source = source
            .strip_prefix(repo())
            .expect("the notice is in the tree")
            .display()
            .to_string();
        // Under its own name, so that it does not land in %{_licensedir} as a bare COPYING.
        assert!(
            install.iter().any(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words.first() == Some(&"cp")
                    && words.contains(&source.as_str())
                    && words.last() == Some(&name)
            }),
            "the spec's %install does not copy {source} to {name}"
        );
        assert!(
            files
                .iter()
                .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["%license", name]),
            "the spec's %files has no `%license {name}`"
        );
    }
}

/// One paragraph of a machine-readable debian/copyright: its fields, continuation lines joined
/// with newlines (a lone `.` is an empty line).
struct Paragraph(Vec<(String, String)>);

impl Paragraph {
    fn field(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }
}

fn dep5(copyright: &str) -> Vec<Paragraph> {
    let mut paragraphs = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in copyright.lines().chain([""]) {
        if line.trim().is_empty() {
            if !fields.is_empty() {
                paragraphs.push(Paragraph(std::mem::take(&mut fields)));
            }
        } else if let Some(continued) = line.strip_prefix(' ') {
            let (_, value) = fields.last_mut().expect("a continuation follows a field");
            value.push('\n');
            if continued.trim() != "." {
                value.push_str(continued);
            }
        } else {
            let (name, value) = line.split_once(':').expect("a field has a colon");
            fields.push((name.to_owned(), value.trim().to_owned()));
        }
    }
    paragraphs
}

/// A DEP-5 `Files:` pattern: `*` is any run of characters, `/` included, and `?` any one.
fn matches(pattern: &[u8], path: &[u8]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => {
            matches(rest, path) || (!path.is_empty() && matches(pattern, &path[1..]))
        }
        (Some((b'?', rest)), Some((_, tail))) => matches(rest, tail),
        (Some((expected, rest)), Some((found, tail))) => expected == found && matches(rest, tail),
        _ => false,
    }
}

/// The licence debian/copyright gives `path`: that of the last `Files:` paragraph that matches
/// it, which is how the format resolves overlaps.
fn licence_of<'a>(paragraphs: &'a [Paragraph], path: &str) -> &'a str {
    paragraphs
        .iter()
        .rev()
        .find(|paragraph| {
            paragraph.field("Files").is_some_and(|files| {
                files
                    .split_whitespace()
                    .any(|pattern| matches(pattern.as_bytes(), path.as_bytes()))
            })
        })
        .and_then(|paragraph| paragraph.field("License"))
        .map(|licence| licence.lines().next().unwrap_or_default())
        .unwrap_or_else(|| panic!("no Files paragraph of debian/copyright covers {path}"))
}

/// Every file under `dir`, as a path relative to the repository.
fn files_under(dir: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![repo().join(dir)];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("the directory is listed") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(repo()).expect("under the repository");
                found.push(relative.display().to_string());
            }
        }
    }
    found.sort();
    found
}

#[test]
fn debian_copyright_gives_the_fonts_and_the_vendored_rnnoise_the_licences_they_carry() {
    let copyright = text(repo().join("packaging/debian/copyright"));
    let paragraphs = dep5(&copyright);

    let mut wrong = Vec::new();
    let mut expect = |path: &str, licence: &str| {
        let found = licence_of(&paragraphs, path);
        if found != licence {
            wrong.push(format!("{path}: {found}, should be {licence}"));
        }
    };
    for path in files_under("crates/fxsound-rnnoise") {
        expect(&path, "BSD-3-Clause");
    }
    for path in files_under("assets/fonts") {
        let name = path.rsplit('/').next().expect("a file name");
        if name.starts_with("Noto") || name == "OFL.txt" {
            expect(&path, "OFL-1.1");
        } else if name.starts_with("Gilroy-") {
            // All rights reserved, which debian/copyright states as it is under a name of its own.
            expect(&path, "Gilroy");
        } else {
            expect(&path, "a licence this test has been told of");
        }
    }
    // And the port itself is still the AGPL.
    expect("crates/fxsound-dsp/src/lib.rs", "AGPL-3.0-or-later");
    expect("assets/presets/Input/Broadcaster.toml", "AGPL-3.0-or-later");
    expect("debian/rules", "AGPL-3.0-or-later");

    assert!(
        wrong.is_empty(),
        "debian/copyright says:\n{}",
        wrong.join("\n")
    );

    // Every licence a Files paragraph names has its text: in the paragraph, or in a License
    // paragraph of its own.
    for paragraph in paragraphs.iter().filter(|p| p.field("Files").is_some()) {
        let licence = paragraph
            .field("License")
            .expect("a Files paragraph has a License");
        let name = licence.lines().next().unwrap_or_default();
        let inline = licence.lines().count() > 1;
        let standalone = paragraphs.iter().any(|other| {
            other.field("Files").is_none()
                && other.field("License").is_some_and(|text| {
                    text.lines().next() == Some(name) && text.lines().count() > 1
                })
        });
        assert!(
            inline || standalone,
            "debian/copyright names {name} without its text"
        );
    }
}

/// The text of a licence, whitespace aside, for comparing a copy with its source.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

#[test]
fn debian_copyright_reproduces_the_rnnoise_notice_and_the_ofl_word_for_word() {
    let copyright = text(repo().join("packaging/debian/copyright"));
    let paragraphs = dep5(&copyright);
    let standalone = |name: &str| -> String {
        let licence = paragraphs
            .iter()
            .filter(|paragraph| paragraph.field("Files").is_none())
            .filter_map(|paragraph| paragraph.field("License"))
            .find(|licence| licence.lines().next() == Some(name))
            .unwrap_or_else(|| panic!("debian/copyright has no License: {name} paragraph"));
        licence
            .split_once('\n')
            .map(|(_, text)| text)
            .unwrap_or_default()
            .to_owned()
    };

    // BSD-3-Clause: the conditions and disclaimer of the vendored COPYING, and every holder it
    // names on the rnnoise paragraph's Copyright.
    let copying = text(repo().join("crates/fxsound-rnnoise/COPYING"));
    let (holders, conditions) = copying
        .split_once("Redistribution and use")
        .expect("COPYING states its conditions");
    assert_eq!(
        words(&standalone("BSD-3-Clause")),
        words(&format!("Redistribution and use{conditions}")),
        "debian/copyright's BSD-3-Clause text is not crates/fxsound-rnnoise/COPYING's"
    );
    let rnnoise = paragraphs
        .iter()
        .find(|paragraph| paragraph.field("Files") == Some("crates/fxsound-rnnoise/*"))
        .expect("debian/copyright has a crates/fxsound-rnnoise/* paragraph");
    let named = rnnoise.field("Copyright").unwrap_or_default();
    for holder in holders
        .lines()
        .filter_map(|line| line.strip_prefix("Copyright (c) "))
    {
        let (years, who) = holder.split_once(", ").expect("years, then the holder");
        assert!(
            named
                .lines()
                .any(|line| line.trim() == format!("{years} {who}")),
            "the rnnoise paragraph's Copyright does not name {years} {who}"
        );
    }

    // OFL-1.1: the licence as assets/fonts/OFL.txt carries it, from its first rule down.
    let ofl = text(repo().join("assets/fonts/OFL.txt"));
    let licence = &ofl[ofl
        .find("-----------------------------------------------------------\nSIL OPEN FONT")
        .expect("OFL.txt carries the licence")..];
    assert_eq!(
        words(&standalone("OFL-1.1")),
        words(licence),
        "debian/copyright's OFL-1.1 text is not assets/fonts/OFL.txt's"
    );
}

#[test]
fn the_noto_notice_covers_every_noto_face_in_the_tree() {
    let notice = text(repo().join("assets/fonts/OFL.txt"));
    for path in files_under("assets/fonts") {
        let name = path.rsplit('/').next().expect("a file name");
        if !name.starts_with("Noto") {
            continue;
        }
        // NotoSansKR-Bold.otf is covered by "NotoSansKR-*.otf".
        let (family, rest) = name.split_once('-').expect("family-weight.extension");
        let extension = rest.rsplit('.').next().expect("an extension");
        let pattern = format!("{family}-*.{extension}");
        assert!(
            notice.contains(&pattern),
            "assets/fonts/OFL.txt does not give the notice of {name} (no {pattern} in it)"
        );
    }
}
