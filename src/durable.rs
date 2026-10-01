//! Replacing a file so that the new content survives a crash.
//!
//! A temporary file renamed over the target is atomic against an interrupted
//! process, but not against power loss: without a flush to disk first, some
//! filesystems keep the rename and lose the data, leaving an empty or partial
//! file where a whole one was. The ledger holds hours that may already have
//! been submitted, so it (and the small state files beside it) is written with
//! the file synced before the rename and the directory synced after it.

use std::fs::File;
use std::io;
use std::path::Path;

use tempfile::NamedTempFile;

/// Syncs `file`, renames it over `path` and, on Unix, syncs the directory so
/// the rename itself is on disk. The directory sync is best effort: a platform
/// or filesystem that cannot do it has still replaced the file whole.
pub(crate) fn persist(file: NamedTempFile, path: &Path) -> io::Result<()> {
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    sync_parent(path);
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::*;

    #[test]
    fn persisting_replaces_the_target_whole_and_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("state.json");
        fs::write(&target, "old").unwrap();
        let mut file = NamedTempFile::new_in(directory.path()).unwrap();
        file.write_all(b"new content").unwrap();
        persist(file, &target).unwrap();
        assert_eq!("new content", fs::read_to_string(&target).unwrap());
        assert_eq!(1, fs::read_dir(directory.path()).unwrap().count());
    }

    #[test]
    fn a_target_in_the_working_directory_has_a_parent_to_sync() {
        // `Path::parent` of a bare file name is the empty path, which `File::open`
        // refuses; the helper must not turn that into an error.
        sync_parent(Path::new("state.json"));
    }
}
