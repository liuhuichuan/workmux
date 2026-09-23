use anyhow::{Context, Result, anyhow};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

const STALE_ATOMIC_TEMP_AGE: Duration = Duration::from_secs(60);

/// Write content through a same-directory temporary file and atomically replace the target.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    write_atomic_with_durability(path, content, false)
}

/// Open a file for reading with the sharing rules POSIX callers expect.
///
/// Windows refuses to replace a file while another handle has it open unless
/// that handle allows deletion, and `File::open` never does. State files are
/// replaced while readers may still hold them, so they are opened through here.
#[cfg(unix)]
pub fn open_shared(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// Open a file for reading with the sharing rules POSIX callers expect.
///
/// Windows refuses to replace a file while another handle has it open, and
/// `File::open` neither allows deletion nor tolerates the replacement window.
/// State files are replaced while readers may still hold them, so readers go
/// through here and wait out a writer that is mid-replacement.
#[cfg(windows)]
pub fn open_shared(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;

    retry_transient_lock(|| {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
    })
}

/// Read a file opened with [`open_shared`].
pub fn read_shared(path: &Path) -> std::io::Result<String> {
    let mut content = String::new();
    open_shared(path)?.read_to_string(&mut content)?;
    Ok(content)
}

/// Replace `destination` with `source`, which must be on the same filesystem.
#[cfg(unix)]
pub fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

/// Volume serial number and file index for `path`, the Windows counterpart of
/// Unix `dev`/`ino`.
///
/// `std` exposes no stable file id on Windows (`volume_serial_number` and
/// `file_index` are still unstable), so the pair is queried directly. It
/// survives renames and distinguishes a file from whatever replaced it.
#[cfg(windows)]
pub fn file_id(path: &Path) -> std::io::Result<(u64, u64)> {
    win_file_id::file_id(path)
}

#[cfg(windows)]
mod win_file_id {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    // Required to open a directory and to identify a link instead of its target.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const INVALID_HANDLE_VALUE: *mut core::ffi::c_void = -1isize as *mut core::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    unsafe extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut core::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn GetFileInformationByHandle(
            handle: *mut core::ffi::c_void,
            information: *mut FileInformation,
        ) -> i32;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    }

    pub(super) fn file_id(path: &Path) -> io::Result<(u64, u64)> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut information = FileInformation::default();
        let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(handle) };
        if result == 0 {
            return Err(error);
        }

        let index =
            ((information.file_index_high as u64) << 32) | information.file_index_low as u64;
        Ok((information.volume_serial_number as u64, index))
    }
}

/// Replace `destination` with `source`, which must be on the same filesystem.
///
/// Windows cannot rename over a file that another handle has open -- not even
/// when that handle allows deletion -- so a replacement that lands while a
/// reader is active fails with a sharing violation. The reader's window is
/// microseconds long, so the rename waits it out instead of giving up.
#[cfg(windows)]
pub fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    retry_transient_lock(|| fs::rename(source, destination))
}

/// Retry an operation while Windows reports another handle in the way.
///
/// Unlike `rename(2)`, replacing a file on Windows is not atomic with respect to
/// open handles: a reader and a writer can only take turns. Neither side holds
/// the file for long, so waiting out the other is enough.
#[cfg(windows)]
fn retry_transient_lock<T>(
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ATTEMPTS: u32 = 200;
    const DELAY: Duration = Duration::from_millis(1);

    for _ in 1..ATTEMPTS {
        match operation() {
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
                ) =>
            {
                std::thread::sleep(DELAY);
            }
            result => return result,
        }
    }

    operation()
}

/// Atomically replace a target and make the replacement durable before returning.
pub(crate) fn write_atomic_durable(path: &Path, content: &[u8]) -> Result<()> {
    write_atomic_with_durability(path, content, true)
}

fn write_atomic_with_durability(path: &Path, content: &[u8], durable: bool) -> Result<()> {
    cleanup_stale_atomic_temps(path)?;

    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = atomic_temp_prefix(path);
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)
        .with_context(|| format!("Failed to create temp file for {}", path.display()))?;

    tmp.write_all(content)
        .with_context(|| format!("Failed to write temp file for {}", path.display()))?;
    tmp.flush()
        .with_context(|| format!("Failed to flush temp file for {}", path.display()))?;
    if durable {
        tmp.as_file()
            .sync_all()
            .with_context(|| format!("Failed to sync temp file for {}", path.display()))?;
    }
    let temp_path = tmp.into_temp_path();
    replace_file(&temp_path, path)
        .with_context(|| format!("Failed to rename temp file for {}", path.display()))?;
    // The temp file now names the target, so the guard must not unlink it.
    let _ = temp_path.keep();
    // A rename is only durable once the parent directory entry is flushed, which
    // requires opening the directory as a file -- possible on Unix only.
    #[cfg(unix)]
    if durable {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("Failed to sync directory for {}", path.display()))?;
    }
    Ok(())
}

fn cleanup_stale_atomic_temps(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(parent) else {
        return Ok(());
    };
    let prefix = atomic_temp_prefix(path);

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) {
            continue;
        }
        if !is_stale_atomic_temp(&entry) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }

    Ok(())
}

fn is_stale_atomic_temp(entry: &fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_ATOMIC_TEMP_AGE)
}

fn atomic_temp_prefix(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("atomic");
    format!(".{file_name}.tmp.")
}

/// Held exclusive advisory lock on a lock file.
///
/// Maps to `flock(2)` on Unix and `LockFileEx` on Windows, so callers do not
/// need a platform-specific `flock` binding.
pub(crate) struct FileLock {
    _file: File,
}

impl FileLock {
    /// Open (creating if needed) and exclusively lock `path`, blocking until available.
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("Failed to open lock file: {}", path.display()))?;
        file.lock()
            .with_context(|| format!("Failed to acquire lock: {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

/// Canonicalize a path, falling back to the original if canonicalization fails.
pub fn canon_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Rewrite a path into the form Git accepts on this platform.
///
/// `std::fs::canonicalize` returns extended-length (`\\?\`) paths on Windows,
/// but Git rejects those both in environment overrides such as `GIT_DIR` and
/// when they are passed as arguments such as `git worktree add <path>`.
///
/// The same spelling is what every other reader of a path wants: a path
/// compared against another (a resolved one and a plain one name the same
/// place and share no component, so a walk from one to the other comes out
/// absolute), and a path handed to a hook or a program that opens it.
pub fn git_path(path: &Path) -> Cow<'_, OsStr> {
    #[cfg(windows)]
    if let Some(stripped) = strip_verbatim_prefix(path) {
        return Cow::Owned(stripped);
    }
    Cow::Borrowed(path.as_os_str())
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: &Path) -> Option<std::ffi::OsString> {
    let text = path.to_str()?;
    let rest = text.strip_prefix(r"\\?\")?;
    Some(std::ffi::OsString::from(match rest.strip_prefix("UNC\\") {
        Some(unc) => format!(r"\\{unc}"),
        None => rest.to_string(),
    }))
}

/// The path a Git command printed, as this machine writes it.
///
/// Git prints paths the way it accepts them: with forward slashes on every
/// platform. A path workmux read out of Git is a path on this machine -- it
/// gets compared, opened and printed back to the user -- and Windows takes
/// either slash when it is opened, but not when it is read: `C:/repo` in
/// `workmux list --json` is a foreign-looking path in a Windows shell, and it
/// is not the string the same path prints as anywhere else.
pub fn path_from_git(text: &str) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(text.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(text)
    }
}

/// A path, spelled the way a POSIX shell reads it.
///
/// The counterpart of `path_from_git`. Windows writes a path with backslashes
/// and a POSIX shell reads one as an escape: `cat .workmux\PROMPT.md` asks for a
/// file called `.workmuxPROMPT.md` and finds nothing, so a command workmux types
/// into a pane's shell has to name the file the way that shell does. A path
/// that came from a POSIX host is already spelled this way.
pub fn path_for_posix_shell(text: &str) -> String {
    #[cfg(windows)]
    {
        text.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        text.to_string()
    }
}

/// Extensions Windows looks a bare name up with, in the order it tries them.
#[cfg(any(windows, test))]
const SHELL_PATH_EXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The program this machine runs for a bare command name.
///
/// Windows starts what it has as an executable image, and `PATH` alone does not
/// find a tool that npm or scoop installed as `NAME.cmd`: a shell finds it by
/// name and a direct spawn does not. Looking the name up the way a shell does
/// hands back a path to spawn, and `Command` runs a batch file given to it by
/// path with the escaping that takes.
#[cfg(windows)]
pub fn program_path(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| SHELL_PATH_EXT.to_string());
    look_up_program(name, &path, &extensions)
}

/// A program this machine starts by name, wherever `PATH` holds it.
#[cfg(not(windows))]
pub fn program_path(name: &str) -> PathBuf {
    PathBuf::from(name)
}

/// Look a bare name up along `path` as a shell would: the name with each of
/// `extensions`, directory by directory.
///
/// A name that already says where it is, or carries an extension of its own, is
/// the answer, and a name nothing answers to stands as it came in -- the spawn
/// that follows is what has to report that.
///
/// The bare name itself is not one of the candidates: Windows starts an image,
/// and a file whose name has no extension is not one. Choosing such a file --
/// a shell script a Git installation left in a directory on `PATH`, say --
/// would answer with a path that cannot be started at all.
#[cfg(any(windows, test))]
fn look_up_program(name: &str, path: &OsStr, extensions: &str) -> PathBuf {
    if name.contains(['\\', '/']) || Path::new(name).extension().is_some() {
        return PathBuf::from(name);
    }

    let named: Vec<String> = extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!("{name}{extension}"))
        .collect();

    for directory in std::env::split_paths(path) {
        for candidate in &named {
            let path = directory.join(candidate);
            if path.is_file() {
                return path;
            }
        }
    }
    PathBuf::from(name)
}

/// Lexically normalize a path by resolving `.` and `..` components without
/// touching the filesystem.  Unlike `canonicalize()` this works even when the
/// target path does not exist yet.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(components.last(), Some(Component::Normal(_))) {
                    components.pop();
                } else if matches!(
                    components.last(),
                    Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    // Already at root, ignore the ".."
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

/// Expand `~` or `~/...` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    expand_tilde_with_home(path, home::home_dir().as_deref())
}

fn expand_tilde_with_home(path: &str, home: Option<&Path>) -> PathBuf {
    if path == "~" {
        if let Some(h) = home {
            return h.to_path_buf();
        }
    } else if let Some(rest) = path.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    PathBuf::from(path)
}

/// Placeholder replaced with the project directory name in config templates.
pub const PROJECT_PLACEHOLDER: &str = "{project}";

/// Replace `{project}` in a template with `project_root`'s directory name.
pub fn expand_project_placeholder(template: &str, project_root: &Path) -> Result<String> {
    let project_name = project_root
        .file_name()
        .ok_or_else(|| {
            anyhow!(
                "Could not determine project name from path: {}",
                project_root.display()
            )
        })?
        .to_string_lossy();
    Ok(template.replace(PROJECT_PLACEHOLDER, &project_name))
}

/// Expand a `worktree_dir` template against a project root.
///
/// Supported syntax:
/// - Leading `~` or `~/...` expands to the user's home directory.
/// - `{project}` is replaced with `project_root.file_name()`.
///
/// Any other `{...}` token is rejected as an unknown placeholder.
/// Relative results are joined to `project_root` and lexically normalized.
/// Absolute results are returned verbatim (no normalization), matching
/// the prior behavior of `workmux add` for absolute `worktree_dir` values.
pub fn expand_worktree_dir(template: &str, project_root: &Path) -> Result<PathBuf> {
    expand_worktree_dir_with_home(template, project_root, home::home_dir().as_deref())
}

pub(crate) fn expand_worktree_dir_with_home(
    template: &str,
    project_root: &Path,
    home: Option<&Path>,
) -> Result<PathBuf> {
    let mut cursor = 0usize;
    while let Some(rel_open) = template[cursor..].find('{') {
        let open = cursor + rel_open;
        let rel_close = template[open..]
            .find('}')
            .ok_or_else(|| anyhow!("worktree_dir: unterminated '{{' in template '{}'", template))?;
        let close = open + rel_close;
        let token = &template[open..=close];
        if token != PROJECT_PLACEHOLDER {
            return Err(anyhow!(
                "worktree_dir: unknown placeholder '{}' in '{}' (only '{{project}}' is supported)",
                token,
                template
            ));
        }
        cursor = close + 1;
    }

    let tilde_expanded = expand_tilde_with_home(template, home);
    let with_project = expand_project_placeholder(&tilde_expanded.to_string_lossy(), project_root)?;
    let path = Path::new(&with_project);

    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(normalize_path(&project_root.join(path)))
    }
}

/// Format an age in seconds as a compact relative string (e.g., "2h", "3d", "1w", "2mo").
pub fn format_compact_age(secs: u64) -> String {
    let mins = secs / 60;
    let hours = secs / 3600;
    let days = secs / 86400;
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;

    if years > 0 {
        format!("{}y", years)
    } else if months > 0 {
        format!("{}mo", months)
    } else if weeks > 0 {
        format!("{}w", weeks)
    } else if days > 0 {
        format!("{}d", days)
    } else if hours > 0 {
        format!("{}h", hours)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        "<1m".to_string()
    }
}

/// Format a duration as a human-readable elapsed time string.
/// Used by `status` and `wait` commands.
pub fn format_elapsed_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h {}m", h, m)
        }
    }
}

/// Format a Duration as a human-readable elapsed time string (with seconds).
/// Used by `wait` command for more precise timing.
pub fn format_elapsed_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{}m {:02}s", m, s)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool Windows installed as a batch file is not something a direct
    /// spawn finds, and a shell finds it by name: workmux looks it up the way
    /// the shell does, extension by extension, as `PATHEXT` orders them.
    #[test]
    fn a_tool_that_is_a_batch_file_is_looked_up_like_a_shell_looks_it_up() {
        /// A name the way Windows compares it: the disk answers either spelling.
        fn named(path: &Path) -> String {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().as_os_str();
        std::fs::write(dir.path().join("gh.cmd"), "@echo off\n").unwrap();

        let found = look_up_program("gh", path, SHELL_PATH_EXT);
        assert_eq!(named(&found), "gh.cmd");
        assert!(found.is_file(), "{found:?} is not the tool that is there");

        // An image outranks the batch file, as it does in a shell.
        std::fs::write(dir.path().join("gh.exe"), "").unwrap();
        assert_eq!(named(&look_up_program("gh", path, SHELL_PATH_EXT)), "gh.exe");

        // A name that says where it is, or carries an extension, stands.
        assert_eq!(
            look_up_program(r"C:\tools\gh.cmd", path, SHELL_PATH_EXT),
            PathBuf::from(r"C:\tools\gh.cmd")
        );
        // A file with no extension is not something Windows starts, so it does
        // not answer for the name either.
        std::fs::write(dir.path().join("extensionless"), "").unwrap();
        assert_eq!(
            look_up_program("extensionless", path, SHELL_PATH_EXT),
            PathBuf::from("extensionless")
        );
        // And a name nothing answers to stands for the spawn to report.
        assert_eq!(
            look_up_program("nothing-by-this-name", path, SHELL_PATH_EXT),
            PathBuf::from("nothing-by-this-name")
        );
    }

    /// Git hands back `C:/repo` on Windows too, and a path workmux prints is
    /// read in a Windows shell, where that is not a path anyone typed.
    #[test]
    fn a_path_git_printed_takes_this_platforms_separators() {
        let expected = if cfg!(windows) {
            r"C:\repo\worktree"
        } else {
            "C:/repo/worktree"
        };

        assert_eq!(
            path_from_git("C:/repo/worktree").to_string_lossy(),
            expected
        );
    }

    /// The spelling `path_from_git` undoes, kept for the commands workmux types
    /// into a pane: a POSIX shell reads a backslash as an escape, so a Windows
    /// path has to arrive with forward slashes or it names a file that is not
    /// there. On a POSIX host a backslash is part of a file name and stays.
    #[test]
    fn a_path_a_posix_shell_reads_loses_this_platforms_separators() {
        let expected = if cfg!(windows) {
            ".workmux/PROMPT.md"
        } else {
            r".workmux\PROMPT.md"
        };

        assert_eq!(path_for_posix_shell(r".workmux\PROMPT.md"), expected);
    }

    #[test]
    fn write_atomic_replaces_target_and_removes_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn write_atomic_durable_replaces_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_atomic_durable(&path, b"first").unwrap();
        write_atomic_durable(&path, b"second").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "second");
    }

    #[test]
    fn write_atomic_does_not_remove_fresh_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let temp = dir.path().join(".state.json.tmp.keep");
        fs::write(&temp, "pending").unwrap();

        write_atomic(&path, b"stored").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "stored");
        assert_eq!(fs::read_to_string(&temp).unwrap(), "pending");
    }

    #[test]
    fn expand_project_placeholder_replaces_every_occurrence() {
        let project = PathBuf::from("/x/y/myproj");
        let expanded = expand_project_placeholder("{project}-{project} ", &project).unwrap();
        assert_eq!(expanded, "myproj-myproj ");
    }

    #[test]
    fn expand_project_placeholder_leaves_plain_template_untouched() {
        let project = PathBuf::from("/x/y/myproj");
        let expanded = expand_project_placeholder("wm-", &project).unwrap();
        assert_eq!(expanded, "wm-");
    }

    #[test]
    fn expand_worktree_dir_tilde_and_project() {
        let home = PathBuf::from("/home/alice");
        let project = PathBuf::from("/Users/alice/code/myproj");
        let expanded =
            expand_worktree_dir_with_home("~/.workmux/{project}", &project, Some(&home)).unwrap();
        assert_eq!(expanded, PathBuf::from("/home/alice/.workmux/myproj"));
    }

    #[test]
    fn expand_worktree_dir_relative_with_project() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("{project}-wts", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/x/y/foo/foo-wts"));
    }

    #[test]
    fn expand_worktree_dir_absolute_with_project() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("/tmp/wts-{project}", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/tmp/wts-foo"));
    }

    #[test]
    fn expand_worktree_dir_relative_no_placeholder() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home(".worktrees", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/x/y/foo/.worktrees"));
    }

    #[test]
    fn expand_worktree_dir_absolute_no_placeholder_preserved() {
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("/abs/path", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_worktree_dir_absolute_with_dotdot_preserved() {
        // Absolute templates must be returned verbatim, matching prior
        // create.rs behavior. No lexical normalization.
        #[cfg(unix)]
        let template = "/tmp/foo/../bar";
        #[cfg(windows)]
        let template = r"C:\tmp\foo\..\bar";
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home(template, &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from(template));
    }

    #[test]
    fn expand_worktree_dir_tilde_only() {
        let home = PathBuf::from("/home/alice");
        let project = PathBuf::from("/x/y/foo");
        let expanded = expand_worktree_dir_with_home("~", &project, Some(&home)).unwrap();
        assert_eq!(expanded, PathBuf::from("/home/alice"));
    }

    #[test]
    fn expand_worktree_dir_unknown_placeholder_errors() {
        let project = PathBuf::from("/x/y/foo");
        let err =
            expand_worktree_dir_with_home("~/.workmux/{unknown}", &project, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("{unknown}"), "error should name token: {msg}");
    }

    #[test]
    fn expand_worktree_dir_unterminated_brace_errors() {
        let project = PathBuf::from("/x/y/foo");
        let err = expand_worktree_dir_with_home("/tmp/{project", &project, None).unwrap_err();
        assert!(err.to_string().contains("unterminated"));
    }

    #[test]
    fn expand_worktree_dir_validates_raw_template_only() {
        // Project name containing `{` must not trigger unknown-placeholder
        // detection because validation runs on the raw template.
        let project = PathBuf::from("/x/y/repo-{core}");
        let expanded = expand_worktree_dir_with_home("/tmp/{project}", &project, None).unwrap();
        assert_eq!(expanded, PathBuf::from("/tmp/repo-{core}"));
    }

    #[test]
    fn expand_tilde_basic() {
        let home = PathBuf::from("/home/u");
        assert_eq!(expand_tilde_with_home("~", Some(&home)), home);
        assert_eq!(
            expand_tilde_with_home("~/foo/bar", Some(&home)),
            PathBuf::from("/home/u/foo/bar")
        );
        assert_eq!(
            expand_tilde_with_home("/abs", Some(&home)),
            PathBuf::from("/abs")
        );
        assert_eq!(
            expand_tilde_with_home("rel", Some(&home)),
            PathBuf::from("rel")
        );
    }

    #[test]
    fn format_elapsed_secs_seconds() {
        assert_eq!(format_elapsed_secs(0), "0s");
        assert_eq!(format_elapsed_secs(30), "30s");
        assert_eq!(format_elapsed_secs(59), "59s");
    }

    #[test]
    fn format_elapsed_secs_minutes() {
        assert_eq!(format_elapsed_secs(60), "1m");
        assert_eq!(format_elapsed_secs(150), "2m");
        assert_eq!(format_elapsed_secs(3599), "59m");
    }

    #[test]
    fn format_elapsed_secs_hours() {
        assert_eq!(format_elapsed_secs(3600), "1h");
        assert_eq!(format_elapsed_secs(7200), "2h");
    }

    #[test]
    fn format_elapsed_secs_hours_and_minutes() {
        assert_eq!(format_elapsed_secs(3660), "1h 1m");
        assert_eq!(format_elapsed_secs(5400), "1h 30m");
        assert_eq!(format_elapsed_secs(86400), "24h");
    }

    #[test]
    fn format_elapsed_duration_seconds() {
        assert_eq!(format_elapsed_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed_duration(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn format_elapsed_duration_minutes_and_seconds() {
        assert_eq!(format_elapsed_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(
            format_elapsed_duration(Duration::from_secs(3599)),
            "59m 59s"
        );
    }

    #[test]
    fn format_elapsed_duration_hours_and_minutes() {
        assert_eq!(format_elapsed_duration(Duration::from_secs(3600)), "1h 00m");
        assert_eq!(format_elapsed_duration(Duration::from_secs(3661)), "1h 01m");
        assert_eq!(format_elapsed_duration(Duration::from_secs(7260)), "2h 01m");
    }

    #[test]
    fn format_compact_age_sub_minute() {
        assert_eq!(format_compact_age(0), "<1m");
        assert_eq!(format_compact_age(30), "<1m");
        assert_eq!(format_compact_age(59), "<1m");
    }

    #[test]
    fn format_compact_age_minutes() {
        assert_eq!(format_compact_age(60), "1m");
        assert_eq!(format_compact_age(300), "5m");
        assert_eq!(format_compact_age(3599), "59m");
    }

    #[test]
    fn format_compact_age_hours() {
        assert_eq!(format_compact_age(3600), "1h");
        assert_eq!(format_compact_age(7200), "2h");
        assert_eq!(format_compact_age(86399), "23h");
    }

    #[test]
    fn format_compact_age_days() {
        assert_eq!(format_compact_age(86400), "1d");
        assert_eq!(format_compact_age(259200), "3d");
        assert_eq!(format_compact_age(604799), "6d");
    }

    #[test]
    fn format_compact_age_weeks() {
        assert_eq!(format_compact_age(604800), "1w");
        assert_eq!(format_compact_age(1209600), "2w");
    }

    #[test]
    fn format_compact_age_months() {
        assert_eq!(format_compact_age(30 * 86400), "1mo");
        assert_eq!(format_compact_age(60 * 86400), "2mo");
        assert_eq!(format_compact_age(364 * 86400), "12mo");
    }

    #[test]
    fn format_compact_age_years() {
        assert_eq!(format_compact_age(365 * 86400), "1y");
        assert_eq!(format_compact_age(730 * 86400), "2y");
    }

    #[test]
    #[cfg(windows)]
    fn git_path_strips_windows_verbatim_prefix() {
        assert_eq!(
            &*git_path(Path::new(r"\\?\C:\repo\.git")),
            OsStr::new(r"C:\repo\.git")
        );
        assert_eq!(
            &*git_path(Path::new(r"\\?\UNC\server\share\.git")),
            OsStr::new(r"\\server\share\.git")
        );
        assert_eq!(
            &*git_path(Path::new(r"C:\repo\.git")),
            OsStr::new(r"C:\repo\.git")
        );
    }

    #[test]
    #[cfg(unix)]
    fn git_path_keeps_unix_paths() {
        assert_eq!(
            &*git_path(Path::new("/repo/.git")),
            OsStr::new("/repo/.git")
        );
    }

    #[test]
    fn open_shared_does_not_block_replacing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"first").unwrap();

        // Readers may still hold the file while a writer replaces it; on Windows
        // that only works because `open_shared` allows delete sharing.
        let reader = open_shared(&path).unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(read_shared(&path).unwrap(), "second");
        drop(reader);
        assert_eq!(read_shared(&path).unwrap(), "second");
    }

    #[test]
    fn normalize_path_collapses_parent_dir() {
        let p = Path::new("/Users/test/repo/../wm/handle");
        assert_eq!(normalize_path(p), PathBuf::from("/Users/test/wm/handle"));
    }

    #[test]
    fn normalize_path_collapses_multiple_parent_dirs() {
        let p = Path::new("/a/b/c/../../d");
        assert_eq!(normalize_path(p), PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_path_strips_cur_dir() {
        let p = Path::new("/a/./b/./c");
        assert_eq!(normalize_path(p), PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_path_preserves_leading_parent() {
        let p = Path::new("../wm/handle");
        assert_eq!(normalize_path(p), PathBuf::from("../wm/handle"));
    }

    #[test]
    fn normalize_path_no_op_for_clean_path() {
        let p = Path::new("/Users/test/wm/handle");
        assert_eq!(normalize_path(p), PathBuf::from("/Users/test/wm/handle"));
    }

    #[test]
    fn normalize_path_root_parent_stays_at_root() {
        let p = Path::new("/../foo");
        assert_eq!(normalize_path(p), PathBuf::from("/foo"));
    }
}
