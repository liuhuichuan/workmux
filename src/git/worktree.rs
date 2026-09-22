use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cmd::Cmd;
use crate::config::MuxMode;

use super::WorktreeNotFound;
use super::branch::unset_branch_upstream_in;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktreeAttachment {
    Headless,
    Multiplexer,
    Legacy,
    Unknown,
}

impl WorktreeAttachment {
    pub fn manages_mux(self) -> bool {
        matches!(self, Self::Multiplexer | Self::Legacy)
    }
}

/// Check if a worktree already exists for a branch
#[allow(dead_code)]
pub fn worktree_exists(branch_name: &str) -> Result<bool> {
    worktree_exists_in(branch_name, None)
}

/// Check if a worktree already exists for a branch in a specific workdir
pub fn worktree_exists_in(branch_name: &str, workdir: Option<&Path>) -> Result<bool> {
    match get_worktree_path_in(branch_name, workdir) {
        Ok(_) => Ok(true),
        Err(e) => {
            // Check if this is a WorktreeNotFound error
            if e.is::<WorktreeNotFound>() {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Create a new git worktree
#[allow(dead_code)]
pub fn create_worktree(
    worktree_path: &Path,
    branch_name: &str,
    create_branch: bool,
    base_branch: Option<&str>,
    track_upstream: bool,
) -> Result<()> {
    create_worktree_in(
        worktree_path,
        branch_name,
        create_branch,
        base_branch,
        track_upstream,
        None,
    )
}

/// Create a new git worktree from a specific workdir
pub fn create_worktree_in(
    worktree_path: &Path,
    branch_name: &str,
    create_branch: bool,
    base_branch: Option<&str>,
    track_upstream: bool,
    workdir: Option<&Path>,
) -> Result<()> {
    // Adding a worktree changes what a listing would print.
    super::forget_repository_readings();

    let path = crate::util::git_path(worktree_path);
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("Invalid worktree path"))?;

    let mut cmd = Cmd::new("git").arg("worktree").arg("add");
    if let Some(path) = workdir {
        cmd = cmd.workdir(path);
    }

    if create_branch {
        cmd = cmd.arg("-b").arg(branch_name).arg(path_str);
        if let Some(base) = base_branch {
            cmd = cmd.arg(base);
        }
    } else {
        cmd = cmd.arg(path_str).arg(branch_name);
    }

    cmd.run().context("Failed to create worktree")?;

    if create_branch && !track_upstream {
        unset_branch_upstream_in(branch_name, workdir)?;
    }

    Ok(())
}

/// Move a registered worktree to a new path using `git worktree move`.
///
/// Git updates the worktree admin dir's `gitdir` file and the worktree's
/// `.git` pointer. Note: the admin dir itself (`.git/worktrees/<basename>/`)
/// keeps its original basename; workmux does not rely on that path shape.
pub fn move_worktree(old_path: &Path, new_path: &Path) -> Result<()> {
    // Moving a worktree changes what a listing would print.
    super::forget_repository_readings();

    let old_path = crate::util::git_path(old_path);
    let new_path = crate::util::git_path(new_path);
    let old = old_path
        .to_str()
        .ok_or_else(|| anyhow!("Invalid old worktree path"))?;
    let new = new_path
        .to_str()
        .ok_or_else(|| anyhow!("Invalid new worktree path"))?;
    Cmd::new("git")
        .args(&["worktree", "move", old, new])
        .run()
        .with_context(|| format!("Failed to move worktree {} -> {}", old, new))?;
    Ok(())
}

/// Migrate all `workmux.worktree.<old_handle>.*` config entries to
/// `workmux.worktree.<new_handle>.*`, then remove the old section.
pub fn migrate_worktree_meta(old_handle: &str, new_handle: &str) -> Result<()> {
    if old_handle == new_handle {
        return Ok(());
    }
    let old_section = format!("workmux.worktree.{}", old_handle);
    let regex_pattern = format!(r"^{}\.", regex::escape(&old_section));
    let output = Cmd::new("git")
        .args(&["config", "--local", "--get-regexp", &regex_pattern])
        .run_and_capture_stdout()
        .unwrap_or_default();

    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let Some(suffix) = key.strip_prefix(&format!("{}.", old_section)) else {
            continue;
        };
        let new_key = format!("workmux.worktree.{}.{}", new_handle, suffix);
        Cmd::new("git")
            .args(&["config", "--local", &new_key, value])
            .run()
            .with_context(|| format!("Failed to set {}", new_key))?;
    }

    // Remove the old section (ignore "no such section" errors).
    let _ = Cmd::new("git")
        .args(&["config", "--local", "--remove-section", &old_section])
        .run();

    Ok(())
}

/// Locate a linked-worktree registration for an exact registered path.
pub fn linked_worktree_registration_in(
    worktree_path: &Path,
    git_common_dir: &Path,
) -> Result<Option<PathBuf>> {
    let registered = list_worktrees_in(Some(git_common_dir))?
        .into_iter()
        .any(|(path, _)| path == worktree_path);
    if !registered {
        return Ok(None);
    }

    let registrations_dir = git_common_dir.join("worktrees");
    let registrations_dir = registrations_dir
        .canonicalize()
        .context("Failed to resolve linked-worktree registrations directory")?;
    let expected_gitdir = worktree_path.join(".git");
    let mut matching = Vec::new();
    for entry in std::fs::read_dir(&registrations_dir)
        .context("Failed to inspect linked-worktree registrations")?
    {
        let entry = entry.context("Failed to inspect linked-worktree registration")?;
        let metadata = entry
            .file_type()
            .context("Failed to inspect linked-worktree registration type")?;
        if !metadata.is_dir() || metadata.is_symlink() {
            continue;
        }
        let admin_dir = entry
            .path()
            .canonicalize()
            .context("Failed to resolve linked-worktree registration")?;
        if admin_dir.parent() != Some(registrations_dir.as_path()) {
            continue;
        }
        let backlink = match std::fs::read_to_string(admin_dir.join("gitdir")) {
            Ok(backlink) => PathBuf::from(backlink.trim()),
            Err(_) => continue,
        };
        if backlink == expected_gitdir {
            matching.push(admin_dir);
        }
    }

    match matching.len() {
        1 => Ok(matching.pop()),
        0 => Err(anyhow!(
            "Registered worktree '{}' has no matching admin directory",
            worktree_path.display()
        )),
        _ => Err(anyhow!(
            "Registered worktree '{}' has multiple admin directories",
            worktree_path.display()
        )),
    }
}

pub fn worktree_registration_exists_in(
    worktree_path: &Path,
    git_common_dir: &Path,
) -> Result<bool> {
    Ok(list_worktrees_in(Some(git_common_dir))?
        .into_iter()
        .any(|(path, _)| path == worktree_path))
}

/// Prune stale worktree metadata.
pub fn prune_worktrees_in(git_common_dir: &Path) -> Result<()> {
    // Pruning drops worktrees a listing would still print.
    super::forget_repository_readings();

    Cmd::new("git")
        .workdir(git_common_dir)
        .args(&["worktree", "prune"])
        .run()
        .context("Failed to prune worktrees")?;
    Ok(())
}

/// Parse the output of `git worktree list --porcelain`
pub(super) fn parse_worktree_list_porcelain(output: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut worktrees = Vec::new();
    for block in output.trim().split("\n\n") {
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;

        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(crate::util::path_from_git(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if line.trim() == "detached" {
                branch = Some("(detached)".to_string());
            }
        }

        if let (Some(p), Some(b)) = (path, branch) {
            worktrees.push((p, b));
        }
    }
    Ok(worktrees)
}

/// Get the path to a worktree for a given branch
pub fn get_worktree_path(branch_name: &str) -> Result<PathBuf> {
    get_worktree_path_in(branch_name, None)
}

/// Get the path to a worktree for a given branch in a specific workdir
pub fn get_worktree_path_in(branch_name: &str, workdir: Option<&Path>) -> Result<PathBuf> {
    let worktrees = list_worktrees_in(workdir)?;

    for (path, branch) in worktrees {
        if branch == branch_name {
            return Ok(path);
        }
    }

    Err(WorktreeNotFound(branch_name.to_string()).into())
}

/// Find a worktree by handle (directory name) or branch name.
/// Tries handle first, then falls back to branch lookup.
/// Returns both the path and the branch name checked out in that worktree.
pub fn find_worktree(name: &str) -> Result<(PathBuf, String)> {
    find_worktree_in(name, None)
}

/// Find a worktree by handle or branch name in a specific workdir.
pub fn find_worktree_in(name: &str, workdir: Option<&Path>) -> Result<(PathBuf, String)> {
    let worktrees = list_worktrees_in(workdir)?;

    // First: try to match by handle (directory name)
    for (path, branch) in &worktrees {
        if let Some(dir_name) = path.file_name()
            && dir_name.to_string_lossy() == name
        {
            return Ok((path.clone(), branch.clone()));
        }
    }

    // Fallback: try to match by branch name
    for (path, branch) in worktrees {
        if branch == name {
            return Ok((path, branch));
        }
    }

    Err(WorktreeNotFound(name.to_string()).into())
}

/// List all worktrees with their branches
pub fn list_worktrees() -> Result<Vec<(PathBuf, String)>> {
    list_worktrees_in(None)
}

/// List all worktrees with their branches, optionally in a specific workdir
pub fn list_worktrees_in(workdir: Option<&Path>) -> Result<Vec<(PathBuf, String)>> {
    let list =
        super::worktrees().get_or_take(super::asked_from(workdir), Instant::now(), || {
            let cmd = Cmd::new("git").args(&["worktree", "list", "--porcelain"]);
            let cmd = match workdir {
                Some(path) => cmd.workdir(path),
                None => cmd,
            };
            cmd.run_and_capture_stdout()
                .context("Failed to list worktrees")
        })?;
    parse_worktree_list_porcelain(&list)
}

/// Store per-worktree metadata in git config.
#[allow(dead_code)]
pub fn set_worktree_meta(handle: &str, key: &str, value: &str) -> Result<()> {
    set_worktree_meta_in(handle, key, value, None)
}

/// Store per-worktree metadata in git config in a specific workdir.
pub fn set_worktree_meta_in(
    handle: &str,
    key: &str,
    value: &str,
    workdir: Option<&Path>,
) -> Result<()> {
    let config_key = format!("workmux.worktree.{}.{}", handle, key);
    let cmd = Cmd::new("git").args(&["config", "--local", &config_key, value]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run()
        .with_context(|| format!("Failed to set worktree metadata {}.{}", handle, key))?;
    Ok(())
}

/// Retrieve per-worktree metadata from git config.
/// Returns None if the key doesn't exist.
#[allow(dead_code)]
pub fn get_worktree_meta(handle: &str, key: &str) -> Option<String> {
    get_worktree_meta_in(handle, key, None)
}

/// Retrieve per-worktree metadata from git config in a specific workdir.
pub fn get_worktree_meta_in(handle: &str, key: &str, workdir: Option<&Path>) -> Option<String> {
    let config_key = format!("workmux.worktree.{}.{}", handle, key);
    let cmd = Cmd::new("git").args(&["config", "--local", "--get", &config_key]);
    let cmd = match workdir {
        Some(path) => cmd.workdir(path),
        None => cmd,
    };
    cmd.run_and_capture_stdout().ok().filter(|s| !s.is_empty())
}

pub fn get_worktree_attachment(handle: &str) -> WorktreeAttachment {
    get_worktree_attachment_in(handle, None)
}

pub fn get_worktree_attachment_in(handle: &str, workdir: Option<&Path>) -> WorktreeAttachment {
    match get_worktree_meta_in(handle, "attachment", workdir).as_deref() {
        Some("headless") => WorktreeAttachment::Headless,
        Some("multiplexer") => WorktreeAttachment::Multiplexer,
        Some(_) => WorktreeAttachment::Unknown,
        None => WorktreeAttachment::Legacy,
    }
}

pub fn set_worktree_attachment_in(
    handle: &str,
    attachment: WorktreeAttachment,
    workdir: Option<&Path>,
) -> Result<()> {
    let value = match attachment {
        WorktreeAttachment::Headless => "headless",
        WorktreeAttachment::Multiplexer => "multiplexer",
        WorktreeAttachment::Legacy | WorktreeAttachment::Unknown => {
            return Err(anyhow!("Only explicit attachment state can be persisted"));
        }
    };
    set_worktree_meta_in(handle, "attachment", value, workdir)
}

pub fn get_worktree_window_token(handle: &str) -> Option<String> {
    get_worktree_meta_in(handle, "window-token", None)
}

pub fn get_worktree_window_token_in(handle: &str, workdir: Option<&Path>) -> Option<String> {
    get_worktree_meta_in(handle, "window-token", workdir)
}

pub fn ensure_worktree_window_token_in(handle: &str, workdir: Option<&Path>) -> Result<String> {
    if let Some(token) = get_worktree_window_token_in(handle, workdir) {
        return Ok(token);
    }

    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("Failed to generate worktree window token")?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    set_worktree_meta_in(handle, "window-token", &token, workdir)?;
    Ok(token)
}

pub fn get_worktree_target_window(handle: &str) -> Option<String> {
    get_worktree_target_window_in(handle, None)
}

pub fn get_worktree_target_window_in(handle: &str, workdir: Option<&Path>) -> Option<String> {
    get_worktree_meta_in(handle, "target-window", workdir)
}

pub fn get_worktree_target_session(handle: &str) -> Option<String> {
    get_worktree_target_session_in(handle, None)
}

pub fn get_worktree_target_session_in(handle: &str, workdir: Option<&Path>) -> Option<String> {
    get_worktree_meta_in(handle, "target-session", workdir)
}

pub fn get_worktree_window_session(handle: &str) -> Option<String> {
    get_worktree_window_session_in(handle, None)
}

pub fn get_worktree_window_session_in(handle: &str, workdir: Option<&Path>) -> Option<String> {
    get_worktree_meta_in(handle, "window-session", workdir)
}

/// Determine the tmux mode for a worktree from git metadata.
/// Returns None if no metadata is found (legacy worktree).
pub fn get_worktree_mode_opt(handle: &str) -> Option<MuxMode> {
    get_worktree_mode_opt_in(handle, None)
}

/// Determine the tmux mode for a worktree from git metadata in a specific workdir.
pub fn get_worktree_mode_opt_in(handle: &str, workdir: Option<&Path>) -> Option<MuxMode> {
    match get_worktree_meta_in(handle, "mode", workdir) {
        Some(mode) if mode == "session" => Some(MuxMode::Session),
        Some(mode) if mode == "window" => Some(MuxMode::Window),
        _ => None,
    }
}

/// Determine the tmux mode for a worktree from git metadata.
/// Falls back to Window mode if no metadata is found (backward compatibility).
pub fn get_worktree_mode(handle: &str) -> MuxMode {
    get_worktree_mode_opt(handle).unwrap_or(MuxMode::Window)
}

/// Every per-worktree setting workmux keeps in git config, read in one go.
///
/// The settings live under `workmux.worktree.<handle>.<key>`, and a listing
/// wants six of them. Reading one key per `git config` is one process per key,
/// and on Windows a process is most of a tenth of a second that every listing,
/// every dashboard refresh and every sidebar poll pays again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeMeta {
    settings: HashMap<String, HashMap<String, String>>,
}

impl WorktreeMeta {
    /// Read every `workmux.worktree.*` setting visible from `workdir`.
    pub fn load_in(workdir: Option<&Path>) -> Self {
        let cmd =
            Cmd::new("git").args(&["config", "--local", "--get-regexp", r"^workmux\.worktree\."]);
        let cmd = match workdir {
            Some(path) => cmd.workdir(path),
            None => cmd,
        };
        let output = cmd.run_and_capture_stdout().unwrap_or_default();
        Self::parse(&output)
    }

    /// Read the settings out of `git config --get-regexp` output.
    ///
    /// Each line is `<key> <value>`, and the key names one setting of one
    /// worktree. The handle is everything up to the last dot, so a handle that
    /// contains dots -- a branch named after a version, say -- keeps all of
    /// itself.
    fn parse(output: &str) -> Self {
        let mut settings: HashMap<String, HashMap<String, String>> = HashMap::new();
        for line in output.lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            let Some(rest) = key.strip_prefix("workmux.worktree.") else {
                continue;
            };
            let Some((handle, name)) = rest.rsplit_once('.') else {
                continue;
            };
            if handle.is_empty() || name.is_empty() {
                continue;
            }
            settings
                .entry(handle.to_string())
                .or_default()
                .insert(name.to_string(), value.trim().to_string());
        }
        Self { settings }
    }

    /// One setting's values, by worktree handle.
    pub fn key(&self, name: &str) -> HashMap<String, String> {
        self.settings
            .iter()
            .filter_map(|(handle, settings)| {
                settings
                    .get(name)
                    .map(|value| (handle.clone(), value.clone()))
            })
            .collect()
    }

    /// The mux mode of every worktree that recorded one.
    ///
    /// An unrecognized mode reads as a window, which is what the caller's
    /// default is: a worktree whose mode was never written is a window too.
    pub fn modes(&self) -> HashMap<String, MuxMode> {
        self.settings
            .iter()
            .filter_map(|(handle, settings)| {
                settings.get("mode").map(|mode| {
                    let mode = if mode == "session" {
                        MuxMode::Session
                    } else {
                        MuxMode::Window
                    };
                    (handle.clone(), mode)
                })
            })
            .collect()
    }
}

/// Remove worktree metadata using an explicitly identified repository.
pub fn remove_worktree_meta_at(handle: &str, git_common_dir: &Path) -> Result<()> {
    let section = format!("workmux.worktree.{handle}");
    let key_pattern = format!(r"^{}\.", regex::escape(&section));
    let mut probe = super::unattended_git(Some(git_common_dir))?;
    let probe_output = probe
        .args([
            "config",
            "--local",
            "--name-only",
            "--get-regexp",
            &key_pattern,
        ])
        .output()
        .context("Failed to inspect Workmux metadata")?;
    if !probe_output.status.success() {
        if probe_output.status.code() == Some(1) {
            return Ok(());
        }
        return Err(anyhow!(
            "Failed to inspect Workmux metadata: {}",
            String::from_utf8_lossy(&probe_output.stderr).trim()
        ));
    }

    let mut remove = super::unattended_git(Some(git_common_dir))?;
    let remove_output = remove
        .args(["config", "--local", "--remove-section", &section])
        .output()
        .context("Failed to remove Workmux metadata")?;
    if !remove_output.status.success() {
        return Err(anyhow!(
            "Failed to remove Workmux metadata: {}",
            String::from_utf8_lossy(&remove_output.stderr).trim()
        ));
    }
    Ok(())
}

/// Main repository name and path used by agent selectors and status JSON.
pub fn project_identity(path: &Path) -> (Option<String>, Option<PathBuf>) {
    let Ok(root) = get_main_worktree_root_in(Some(path)) else {
        return (None, None);
    };
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    (name, Some(root))
}

/// Get the main worktree root directory (not a linked worktree)
///
/// For bare repositories with linked worktrees, this returns the bare repo path.
/// For regular repositories, this returns the first worktree that exists on disk.
pub fn get_main_worktree_root() -> Result<PathBuf> {
    get_main_worktree_root_in(None)
}

/// Get the main worktree root directory from a specific workdir
pub fn get_main_worktree_root_in(workdir: Option<&Path>) -> Result<PathBuf> {
    let list_str =
        super::worktrees().get_or_take(super::asked_from(workdir), Instant::now(), || {
            let cmd = Cmd::new("git").args(&["worktree", "list", "--porcelain"]);
            let cmd = match workdir {
                Some(path) => cmd.workdir(path),
                None => cmd,
            };
            cmd.run_and_capture_stdout()
                .context("Failed to list worktrees while locating main worktree")
        })?;

    // Check if this is a bare repo setup.
    // The first entry in `git worktree list` is always the main worktree or bare repo.
    // For bare repos, it looks like:
    //   worktree /path/to/.bare
    //   bare
    if let Some(first_block) = list_str.trim().split("\n\n").next() {
        let mut path: Option<PathBuf> = None;
        let mut is_bare = false;

        for line in first_block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(crate::util::path_from_git(p));
            } else if line.trim() == "bare" {
                is_bare = true;
            }
        }

        // If this is a bare repo, return its path immediately.
        // Git commands like `git worktree prune` work correctly from bare repo directories.
        if is_bare && let Some(p) = path {
            return Ok(p);
        }
    }

    // Not a bare repo - find the first worktree that exists on disk.
    // This handles edge cases where a worktree was deleted but not yet pruned.
    let worktrees = parse_worktree_list_porcelain(&list_str)?;

    for (path, _) in &worktrees {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    // Fallback: return the first worktree even if it doesn't exist
    if let Some((path, _)) = worktrees.first() {
        Ok(path.clone())
    } else {
        Err(anyhow!("No main worktree found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use std::path::PathBuf;
    use std::process::Command;

    /// Git prints a listing's paths with forward slashes on every platform.
    /// They are read back as paths of this machine -- compared with the paths
    /// workmux builds, printed in `workmux list --json`, opened on disk.
    #[test]
    fn a_listed_path_is_written_the_way_this_machine_writes_it() {
        let listed = "worktree C:/repo/feature\nbranch refs/heads/feature\n";

        let worktrees = parse_worktree_list_porcelain(listed).unwrap();

        let expected = if cfg!(windows) {
            r"C:\repo\feature"
        } else {
            "C:/repo/feature"
        };
        assert_eq!(worktrees[0].0.to_string_lossy(), expected);
    }

    /// Every key a listing asks for comes out of one reading of git config.
    /// The keys used to be fetched one `git config` process at a time.
    #[test]
    fn worktree_meta_answers_every_key_from_one_reading() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        set_worktree_meta_in("feature", "mode", "session", Some(&repo)).unwrap();
        set_worktree_meta_in("feature", "target-session", "wm-feature", Some(&repo)).unwrap();
        set_worktree_meta_in("other", "mode", "window", Some(&repo)).unwrap();
        set_worktree_meta_in("other", "window-token", "token with spaces", Some(&repo)).unwrap();

        let meta = WorktreeMeta::load_in(Some(&repo));

        assert_eq!(
            meta.modes(),
            HashMap::from([
                ("feature".to_string(), MuxMode::Session),
                ("other".to_string(), MuxMode::Window),
            ])
        );
        assert_eq!(
            meta.key("target-session"),
            HashMap::from([("feature".to_string(), "wm-feature".to_string())])
        );
        assert_eq!(
            meta.key("window-token"),
            HashMap::from([("other".to_string(), "token with spaces".to_string())])
        );
        assert!(meta.key("attachment").is_empty());
    }

    /// A repository that never recorded a setting reports none, rather than
    /// whatever the last repository did.
    #[test]
    fn worktree_meta_is_empty_without_settings() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        let meta = WorktreeMeta::load_in(Some(&repo));

        assert_eq!(meta, WorktreeMeta::default());
    }

    /// The handle is everything up to the last dot, so a handle that contains
    /// dots keeps all of itself.
    #[test]
    fn worktree_meta_keeps_dots_inside_a_handle() {
        let output = "workmux.worktree.v1.2.mode session\n\
                      workmux.worktree.v1.2.window-token w1\n";

        let meta = WorktreeMeta::parse(output);

        assert_eq!(
            meta.modes(),
            HashMap::from([("v1.2".to_string(), MuxMode::Session)])
        );
        assert_eq!(
            meta.key("window-token"),
            HashMap::from([("v1.2".to_string(), "w1".to_string())])
        );
    }

    /// Settings outside the section, and lines that name no setting, are not
    /// worktree metadata.
    #[test]
    fn worktree_meta_ignores_what_is_not_a_worktree_setting() {
        let output = "workmux.hook-shell sh\n\
                      workmux.worktree.\n\
                      workmux.worktree.feature. mode\n\
                      other.setting value\n";

        assert_eq!(WorktreeMeta::parse(output), WorktreeMeta::default());
    }

    /// A setting written twice keeps the last value, which is the one git
    /// itself would report for the key.
    #[test]
    fn worktree_meta_keeps_the_last_of_a_repeated_setting() {
        let output = "workmux.worktree.feature.mode window\n\
                      workmux.worktree.feature.mode session\n";

        assert_eq!(
            WorktreeMeta::parse(output).modes(),
            HashMap::from([("feature".to_string(), MuxMode::Session)])
        );
    }

    /// A worktree added in this process is in the next listing, rather than
    /// the listing this process took before the add.
    #[test]
    fn a_created_worktree_is_listed_right_after() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        let before = list_worktrees_in(Some(&repo)).unwrap();
        let worktree_path = temp.path().join("repo__worktrees").join("feature");
        create_worktree_in(
            &worktree_path,
            "feature",
            true,
            Some("main"),
            false,
            Some(&repo),
        )
        .unwrap();
        let after = list_worktrees_in(Some(&repo)).unwrap();

        let added = test_support::canonical_dir(&worktree_path);
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 2);
        assert!(
            after
                .iter()
                .any(|(path, branch)| path == &added && branch == "feature"),
            "the worktree added in this process is not in the listing: {after:?}"
        );
    }

    /// A worktree pruned in this process is gone from the next listing.
    #[test]
    fn a_pruned_worktree_is_gone_from_the_next_listing() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        let worktree_path = temp.path().join("repo__worktrees").join("feature");
        create_worktree_in(
            &worktree_path,
            "feature",
            true,
            Some("main"),
            false,
            Some(&repo),
        )
        .unwrap();
        assert_eq!(list_worktrees_in(Some(&repo)).unwrap().len(), 2);

        std::fs::remove_dir_all(&worktree_path).unwrap();
        prune_worktrees_in(&repo.join(".git")).unwrap();

        let after = list_worktrees_in(Some(&repo)).unwrap();
        assert_eq!(after.len(), 1);
        assert!(
            after
                .iter()
                .all(|(path, _)| path.file_name() != Some(std::ffi::OsStr::new("feature"))),
            "the pruned worktree is still in the listing: {after:?}"
        );
    }

    #[test]
    fn create_worktree_in_accepts_canonicalized_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        test_support::init_repo(&repo);

        // `canonicalize` produces extended-length paths on Windows, which Git
        // rejects when they are used as `git worktree add` arguments.
        let base = temp.path().canonicalize().unwrap();
        let worktree_path = base.join("wts").join("feature");
        create_worktree_in(
            &worktree_path,
            "feature",
            true,
            Some("main"),
            false,
            Some(&repo),
        )
        .unwrap();

        let listed = Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(listed.status.success());
        assert!(
            String::from_utf8_lossy(&listed.stdout).contains("branch refs/heads/feature"),
            "worktree was not registered: {}",
            String::from_utf8_lossy(&listed.stdout)
        );
    }

    #[test]
    fn create_worktree_in_uses_explicit_repo_not_process_cwd() {
        const TEST_NAME: &str =
            "git::worktree::tests::create_worktree_in_uses_explicit_repo_not_process_cwd";
        if !test_support::is_isolated_child(TEST_NAME) {
            let temp = tempfile::tempdir().unwrap();
            let repo_a = temp.path().join("repo-a");
            let repo_b = temp.path().join("repo-b");
            std::fs::create_dir_all(&repo_a).unwrap();
            std::fs::create_dir_all(&repo_b).unwrap();
            test_support::init_repo(&repo_a);
            test_support::init_repo(&repo_b);

            test_support::run_isolated_test(TEST_NAME, &repo_a, &[("WM_TEST_TEMP", temp.path())]);
            return;
        }

        println!("{}", test_support::ISOLATED_TEST_CANARY);
        let temp = std::env::var_os("WM_TEST_TEMP").map(PathBuf::from).unwrap();
        let repo_a = temp.join("repo-a");
        let repo_b = temp.join("repo-b");
        assert_eq!(
            std::env::current_dir().unwrap(),
            test_support::canonical_dir(&repo_a)
        );

        let worktree_path = temp.join("repo-b__worktrees").join("feature");
        create_worktree_in(
            &worktree_path,
            "feature",
            true,
            Some("main"),
            false,
            Some(&repo_b),
        )
        .unwrap();
        set_worktree_meta_in("feature", "mode", "window", Some(&repo_b)).unwrap();

        let repo_b_worktrees = Command::new("git")
            .current_dir(&repo_b)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(repo_b_worktrees.status.success());
        let repo_b_list = String::from_utf8(repo_b_worktrees.stdout).unwrap();
        assert!(repo_b_list.contains("branch refs/heads/feature"));
        assert_eq!(
            get_worktree_meta_in("feature", "mode", Some(&repo_b)).as_deref(),
            Some("window")
        );

        let repo_a_has_branch = Command::new("git")
            .current_dir(&repo_a)
            .args(["rev-parse", "--verify", "--quiet", "feature"])
            .status()
            .unwrap()
            .success();
        assert!(!repo_a_has_branch);
    }
}
