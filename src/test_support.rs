//! Test-only helpers shared across modules.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub const ISOLATED_TEST_ENV: &str = "WM_ISOLATED_TEST";
pub const ISOLATED_TEST_CANARY: &str = "WM_ISOLATED_TEST_EXECUTED";

/// Host directory for fixtures that must be absolute for container-style
/// tooling. `/tmp` is not absolute on Windows, so those fixtures root
/// themselves at a drive instead.
pub const FIXTURE_ROOT: &str = if cfg!(windows) {
    "C:/workmux-test"
} else {
    "/tmp"
};

pub fn is_isolated_child(test_name: &str) -> bool {
    std::env::var_os(ISOLATED_TEST_ENV).as_deref() == Some(std::ffi::OsStr::new(test_name))
}

/// Canonical form of `dir` as `std::env::current_dir` reports it.
///
/// `Path::canonicalize` returns `\\?\`-prefixed paths on Windows while
/// `current_dir` does not, so the two only compare equal once the prefix is
/// stripped.
pub fn canonical_dir(dir: &Path) -> PathBuf {
    let canonical = dir.canonicalize().expect("directory should canonicalize");
    PathBuf::from(crate::util::git_path(&canonical).into_owned())
}

/// Whether this process may create file symlinks.
///
/// Windows needs Developer Mode or `SeCreateSymbolicLinkPrivilege`, which
/// sandboxed test runners usually lack. Probed once per process: a denied
/// `CreateSymbolicLink` can take tens of seconds on a Defender-heavy machine.
pub fn file_symlinks_supported(dir: &Path) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| probe_file_symlinks(dir))
}

fn probe_file_symlinks(dir: &Path) -> bool {
    let target = dir.join("workmux-symlink-probe-target");
    let link = dir.join("workmux-symlink-probe-link");
    if std::fs::write(&target, b"").is_err() {
        return false;
    }
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(&target, &link).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(&target, &link).is_ok();
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&target);
    created
}

pub fn run_isolated_test(test_name: &str, cwd: &Path, envs: &[(&str, &Path)]) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg(test_name)
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(cwd)
        .env(ISOLATED_TEST_ENV, test_name);
    clear_local_git_env(&mut command);

    for (key, value) in envs {
        command.env(key, value);
    }

    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolated test {test_name} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );
    assert!(
        stdout.contains(ISOLATED_TEST_CANARY),
        "isolated test {test_name} did not execute\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
}

pub fn run_git(repo: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run a git command and return its trimmed stdout.
pub fn run_git_output(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn init_repo(dir: &Path) {
    let mut command = Command::new("git");
    clear_local_git_env(&mut command);
    let output = command
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .output()
        .expect("git init should run");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test User"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    run_git(dir, &["add", "README.md"]);
    run_git(dir, &["commit", "-m", "initial"]);
}

pub(crate) fn clear_local_git_env(command: &mut Command) {
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_DIR",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_SHALLOW_FILE",
        "GIT_WORK_TREE",
    ] {
        command.env_remove(key);
    }
    command.env_remove("GIT_CONFIG_COUNT");
    command.env_remove("GIT_CONFIG_PARAMETERS");
    for i in 0..32 {
        command.env_remove(format!("GIT_CONFIG_KEY_{i}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{i}"));
    }
}
