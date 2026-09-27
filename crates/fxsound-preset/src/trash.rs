//! Putting a deleted preset where a person can get it back (0.4.0 audit #16).
//!
//! The original deletes a user preset for good (`FxController.cpp:1278-1300`, `SHFileOperation`
//! without `FOF_ALLOWUNDO`), one menu click from a thirty-one-band curve that took an evening.
//! Here the file goes to the desktop's trash, the one every file manager lists and can restore
//! from — the home trash of the FreeDesktop.org Trash specification 1.0: the file under
//! `$XDG_DATA_HOME/Trash/files`, and beside it under `Trash/info` a `.trashinfo` saying where it
//! came from and when it went.
//!
//! Only the home trash, and only for a file on the same filesystem as it, since the move has to
//! be a rename: the spec's per-volume `$topdir/.Trash-$uid` needs the mount point, and a copy
//! across filesystems followed by a delete is the kind of half-done move this is here to avoid.
//! A preset the home trash cannot take is set aside where it was instead ([`set_aside`]), under
//! `<file>.1.bak` or the next free number, which the preset list does not show and a person can
//! rename back. The unnumbered `<file>.bak` is the overwrite's: each overwrite of a preset
//! replaces it with the version before, so a file set aside never goes there, and nothing set
//! aside is ever written over.

use fxsound_core::atomic::numbered;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Where a deleted preset went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discarded {
    /// Into the desktop's trash, as this file.
    Trash(PathBuf),
    /// Beside where it was, renamed to this `.bak` ([`set_aside`]), because the trash could not
    /// take it.
    Backup(PathBuf),
}

/// Move `path` to the home trash, or failing that set it aside as `<path>.N.bak`.
///
/// # Errors
/// Neither move could be made; the file is where it was.
pub fn discard(path: &Path) -> io::Result<Discarded> {
    discard_to(path, home_trash())
}

/// [`discard`] into `trash`, laid out as a trash, rather than the home trash.
///
/// # Errors
/// Neither move could be made; the file is where it was.
pub fn discard_into(path: &Path, trash: &Path) -> io::Result<Discarded> {
    discard_to(path, Ok(trash.to_path_buf()))
}

fn discard_to(path: &Path, trash: io::Result<PathBuf>) -> io::Result<Discarded> {
    match trash.and_then(|trash| move_to_trash(path, &trash)) {
        Ok(trashed) => Ok(Discarded::Trash(trashed)),
        Err(err) => {
            log::info!(
                "{}: not moved to the trash ({err}); keeping it as a .bak",
                path.display()
            );
            set_aside(path).map(Discarded::Backup)
        }
    }
}

/// Rename `path` to `<path>.1.bak` beside itself, or `<path>.2.bak` and so on when that is
/// taken, and return the new path; whatever already has one of those names is left as it is.
///
/// The rename cannot land on a file that appears under the name in the meantime either
/// ([`fxsound_core::atomic::rename_to_free_name`]).
///
/// # Errors
/// The file could not be renamed, or a thousand names were taken; it is where it was.
pub fn set_aside(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let mut backup = name.to_owned();
    backup.push(".bak");
    fxsound_core::atomic::rename_to_free_name(
        path,
        (1..=1000_u32).map(|n| path.with_file_name(numbered(&backup, n))),
    )
}

/// `$XDG_DATA_HOME/Trash`, with the specification's fallback of `~/.local/share`.
fn home_trash() -> io::Result<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".local/share"))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
    Ok(data_home.join("Trash"))
}

/// Move `path` into `trash` as the specification lays it out, and return where it went.
///
/// The `.trashinfo` is made first, with `create_new`, which is how the specification reserves a
/// name: whoever creates it owns that name in `files`. Should the rename then fail, it is taken
/// back, and the trash holds nothing of this file.
fn move_to_trash(path: &Path, trash: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    let path = std::path::absolute(path)?;
    let data_home = trash.parent().unwrap_or(trash);
    std::fs::create_dir_all(data_home)?;
    let data_home = &std::fs::canonicalize(data_home)?;
    // "the implementation SHOULD check that it is on the same device" — a rename across devices
    // fails anyway, but only after the `.trashinfo` would have promised otherwise.
    if std::fs::metadata(&path)?.dev() != std::fs::metadata(data_home)?.dev() {
        return Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "the trash is on another filesystem",
        ));
    }
    let files = trash.join("files");
    let info = trash.join("info");
    for dir in [trash, files.as_path(), info.as_path()] {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_path(&path),
        deletion_date()
    );
    for attempt in 1..=1000_u32 {
        let candidate = if attempt == 1 {
            name.to_owned()
        } else {
            numbered(name, attempt)
        };
        let mut info_name = candidate.clone();
        info_name.push(".trashinfo");
        let info_path = info.join(&info_name);
        let target = files.join(&candidate);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        // A name whose `.trashinfo` went missing can still be taken in `files`.
        if target.symlink_metadata().is_ok() {
            drop(file);
            let _ = std::fs::remove_file(&info_path);
            continue;
        }
        let moved = file
            .write_all(record.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&path, &target));
        return match moved {
            Ok(()) => Ok(target),
            Err(err) => {
                let _ = std::fs::remove_file(&info_path);
                Err(err)
            }
        };
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name in the trash",
    ))
}

/// The `Path=` value: the absolute path, URI-escaped byte by byte as RFC 2396 says, `/` kept.
pub(crate) fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let mut out = String::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `DeletionDate=`: now, in the user's local time, `YYYY-MM-DDThh:mm:ss`, as the specification
/// asks (UTC when the local zone cannot be read).
fn deletion_date() -> String {
    let now = jiff::Zoned::now();
    now.strftime("%Y-%m-%dT%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::test_support::ScratchDir;

    /// A new directory of the test's own, removed when the test ends — also when it panics.
    fn scratch(tag: &str) -> ScratchDir {
        ScratchDir::new(&format!("trash-test-{tag}"))
    }

    #[test]
    fn a_file_goes_into_files_with_a_trashinfo_saying_where_it_came_from() {
        let dir = scratch("move");
        let trash = dir.join("data/Trash");
        let preset = dir.join("presets/Rock Ballad.fac");
        std::fs::create_dir_all(preset.parent().unwrap()).unwrap();
        std::fs::write(&preset, b"CLASS1").unwrap();

        let target = move_to_trash(&preset, &trash).expect("trashed");
        assert_eq!(target, trash.join("files/Rock Ballad.fac"));
        assert!(!preset.exists(), "gone from where it was");
        assert_eq!(std::fs::read(&target).unwrap(), b"CLASS1");

        let info = std::fs::read_to_string(trash.join("info/Rock Ballad.fac.trashinfo")).unwrap();
        let mut lines = info.lines();
        assert_eq!(lines.next(), Some("[Trash Info]"));
        let path_line = lines.next().unwrap();
        assert_eq!(
            path_line,
            format!("Path={}", encode_path(&preset)),
            "the absolute path, escaped"
        );
        assert!(
            path_line.ends_with("/presets/Rock%20Ballad.fac"),
            "{path_line}"
        );
        let date = lines.next().unwrap().strip_prefix("DeletionDate=").unwrap();
        assert_eq!(date.len(), "2026-09-24T10:00:00".len(), "{date}");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[10..11], "T");
    }

    #[test]
    fn a_second_file_of_the_same_name_takes_a_numbered_one_and_neither_is_lost() {
        let dir = scratch("twice");
        let trash = dir.join("data/Trash");
        let preset = dir.join("Rock.fac");
        std::fs::write(&preset, b"one").unwrap();
        move_to_trash(&preset, &trash).expect("first");
        std::fs::write(&preset, b"two").unwrap();
        let second = move_to_trash(&preset, &trash).expect("second");
        assert_eq!(second, trash.join("files/Rock.2.fac"));
        assert_eq!(std::fs::read(trash.join("files/Rock.fac")).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert!(trash.join("info/Rock.2.fac.trashinfo").is_file());
    }

    #[test]
    fn a_trash_that_cannot_be_made_leaves_the_file_as_a_backup_beside_itself() {
        // The home trash under a path that is a file, not a directory: nothing can be made there.
        let dir = scratch("fallback");
        let blocked = dir.join("data");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let preset = dir.join("Rock.fac");
        std::fs::write(&preset, b"CLASS1").unwrap();
        assert!(move_to_trash(&preset, &blocked.join("Trash")).is_err());
        assert!(
            preset.is_file(),
            "a failed move leaves the file where it was"
        );

        let went = discard_into(&preset, &blocked.join("Trash")).expect("kept as a backup");
        assert_eq!(went, Discarded::Backup(dir.join("Rock.fac.1.bak")));
        assert!(!preset.exists());
        assert_eq!(
            std::fs::read(dir.join("Rock.fac.1.bak")).unwrap(),
            b"CLASS1"
        );
    }

    #[test]
    fn a_file_set_aside_takes_the_first_free_number_and_leaves_every_bak_there_as_it_was() {
        let dir = scratch("aside");
        let preset = dir.join("Rock.fac");
        // The overwrite's backup and an earlier deletion's, both already there.
        std::fs::write(dir.join("Rock.fac.bak"), b"overwritten").unwrap();
        std::fs::write(dir.join("Rock.fac.1.bak"), b"deleted before").unwrap();
        std::fs::write(&preset, b"deleted now").unwrap();

        let aside = set_aside(&preset).expect("set aside");
        assert_eq!(aside, dir.join("Rock.fac.2.bak"));
        assert!(!preset.exists(), "gone from its own name");
        assert_eq!(std::fs::read(&aside).unwrap(), b"deleted now");
        assert_eq!(
            std::fs::read(dir.join("Rock.fac.bak")).unwrap(),
            b"overwritten"
        );
        assert_eq!(
            std::fs::read(dir.join("Rock.fac.1.bak")).unwrap(),
            b"deleted before"
        );
    }

    #[test]
    fn a_file_that_cannot_be_set_aside_stays_where_it_was() {
        let dir = scratch("aside-missing");
        let missing = dir.join("Gone.fac");
        assert!(set_aside(&missing).is_err());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing made on the way"
        );
    }

    #[test]
    fn non_ascii_and_reserved_bytes_are_escaped_in_the_path() {
        assert_eq!(
            encode_path(Path::new("/home/я/a b%#.fac")),
            "/home/%D1%8F/a%20b%25%23.fac"
        );
    }

    #[test]
    fn numbering_keeps_the_extension() {
        assert_eq!(numbered(std::ffi::OsStr::new("Rock.fac"), 3), "Rock.3.fac");
        assert_eq!(numbered(std::ffi::OsStr::new("Rock"), 2), "Rock.2");
    }
}
