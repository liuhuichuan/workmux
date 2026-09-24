use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::MuxMode;
use crate::multiplexer::handle::mode_label;
use crate::multiplexer::{AgentPane, Multiplexer, ResumeMode, util::prefixed};
use crate::state::StateStore;
use crate::util::canon_or_self;
use crate::{git, naming};
use tracing::{info, warn};

use super::context::WorkflowContext;
use super::types::{RenameResult, SetupOptions};

/// Rename a worktree, its tmux window/session, per-worktree git metadata,
/// agent state files, and (optionally) the branch.
pub fn rename(
    user_target: &str,
    new_name: &str,
    rename_branch: bool,
    context: &WorkflowContext,
) -> Result<RenameResult> {
    // 1. Resolve source worktree. `user_target` may be a handle OR a branch;
    //    `find_worktree` handles both. Always derive the authoritative handle
    //    from the worktree's directory basename to keep metadata/tmux/state
    //    migrations consistent regardless of what the user typed.
    let (old_path, branch_name) = git::find_worktree(user_target).with_context(|| {
        format!(
            "Worktree '{}' not found. Use 'workmux list' to see available worktrees.",
            user_target
        )
    })?;

    let old_handle = old_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            anyhow!(
                "Could not derive handle from worktree path: {}",
                old_path.display()
            )
        })?
        .to_string();

    // 2. Reject main worktree
    if old_path == context.main_worktree_root {
        return Err(anyhow!("Cannot rename the main worktree"));
    }

    // 3. Derive new handle (slugify + validate). Treat new_name as an explicit
    //    handle override, matching `add --name` semantics: prefix is bypassed,
    //    slugify still runs to keep it filesystem/tmux safe.
    let new_handle = naming::derive_handle(&branch_name, Some(new_name), &context.config)?;

    if new_handle == old_handle && !rename_branch {
        return Err(anyhow!(
            "Nothing to rename: new name '{}' matches current handle",
            new_handle
        ));
    }

    // 4. Detached HEAD + branch rename is nonsensical
    if rename_branch && branch_name == "(detached)" {
        return Err(anyhow!(
            "Cannot rename the branch of a detached HEAD worktree. \
             Omit --branch to rename only the worktree and tmux window."
        ));
    }

    // 5. Determine new worktree path
    let parent = old_path
        .parent()
        .ok_or_else(|| anyhow!("Cannot determine parent directory of worktree"))?;
    let new_path = parent.join(&new_handle);

    let new_branch = if rename_branch {
        // Keep it simple: the new branch name equals the new handle.
        // Users wanting asymmetric names can run `git branch -m` separately.
        Some(new_handle.clone())
    } else {
        None
    };

    // 6. Collision preflight
    if new_handle != old_handle {
        if new_path.exists() {
            return Err(anyhow!(
                "Target path already exists: {}",
                new_path.display()
            ));
        }
        if git::find_worktree(&new_handle).is_ok() {
            return Err(anyhow!(
                "Another worktree with handle '{}' already exists",
                new_handle
            ));
        }
    }

    if let Some(ref b) = new_branch
        && b != &branch_name
        && git::branch_exists(b).unwrap_or(false)
    {
        return Err(anyhow!("Branch '{}' already exists", b));
    }

    // 7. tmux target collision check (only if handle is changing)
    let mode = git::get_worktree_mode(&old_handle);
    let attachment = git::get_worktree_attachment_in(&old_handle, Some(&context.execution_dir));
    let old_full = prefixed(&context.prefix, &old_handle);
    let new_full = prefixed(&context.prefix, &new_handle);
    let mux_running = context.mux.is_running().unwrap_or(false);

    if mux_running && attachment.manages_mux() && new_handle != old_handle {
        match mode {
            MuxMode::Session => {
                if context.mux.session_exists(&new_full)? {
                    return Err(anyhow!("tmux session '{}' already exists", new_full));
                }
            }
            MuxMode::Window => {
                let all = context.mux.get_all_window_names()?;
                let re = duplicate_name_regex(&new_full);
                if all.iter().any(|w| re.is_match(w)) {
                    return Err(anyhow!(
                        "tmux window '{}' (or a numbered duplicate) already exists",
                        new_full
                    ));
                }
            }
        }
    }

    info!(
        old_handle = %old_handle,
        new_handle = %new_handle,
        rename_branch,
        "rename:starting"
    );

    // 8. Capture the old canonical path before we move it. After `git worktree
    //    move`, `canonicalize(old_path)` would fail and we'd be unable to
    //    match stored agent workdirs (which are usually canonicalized).
    let old_canonical = canon_or_self(&old_path);

    // 9. Change to safe CWD before filesystem ops. If we're running from inside
    //    the worktree being moved, we'd otherwise lose our CWD.
    context.chdir_to_main_worktree()?;

    // 9a. Windows cannot rename a directory while a live process sits in it, and
    //     a workmux pane's shell always sits in its worktree, so close the
    //     affected pane(s) and wait for the shell to release the path. Runs
    //     before the metadata migration so a failure here needs no rollback.
    let closed_panes = if should_close_panes(
        MOVE_NEEDS_CLOSED_PANES,
        mux_running,
        attachment.manages_mux(),
        new_handle != old_handle,
    ) {
        let agent = agent_running_in(&old_path, context.mux.as_ref());
        info!(handle = %old_handle, mode = ?mode, "rename:closing panes that hold the directory");
        let closed = close_panes_for_move(
            context.mux.as_ref(),
            mode,
            &old_full,
            context.config.default_session(),
        )?;
        // Only reconnect what was connected: a worktree whose window is already
        // closed (`workmux close`) has to stay closed.
        closed.then_some(ClosedPanes { agent })
    } else {
        None
    };

    // 10. Migrate metadata first so a successful path move leaves attachment
    // state keyed by the resulting handle.
    if new_handle != old_handle {
        git::migrate_worktree_meta(&old_handle, &new_handle)
            .context("Failed to migrate worktree metadata")?;
    }

    // 11. Execute: git worktree move
    if new_handle != old_handle
        && let Err(error) = git::move_worktree(&old_path, &new_path)
    {
        return match git::migrate_worktree_meta(&new_handle, &old_handle) {
            Ok(()) => Err(error).context("Failed to move worktree (is the directory in use?)"),
            Err(rollback_error) => Err(error).context(format!(
                "Failed to move worktree and restore its metadata: {rollback_error:#}"
            )),
        };
    }
    if new_handle != old_handle {
        info!(from = %old_path.display(), to = %new_path.display(), "rename:worktree moved");
    }

    // 12. Execute: git branch rename
    if let Some(ref nb) = new_branch
        && nb != &branch_name
    {
        git::rename_branch(&branch_name, nb)?;
        info!(old = branch_name, new = nb, "rename:branch renamed");
    }

    // 13. Rename tmux window(s)/session
    let mut tmux_renamed = 0;
    if mux_running && attachment.manages_mux() && new_handle != old_handle {
        match mode {
            MuxMode::Session => {
                if context.mux.session_exists(&old_full).unwrap_or(false) {
                    match context.mux.rename_session(&old_full, &new_full) {
                        Ok(()) => {
                            tmux_renamed += 1;
                            info!(old = %old_full, new = %new_full, "rename:session renamed");
                        }
                        Err(e) => {
                            warn!(error = %e, "rename:failed to rename tmux session");
                        }
                    }
                }
            }
            MuxMode::Window => {
                let all = context.mux.get_all_window_names().unwrap_or_default();
                let re = duplicate_name_regex(&old_full);
                let matches: Vec<String> = all.into_iter().filter(|w| re.is_match(w)).collect();
                for old_name in &matches {
                    let new_name = remap_duplicate_name(old_name, &old_full, &new_full);
                    if let Err(e) = context.mux.rename_window(old_name, &new_name) {
                        warn!(window = old_name, error = %e, "rename:tmux rename_window failed");
                    } else {
                        tmux_renamed += 1;
                        info!(old = old_name, new = new_name, "rename:window renamed");
                    }
                }
            }
        }
    }

    // 14. Migrate agent state files + container markers (best-effort)
    let agents_migrated = match StateStore::new() {
        Ok(store) => {
            let migrated = store
                .migrate_worktree_paths(&old_canonical, &new_path, &old_full, &new_full)
                .unwrap_or_else(|e| {
                    warn!(error = %e, "rename:failed to migrate agent state");
                    0
                });
            if new_handle != old_handle
                && let Err(e) = store.migrate_container_handle(&old_handle, &new_handle)
            {
                warn!(error = %e, "rename:failed to migrate container markers");
            }
            migrated
        }
        Err(e) => {
            warn!(error = %e, "rename:state store unavailable, skipping state migration");
            0
        }
    };

    // 15. Reopen the pane(s) the move had to close, with the options `resurrect`
    //     uses for an existing worktree: no hooks or file ops, but the pane
    //     commands and the agent's previous conversation.
    let mut mux_reopened = None;
    let mut mux_reopen_error = None;
    if let Some(closed) = closed_panes {
        let options = SetupOptions {
            run_hooks: false,
            run_file_ops: false,
            run_pane_commands: true,
            prompt_file_path: None,
            focus_window: false,
            working_dir: None,
            config_root: None,
            open_if_exists: false,
            mode,
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            window_token: None,
            primary_window: true,
            resume_mode: ResumeMode::Continue,
        };
        let target_name = new_full.clone();
        match super::open(
            &new_handle,
            context,
            options,
            false,
            None,
            None,
            closed.agent.as_deref(),
        ) {
            Ok(_) => {
                info!(handle = %new_handle, target = %target_name, "rename:reopened worktree");
                mux_reopened = Some(target_name);
            }
            Err(error) => {
                let error = error.context(format!(
                    "Worktree renamed, but its {} '{}' could not be reopened; run \
                     'workmux open {}' to reopen it",
                    mode_label(mode),
                    target_name,
                    new_handle
                ));
                warn!(handle = %new_handle, error = %error, "rename:failed to reopen worktree");
                mux_reopen_error = Some(error);
            }
        }
    }

    Ok(RenameResult {
        old_path,
        new_path,
        old_handle,
        new_handle,
        old_branch: branch_name,
        new_branch,
        tmux_renamed,
        agents_migrated,
        mux_reopened,
        mux_reopen_error,
    })
}

/// Windows cannot rename a directory while any process sits in it, and a
/// workmux pane's shell always sits in its worktree, so the target has to be
/// closed for the move. Unix renames the directory under live processes without
/// complaint, so it keeps the pane and its scrollback.
#[cfg(windows)]
const MOVE_NEEDS_CLOSED_PANES: bool = true;
#[cfg(not(windows))]
const MOVE_NEEDS_CLOSED_PANES: bool = false;

/// How long a closed pane's shell gets to release the worktree path, matching
/// the deferred-cleanup worker that waits for the same release.
const PANE_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const PANE_CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Whether the move needs the worktree's multiplexer target closed first. The
/// platform flag is a parameter rather than a `cfg!` so tests can pin both
/// behaviours.
fn should_close_panes(
    platform_needs_closed_panes: bool,
    mux_running: bool,
    manages_mux: bool,
    handle_changed: bool,
) -> bool {
    platform_needs_closed_panes && mux_running && manages_mux && handle_changed
}

/// Panes closed so their worktree could be renamed, kept so the caller can
/// reopen the worktree afterwards.
struct ClosedPanes {
    /// Agent the closed panes were running; relaunched on reopen.
    agent: Option<String>,
}

/// Close the worktree's live target and wait until it is gone, so the move
/// cannot race a shell that still sits in the directory. Reports whether there
/// was a target to close, so an already-closed worktree stays closed.
fn close_panes_for_move(
    mux: &dyn Multiplexer,
    mode: MuxMode,
    full_name: &str,
    default_session: Option<&str>,
) -> Result<bool> {
    match mode {
        MuxMode::Session => {
            if !mux.session_exists(full_name)? {
                return Ok(false);
            }
            if mux.current_session().as_deref() == Some(full_name) {
                bail!("{}", own_pane_error(mode, full_name));
            }
            mux.kill_session_to(full_name, default_session)?;
            wait_for_panes_to_close(full_name, PANE_CLOSE_TIMEOUT, || {
                mux.session_exists(full_name)
            })?;
            Ok(true)
        }
        MuxMode::Window => {
            let current = mux.current_window_name().unwrap_or(None);
            let names = window_names_for_handle(
                &mux.get_all_window_names()?,
                full_name,
                current.as_deref(),
            )?;
            if names.is_empty() {
                return Ok(false);
            }
            for name in &names {
                mux.kill_window(name)?;
            }
            wait_for_panes_to_close(full_name, PANE_CLOSE_TIMEOUT, || {
                let live = mux.get_all_window_names()?;
                Ok(names.iter().any(|name| live.contains(name)))
            })?;
            Ok(true)
        }
    }
}

/// Names of the worktree's open window(s), numbered duplicates included.
fn window_names_for_handle(
    all: &HashSet<String>,
    full_name: &str,
    current: Option<&str>,
) -> Result<Vec<String>> {
    let re = duplicate_name_regex(full_name);
    let mut names: Vec<String> = all
        .iter()
        .filter(|name| re.is_match(name))
        .cloned()
        .collect();
    names.sort();
    if let Some(current) = current
        && names.iter().any(|name| name == current)
    {
        bail!("{}", own_pane_error(MuxMode::Window, current));
    }
    Ok(names)
}

/// Renaming cannot close the caller's own pane: the shell hosting this process
/// sits in the directory too, and closing it would kill us mid-move.
fn own_pane_error(mode: MuxMode, full_name: &str) -> String {
    format!(
        "Cannot rename while running inside {} '{}': a directory with a live process \
         in it cannot be moved. Run 'workmux rename' from another window.",
        mode_label(mode),
        full_name
    )
}

/// Poll `still_open` until the closed pane releases the worktree path.
fn wait_for_panes_to_close(
    full_name: &str,
    timeout: Duration,
    mut still_open: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if !still_open()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "Timed out waiting for '{}' to close; close it and retry the rename",
                full_name
            );
        }
        std::thread::sleep(PANE_CLOSE_POLL_INTERVAL);
    }
}

/// Agent the worktree's live panes run, so reopening it resumes the same one.
/// Best effort: a failed lookup just means the configured agent is used.
fn agent_running_in(worktree: &Path, mux: &dyn Multiplexer) -> Option<String> {
    let panes = StateStore::new().ok()?.load_reconciled_agents(mux).ok()?;
    reopen_agent(&panes, worktree)
}

/// Pick the agent to relaunch from the panes of one worktree, preferring the
/// canonical profile name so `open` can resolve it through the agent config.
fn reopen_agent(panes: &[AgentPane], worktree: &Path) -> Option<String> {
    let worktree = canon_or_self(worktree);
    let mut matching: Vec<&AgentPane> = panes
        .iter()
        .filter(|pane| {
            let path = canon_or_self(&pane.path);
            path == worktree || path.starts_with(&worktree)
        })
        .collect();
    matching.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    matching.into_iter().find_map(|pane| {
        pane.agent_kind
            .clone()
            .or_else(|| pane.agent_command.clone())
    })
}

/// Build a regex that matches `base` or `base-<digits>`.
fn duplicate_name_regex(base: &str) -> Regex {
    let pattern = format!(r"^{}(-\d+)?$", regex::escape(base));
    Regex::new(&pattern).expect("static regex pattern")
}

/// Rename a window name that may carry a numeric `-N` duplicate suffix.
fn remap_duplicate_name(name: &str, old_base: &str, new_base: &str) -> String {
    if name == old_base {
        return new_base.to_string();
    }
    if let Some(suffix) = name.strip_prefix(&format!("{}-", old_base))
        && !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}-{}", new_base, suffix);
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_exact_match() {
        assert_eq!(remap_duplicate_name("wm-old", "wm-old", "wm-new"), "wm-new");
    }

    #[test]
    fn remap_numeric_suffix() {
        assert_eq!(
            remap_duplicate_name("wm-old-2", "wm-old", "wm-new"),
            "wm-new-2"
        );
        assert_eq!(
            remap_duplicate_name("wm-old-42", "wm-old", "wm-new"),
            "wm-new-42"
        );
    }

    #[test]
    fn remap_non_matching_unchanged() {
        assert_eq!(remap_duplicate_name("other", "wm-old", "wm-new"), "other");
        // Non-numeric suffix: not a duplicate pattern
        assert_eq!(
            remap_duplicate_name("wm-old-abc", "wm-old", "wm-new"),
            "wm-old-abc"
        );
    }

    #[test]
    fn duplicate_regex_matches_base_and_suffixes() {
        let re = duplicate_name_regex("wm-feature");
        assert!(re.is_match("wm-feature"));
        assert!(re.is_match("wm-feature-2"));
        assert!(re.is_match("wm-feature-99"));
        assert!(!re.is_match("wm-feature-abc"));
        assert!(!re.is_match("wm-feature-x"));
        assert!(!re.is_match("wm-feature2"));
        assert!(!re.is_match("other"));
    }

    fn pane_at(
        path: &Path,
        pane_id: &str,
        agent_kind: Option<&str>,
        agent_command: Option<&str>,
    ) -> AgentPane {
        serde_json::from_value(serde_json::json!({
            "session": "test",
            "window_name": "wm-feature",
            "pane_id": pane_id,
            "path": path,
            "agent_kind": agent_kind,
            "agent_command": agent_command,
        }))
        .unwrap()
    }

    #[test]
    fn should_close_panes_only_where_the_platform_needs_it() {
        assert!(should_close_panes(true, true, true, true));
        assert!(!should_close_panes(false, true, true, true));
        assert!(!should_close_panes(true, false, true, true));
        assert!(!should_close_panes(true, true, false, true));
        assert!(!should_close_panes(true, true, true, false));
    }

    #[test]
    fn window_names_for_handle_includes_numbered_duplicates() {
        let all: HashSet<String> = ["wm-feature", "wm-feature-2", "wm-feature-x", "wm-other"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(
            window_names_for_handle(&all, "wm-feature", None).unwrap(),
            vec!["wm-feature".to_string(), "wm-feature-2".to_string()]
        );
        assert!(
            window_names_for_handle(&all, "wm-absent", None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn window_names_for_handle_refuses_the_callers_own_window() {
        let all: HashSet<String> = ["wm-feature", "wm-feature-2"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let error = window_names_for_handle(&all, "wm-feature", Some("wm-feature-2"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("running inside window 'wm-feature-2'"),
            "{error}"
        );
    }

    #[test]
    fn wait_for_panes_to_close_returns_once_the_pane_is_gone() {
        let mut polls = 0;
        wait_for_panes_to_close("wm-feature", Duration::from_secs(30), || {
            polls += 1;
            Ok(polls < 3)
        })
        .unwrap();
        assert_eq!(polls, 3);
    }

    #[test]
    fn wait_for_panes_to_close_times_out() {
        let error = wait_for_panes_to_close("wm-feature", Duration::ZERO, || Ok(true))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Timed out waiting for 'wm-feature'"),
            "{error}"
        );
    }

    #[test]
    fn wait_for_panes_to_close_propagates_errors() {
        let error = wait_for_panes_to_close("wm-feature", Duration::from_secs(30), || {
            Err(anyhow!("multiplexer went away"))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("multiplexer went away"), "{error}");
    }

    #[test]
    fn reopen_agent_prefers_the_canonical_profile_name() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        let panes = [pane_at(
            &worktree,
            "1",
            Some("claude"),
            Some("claude --verbose"),
        )];
        assert_eq!(reopen_agent(&panes, &worktree).as_deref(), Some("claude"));
    }

    #[test]
    fn reopen_agent_falls_back_to_the_launch_command() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("feature");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        let panes = [pane_at(
            &worktree.join("src"),
            "1",
            None,
            Some("codex exec"),
        )];
        assert_eq!(
            reopen_agent(&panes, &worktree).as_deref(),
            Some("codex exec")
        );
    }

    #[test]
    fn reopen_agent_ignores_panes_of_other_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(temp.path().join("other")).unwrap();
        let panes = [
            pane_at(&temp.path().join("other"), "1", Some("codex"), None),
            pane_at(&worktree, "2", None, None),
        ];
        assert_eq!(reopen_agent(&panes, &worktree), None);
        assert_eq!(reopen_agent(&panes[..1], &worktree), None);
    }
}
