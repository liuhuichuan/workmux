//! Recursive deletion that continues after individual failures.
//!
//! Unix removes through held directory descriptors, so a path swapped mid-delete
//! is never followed. Windows has no descriptor-relative delete, so it removes by
//! path and never descends into reparse points.

use std::io;
use std::path::Path;

#[cfg(unix)]
use std::ffi::{CStr, OsStr};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(unix)]
use nix::dir::Dir;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{AtFlags, OFlag, open, openat};
#[cfg(unix)]
use nix::sys::stat::{Mode, fstat, fstatat};
#[cfg(unix)]
use nix::unistd::{UnlinkatFlags, unlinkat};

use super::cleanup::{DirectoryIdentity, metadata_matches};

const MAX_DEPTH: usize = 64;
const MAX_ERROR_BYTES: usize = 16 * 1024;

#[cfg(unix)]
fn directory_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

/// Open and validate the quarantine root before touching its contents. All child
/// traversal and deletion is relative to held descriptors, never joined paths.
#[cfg(unix)]
pub(super) fn remove(path: &Path, expected: DirectoryIdentity) -> io::Result<()> {
    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing quarantine parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("Missing quarantine filename"))?;
    let parent = match open(parent_path, directory_flags(), Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let directory = match openat(&parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::ENOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata_matches(&directory.metadata()?, expected) {
        return Err(io::Error::other(
            "Quarantined worktree identity changed before deletion",
        ));
    }
    let mut failures = Failures::default();
    clear_directory(&directory, path, 0, &mut failures);
    failures.record(path, unlink_directory(&parent, name, &directory));
    failures.finish()
}

#[cfg(unix)]
fn clear_directory(directory: &File, path: &Path, depth: usize, failures: &mut Failures) {
    if depth >= MAX_DEPTH {
        failures.record(
            path,
            Err(io::Error::other("Directory nesting exceeds cleanup limit")),
        );
        return;
    }
    // A separate stream descriptor avoids borrowing the traversal handle while
    // readdir advances. Dot entries must never participate in deletion.
    let entries = match Dir::openat(directory, c".", directory_flags(), Mode::empty()) {
        Ok(entries) => entries,
        Err(error) => {
            failures.record(path, Err(error.into()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.record(path, Err(error.into()));
                break;
            }
        };
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        // Joined paths are diagnostic labels only, never filesystem operands.
        let child_path = path.join(OsStr::from_bytes(name.to_bytes()));
        remove_entry(directory, name, &child_path, depth + 1, failures);
    }
}

#[cfg(unix)]
fn remove_entry(parent: &File, name: &CStr, path: &Path, depth: usize, failures: &mut Failures) {
    match openat(parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => {
            let directory = File::from(fd);
            clear_directory(&directory, path, depth, failures);
            failures.record(
                path,
                unlink_directory(parent, OsStr::from_bytes(name.to_bytes()), &directory),
            );
        }
        Err(Errno::ENOTDIR | Errno::ELOOP) => {
            // unlinkat without RemoveDir removes the entry itself, not its target.
            failures.record(
                path,
                unlinkat(parent, name, UnlinkatFlags::NoRemoveDir).map_err(Into::into),
            );
        }
        Err(error) => failures.record(path, Err(error.into())),
    }
}

#[cfg(unix)]
fn unlink_directory(parent: &File, name: &OsStr, directory: &File) -> io::Result<()> {
    let opened = fstat(directory)?;
    let current = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if opened.st_dev != current.st_dev || opened.st_ino != current.st_ino {
        return Err(io::Error::other(
            "Directory identity changed before removal",
        ));
    }
    #[cfg(test)]
    before_rmdir::fire(name);
    unlinkat(parent, name, UnlinkatFlags::RemoveDir).map_err(Into::into)
}

/// Remove the quarantine root and everything below it without following reparse
/// points, so a linked directory outside the quarantine is never deleted.
#[cfg(windows)]
pub(super) fn remove(path: &Path, expected: DirectoryIdentity) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other("Quarantined worktree is not a directory"));
    }
    if !metadata_matches(path, &metadata, expected) {
        return Err(io::Error::other(
            "Quarantined worktree identity changed before deletion",
        ));
    }
    let mut failures = Failures::default();
    clear_directory(path, 0, &mut failures);
    failures.record(path, remove_directory(path));
    failures.finish()
}

/// True for symlinks, junctions, and every other reparse point. `file_type()`
/// alone misses junctions, whose targets must never be traversed.
#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// `Metadata::is_dir` reports false for a junction, so ask the attributes.
#[cfg(windows)]
fn is_directory(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// Read-only entries -- common in Git object stores and copied trees -- cannot be
/// unlinked until their attribute is cleared.
#[cfg(windows)]
fn clear_readonly(path: &Path) {
    const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
    use std::os::windows::fs::MetadataExt;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_READONLY == 0 {
        return;
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    let _ = std::fs::set_permissions(path, permissions);
}

#[cfg(windows)]
fn clear_directory(directory: &Path, depth: usize, failures: &mut Failures) {
    if depth >= MAX_DEPTH {
        failures.record(
            directory,
            Err(io::Error::other("Directory nesting exceeds cleanup limit")),
        );
        return;
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            failures.record(directory, Err(error));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.record(directory, Err(error));
                break;
            }
        };
        remove_entry(&entry.path(), depth + 1, failures);
    }
}

#[cfg(windows)]
fn remove_entry(path: &Path, depth: usize, failures: &mut Failures) {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            failures.record(path, Err(error));
            return;
        }
    };
    // A reparse point is removed as the link it is; descending would delete the
    // contents of whatever it points at.
    if is_reparse_point(&metadata) {
        let result = if is_directory(&metadata) {
            remove_directory(path)
        } else {
            std::fs::remove_file(path)
        };
        failures.record(path, result);
        return;
    }
    if metadata.is_dir() {
        clear_directory(path, depth, failures);
        #[cfg(test)]
        before_rmdir::fire(&name_of(path));
        failures.record(path, remove_directory(path));
    } else {
        clear_readonly(path);
        failures.record(path, std::fs::remove_file(path));
    }
}

#[cfg(windows)]
fn remove_directory(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Windows refuses to unlink a read-only directory or a reparse point
            // whose target is gone, so clear the attribute and retry once.
            clear_readonly(path);
            match std::fs::remove_dir(path) {
                Ok(()) => Ok(()),
                Err(_) => Err(error),
            }
        }
    }
}

#[cfg(all(windows, test))]
fn name_of(path: &Path) -> std::ffi::OsString {
    path.file_name().unwrap_or_default().to_os_string()
}

/// Keep diagnostics bounded while remembering whether any non-transient error
/// occurred. Ancestor ENOTEMPTY errors must not hide a child's permanent error.
#[derive(Default)]
struct Failures {
    count: usize,
    permanent_kind: Option<io::ErrorKind>,
    details: String,
}

impl Failures {
    fn record(&mut self, path: &Path, result: io::Result<()>) {
        let Err(error) = result else { return };
        if error.kind() == io::ErrorKind::NotFound {
            return;
        }
        self.count += 1;
        if error.kind() != io::ErrorKind::DirectoryNotEmpty {
            self.permanent_kind.get_or_insert(error.kind());
        }
        if self.details.len() < MAX_ERROR_BYTES {
            let line = format!("\n{path:?}: {error} (errno={:?})", error.raw_os_error());
            let mut end = line.len().min(MAX_ERROR_BYTES - self.details.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            self.details.push_str(&line[..end]);
        }
    }

    fn finish(self) -> io::Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        let truncated = if self.details.len() >= MAX_ERROR_BYTES {
            " [error details truncated]"
        } else {
            ""
        };
        Err(io::Error::new(
            self.permanent_kind
                .unwrap_or(io::ErrorKind::DirectoryNotEmpty),
            format!(
                "Recursive deletion encountered {} errors:{}{}",
                self.count, self.details, truncated
            ),
        ))
    }
}

/// Coordinate late file creation after enumeration and before the real rmdir.
/// Thread-local guards keep parallel tests isolated and restore hooks on panic.
#[cfg(test)]
pub(super) mod before_rmdir {
    use std::cell::RefCell;
    use std::ffi::OsStr;

    type Hook = Box<dyn FnMut(&OsStr)>;
    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub struct Guard(Option<Hook>);

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }

    pub fn install(hook: impl FnMut(&OsStr) + 'static) -> Guard {
        Guard(HOOK.with(|slot| slot.replace(Some(Box::new(hook)))))
    }

    pub(super) fn fire(name: &OsStr) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(name);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::cleanup::test_identity as identity;
    use super::super::cleanup::test_symlink_dir;
    use super::*;

    #[test]
    fn removes_nested_files_without_following_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tree = root.path().join("trash with 'quotes' and $dollars");
        std::fs::create_dir_all(tree.join("a/b")).unwrap();
        std::fs::write(tree.join("a/b/file"), "remove").unwrap();
        std::fs::write(outside.path().join("sentinel"), "keep").unwrap();
        if !test_symlink_dir(outside.path(), &tree.join("link")) {
            eprintln!("skipping: this host cannot create directory links");
            return;
        }
        // A dangling link is a Unix-only fixture: Windows needs a privilege to
        // create one and the traversal guard is identical for both kinds.
        #[cfg(unix)]
        std::os::unix::fs::symlink("missing", tree.join("broken-link")).unwrap();
        remove(&tree, identity(&tree)).unwrap();
        assert!(!tree.exists());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn rejects_a_different_root_and_accepts_an_absent_root() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sentinel"), "keep").unwrap();
        assert!(remove(root.path(), identity(other.path())).is_err());
        assert!(root.path().join("sentinel").exists());
        remove(&root.path().join("missing"), identity(root.path())).unwrap();
    }

    #[test]
    fn nesting_failure_does_not_prevent_sibling_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        let mut deep = tree.clone();
        for _ in 0..=MAX_DEPTH {
            deep.push("nested");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir(tree.join("unrelated-build")).unwrap();
        std::fs::write(tree.join("unrelated-build/artifact"), "remove").unwrap();
        let error = remove(&tree, identity(&tree)).unwrap_err();
        assert!(error.to_string().contains("nesting exceeds cleanup limit"));
        assert!(deep.exists());
        assert!(!tree.join("unrelated-build").exists());
    }

    #[test]
    fn rejects_a_symlink_quarantine_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel"), "keep").unwrap();
        let link = root.path().join("link");
        if !test_symlink_dir(outside.path(), &link) {
            eprintln!("skipping: this host cannot create directory links");
            return;
        }
        assert!(remove(&link, identity(outside.path())).is_err());
        assert!(link.is_symlink());
        assert!(outside.path().join("sentinel").exists());
    }

    #[test]
    fn reports_permanent_errors_without_masking_them_with_nonempty_ancestors() {
        let mut failures = Failures::default();
        failures.record(Path::new("missing"), Err(io::ErrorKind::NotFound.into()));
        failures.record(
            Path::new("busy"),
            Err(io::ErrorKind::DirectoryNotEmpty.into()),
        );
        failures.record(
            Path::new("protected"),
            Err(io::ErrorKind::PermissionDenied.into()),
        );
        let error = failures.finish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("2 errors"));
        assert!(error.to_string().contains("protected"));
    }

    #[test]
    fn bounds_failure_details() {
        let mut failures = Failures::default();
        for _ in 0..1000 {
            failures.record(
                Path::new("entry"),
                Err(io::ErrorKind::DirectoryNotEmpty.into()),
            );
        }
        assert!(failures.details.len() <= MAX_ERROR_BYTES);
        assert!(
            failures
                .finish()
                .unwrap_err()
                .to_string()
                .contains("1000 errors")
        );
    }
}
