//! Replacing a file without ever leaving a half-written one behind.
//!
//! Every persistent file this application owns — the settings, a saved preset, the autosave of
//! unsaved edits — is rewritten in place while the application is running, and two of those are
//! written on paths a user triggers constantly: the autosave fires on every preset switch with
//! unsaved changes and again on shutdown. A plain `fs::write` truncates first, so an interrupted
//! one leaves a zero-length or half-written file, and the reader on the next launch finds a
//! preset it cannot parse where a perfectly good one used to be.
//!
//! The sequence here is the usual durable-replace: write a temporary file beside the target,
//! `fsync` it, rename over the target, then `fsync` the directory so the rename itself is on
//! disk. The temp file lives in the same directory as the target because a rename is only atomic
//! within one filesystem, and it carries the process id so two instances cannot collide on it.
//!
//! What this does **not** promise: that the *new* contents survive a power cut. A durable replace
//! guarantees the reader sees either the old file or the new one, never a mixture — which is the
//! property that matters, because the alternative is losing data that was already safely on disk.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes temporary files written by the same process in the same millisecond.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `contents`, atomically and durably.
///
/// Creates the parent directory if it is missing. On any failure the temporary file is removed
/// and the existing file is left exactly as it was.
pub fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = parent.join(format!(
        ".{stem}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    match write_and_sync(&temp, contents).and_then(|()| std::fs::rename(&temp, path)) {
        Ok(()) => {}
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
    }

    // Without this the rename can still be lost on a crash even though the file's contents were
    // synced: the directory entry is its own piece of metadata. A filesystem that refuses to open
    // a directory for this is not a reason to report the write as failed — the data is already
    // there.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// As [`write`], but first copies any existing file to `<path>.bak`.
///
/// For files a user authored and cannot regenerate. The copy is not itself atomic — it is a
/// convenience for someone who overwrote the wrong preset, not a guarantee.
pub fn write_with_backup(path: &Path, contents: &[u8]) -> io::Result<()> {
    if path.exists() {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".bak");
        if let Err(err) = std::fs::copy(path, Path::new(&backup)) {
            log::warn!("could not back up {}: {err}", path.display());
        }
    }
    write(path, contents)
}

fn write_and_sync(temp: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::File::create(temp)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fxsound-atomic-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    #[test]
    fn a_write_creates_the_file_and_its_parent() {
        let dir = temp_dir("create");
        let path = dir.join("nested").join("settings.toml");
        write(&path, b"hello").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_leaves_no_temporary_file_behind() {
        let dir = temp_dir("clean");
        let path = dir.join("preset.fac");
        write(&path, b"one").expect("write");
        write(&path, b"two").expect("overwrite");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["preset.fac".to_owned()], "stray files: {left:?}");
        assert_eq!(std::fs::read(&path).expect("read"), b"two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_the_previous_contents_intact() {
        let dir = temp_dir("failure");
        let path = dir.join("preset.fac");
        write(&path, b"original").expect("write");

        // A directory cannot be replaced by a file, so the rename fails while the temporary file
        // has already been written — the one ordering where a naive implementation would have
        // truncated the target first.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(&blocked).expect("create");
        assert!(write(&blocked, b"nope").is_err());

        assert_eq!(std::fs::read(&path).expect("read"), b"original");
        assert!(blocked.is_dir(), "the directory should be untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_keeps_what_was_overwritten() {
        let dir = temp_dir("backup");
        let path = dir.join("My Preset.fac");
        write(&path, b"first").expect("write");
        write_with_backup(&path, b"second").expect("overwrite");

        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        assert_eq!(
            std::fs::read(dir.join("My Preset.fac.bak")).expect("read the backup"),
            b"first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_of_a_file_that_does_not_exist_yet_is_not_an_error() {
        let dir = temp_dir("backup-missing");
        let path = dir.join("new.fac");
        write_with_backup(&path, b"only").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"only");
        assert!(!dir.join("new.fac.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
