//! WezTerm backend implementation for the Multiplexer trait.
//!
//! This module provides WezTermBackend, which wraps all WezTerm-specific operations
//! and exposes them through the Multiplexer trait interface.

use anyhow::{Context, Result, anyhow};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::Multiplexer;
use super::types::*;
use super::util;

/// File name of the WezTerm CLI, which ships beside WezTerm's other binaries.
const WEZTERM_CLI: &str = if cfg!(windows) {
    "wezterm.exe"
} else {
    "wezterm"
};

/// Program that speaks `wezterm cli`.
///
/// A Windows install is often portable and never reaches `PATH`, so the bare
/// name is not enough. Every pane inherits `WEZTERM_EXECUTABLE`, naming the
/// binary that owns it -- the GUI, or the mux server -- and the CLI sits next
/// to that one.
fn wezterm_program() -> &'static str {
    static PROGRAM: OnceLock<String> = OnceLock::new();

    PROGRAM.get_or_init(|| {
        wezterm_program_from(std::env::var_os("WEZTERM_EXECUTABLE").map(PathBuf::from))
    })
}

fn wezterm_program_from(executable: Option<PathBuf>) -> String {
    let Some(executable) = executable else {
        return WEZTERM_CLI.to_string();
    };

    let cli = executable.with_file_name(WEZTERM_CLI);
    if cli.is_file() {
        return cli.to_string_lossy().into_owned();
    }
    // The named binary is not the CLI and nothing sits beside it, so the
    // install is not the one that owns this pane; `PATH` decides.
    WEZTERM_CLI.to_string()
}

/// WezTerm pane information from `wezterm cli list --format json`
#[derive(Debug, Deserialize)]
struct WezTermPane {
    // The following fields are required for JSON deserialization from `wezterm cli list`
    // but are not used after parsing. The allow(dead_code) suppresses false positives.
    #[allow(dead_code)]
    window_id: u64,
    tab_id: u64,
    pane_id: u64,
    workspace: String,
    /// Terminal title (set by running process via escape sequences)
    title: String,
    /// Explicit tab title (we set this for window names)
    tab_title: String,
    /// Working directory in format "file://hostname/path"
    cwd: String,
    #[allow(dead_code)]
    tty_name: Option<String>,
    is_active: bool,
    #[allow(dead_code)]
    is_zoomed: bool,
    #[allow(dead_code)]
    cursor_x: u64,
    #[allow(dead_code)]
    cursor_y: u64,
}

impl WezTermPane {
    /// Parse cwd from "file://hostname/path" format to PathBuf
    fn cwd_path(&self) -> PathBuf {
        file_url_to_path(&self.cwd)
    }
}

/// The path named by a pane's `cwd`.
///
/// The field is a `file://` URL, so its text is not the path. A Windows cwd
/// arrives as `file:///C:/Users/me/project/`: forward slashes, a `/` ahead of
/// the drive letter, a trailing separator the directory name does not have,
/// and anything outside the URL's unreserved set escaped -- the space in
/// `wm%20e2e`, say. Text that is not a `file://` URL is taken as a path
/// already.
fn file_url_to_path(url: &str) -> PathBuf {
    let Some(rest) = url.strip_prefix("file://") else {
        return PathBuf::from(url);
    };

    // The authority runs up to the first separator, and names the machine the
    // pane runs on.
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    let path = percent_decode_str(path).decode_utf8_lossy();
    drop_url_trailing_separator(url_path(authority, &path))
}

/// Turn the decoded path half of a `file://` URL into the path it names, given
/// the authority that preceded it.
#[cfg(windows)]
fn url_path(authority: &str, path: &str) -> PathBuf {
    // A named authority is the UNC host of a share. WezTerm normally reaches a
    // share through a drive letter (the one `pushd` maps), so this covers a URL
    // that names the host outright.
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return PathBuf::from(format!(
            "\\\\{}\\{}",
            authority,
            path.trim_start_matches('/').replace('/', "\\")
        ));
    }

    // A URL path always begins with "/"; a drive path never does.
    let path = path.strip_prefix('/').unwrap_or(path);
    PathBuf::from(path.replace('/', "\\"))
}

#[cfg(unix)]
fn url_path(_authority: &str, path: &str) -> PathBuf {
    // The authority names the machine the pane runs on, and it is this one.
    PathBuf::from(path)
}

/// Drop the separator a URL path ends with, unless it is all that keeps the
/// path a drive root (`C:\`) rather than a drive name (`C:`, which is relative).
fn drop_url_trailing_separator(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches(std::path::MAIN_SEPARATOR);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        return path;
    }
    PathBuf::from(trimmed)
}

/// WezTerm backend implementation.
///
/// Relies on inherited WEZTERM_UNIX_SOCKET and WEZTERM_PANE environment variables.
/// Requires proper WezTerm config (see docs/guide/wezterm.md).
#[derive(Debug)]
pub struct WezTermBackend;

impl Default for WezTermBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WezTermBackend {
    /// Create a new WezTermBackend instance.
    ///
    /// Requires proper WezTerm config:
    /// - `default_gui_startup_args = { 'connect', 'unix' }` in wezterm.lua
    /// - `SpawnTab('CurrentPaneDomain')` for new tab keybindings
    ///
    /// This ensures WEZTERM_UNIX_SOCKET and WEZTERM_PANE are consistent.
    pub fn new() -> Self {
        Self
    }

    /// Create a wezterm CLI command.
    /// Uses inherited WEZTERM_UNIX_SOCKET from environment.
    fn wezterm_cmd(&self) -> Cmd<'static> {
        Cmd::new(wezterm_program())
    }

    /// Query all panes from WezTerm.
    fn list_panes(&self) -> Result<Vec<WezTermPane>> {
        let output = self
            .wezterm_cmd()
            .args(&["cli", "list", "--format", "json"])
            .run_and_capture_stdout()
            .context("Failed to list WezTerm panes")?;

        let panes: Vec<WezTermPane> =
            serde_json::from_str(&output).context("Failed to parse WezTerm pane list")?;

        Ok(panes)
    }

    /// Get the current foreground process details for a pane tty.
    ///
    /// Unix reads the tty's foreground process group. Windows panes report no
    /// tty and have no `ps`, so there is nothing to inspect yet.
    #[cfg(unix)]
    fn foreground_process_info(&self, tty_name: Option<&str>) -> (Option<u32>, Option<String>) {
        let tty = tty_name.map(|t| t.trim_start_matches("/dev/"));

        let pid = tty
            .and_then(|tty| {
                Cmd::new("sh")
                    .args(&[
                        "-c",
                        &format!(
                            "ps -t {} -o pid=,stat= | grep '+' | head -1 | awk '{{print $1}}'",
                            tty
                        ),
                    ])
                    .run_and_capture_stdout()
                    .ok()
            })
            .and_then(|output| output.trim().parse::<u32>().ok());

        let current_command = tty
            .and_then(|tty| {
                Cmd::new("sh")
                    .args(&[
                        "-c",
                        &format!(
                            "ps -t {} -o stat=,comm= | grep '+' | head -1 | awk '{{print $2}}'",
                            tty
                        ),
                    ])
                    .run_and_capture_stdout()
                    .ok()
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        (pid, current_command)
    }

    #[cfg(windows)]
    fn foreground_process_info(&self, _tty_name: Option<&str>) -> (Option<u32>, Option<String>) {
        (None, None)
    }

    fn live_pane_snapshot(&self, p: &WezTermPane) -> util::LivePaneSnapshot {
        let (pid, current_command) = self.foreground_process_info(p.tty_name.as_deref());
        util::LivePaneSnapshot {
            pane_id: p.pane_id.to_string(),
            pid,
            current_command,
            working_dir: p.cwd_path(),
            title: p.title.clone(),
            session: p.workspace.clone(),
            window: p.tab_title.clone(),
        }
    }

    fn tab_panes<'a>(
        panes: &'a [WezTermPane],
        tab_title: &str,
        workspace: Option<&str>,
    ) -> Vec<&'a WezTermPane> {
        panes
            .iter()
            .filter(|p| p.tab_title == tab_title && workspace.is_none_or(|ws| p.workspace == ws))
            .collect()
    }

    fn matching_tab_panes<'a>(
        &self,
        panes: &'a [WezTermPane],
        tab_title: &str,
    ) -> Vec<&'a WezTermPane> {
        Self::tab_panes(panes, tab_title, self.current_workspace().as_deref())
    }

    /// Get the current workspace name from the environment.
    /// Returns the workspace of the current pane.
    /// Returns None if not running inside WezTerm or if the pane can't be found.
    fn current_workspace(&self) -> Option<String> {
        let pane_id: u64 = std::env::var("WEZTERM_PANE").ok()?.parse().ok()?;
        let panes = self.list_panes().ok()?;
        panes
            .iter()
            .find(|p| p.pane_id == pane_id)
            .map(|p| p.workspace.clone())
    }

    /// Set the tab title for a pane.
    fn set_tab_title(&self, pane_id: &str, title: &str) -> Result<()> {
        self.wezterm_cmd()
            .args(&["cli", "set-tab-title", "--pane-id", pane_id, title])
            .run()
            .context("Failed to set tab title")?;
        Ok(())
    }

    /// Split a pane with optional command.
    fn split_pane_internal(
        &self,
        target_pane_id: &str,
        direction: SplitDirection,
        cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        let direction_arg = match direction {
            SplitDirection::Horizontal => "--horizontal",
            SplitDirection::Vertical => "--top-level",
            SplitDirection::Stacked => {
                return Err(anyhow!(
                    "split: stacked is only supported by the Zellij backend"
                ));
            }
        };

        let cwd_str = cwd.to_string_lossy();
        let mut args = vec![
            "cli",
            "split-pane",
            "--pane-id",
            target_pane_id,
            "--cwd",
            &*cwd_str,
            direction_arg,
        ];

        let percent_arg;
        if let Some(p) = percentage {
            percent_arg = format!("{}", p);
            args.push("--percent");
            args.push(&percent_arg);
        }
        let _ = size; // WezTerm doesn't support absolute sizes via CLI

        // Route the command through the platform shell so simple commands and
        // multi-statement scripts are handled the same way.
        let snippet = command.map(crate::shell::snippet_argv);
        if let Some(snippet) = &snippet {
            args.push("--");
            for arg in snippet {
                args.push(arg.as_str());
            }
        }

        let output = self
            .wezterm_cmd()
            .args(&args)
            .run_and_capture_stdout()
            .context("Failed to split WezTerm pane")?;

        Ok(output.trim().to_string())
    }
}

impl Multiplexer for WezTermBackend {
    fn name(&self) -> &'static str {
        "wezterm"
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        self.wezterm_cmd().args(&["cli", "list"]).run_as_check()
    }

    fn current_pane_id(&self) -> Option<String> {
        // WEZTERM_PANE is reliable when WezTerm is properly configured
        // (default_gui_startup_args = { 'connect', 'unix' })
        std::env::var("WEZTERM_PANE").ok()
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let pane_id: u64 = std::env::var("WEZTERM_PANE")
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("WEZTERM_PANE not set or invalid"))?;

        let panes = self.list_panes()?;
        let current = panes
            .iter()
            .find(|p| p.pane_id == pane_id)
            .ok_or_else(|| anyhow!("Current pane {} not found", pane_id))?;

        let path = current.cwd_path();
        if path.as_os_str().is_empty() {
            return Err(anyhow!("Empty path returned from WezTerm"));
        }

        Ok(path)
    }

    // === Window/Tab Management ===

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let full_name = util::prefixed(params.prefix, params.name);
        let cwd_str = params.cwd.to_string_lossy();

        // Note: WezTerm doesn't support "insert after" - tabs appear at end
        // params.after_window is ignored (different from tmux)
        // spawn without --new-window creates a new tab in the current window
        let output = self
            .wezterm_cmd()
            .args(&["cli", "spawn", "--cwd", &*cwd_str])
            .run_and_capture_stdout()
            .context("Failed to create WezTerm tab")?;

        let pane_id = output.trim().to_string();

        // CRITICAL: Set tab_title for persistent window naming
        self.set_tab_title(&pane_id, &full_name)?;

        Ok(pane_id)
    }

    fn create_session(&self, _params: CreateSessionParams) -> Result<String> {
        Err(anyhow!(
            "Session mode (--session) is not supported in WezTerm.\n\
             WezTerm workspaces work differently from tmux sessions.\n\
             Use the default window mode instead (omit --session flag)."
        ))
    }

    fn switch_to_session(&self, _prefix: &str, _name: &str) -> Result<()> {
        Err(anyhow!(
            "Session mode is not supported in WezTerm.\n\
             Use the default window mode instead."
        ))
    }

    fn session_exists(&self, _full_name: &str) -> Result<bool> {
        // WezTerm doesn't have persistent sessions like tmux.
        // Workspaces are ephemeral and not queryable via CLI.
        Ok(false)
    }

    fn kill_session(&self, _full_name: &str) -> Result<()> {
        // WezTerm doesn't have persistent sessions to kill.
        // Workspaces disappear when their last window closes.
        Ok(())
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let panes = self.list_panes()?;
        let tab_panes = self.matching_tab_panes(&panes, full_name);

        if tab_panes.is_empty() {
            return Ok(()); // Already gone
        }

        // Kill in reverse order (last pane first)
        for pane in tab_panes.iter().rev() {
            let _ = self
                .wezterm_cmd()
                .args(&["cli", "kill-pane", "--pane-id", &pane.pane_id.to_string()])
                .run();
        }
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let panes = self.list_panes()?;
        let tab_panes = self.matching_tab_panes(&panes, full_name);

        if tab_panes.is_empty() {
            return Ok(());
        }

        // Build kill commands for all panes (reverse order)
        let kill_cmds: String = tab_panes
            .iter()
            .rev()
            .map(|p| deferred_wezterm_cmd(&["kill-pane", "--pane-id", &p.pane_id.to_string()]))
            .collect::<Vec<_>>()
            .join("; ");

        // The detached process inherits WEZTERM_UNIX_SOCKET from the environment.
        let script = if cfg!(windows) {
            format!(
                "Start-Sleep -Milliseconds {}; {}",
                delay.as_millis(),
                kill_cmds
            )
        } else {
            format!("sleep {}; {}", delay.as_secs_f64(), kill_cmds)
        };

        util::run_detached_script(&script)?;
        Ok(())
    }

    fn schedule_session_close(&self, _full_name: &str, _delay: Duration) -> Result<()> {
        Err(anyhow::anyhow!(
            "Session mode is not supported in WezTerm. Use window mode instead."
        ))
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        util::run_detached_script(script)
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        let panes = self.list_panes()?;
        let target = self
            .matching_tab_panes(&panes, full_name)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))?;
        Ok(deferred_wezterm_cmd(&[
            "activate-tab",
            "--tab-id",
            &target.tab_id.to_string(),
        ]))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let panes = self.list_panes()?;
        let tab_panes = self.matching_tab_panes(&panes, full_name);

        if tab_panes.is_empty() {
            return Err(anyhow!("Window '{}' not found", full_name));
        }

        let kill_cmds: String = tab_panes
            .iter()
            .rev()
            .map(|p| deferred_wezterm_cmd(&["kill-pane", "--pane-id", &p.pane_id.to_string()]))
            .collect::<Vec<_>>()
            .join("; ");
        Ok(kill_cmds)
    }

    fn shell_switch_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(anyhow!(
            "Session mode is not supported in WezTerm. Use window mode instead."
        ))
    }

    fn shell_kill_session_cmd(&self, _full_name: &str) -> Result<String> {
        Err(anyhow!(
            "Session mode is not supported in WezTerm. Use window mode instead."
        ))
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        let panes = self.list_panes()?;
        let target = self
            .matching_tab_panes(&panes, &full_name)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))?;

        self.wezterm_cmd()
            .args(&[
                "cli",
                "activate-tab",
                "--tab-id",
                &target.tab_id.to_string(),
            ])
            .run()
            .context("Failed to activate tab")?;
        Ok(())
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let pane_id: u64 = match std::env::var("WEZTERM_PANE")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => return Ok(None),
        };

        let panes = self.list_panes()?;
        let current = panes.iter().find(|p| p.pane_id == pane_id);

        Ok(current.map(|p| p.tab_title.clone()))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let panes = self.list_panes()?;
        let current_ws = self.current_workspace();

        // Collect unique tab_titles (our window names), filtered by current workspace
        // If we can't determine the workspace (not in WezTerm), show all
        let names: HashSet<String> = panes
            .iter()
            .filter(|p| current_ws.as_ref().is_none_or(|ws| &p.workspace == ws))
            .map(|p| p.tab_title.clone())
            .collect();

        Ok(names)
    }

    fn wait_until_session_closed(&self, _full_session_name: &str) -> Result<()> {
        Err(anyhow::anyhow!(
            "Session mode is not supported in WezTerm. Use window mode instead."
        ))
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.wezterm_cmd()
            .args(&["cli", "activate-pane", "--pane-id", pane_id])
            .run()
            .context("Failed to select pane")?;
        Ok(())
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        // Check if we need to switch workspaces first
        let panes = self.list_panes()?;
        if let Some(target) = panes.iter().find(|p| p.pane_id.to_string() == pane_id) {
            let target_workspace = &target.workspace;
            if let Some(current) = self.current_workspace()
                && &current != target_workspace
            {
                // Cross-workspace switch: send escape sequence to trigger Lua handler
                // Use tab_title (stable across mux contexts) instead of pane_id
                send_pane_switch_signal(target_workspace, &target.tab_title);
                return Ok(());
            }
        }

        // Same workspace: use CLI directly
        self.select_pane(pane_id)
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.wezterm_cmd()
            .args(&["cli", "kill-pane", "--pane-id", pane_id])
            .run()?;
        Ok(())
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        let panes = self.list_panes()?;
        let target = panes
            .iter()
            .find(|p| p.pane_id.to_string() == pane_id)
            .ok_or_else(|| anyhow!("Pane {} not found", pane_id))?;

        let tab_id = target.tab_id;
        let original_tab_title = target.tab_title.clone();

        // Find a sibling pane in the same tab (to split from after kill)
        let sibling = panes
            .iter()
            .find(|p| p.tab_id == tab_id && p.pane_id.to_string() != pane_id);

        if let Some(sib) = sibling {
            // Has sibling: kill target, split from sibling
            self.wezterm_cmd()
                .args(&["cli", "kill-pane", "--pane-id", pane_id])
                .run()?;

            let new_pane_id = self.split_pane_internal(
                &sib.pane_id.to_string(),
                SplitDirection::Horizontal,
                cwd,
                None,
                None,
                cmd,
            )?;

            Ok(new_pane_id)
        } else {
            // Only pane in tab: spawn new tab, kill old
            let cwd_str = cwd.to_string_lossy();
            let mut args = vec!["cli", "spawn", "--cwd", &*cwd_str];

            // Route the command through the platform shell, as in split_pane.
            let snippet = cmd.map(crate::shell::snippet_argv);
            if let Some(snippet) = &snippet {
                args.push("--");
                for arg in snippet {
                    args.push(arg.as_str());
                }
            }

            let output = self
                .wezterm_cmd()
                .args(&args)
                .run_and_capture_stdout()
                .context("Failed to spawn new tab")?;

            let new_pane_id = output.trim().to_string();

            // Set tab title to preserve window name
            self.set_tab_title(&new_pane_id, &original_tab_title)?;

            // Kill old pane (tab will close but new tab exists)
            let _ = self
                .wezterm_cmd()
                .args(&["cli", "kill-pane", "--pane-id", pane_id])
                .run();

            Ok(new_pane_id)
        }
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        // Note: We don't use --escapes to avoid partial escape sequences like (B
        // appearing in the preview. Plain text is cleaner for dashboard display.
        let output = self
            .wezterm_cmd()
            .args(&["cli", "get-text", "--pane-id", pane_id])
            .run_and_capture_stdout()
            .ok()?;

        Some(util::tail_lines(&output, lines))
    }

    // === Text I/O ===

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        self.wezterm_cmd()
            .args(&["cli", "send-text", "--pane-id", pane_id, "--no-paste", text])
            .run()
            .context("Failed to send text to pane")
            .map(|_| ())
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.send_text_fragment(pane_id, "\r")
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.wezterm_cmd()
            .args(&["cli", "send-text", "--pane-id", pane_id, "--no-paste", key])
            .run()
            .context("Failed to send key to pane")?;
        Ok(())
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        // Without --no-paste, WezTerm uses bracketed paste
        self.wezterm_cmd()
            .args(&["cli", "send-text", "--pane-id", pane_id, content])
            .run()?;

        Ok(())
    }

    // === Status ===

    fn set_status(&self, pane_id: &str, icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        // For WezTerm, we could update the tab title to include the icon.
        // However, agent state is now managed by StateStore, so this is just UI feedback.
        // For now, we just log the status change - tab title remains stable.
        // Future: could update tab title to show icon like "🔄 wm-feature"
        let _ = (pane_id, icon); // Acknowledge parameters
        Ok(())
    }

    fn clear_status(&self, _pane_id: &str) -> Result<()> {
        // No UI cleanup needed - tab title remains stable
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        // No-op for WezTerm - status is displayed via tab title, not tmux-style format
        Ok(())
    }

    // === Multi-Session/Workspace Support ===

    fn current_session(&self) -> Option<String> {
        self.current_workspace()
    }

    // === State Reconciliation ===

    fn instance_id(&self) -> String {
        // Use the unix socket path as instance ID so all workspaces on the same
        // WezTerm server share one instance, matching tmux server scope.
        std::env::var("WEZTERM_UNIX_SOCKET").unwrap_or_else(|_| "default".to_string())
    }

    fn resolve_instance_id(&self) -> Result<String> {
        std::env::var("WEZTERM_UNIX_SOCKET")
            .ok()
            .filter(|instance| !instance.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("WEZTERM_UNIX_SOCKET is required to resolve the WezTerm instance")
            })
    }

    fn active_pane_id(&self) -> Option<String> {
        // Query WezTerm for the active pane
        self.list_panes().ok().and_then(|panes| {
            panes
                .into_iter()
                .find(|p| p.is_active)
                .map(|p| p.pane_id.to_string())
        })
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let pane_id_num: u64 = pane_id.parse().ok().unwrap_or(0);

        let panes = self.list_panes()?;
        let pane = panes.into_iter().find(|p| p.pane_id == pane_id_num);

        match pane {
            Some(p) => Ok(Some(self.live_pane_snapshot(&p).into_pair().1)),
            None => Ok(None),
        }
    }

    fn get_all_window_names_all_sessions(&self) -> Result<HashSet<String>> {
        // `wezterm cli list` returns ALL panes across ALL workspaces.
        // Just collect unique tab_titles.
        let panes = self.list_panes()?;
        let names: HashSet<String> = panes.iter().map(|p| p.tab_title.clone()).collect();
        Ok(names)
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        Ok(util::live_pane_map(
            self.list_panes()?
                .iter()
                .map(|p| self.live_pane_snapshot(p)),
        ))
    }

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        size: Option<u16>,
        percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        self.split_pane_internal(
            target_pane_id,
            direction.clone(),
            cwd,
            size,
            percentage,
            command,
        )
    }
}

/// A `wezterm cli` invocation for a deferred script.
///
/// The script is handed to the platform interpreter (`sh` on Unix, PowerShell
/// on Windows), so the output redirection has to use that interpreter's null
/// device.
fn deferred_wezterm_cmd(args: &[&str]) -> String {
    format!(
        "{} cli {} {}",
        deferred_wezterm_program(),
        args.join(" "),
        crate::shell::silent_output_suffix()
    )
}

/// The CLI as the interpreter that reads a deferred script must spell it.
///
/// PowerShell runs the Windows scripts and `sh` the Unix ones; both take a
/// single-quoted path, and PowerShell needs `&` to run a command named by one.
fn deferred_wezterm_program() -> String {
    quote_for_deferred_script(wezterm_program())
}

/// Quote a program path for the interpreter that reads a deferred script.
fn quote_for_deferred_script(program: &str) -> String {
    if cfg!(windows) {
        format!("& '{}'", program.replace('\'', "''"))
    } else {
        format!("'{}'", program.replace('\'', "'\\''"))
    }
}

/// Send escape sequence to trigger cross-workspace pane switch via WezTerm's user-var-changed event.
///
/// This requires the user to have a Lua handler in their wezterm.lua.
/// The value is a JSON payload with workspace and tab_title.
/// See docs/guide/wezterm.md for the required handler.
///
/// Without this handler, the escape sequence is silently ignored.
fn send_pane_switch_signal(workspace: &str, tab_title: &str) {
    use base64::Engine;
    use std::io::Write;

    // Send JSON with workspace and tab_title (stable across mux contexts)
    let payload = serde_json::json!({
        "workspace": workspace,
        "tab_title": tab_title
    });
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.to_string());
    // OSC 1337 ; SetUserVar=name=base64_value BEL
    print!("\x1b]1337;SetUserVar=workmux-switch-pane={}\x07", encoded);
    // Flush to ensure it's sent immediately
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane carrying `cwd`, with the other fields at values nothing reads.
    fn pane_at(cwd: &str) -> WezTermPane {
        WezTermPane {
            window_id: 0,
            tab_id: 0,
            pane_id: 0,
            workspace: "default".to_string(),
            title: String::new(),
            tab_title: "test".to_string(),
            cwd: cwd.to_string(),
            tty_name: None,
            is_active: true,
            is_zoomed: false,
            cursor_x: 0,
            cursor_y: 0,
        }
    }

    /// A cwd that is not a URL is a path already.
    #[test]
    fn cwd_path_passes_through_a_plain_path() {
        assert_eq!(
            pane_at(r"C:\Users\me\project").cwd_path(),
            PathBuf::from(r"C:\Users\me\project")
        );
    }

    /// `wezterm cli list` reports a Windows cwd as `file:///C:/Users/me/x/`:
    /// the drive keeps a leading `/` and the URL form appends a separator that
    /// the directory name does not have.
    #[cfg(windows)]
    #[test]
    fn cwd_path_parses_a_windows_cwd() {
        let pane = pane_at(
            "file:///C:/Users/Administrator/AppData/Local/Temp/wm-e2e/demo__worktrees/track-a/",
        );
        assert_eq!(
            pane.cwd_path(),
            PathBuf::from(
                r"C:\Users\Administrator\AppData\Local\Temp\wm-e2e\demo__worktrees\track-a"
            )
        );
    }

    /// WezTerm escapes characters a URL cannot carry bare, such as the space in
    /// a worktree path.
    #[cfg(windows)]
    #[test]
    fn cwd_path_decodes_an_escaped_windows_cwd() {
        let pane = pane_at("file:///C:/Users/Administrator/AppData/Local/Temp/wm%20e2e/");
        assert_eq!(
            pane.cwd_path(),
            PathBuf::from(r"C:\Users\Administrator\AppData\Local\Temp\wm e2e")
        );
    }

    /// Stripping the trailing separator must not turn a drive root into a drive
    /// name, which would be a relative path.
    #[cfg(windows)]
    #[test]
    fn cwd_path_keeps_a_windows_drive_root() {
        assert_eq!(pane_at("file:///C:/").cwd_path(), PathBuf::from(r"C:\"));
    }

    /// An authority naming a machine is a UNC host. `localhost` is this machine,
    /// so it keeps the plain drive path.
    #[cfg(windows)]
    #[test]
    fn cwd_path_reads_a_windows_authority() {
        assert_eq!(
            pane_at("file://server/share/project").cwd_path(),
            PathBuf::from(r"\\server\share\project")
        );
        assert_eq!(
            pane_at("file://localhost/C:/Users/me").cwd_path(),
            PathBuf::from(r"C:\Users\me")
        );
    }

    /// On Unix the authority is the machine the pane runs on, which is this one,
    /// so only the path half matters.
    #[cfg(unix)]
    #[test]
    fn cwd_path_parses_a_unix_cwd() {
        assert_eq!(
            pane_at("file://hostname/home/user/project").cwd_path(),
            PathBuf::from("/home/user/project")
        );
        assert_eq!(
            pane_at("file:///home/user/project").cwd_path(),
            PathBuf::from("/home/user/project")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cwd_path_decodes_and_trims_a_unix_cwd() {
        assert_eq!(
            pane_at("file:///home/user/my%20project/").cwd_path(),
            PathBuf::from("/home/user/my project")
        );
        assert_eq!(pane_at("file:///").cwd_path(), PathBuf::from("/"));
    }

    /// Deferred scripts run under the platform shell, so pane commands built
    /// for them must redirect through that shell's null device.
    #[test]
    fn deferred_wezterm_cmd_uses_the_deferred_shells_null_device() {
        let cmd = deferred_wezterm_cmd(&["activate-tab", "--tab-id", "7"]);
        assert!(cmd.starts_with(&deferred_wezterm_program()));
        assert!(cmd.contains(" cli activate-tab --tab-id 7 "));
        assert!(cmd.ends_with(crate::shell::silent_output_suffix()));
    }

    /// A portable Windows install never reaches `PATH`, so the CLI is looked
    /// up beside the binary `WEZTERM_EXECUTABLE` names.
    #[test]
    fn wezterm_program_prefers_the_cli_beside_the_named_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mux_server = dir.path().join("wezterm-mux-server");
        std::fs::write(&mux_server, "").unwrap();

        // A binary that is not the CLI, with nothing beside it: `PATH`.
        assert_eq!(wezterm_program_from(Some(mux_server.clone())), WEZTERM_CLI);

        let cli = dir.path().join(WEZTERM_CLI);
        std::fs::write(&cli, "").unwrap();
        assert_eq!(
            wezterm_program_from(Some(mux_server)),
            cli.to_string_lossy()
        );
        assert_eq!(wezterm_program_from(None), WEZTERM_CLI);
    }

    /// The program goes into a script the platform shell reads, so a path with
    /// a space survives and a quote in it cannot break out of the string.
    #[test]
    fn deferred_wezterm_program_quotes_the_path_for_its_interpreter() {
        let spaced = "C:\\Program Files\\WezTerm\\wezterm.exe";
        let quoted_quote = "/tmp/it's/wezterm";

        if cfg!(windows) {
            assert_eq!(
                quote_for_deferred_script(spaced),
                "& 'C:\\Program Files\\WezTerm\\wezterm.exe'"
            );
            assert_eq!(
                quote_for_deferred_script(quoted_quote),
                "& '/tmp/it''s/wezterm'"
            );
        } else {
            assert_eq!(
                quote_for_deferred_script(spaced),
                "'C:\\Program Files\\WezTerm\\wezterm.exe'"
            );
            assert_eq!(
                quote_for_deferred_script(quoted_quote),
                "'/tmp/it'\\''s/wezterm'"
            );
        }
    }
}
