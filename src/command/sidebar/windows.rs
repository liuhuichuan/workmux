//! The sidebar as it runs on Windows, inside a WezTerm pane.
//!
//! tmux's sidebar is one half of a pair: a daemon polls and pushes snapshots to
//! render-only clients. WezTerm offers no daemon, so this pane does both -- it
//! polls the state store and the multiplexer on a timer and renders the same
//! `SidebarApp` through the same widgets the tmux client renders.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;

use crate::config::{Config, SidebarPosition, StatusIcons};
use crate::git::{self, GitStatus};
use crate::multiplexer::wezterm;
use crate::multiplexer::{Multiplexer, create_backend, detect_backend};
use crate::state::StateStore;

use super::app::SidebarApp;
use super::input::{LastPaneCheck, apply_input, quit_for_last_pane};
use super::snapshot::{SidebarSnapshot, build_snapshot};
use super::ui::render_sidebar;

/// How often the pane re-reads the state store and the multiplexer.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often git status is recomputed. Slower than the poll, because a refresh
/// runs git once per agent and git answers in tens of milliseconds.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// Title this pane claims, and the only way to find it again: WezTerm has no
/// per-pane user option to hang a role on, but it does report pane titles.
const SIDEBAR_PANE_TITLE: &str = "workmux-sidebar";

/// Restores the console however the loop leaves.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

/// How the loop ended, which decides whether sibling sidebars go with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quit {
    /// The host window is gone and only this pane is left.
    Silent,
    /// The user asked for the sidebar to stop, so every sidebar in this
    /// workspace goes with it. A workspace is what `on` turned the sidebar on
    /// in, and a sidebar the user cannot see is not theirs to keep running.
    Workspace,
}

enum AppEvent {
    /// A terminal input event (key press, resize, ...).
    Input(Event),
    /// A refreshed git status map from the background worker.
    GitStatuses(HashMap<PathBuf, GitStatus>),
}

/// Spawn a thread that reads terminal events and forwards them.
/// Must be called AFTER terminal raw mode is enabled.
fn spawn_input_thread(tx: mpsc::Sender<AppEvent>) {
    thread::spawn(move || {
        // event::read() blocks until input is available - zero CPU
        while let Ok(ev) = event::read() {
            if tx.send(AppEvent::Input(ev)).is_err() {
                break;
            }
        }
    });
}

/// Spawn the thread that keeps git status fresh.
///
/// It only ever works on the paths the render loop publishes, so an agent that
/// leaves the list stops costing git calls, and nothing here blocks a frame.
fn spawn_git_worker(
    tx: mpsc::Sender<AppEvent>,
    main_branch: Option<String>,
) -> Arc<Mutex<Vec<PathBuf>>> {
    let published: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let paths = Arc::clone(&published);

    thread::spawn(move || {
        loop {
            let wanted = paths.lock().map(|paths| paths.clone()).unwrap_or_default();
            let statuses: HashMap<PathBuf, GitStatus> = wanted
                .into_iter()
                .map(|path| {
                    let status = git::get_git_status(&path, main_branch.as_deref());
                    (path, status)
                })
                .collect();

            if tx.send(AppEvent::GitStatuses(statuses)).is_err() {
                break;
            }
            thread::sleep(GIT_REFRESH_INTERVAL);
        }
    });

    published
}

pub(super) fn tabs_without_sidebar(workspace: &str) -> Result<Vec<String>> {
    let panes = wezterm::panes()?;
    Ok(plan_tabs(&panes, workspace))
}

/// One pane per tab of `workspace` that has no sidebar yet.
///
/// Whatever pane is found first in a tab is the one to split; `--top-level`
/// makes the new pane span the tab regardless of which pane it was aimed at.
/// Tabs are visited in id order so a run is reproducible.
fn plan_tabs(panes: &[wezterm::PaneSummary], workspace: &str) -> Vec<String> {
    let mut tabs: BTreeMap<u64, (String, bool)> = BTreeMap::new();

    for pane in panes.iter().filter(|pane| pane.workspace == workspace) {
        let entry = tabs
            .entry(pane.tab_id)
            .or_insert_with(|| (pane.pane_id.clone(), false));
        entry.1 |= pane.title == SIDEBAR_PANE_TITLE;
    }

    tabs
        .into_values()
        .filter(|(_, has_sidebar)| !has_sidebar)
        .map(|(pane_id, _)| pane_id)
        .collect()
}

/// Pane ids of the running sidebars, optionally only those of one workspace.
pub(super) fn sidebar_panes(workspace: Option<&str>) -> Result<Vec<String>> {
    Ok(sidebar_pane_ids(&wezterm::panes()?, workspace))
}

fn sidebar_pane_ids(panes: &[wezterm::PaneSummary], workspace: Option<&str>) -> Vec<String> {
    panes
        .iter()
        .filter(|pane| pane.title == SIDEBAR_PANE_TITLE)
        .filter(|pane| workspace.is_none_or(|workspace| pane.workspace == workspace))
        .map(|pane| pane.pane_id.clone())
        .collect()
}

/// Split the tab holding `target_pane_id` and run the sidebar in the new pane.
pub(super) fn open(target_pane_id: &str, position: SidebarPosition, cells: u16) -> Result<String> {
    let exe = std::env::current_exe().context("failed to locate the workmux executable")?;
    let exe = exe.to_string_lossy().into_owned();
    let cells = cells.to_string();
    let direction = match position {
        SidebarPosition::Left => "--left",
        SidebarPosition::Top => "--top",
    };

    let pane_id = wezterm::cli(&[
        "cli",
        "split-pane",
        "--pane-id",
        target_pane_id,
        "--top-level",
        direction,
        "--cells",
        &cells,
        "--",
        &exe,
        "_sidebar-run",
    ])?;
    let pane_id = pane_id.trim().to_string();
    if pane_id.is_empty() {
        bail!("wezterm cli split-pane returned no pane id");
    }

    // Every split takes the focus, and the sidebar is a monitor: the user was
    // looking at the pane we split, so hand it back.
    activate(target_pane_id)?;
    Ok(pane_id)
}

/// Focus a pane.
pub(super) fn activate(pane_id: &str) -> Result<()> {
    wezterm::cli(&["cli", "activate-pane", "--pane-id", pane_id])?;
    Ok(())
}

/// Kill the running sidebars, optionally only those of one workspace.
pub(super) fn close(workspace: Option<&str>) -> Result<()> {
    close_except(workspace, None)
}

/// Kill the sidebars of `workspace`, sparing `keep`.
///
/// A sidebar that quits runs this from its own pane, and the pane it kills
/// first is the one whose process is running the loop: it has to keep its own
/// pane out of the list and kill it after everything else.
pub(super) fn close_except(workspace: Option<&str>, keep: Option<&str>) -> Result<()> {
    for pane_id in panes_to_close(&wezterm::panes()?, workspace, keep) {
        kill(&pane_id)?;
    }
    Ok(())
}

fn panes_to_close(
    panes: &[wezterm::PaneSummary],
    workspace: Option<&str>,
    keep: Option<&str>,
) -> Vec<String> {
    sidebar_pane_ids(panes, workspace)
        .into_iter()
        .filter(|pane_id| Some(pane_id.as_str()) != keep)
        .collect()
}

fn kill(pane_id: &str) -> Result<()> {
    wezterm::cli(&["cli", "kill-pane", "--pane-id", pane_id])?;
    Ok(())
}

/// Whether the sidebar is the only pane left in its tab.
fn sidebar_is_only_pane(window_id: &str, pane_id: &str) -> bool {
    wezterm::panes().is_ok_and(|panes| {
        let mut in_tab = panes.iter().filter(|pane| pane.window_id == window_id);
        in_tab.next().is_some_and(|first| first.pane_id == pane_id) && in_tab.next().is_none()
    })
}

/// Build what the sidebar renders from the current state of the world.
///
/// The preferences come from workmux's own store rather than from the app: the
/// snapshot overwrites the app's copy of them, so reading them back out of the
/// app would make the last value its own source.
fn build_view(
    mux: &dyn Multiplexer,
    status_icons: &StatusIcons,
    git_statuses: HashMap<PathBuf, GitStatus>,
) -> Result<SidebarSnapshot> {
    let config = Config::load(None).unwrap_or_default();
    let position = super::read_sidebar_position(&config);
    let layout_mode = super::read_sidebar_layout_mode();
    let filter_mode = super::read_sidebar_filter_mode();
    let agents = StateStore::new()?.load_reconciled_agents(mux)?;
    let panes = wezterm::panes()?;

    let mut pane_window_ids = HashMap::new();
    let mut pane_window_indexes = HashMap::new();
    let mut window_pane_counts: HashMap<String, usize> = HashMap::new();
    let mut active_pane_ids = HashSet::new();
    let mut active_windows = HashSet::new();

    for pane in &panes {
        pane_window_ids.insert(pane.pane_id.clone(), pane.window_id.clone());
        pane_window_indexes.insert(pane.pane_id.clone(), pane.window_index);
        *window_pane_counts.entry(pane.window_id.clone()).or_default() += 1;
        if pane.is_active {
            active_pane_ids.insert(pane.pane_id.clone());
            active_windows.insert((pane.workspace.clone(), pane.window_id.clone()));
        }
    }

    Ok(build_snapshot(
        agents,
        // tmux's window-status icons have no WezTerm counterpart, and a pane
        // that is absent from the map is simply never suppressed.
        &HashMap::new(),
        &pane_window_ids,
        &pane_window_indexes,
        active_windows,
        active_pane_ids,
        window_pane_counts,
        position,
        layout_mode,
        filter_mode,
        config.sidebar.sort.unwrap_or_default(),
        status_icons,
        git_statuses,
        HashMap::new(),
        HashMap::new(),
        &super::read_sidebar_sleeping(),
    ))
}

/// Take one poll: rebuild the list and notice a window that has emptied out.
fn poll(
    mux: &Arc<dyn Multiplexer>,
    app: &mut SidebarApp,
    last_pane_check: &mut LastPaneCheck,
    git_statuses: &HashMap<PathBuf, GitStatus>,
    git_paths: &Arc<Mutex<Vec<PathBuf>>>,
) -> Result<()> {
    let snapshot = build_view(mux.as_ref(), &app.status_icons, git_statuses.clone())?;

    if let Ok(mut published) = git_paths.lock() {
        let mut paths: Vec<PathBuf> = snapshot.agents.iter().map(|a| a.path.clone()).collect();
        paths.sort();
        paths.dedup();
        *published = paths;
    }

    last_pane_check.pane_count = app
        .host_window_id()
        .and_then(|window_id| snapshot.window_pane_counts.get(window_id))
        .copied();
    if last_pane_check.should_exit(app.host_identity(), sidebar_is_only_pane) {
        quit_for_last_pane(app);
    }

    app.apply_snapshot(snapshot);
    Ok(())
}

fn process_event(
    event: AppEvent,
    app: &mut SidebarApp,
    git_statuses: &mut HashMap<PathBuf, GitStatus>,
    needs_render: &mut bool,
    needs_clear: &mut bool,
) {
    match event {
        AppEvent::Input(event) => {
            let outcome = apply_input(app, event);
            *needs_render |= outcome.render;
            *needs_clear |= outcome.clear;
        }
        AppEvent::GitStatuses(statuses) => {
            *git_statuses = statuses;
            *needs_render = true;
        }
    }
}

/// Run the sidebar TUI (the hidden `_sidebar-run` command).
pub(super) fn run_sidebar() -> Result<()> {
    let mux = create_backend(detect_backend());
    if !mux.is_running().unwrap_or(false) {
        tracing::info!("sidebar-run exiting: mux not running");
        return Ok(());
    }

    let config = Config::load(None).unwrap_or_default();
    let mut app = SidebarApp::new_client(Arc::clone(&mux))?;
    let Some(host_identity) = app.host_identity().cloned() else {
        tracing::error!("sidebar-run exiting: host pane identity unavailable");
        return Ok(());
    };

    // Claim the pane before the TUI paints: everything that finds a sidebar to
    // toggle or kill looks for this title, and a pane that never sets it is
    // invisible to them.
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]0;{SIDEBAR_PANE_TITLE}\x07")?;
    stdout.flush()?;

    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let (tx, rx) = mpsc::channel();
    let git_paths = spawn_git_worker(tx.clone(), config.main_branch.clone());
    spawn_input_thread(tx);

    let mut git_statuses: HashMap<PathBuf, GitStatus> = HashMap::new();
    let mut needs_render = true;
    let mut needs_clear = false;
    let startup = Instant::now();
    let startup_grace = Duration::from_secs(3);
    let mut last_pane_check = LastPaneCheck::new(startup + startup_grace);
    let mut last_poll: Option<Instant> = None;
    let mut last_tick = Instant::now();
    let mut quit = Quit::Workspace;

    loop {
        if needs_render {
            if needs_clear {
                terminal.clear()?;
                needs_clear = false;
            }
            terminal.draw(|f| render_sidebar(f, &mut app))?;
            needs_render = false;
        }

        // Sleep exactly until the next thing that comes due: the poll, a
        // spinner frame, the startup recheck, or a pending resize.
        let now = Instant::now();
        let mut wait = last_poll.map_or(Duration::ZERO, |last| {
            POLL_INTERVAL.saturating_sub(now.duration_since(last))
        });
        if let Some(interval) = app
            .host_window_active()
            .then(|| app.refresh_interval())
            .flatten()
        {
            wait = wait.min(interval.saturating_sub(now.duration_since(last_tick)));
        }
        if let Some(grace) = last_pane_check.timeout(now) {
            wait = wait.min(grace);
        }
        if let Some(deadline) = app.resize_deadline {
            wait = wait.min(deadline.saturating_duration_since(now));
        }

        match rx.recv_timeout(wait) {
            Ok(event) => process_event(
                event,
                &mut app,
                &mut git_statuses,
                &mut needs_render,
                &mut needs_clear,
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("sidebar-run exiting: event channel disconnected");
                break;
            }
        }
        while let Ok(event) = rx.try_recv() {
            process_event(
                event,
                &mut app,
                &mut git_statuses,
                &mut needs_render,
                &mut needs_clear,
            );
        }

        let now = Instant::now();

        if last_poll.is_none_or(|last| now.duration_since(last) >= POLL_INTERVAL) {
            if let Err(error) = poll(&mux, &mut app, &mut last_pane_check, &git_statuses, &git_paths)
            {
                tracing::warn!(%error, "sidebar poll failed");
            }
            needs_render = true;
            // From the end of the poll, not its start: a slow poll must not
            // make the next one look overdue and spin.
            last_poll = Some(Instant::now());
        }

        if last_pane_check.grace_expired(now)
            && last_pane_check.should_exit(app.host_identity(), sidebar_is_only_pane)
        {
            quit_for_last_pane(&mut app);
        }

        // Time-dependent content (a working agent's spinner, elapsed times)
        // supplies its own interval; a static sidebar needs no redraw at all.
        if let Some(interval) = app
            .host_window_active()
            .then(|| app.refresh_interval())
            .flatten()
            && now.duration_since(last_tick) >= interval
        {
            last_tick = now;
            app.tick();
            needs_render = true;
        }

        app.process_pending_resize(&startup, startup_grace);

        if app.should_quit {
            tracing::info!(
                host_window = ?app.host_window_id(),
                quit_reason = app.quit_reason.as_deref().unwrap_or("unknown"),
                "sidebar-run quitting"
            );
            quit = if app.quit_silent {
                Quit::Silent
            } else {
                Quit::Workspace
            };
            break;
        }
    }

    // The console has to be handed back before this pane is killed, or the
    // dying console keeps the alternate screen.
    drop(terminal);
    drop(guard);
    let host_pane_id = host_identity.pane_id.to_string();
    let _ = match quit {
        Quit::Silent => kill(&host_pane_id),
        // Ourselves last: killing this pane takes this process with it, so
        // anything after it would never run.
        Quit::Workspace => {
            close_except(Some(&host_identity.session_name), Some(&host_pane_id))
                .and_then(|()| kill(&host_pane_id))
        }
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::wezterm::PaneSummary;

    fn pane(pane_id: &str, tab_id: u64, workspace: &str, title: &str) -> PaneSummary {
        PaneSummary {
            pane_id: pane_id.to_string(),
            tab_id,
            window_id: tab_id.to_string(),
            window_index: 0,
            workspace: workspace.to_string(),
            title: title.to_string(),
            is_active: false,
        }
    }

    /// Sidebars are placed per tab, so `on` walks the tabs that lack one.
    #[test]
    fn tabs_are_planned_in_id_order_and_skip_the_ones_that_have_a_sidebar() {
        let panes = vec![
            pane("1", 10, "default", "cmd.exe"),
            pane("2", 10, "default", "workmux-sidebar"),
            pane("3", 11, "default", "cmd.exe"),
            pane("5", 12, "default", "cmd.exe"),
            pane("4", 13, "other", "workmux-sidebar"),
        ];

        assert_eq!(
            plan_tabs(&panes, "default"),
            vec!["3".to_string(), "5".to_string()]
        );
        assert!(plan_tabs(&panes, "other").is_empty());
    }

    /// A sidebar is found by the title it claims, and only within the
    /// workspace being toggled.
    #[test]
    fn sidebars_are_found_by_title_within_a_workspace() {
        let panes = vec![
            pane("1", 10, "default", "cmd.exe"),
            pane("2", 10, "default", "workmux-sidebar"),
            pane("3", 11, "other", "workmux-sidebar"),
        ];

        assert_eq!(sidebar_pane_ids(&panes, None), vec!["2", "3"]);
        assert_eq!(sidebar_pane_ids(&panes, Some("default")), vec!["2"]);
        assert!(sidebar_pane_ids(&panes, Some("missing")).is_empty());
    }

    /// A quitting sidebar must not be in its own kill list: killing that pane
    /// ends the process before the loop reaches the others.
    #[test]
    fn a_quitting_sidebar_is_kept_out_of_its_own_kill_list() {
        let panes = vec![
            pane("2", 10, "default", "workmux-sidebar"),
            pane("3", 11, "default", "workmux-sidebar"),
            pane("4", 11, "other", "workmux-sidebar"),
            pane("5", 11, "default", "cmd.exe"),
        ];

        assert_eq!(
            panes_to_close(&panes, Some("default"), Some("2")),
            vec!["3".to_string()]
        );
        assert_eq!(
            panes_to_close(&panes, Some("default"), None),
            vec!["2".to_string(), "3".to_string()]
        );
        assert_eq!(
            panes_to_close(&panes, None, Some("2")),
            vec!["3".to_string(), "4".to_string()]
        );
    }
}
