//! Putting a deleted preset where a person can get it back (0.4.0 audit #16).
//!
//! The original deletes a user preset for good (`FxController.cpp:1278-1300`, `SHFileOperation`
//! without `FOF_ALLOWUNDO`), one menu click from a thirty-one-band curve that took an evening.
//! Here the file goes to the desktop's trash, the one every file manager lists and can restore
//! from — the home trash of the FreeDesktop.org Trash specification 1.0: the file under
//! `$XDG_DATA_HOME/Trash/files`, and beside it under `Trash/info` a `.trashinfo` saying where it
//! came from and when it went.
//!
//! The move is always a rename, never a copy across filesystems followed by a delete, which is the
//! kind of half-done move this is here to avoid. So a preset on another filesystem than the home
//! trash goes to that filesystem's own trash, as the specification lays it out and file managers
//! list it: `$topdir/.Trash/$uid` where the administrator made a shared `$topdir/.Trash` (a real
//! directory with the sticky bit), else `$topdir/.Trash-$uid`, made at once when it is missing,
//! `$topdir` being where that filesystem is mounted ([`move_to_volume_trash`]). 0.4.0 knew only the
//! home trash. A preset no trash can take — a volume whose top FxSound may not write to, a trash
//! directory that is a symbolic link or someone else's — is set aside where it was instead
//! ([`set_aside`]), under `<file>.1.bak` or the next free number, which the preset list does not
//! show and a person can rename back. The unnumbered `<file>.bak` is the overwrite's: each
//! overwrite of a preset replaces it with the version before, so a file set aside never goes
//! there, and nothing set aside is ever written over.

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

/// Move `path` to the home trash, or to its own filesystem's trash when the home trash is on
/// another one, or failing both set it aside as `<path>.N.bak`.
///
/// # Errors
/// No move could be made; the file is where it was.
pub fn discard(path: &Path) -> io::Result<Discarded> {
    discard_to(path, home_trash(), move_to_volume_trash)
}

/// [`discard`] into `trash`, laid out as a trash, rather than the home trash.
///
/// # Errors
/// No move could be made; the file is where it was.
pub fn discard_into(path: &Path, trash: &Path) -> io::Result<Discarded> {
    discard_to(path, Ok(trash.to_path_buf()), move_to_volume_trash)
}

/// Into `trash`; into the trash `volume` finds when `trash` is on another filesystem; else
/// beside itself.
fn discard_to(
    path: &Path,
    trash: io::Result<PathBuf>,
    volume: impl FnOnce(&Path) -> io::Result<PathBuf>,
) -> io::Result<Discarded> {
    let err = match trash.and_then(|trash| move_to_trash(path, &trash)) {
        Ok(trashed) => return Ok(Discarded::Trash(trashed)),
        Err(err) => err,
    };
    let err = if err.kind() == io::ErrorKind::CrossesDevices {
        match volume(path) {
            Ok(trashed) => return Ok(Discarded::Trash(trashed)),
            Err(volume_err) => format!("{err}; its own filesystem's: {volume_err}"),
        }
    } else {
        err.to_string()
    };
    log::info!(
        "{}: not moved to the trash ({err}); keeping it as a .bak",
        path.display()
    );
    set_aside(path).map(Discarded::Backup)
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

/// Move `path` into `trash`, the home trash, as the specification lays it out, and return where
/// it went. An error of kind [`io::ErrorKind::CrossesDevices`] when the trash is on another
/// filesystem, with nothing made.
fn move_to_trash(path: &Path, trash: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

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
    make_private_dir(trash)?;
    // The home trash records the absolute path.
    trash_into(&path, trash, &encode_path(&path))
}

/// Move `path` into the trash of the filesystem it is on, `$topdir/.Trash/$uid` or
/// `$topdir/.Trash-$uid` ([`volume_trash`]), and return where it went.
fn move_to_volume_trash(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let dir = std::fs::canonicalize(path.parent().unwrap_or(Path::new("/")))?;
    let topdir = mount_point(&dir)?;
    move_to_trash_on(&dir.join(name), &topdir, own_uid()?)
}

/// [`move_to_volume_trash`] for `path`, which is under `topdir`, as user `uid`.
fn move_to_trash_on(path: &Path, topdir: &Path, uid: u32) -> io::Result<PathBuf> {
    let trash = volume_trash(topdir, uid)?;
    // "The system SHOULD support absolute pathnames only in the home trash directory": here the
    // path is relative to `$topdir`, so that the volume can be mounted anywhere and still restore.
    let relative = path.strip_prefix(topdir).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the file is not under the volume",
        )
    })?;
    trash_into(path, &trash, &encode_path(relative))
}

/// The trash directory of the volume mounted at `topdir`, for user `uid`, made when missing.
///
/// `$topdir/.Trash/$uid` when `$topdir/.Trash` is a real directory with the sticky bit — the
/// specification's shared trash, which an administrator makes — and otherwise, or when that
/// cannot be used, `$topdir/.Trash-$uid`. Either must be a directory of the user's own, not a
/// symbolic link: a trash someone else controls would receive the user's file.
fn volume_trash(topdir: &Path, uid: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let shared = topdir.join(".Trash");
    if let Ok(meta) = std::fs::symlink_metadata(&shared)
        && meta.is_dir()
        && meta.permissions().mode() & 0o1000 != 0
    {
        let own = shared.join(uid.to_string());
        match own_dir(&own, uid) {
            Ok(()) => return Ok(own),
            Err(err) => log::info!("{}: not usable ({err})", own.display()),
        }
    }
    let own = topdir.join(format!(".Trash-{uid}"));
    own_dir(&own, uid)?;
    Ok(own)
}

/// Make `dir` with mode 0700 when it is missing; either way it must then be a real directory
/// owned by `uid`.
fn own_dir(dir: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    make_private_dir(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a directory of the user's own",
        ));
    }
    Ok(())
}

/// `mkdir` with mode 0700; one already there is fine.
fn make_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => Err(err),
        _ => Ok(()),
    }
}

/// Where the filesystem `dir` is on is mounted: the highest directory above `dir` still on its
/// device. `dir` is canonical, so no symbolic link takes the walk elsewhere.
fn mount_point(dir: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

    let device = std::fs::metadata(dir)?.dev();
    let mut top = dir;
    while let Some(up) = top.parent() {
        if std::fs::metadata(up)?.dev() != device {
            break;
        }
        top = up;
    }
    Ok(top.to_path_buf())
}

/// This process's uid, read from `/proc/self` so that no libc binding is needed in a crate that
/// forbids `unsafe` (as `fxsound-app`'s control socket does).
fn own_uid() -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;

    std::fs::metadata("/proc/self").map(|meta| meta.uid())
}

/// Move `path` into `trash` as the specification lays it out, the `.trashinfo` saying
/// `Path=record_path`, and return where it went.
///
/// The `.trashinfo` is made first, with `create_new`, which is how the specification reserves a
/// name: whoever creates it owns that name in `files`. Should the rename then fail, it is taken
/// back, and the trash holds nothing of this file.
fn trash_into(path: &Path, trash: &Path, record_path: &str) -> io::Result<PathBuf> {
    let files = trash.join("files");
    let info = trash.join("info");
    for dir in [files.as_path(), info.as_path()] {
        make_private_dir(dir)?;
    }

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let record = format!(
        "[Trash Info]\nPath={record_path}\nDeletionDate={}\n",
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
            .and_then(|()| std::fs::rename(path, &target));
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

    /// A volume of its own inside `dir`, with a preset under it, and the home trash out of reach:
    /// what [`discard`] meets for a preset on another filesystem than the home directory.
    fn on_another_volume(dir: &Path) -> (PathBuf, PathBuf) {
        let topdir = dir.join("volume");
        let preset = topdir.join("Music presets/Rock.fac");
        std::fs::create_dir_all(preset.parent().unwrap()).unwrap();
        std::fs::write(&preset, b"CLASS1").unwrap();
        (topdir, preset)
    }

    fn elsewhere() -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "the trash is on another filesystem",
        ))
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn a_file_on_another_filesystem_goes_to_that_volumes_trash_with_its_path_from_the_top() {
        let dir = scratch("volume");
        let (topdir, preset) = on_another_volume(&dir);
        let uid = own_uid().expect("our uid");
        let went = discard_to(&preset, elsewhere(), |path| {
            move_to_trash_on(path, &topdir, uid)
        })
        .expect("trashed");
        let trash = topdir.join(format!(".Trash-{uid}"));
        assert_eq!(went, Discarded::Trash(trash.join("files/Rock.fac")));
        assert!(!preset.exists(), "gone from where it was");
        assert_eq!(
            std::fs::read(trash.join("files/Rock.fac")).unwrap(),
            b"CLASS1"
        );
        for made in [&trash, &trash.join("files"), &trash.join("info")] {
            assert_eq!(mode(made), 0o700, "{}", made.display());
        }
        let info = std::fs::read_to_string(trash.join("info/Rock.fac.trashinfo")).unwrap();
        assert_eq!(
            info.lines().nth(1),
            Some("Path=Music%20presets/Rock.fac"),
            "relative to the volume's top, as the specification asks: {info}"
        );
    }

    #[test]
    fn a_shared_trash_with_the_sticky_bit_takes_the_file_under_the_users_number() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("volume-shared");
        let (topdir, preset) = on_another_volume(&dir);
        let shared = topdir.join(".Trash");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let uid = own_uid().expect("our uid");
        let went = move_to_trash_on(&preset, &topdir, uid).expect("trashed");
        assert_eq!(went, shared.join(format!("{uid}/files/Rock.fac")));
        assert!(!topdir.join(format!(".Trash-{uid}")).exists());
    }

    #[test]
    fn a_shared_trash_without_the_sticky_bit_or_behind_a_link_is_passed_over() {
        use std::os::unix::fs::PermissionsExt as _;
        let uid = own_uid().expect("our uid");
        // Without the sticky bit anyone could take the others' files out of it.
        let dir = scratch("volume-not-sticky");
        let (topdir, preset) = on_another_volume(&dir);
        std::fs::create_dir(topdir.join(".Trash")).unwrap();
        std::fs::set_permissions(
            topdir.join(".Trash"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let went = move_to_trash_on(&preset, &topdir, uid).expect("trashed");
        assert_eq!(went, topdir.join(format!(".Trash-{uid}/files/Rock.fac")));
        assert_eq!(std::fs::read_dir(topdir.join(".Trash")).unwrap().count(), 0);

        // A link to a proper shared trash is not one.
        let dir = scratch("volume-linked");
        let (topdir, preset) = on_another_volume(&dir);
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o1777)).unwrap();
        std::os::unix::fs::symlink(&real, topdir.join(".Trash")).unwrap();
        let went = move_to_trash_on(&preset, &topdir, uid).expect("trashed");
        assert_eq!(went, topdir.join(format!(".Trash-{uid}/files/Rock.fac")));
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
    }

    #[test]
    fn a_volume_trash_that_is_a_link_or_someone_elses_is_refused_and_the_file_set_aside() {
        let uid = own_uid().expect("our uid");
        let dir = scratch("volume-link");
        let (topdir, preset) = on_another_volume(&dir);
        let elsewhere_dir = dir.join("somewhere else");
        std::fs::create_dir(&elsewhere_dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere_dir, topdir.join(format!(".Trash-{uid}"))).unwrap();
        let went = discard_to(&preset, elsewhere(), |path| {
            move_to_trash_on(path, &topdir, uid)
        })
        .expect("kept as a backup");
        assert_eq!(
            went,
            Discarded::Backup(topdir.join("Music presets/Rock.fac.1.bak"))
        );
        assert_eq!(
            std::fs::read_dir(&elsewhere_dir).unwrap().count(),
            0,
            "nothing went through the link"
        );

        // A trash of another uid's number, as the user's own would be named for them.
        let dir = scratch("volume-other-uid");
        let (topdir, preset) = on_another_volume(&dir);
        std::fs::create_dir(topdir.join(format!(".Trash-{}", uid + 1))).unwrap();
        assert!(move_to_trash_on(&preset, &topdir, uid + 1).is_err());
        assert!(preset.is_file(), "left where it was");
    }

    #[test]
    fn a_trash_that_fails_for_another_reason_is_not_swapped_for_the_volumes() {
        // Only a home trash on another filesystem sends the file to its own volume's trash.
        let dir = scratch("home-blocked");
        let blocked = dir.join("data");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let (_, preset) = on_another_volume(&dir);
        let went = discard_to(&preset, Ok(blocked.join("Trash")), |_| {
            panic!("the volume's trash was tried")
        })
        .expect("kept as a backup");
        assert!(matches!(went, Discarded::Backup(_)), "{went:?}");
    }

    #[test]
    fn the_mount_point_is_the_highest_directory_still_on_the_same_device() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = scratch("mount-point");
        let here = std::fs::canonicalize(&*dir).unwrap();
        let top = mount_point(&here).expect("a mount point");
        assert!(
            here.starts_with(&top),
            "{} above {}",
            top.display(),
            here.display()
        );
        let device = std::fs::metadata(&here).unwrap().dev();
        assert_eq!(std::fs::metadata(&top).unwrap().dev(), device);
        if let Some(up) = top.parent() {
            assert_ne!(std::fs::metadata(up).unwrap().dev(), device);
        }
        // procfs is a filesystem of its own wherever this runs.
        if Path::new("/proc/sys/kernel").is_dir() {
            assert_eq!(
                mount_point(Path::new("/proc/sys/kernel")).unwrap(),
                Path::new("/proc/sys").parent().unwrap()
            );
        }
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
