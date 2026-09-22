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
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;

use crate::config::{Config, SidebarPosition};
use crate::git::{self, GitStatus};
use crate::multiplexer::wezterm;
use crate::multiplexer::{AgentPane, Multiplexer, create_backend, detect_backend};
use crate::state::{AgentStateCache, GlobalSettings, StateStore};

use super::app::{SidebarApp, SidebarFilterMode};
use super::input::{LastPaneCheck, apply_input, quit_for_last_pane};
use super::snapshot::{SidebarSnapshot, build_snapshot};
use super::ui::render_sidebar;

/// How often the pane re-reads the state store and the multiplexer.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often git status is recomputed. Slower than the poll, because a refresh
/// runs git once per agent and git answers in tens of milliseconds.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
/// How often the pane looks for a wake-up from a command that changed state.
///
/// Reading the token is one read of a small file, orders of magnitude below the
/// ~70 ms a `wezterm cli list` costs on this machine, so a state change lands
/// within one of these plus one listing instead of waiting out the poll.
const SIGNAL_INTERVAL: Duration = Duration::from_millis(50);

/// Title this pane claims, so a human can tell a sidebar pane from a shell.
///
/// It is not the pane's identity: WezTerm applies a title only while the pane's
/// tab has the focus, which is exactly when nobody is reading a sidebar. The
/// ids the settings store remembers are the identity; a title is a second,
/// best-effort signal for panes no id covers.
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

pub(super) fn tabs_without_sidebar(
    workspace: &str,
    position: SidebarPosition,
) -> Result<Vec<String>> {
    let panes = wezterm::panes()?;
    let sidebars = live_sidebar_ids(&panes);
    Ok(plan_tabs(&panes, &sidebars, workspace, position))
}

/// One pane per tab of `workspace` that has no sidebar yet.
///
/// The pane named is the one the sidebar is split off, and the pane to split is
/// the one that reaches the tab's edge: a split puts the new pane against the
/// edge of the pane it split, so the tallest pane carries a sidebar down the
/// side and the widest one carries a sidebar across the top. A pane reaching the
/// edge always exists -- panes tile their tab, and the outermost of them extends
/// to it -- apart from a layout with a split inside every one of its edges,
/// where the first pane of the tab is split instead and the sidebar is as long
/// as that pane. Ties go to the pane listed first. Tabs are visited in id order
/// so a run is reproducible.
fn plan_tabs(
    panes: &[wezterm::PaneSummary],
    sidebars: &HashSet<String>,
    workspace: &str,
    position: SidebarPosition,
) -> Vec<String> {
    // Per tab: the pane to split, how far it reaches across the sidebar's axis,
    // and whether the tab holds a sidebar already.
    let mut tabs: BTreeMap<u64, (String, u16, bool)> = BTreeMap::new();

    for pane in panes.iter().filter(|pane| pane.workspace == workspace) {
        let extent = match position {
            SidebarPosition::Left => pane.rows,
            SidebarPosition::Top => pane.cols,
        };
        let entry = tabs
            .entry(pane.tab_id)
            .or_insert_with(|| (pane.pane_id.clone(), extent, false));
        // Strictly greater, so the pane listed first wins a tie.
        if extent > entry.1 {
            entry.0 = pane.pane_id.clone();
            entry.1 = extent;
        }
        entry.2 |= sidebars.contains(&pane.pane_id);
    }

    tabs.into_values()
        .filter(|(_, _, has_sidebar)| !has_sidebar)
        .map(|(pane_id, _, _)| pane_id)
        .collect()
}

/// The panes that are sidebars right now.
///
/// Read from the ids the store remembers rather than from the panes' titles,
/// and resolved against the live panes: an id whose pane is gone is dropped
/// rather than naming whichever unrelated pane holds that id next.
fn live_sidebar_ids(panes: &[wezterm::PaneSummary]) -> HashSet<String> {
    live_sidebar_ids_from(panes, &recorded_sidebar_ids())
}

/// `live_sidebar_ids` with the record handed in, so a test can drive it.
fn live_sidebar_ids_from(panes: &[wezterm::PaneSummary], recorded: &[String]) -> HashSet<String> {
    panes
        .iter()
        .filter(|pane| {
            recorded.iter().any(|id| id == &pane.pane_id) || pane.title == SIDEBAR_PANE_TITLE
        })
        .map(|pane| pane.pane_id.clone())
        .collect()
}

/// The sidebar ids the store recorded, or none when the WezTerm server that
/// recorded them is no longer the one running.
///
/// A restarted server hands the same small pane ids out again, so an id from
/// before the restart names an unrelated pane now, and `off` must not kill that
/// pane on the strength of it.
fn recorded_sidebar_ids() -> Vec<String> {
    let Ok(store) = StateStore::new() else {
        return Vec::new();
    };
    let Ok(settings) = store.load_settings() else {
        return Vec::new();
    };
    sidebar_ids_from_settings(&settings, wezterm_boot_id().as_deref())
}

/// Split out of `recorded_sidebar_ids` so a test can read both sides of the
/// server-identity check without a store and a mux.
fn sidebar_ids_from_settings(settings: &GlobalSettings, boot_id: Option<&str>) -> Vec<String> {
    if settings.sidebar_boot_id.as_deref() != boot_id {
        return Vec::new();
    }
    settings.sidebar_panes.clone()
}

/// The running WezTerm server's identity, when it can be had at all.
fn wezterm_boot_id() -> Option<String> {
    wezterm::WezTermBackend::new()
        .server_boot_id()
        .ok()
        .flatten()
}

/// Remember a pane as a sidebar, so that a later `off` can find it again.
fn record_sidebar_pane(pane_id: &str) {
    let boot_id = wezterm_boot_id();
    super::update_sidebar_settings(|settings| {
        record_sidebar(settings, pane_id, boot_id.as_deref())
    });
}

/// The mutation behind `record_sidebar_pane`.
fn record_sidebar(settings: &mut GlobalSettings, pane_id: &str, boot_id: Option<&str>) {
    // Ids from another server name other panes now.
    if settings.sidebar_boot_id.as_deref() != boot_id {
        settings.sidebar_panes.clear();
        settings.sidebar_boot_id = boot_id.map(str::to_string);
    }
    if !settings.sidebar_panes.iter().any(|id| id == pane_id) {
        settings.sidebar_panes.push(pane_id.to_string());
    }
}

/// Drop panes from the record, for the ones that are gone or going.
fn forget_sidebar_panes(pane_ids: &[String]) {
    if pane_ids.is_empty() {
        return;
    }
    super::update_sidebar_settings(|settings| drop_sidebar_panes(settings, pane_ids));
}

/// The mutation behind `forget_sidebar_panes`.
fn drop_sidebar_panes(settings: &mut GlobalSettings, pane_ids: &[String]) {
    settings
        .sidebar_panes
        .retain(|id| !pane_ids.iter().any(|gone| gone == id));
}

/// Pane ids of the running sidebars, optionally only those of one workspace.
pub(super) fn sidebar_panes(workspace: Option<&str>) -> Result<Vec<String>> {
    let panes = wezterm::panes()?;
    let sidebars = live_sidebar_ids(&panes);
    Ok(sidebar_pane_ids(&panes, &sidebars, workspace))
}

fn sidebar_pane_ids(
    panes: &[wezterm::PaneSummary],
    sidebars: &HashSet<String>,
    workspace: Option<&str>,
) -> Vec<String> {
    panes
        .iter()
        .filter(|pane| sidebars.contains(&pane.pane_id))
        .filter(|pane| workspace.is_none_or(|workspace| pane.workspace == workspace))
        .map(|pane| pane.pane_id.clone())
        .collect()
}

/// Split `target_pane_id` and run the sidebar in the new pane.
///
/// The pane is split rather than the tab. WezTerm can split a whole tab
/// (`--top-level`, tmux's full-window split), but on Windows it makes that split
/// in two steps: it first resizes the tab down to the part the panes already
/// there keep, and the tab keeps that smaller size afterwards. Every pane in it
/// then draws inside a strip of the window, with a band of the window left
/// empty, until something resizes the window -- and nothing does. A split of a
/// pane has no such step, and the sidebar reaches the tab's edge wherever the
/// pane it was split off does.
pub(super) fn open(target_pane_id: &str, position: SidebarPosition, cells: u16) -> Result<String> {
    let exe = std::env::current_exe().context("failed to locate the workmux executable")?;
    let exe = exe.to_string_lossy().into_owned();
    let cells = cells.to_string();
    let pane_id = wezterm::cli(&open_args(&exe, target_pane_id, position, &cells))?;
    let pane_id = pane_id.trim().to_string();
    if pane_id.is_empty() {
        bail!("wezterm cli split-pane returned no pane id");
    }
    // Recorded before the pane is handed back: the pane exists either way, and
    // an unrecorded sidebar is one no `off` can turn off.
    record_sidebar_pane(&pane_id);

    // Every split takes the focus, and the sidebar is a monitor: the user was
    // looking at the pane we split, so hand it back.
    activate(target_pane_id)?;
    Ok(pane_id)
}

/// The `wezterm cli` arguments that open a sidebar pane.
fn open_args<'a>(
    exe: &'a str,
    target_pane_id: &'a str,
    position: SidebarPosition,
    cells: &'a str,
) -> Vec<&'a str> {
    vec![
        "cli",
        "split-pane",
        "--pane-id",
        target_pane_id,
        match position {
            SidebarPosition::Left => "--left",
            SidebarPosition::Top => "--top",
        },
        "--cells",
        cells,
        "--",
        exe,
        "_sidebar-run",
    ]
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
    let panes = wezterm::panes()?;
    let sidebars = live_sidebar_ids(&panes);
    let mut killed: Vec<String> = Vec::new();
    for pane_id in panes_to_close(&panes, &sidebars, workspace, keep) {
        kill(&pane_id)?;
        killed.push(pane_id);
    }
    forget_sidebar_panes(&killed);
    Ok(())
}

fn panes_to_close(
    panes: &[wezterm::PaneSummary],
    sidebars: &HashSet<String>,
    workspace: Option<&str>,
    keep: Option<&str>,
) -> Vec<String> {
    sidebar_pane_ids(panes, sidebars, workspace)
        .into_iter()
        .filter(|pane_id| Some(pane_id.as_str()) != keep)
        .collect()
}

fn kill(pane_id: &str) -> Result<()> {
    wezterm::cli(&["cli", "kill-pane", "--pane-id", pane_id])?;
    Ok(())
}

/// Whether the sidebar is the only pane left in its tab, as of `panes`.
fn sidebar_is_only_pane(panes: &[wezterm::PaneSummary], window_id: &str, pane_id: &str) -> bool {
    let mut in_tab = panes.iter().filter(|pane| pane.window_id == window_id);
    in_tab.next().is_some_and(|first| first.pane_id == pane_id) && in_tab.next().is_none()
}

/// The wake-up a workmux command leaves for the sidebars of its multiplexer.
///
/// There is no daemon here to receive a signal, so a command that changed state
/// writes a token into the state store and the sidebar's loop compares it with
/// the last one it read. No store at all, or nothing written yet, is simply no
/// wake-up: the timed poll still runs.
struct RefreshSignal {
    path: Option<PathBuf>,
    seen: Option<String>,
}

impl RefreshSignal {
    /// The wake-up of the multiplexer this pane runs under.
    fn new(mux: &dyn Multiplexer) -> Self {
        let path = StateStore::new()
            .ok()
            .map(|store| store.refresh_signal_path(mux.name(), &mux.instance_id()));
        Self::at(path)
    }

    /// A wake-up at `path`; `None` is a sidebar nothing can wake.
    fn at(path: Option<PathBuf>) -> Self {
        Self { path, seen: None }
    }

    /// The token the last command left behind, if any.
    fn read(&self) -> Option<String> {
        let token = crate::util::read_shared(self.path.as_ref()?).ok()?;
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_string())
    }

    /// Whether a command asked for a poll since the last call.
    fn requested(&mut self) -> bool {
        let Some(token) = self.read() else {
            return false;
        };
        if self.seen.as_deref() == Some(token.as_str()) {
            return false;
        }
        self.seen = Some(token);
        true
    }
}

/// Ask every running sidebar of `mux` for a poll now.
///
/// A state change used to wait for the sidebar's next timed poll -- up to a
/// second of rows that are already wrong. The command that made the change says
/// so instead, and the wait becomes one read of a token.
pub(super) fn request_refresh(mux: &dyn Multiplexer) {
    let Ok(store) = StateStore::new() else {
        return;
    };
    if let Err(error) = store.signal_sidebar_refresh(mux.name(), &mux.instance_id()) {
        tracing::warn!(%error, "sidebar refresh signal could not be written");
    }
}

/// What a poll reads, and what it keeps from one poll to the next.
///
/// What a config file looked like the last time it was read.
///
/// A modification time is the whole point: the sidebar exists to show settings
/// a user edits in a file, and it has to notice the edit. A file that is
/// rewritten within the same timestamp and to the same length is the one case
/// this misses, and editing a config by hand cannot hit it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    /// The stamp of the file at `path`, or `None` if there is no such file.
    fn read(path: &std::path::Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

/// Whether the file at `path` no longer matches the stamp it was given.
///
/// A file that was not there and still is not has not changed; one that
/// appeared, disappeared or was written to has.
fn stamp_moved(stamp: Option<FileStamp>, path: &std::path::Path) -> bool {
    FileStamp::read(path) != stamp
}

/// The config the sidebar renders with, loaded when it changes rather than
/// when it is asked for.
///
/// A sidebar polls once a second for as long as it is open, and every poll used
/// to load the config: a git process to find the project's, two file reads and
/// a parse, a third of a second on Windows, spent to arrive at the same answer
/// every second of every sidebar's life. The files it came from are stat'ed
/// instead, which costs nothing, and a load only happens when one of them has
/// moved.
struct ConfigWatcher {
    config: Config,
    global_path: PathBuf,
    global_stamp: Option<FileStamp>,
    /// The project config the load settled on, and its stamp. A config that
    /// appears closer to the working directory than this one is not noticed
    /// until something else moves; settings a user edits are noticed at once.
    project: Option<(PathBuf, Option<FileStamp>)>,
}

impl ConfigWatcher {
    /// Load the config for this directory and remember what it came from.
    fn load() -> Self {
        let (config, location) = Config::load_with_location(None, None).unwrap_or_default();
        let global_path = crate::config::global_config_path().unwrap_or_default();
        Self {
            global_stamp: FileStamp::read(&global_path),
            global_path,
            project: location.map(|location| {
                let stamp = FileStamp::read(&location.config_path);
                (location.config_path, stamp)
            }),
            config,
        }
    }

    /// The config, loaded again first if one of its files has changed.
    fn config(&mut self) -> &Config {
        if self.stale() {
            *self = Self::load();
        }
        &self.config
    }

    /// Whether one of the files the config came from has moved.
    fn stale(&self) -> bool {
        stamp_moved(self.global_stamp, &self.global_path)
            || self
                .project
                .as_ref()
                .is_some_and(|(path, stamp)| stamp_moved(*stamp, path))
    }
}

/// The store and the cache of already-parsed state files outlive a poll:
/// without the cache every poll re-reads every agent's file, which is what the
/// tmux daemon's own cache is for.
struct Reader<'a> {
    store: StateStore,
    cache: AgentStateCache,
    config: ConfigWatcher,
    mux: &'a dyn Multiplexer,
}

impl<'a> Reader<'a> {
    fn new(mux: &'a dyn Multiplexer) -> Result<Self> {
        Ok(Self {
            store: StateStore::new()?,
            cache: AgentStateCache::default(),
            config: ConfigWatcher::load(),
            mux,
        })
    }

    /// Build what the sidebar renders out of one reading of the world.
    ///
    /// The preferences come from workmux's own store rather than from the app:
    /// the snapshot overwrites the app's copy of them, so reading them back out
    /// of the app would make the last value its own source.
    fn view(
        &mut self,
        panes: &wezterm::InstancePanes,
        git_statuses: HashMap<PathBuf, GitStatus>,
    ) -> Result<SidebarSnapshot> {
        // Taken together, and before the store is read: the config borrow ends
        // here so that the reading below can take the reader mutably.
        let (position, layout_mode, filter_mode, sort, status_icons) = {
            let config = self.config.config();
            (
                super::read_sidebar_position(config),
                super::read_sidebar_layout_mode(),
                super::read_sidebar_filter_mode(),
                config.sidebar.sort.unwrap_or_default(),
                config.status_icons.clone(),
            )
        };
        // Read per poll rather than once: a server that restarts mid-session
        // hands out pane ids that the state written before it no longer fits.
        let boot_id = self.mux.server_boot_id().ok().flatten();
        let (agents, _) = self.store.load_reconciled_agents_from_snapshot_cached(
            &mut self.cache,
            self.mux,
            &panes.live,
            boot_id.as_deref(),
        )?;

        let mut pane_window_ids = HashMap::new();
        let mut pane_window_indexes = HashMap::new();
        let mut window_pane_counts: HashMap<String, usize> = HashMap::new();
        let mut active_pane_ids = HashSet::new();
        let mut active_windows = HashSet::new();

        for pane in &panes.summaries {
            pane_window_ids.insert(pane.pane_id.clone(), pane.window_id.clone());
            pane_window_indexes.insert(pane.pane_id.clone(), pane.window_index);
            *window_pane_counts
                .entry(pane.window_id.clone())
                .or_default() += 1;
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
            sort,
            &status_icons,
            git_statuses,
            HashMap::new(),
            HashMap::new(),
            &super::read_sidebar_sleeping(),
        ))
    }
}

/// The agent pane ids in the order the sidebar lists them.
///
/// `sidebar next` runs outside any sidebar, and on tmux it reads back the order
/// the daemon published. Nothing publishes one here, and the order is a
/// function of the live panes, the state store and the settings, so it is
/// recomputed instead: there is no copy to fall out of step.
pub(super) fn listed_agent_panes(workspace: &str, mux: &dyn Multiplexer) -> Result<Vec<String>> {
    let panes = wezterm::instance_panes()?;
    let mut reader = Reader::new(mux)?;
    let snapshot = reader.view(&panes, HashMap::new())?;
    Ok(listed_pane_ids(
        snapshot.agents,
        snapshot.filter_mode,
        workspace,
    ))
}

/// The pane ids the sidebar would show for `workspace`'s own session filter.
///
/// This is the same rule `SidebarApp::apply_snapshot` applies: a sidebar scoped
/// to its session lists only the agents in that session.
fn listed_pane_ids(
    agents: Vec<AgentPane>,
    filter_mode: SidebarFilterMode,
    workspace: &str,
) -> Vec<String> {
    agents
        .into_iter()
        .filter(|agent| filter_mode != SidebarFilterMode::Session || agent.session == workspace)
        .map(|agent| agent.pane_id)
        .collect()
}

/// Take one poll: rebuild the list and notice a window that has emptied out.
fn poll(
    reader: &mut Reader,
    app: &mut SidebarApp,
    last_pane_check: &mut LastPaneCheck,
    git_statuses: &HashMap<PathBuf, GitStatus>,
    git_paths: &Arc<Mutex<Vec<PathBuf>>>,
    panes: &wezterm::InstancePanes,
) -> Result<()> {
    let snapshot = reader.view(panes, git_statuses.clone())?;

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
    if last_pane_check.should_exit(app.host_identity(), |window_id, pane_id| {
        sidebar_is_only_pane(&panes.summaries, window_id, pane_id)
    }) {
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
    let mut reader = match Reader::new(mux.as_ref()) {
        Ok(reader) => Some(reader),
        Err(error) => {
            tracing::warn!(%error, "sidebar has no state store to read");
            None
        }
    };
    // The last reading of the panes: the poll replaces it, and the last-pane
    // check between polls reads it rather than asking again.
    let mut panes: Option<wezterm::InstancePanes> = None;
    let mut refresh = RefreshSignal::new(mux.as_ref());

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
        let requested = refresh.requested();
        if requested {
            tracing::debug!("sidebar woken by a refresh signal");
        }
        let mut wait = last_poll.map_or(Duration::ZERO, |last| {
            POLL_INTERVAL.saturating_sub(now.duration_since(last))
        });
        // A wake-up is only read at the top of the loop, so the sleep is never
        // longer than the interval one is written on.
        wait = wait.min(SIGNAL_INTERVAL);
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

        if requested || last_poll.is_none_or(|last| now.duration_since(last) >= POLL_INTERVAL) {
            match wezterm::instance_panes() {
                Ok(fresh) => {
                    if let Some(reader) = reader.as_mut()
                        && let Err(error) = poll(
                            reader,
                            &mut app,
                            &mut last_pane_check,
                            &git_statuses,
                            &git_paths,
                            &fresh,
                        )
                    {
                        tracing::warn!(%error, "sidebar poll failed");
                    }
                    panes = Some(fresh);
                }
                Err(error) => tracing::warn!(%error, "sidebar cannot read the panes"),
            }
            needs_render = true;
            // From the end of the poll, not its start: a slow poll must not
            // make the next one look overdue and spin.
            last_poll = Some(Instant::now());
        }

        if last_pane_check.grace_expired(now)
            && let Some(panes) = panes.as_ref()
            && last_pane_check.should_exit(app.host_identity(), |window_id, pane_id| {
                sidebar_is_only_pane(&panes.summaries, window_id, pane_id)
            })
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
    // Dropped before the kill rather than after: killing this pane ends this
    // process, so nothing after it runs, and an id left behind would name
    // whichever pane inherits it.
    forget_sidebar_panes(std::slice::from_ref(&host_pane_id));
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

    /// A pane filling its tab, for the tests that do not care where a sidebar
    /// lands.
    fn pane(pane_id: &str, tab_id: u64, workspace: &str, title: &str) -> PaneSummary {
        PaneSummary {
            pane_id: pane_id.to_string(),
            tab_id,
            window_id: tab_id.to_string(),
            window_index: 0,
            workspace: workspace.to_string(),
            title: title.to_string(),
            is_active: false,
            cols: 80,
            rows: 24,
        }
    }

    /// The same pane, reaching only part of its tab.
    fn pane_of_extent(
        pane_id: &str,
        tab_id: u64,
        workspace: &str,
        cols: u16,
        rows: u16,
    ) -> PaneSummary {
        PaneSummary {
            cols,
            rows,
            ..pane(pane_id, tab_id, workspace, "cmd.exe")
        }
    }

    /// A watcher over the given files, as a load would leave it.
    fn watcher(global: &std::path::Path, project: Option<&std::path::Path>) -> ConfigWatcher {
        ConfigWatcher {
            config: Config::default(),
            global_path: global.to_path_buf(),
            global_stamp: FileStamp::read(global),
            project: project.map(|path| (path.to_path_buf(), FileStamp::read(path))),
        }
    }

    /// A config nobody has touched is not read again: this is what keeps a
    /// poll from being a config load a second, per sidebar.
    #[test]
    fn a_config_nobody_touched_is_not_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yaml");
        let project = dir.path().join(".workmux.yaml");
        std::fs::write(&global, "agent: claude\n").unwrap();
        std::fs::write(&project, "agent: claude\n").unwrap();

        let watcher = watcher(&global, Some(&project));

        assert!(!watcher.stale());
    }

    /// An edit is noticed: the sidebar's settings live in this file.
    #[test]
    fn a_config_file_that_is_written_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yaml");
        std::fs::write(&global, "agent: claude\n").unwrap();
        let watcher = watcher(&global, None);

        std::fs::write(&global, "agent: claude\nsidebar:\n  sort: name\n").unwrap();

        assert!(watcher.stale());
    }

    /// A project config written while the sidebar runs is noticed too, which is
    /// what `workmux init` in that directory leaves behind.
    #[test]
    fn a_project_config_that_appears_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yaml");
        let project = dir.path().join(".workmux.yaml");
        std::fs::write(&global, "agent: claude\n").unwrap();
        let watcher = watcher(&global, Some(&project));
        assert!(!watcher.stale());

        std::fs::write(&project, "agent: claude\n").unwrap();

        assert!(watcher.stale());
    }

    /// A project config that is removed falls back to the global one, so the
    /// removal is a change like any other.
    #[test]
    fn a_project_config_that_disappears_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yaml");
        let project = dir.path().join(".workmux.yaml");
        std::fs::write(&global, "agent: claude\n").unwrap();
        std::fs::write(&project, "agent: claude\n").unwrap();
        let watcher = watcher(&global, Some(&project));

        std::fs::remove_file(&project).unwrap();

        assert!(watcher.stale());
    }

    /// A user with no config file at all is not a user whose config changed on
    /// every poll.
    #[test]
    fn a_config_file_that_was_never_there_does_not_age_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yaml");
        let project = dir.path().join(".workmux.yaml");

        let watcher = watcher(&global, Some(&project));

        assert!(!watcher.stale());
    }

    /// The global config moving is enough on its own, whatever the project
    /// config is doing.
    #[test]
    fn the_global_config_alone_can_age_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.yaml");
        let project = dir.path().join(".workmux.yaml");
        std::fs::write(&global, "agent: claude\n").unwrap();
        std::fs::write(&project, "agent: claude\n").unwrap();
        let watcher = watcher(&global, Some(&project));
        assert!(!watcher.stale());

        std::fs::write(&global, "agent: claude\nwindow_prefix: wm-\n").unwrap();

        assert!(watcher.stale());
    }

    /// The sidebar record, as the settings store keeps it.
    fn ids(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    fn agent(pane_id: &str, workspace: &str) -> AgentPane {
        AgentPane {
            session: workspace.to_string(),
            window_name: "w".to_string(),
            pane_id: pane_id.to_string(),
            window_id: String::new(),
            window_index: None,
            path: PathBuf::from("C:\\repo"),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
        }
    }

    /// Sidebars are placed per tab, so `on` walks the tabs that lack one.
    #[test]
    fn tabs_are_planned_in_id_order_and_skip_the_ones_that_have_a_sidebar() {
        let panes = vec![
            pane("1", 10, "default", "cmd.exe"),
            pane("2", 10, "default", "cmd.exe"),
            pane("3", 11, "default", "cmd.exe"),
            pane("5", 12, "default", "cmd.exe"),
            pane("4", 13, "other", "cmd.exe"),
        ];
        let sidebars = ids(&["2", "4"]);

        assert_eq!(
            plan_tabs(&panes, &sidebars, "default", SidebarPosition::Left),
            vec!["3".to_string(), "5".to_string()]
        );
        assert!(plan_tabs(&panes, &sidebars, "other", SidebarPosition::Left).is_empty());
    }

    /// A sidebar is the pane the record names -- WezTerm withholds the title of
    /// every pane whose tab is unfocused, which is every sidebar doing its job
    /// -- plus any pane claiming the title, and only within the workspace being
    /// asked about. A recorded id whose pane is gone names nothing at all.
    #[test]
    fn sidebars_are_the_recorded_panes_and_any_pane_claiming_the_title() {
        let panes = vec![
            pane("1", 10, "default", "cmd.exe"),
            pane("2", 10, "default", "cmd.exe"),
            pane("3", 11, "default", "workmux-sidebar"),
            pane("4", 11, "other", "cmd.exe"),
        ];
        let recorded: Vec<String> = ["2", "4", "9"].iter().map(|id| id.to_string()).collect();
        let sidebars = live_sidebar_ids_from(&panes, &recorded);

        assert_eq!(sidebars, ids(&["2", "3", "4"]));
        assert_eq!(
            sidebar_pane_ids(&panes, &sidebars, None),
            vec!["2", "3", "4"]
        );
        assert_eq!(
            sidebar_pane_ids(&panes, &sidebars, Some("default")),
            vec!["2", "3"]
        );
        assert!(
            sidebar_pane_ids(&panes, &sidebars, Some("missing")).is_empty(),
            "a sidebar of another workspace is not one the caller can toggle"
        );
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
        let sidebars = ids(&["2", "3", "4"]);

        assert_eq!(
            panes_to_close(&panes, &sidebars, Some("default"), Some("2")),
            vec!["3".to_string()]
        );
        assert_eq!(
            panes_to_close(&panes, &sidebars, Some("default"), None),
            vec!["2".to_string(), "3".to_string()]
        );
        assert_eq!(
            panes_to_close(&panes, &sidebars, None, Some("2")),
            vec!["3".to_string(), "4".to_string()]
        );
    }

    /// Pane ids come round again after WezTerm restarts, so ids recorded under
    /// the server that is gone must not be read as sidebars of this one.
    #[test]
    fn recorded_ids_are_dropped_when_the_wezterm_server_is_not_the_same_one() {
        let settings = GlobalSettings {
            sidebar_panes: vec!["21".to_string()],
            sidebar_boot_id: Some("wezterm:2".to_string()),
            ..Default::default()
        };

        assert_eq!(
            sidebar_ids_from_settings(&settings, Some("wezterm:2")),
            vec!["21".to_string()]
        );
        assert!(sidebar_ids_from_settings(&settings, Some("wezterm:9")).is_empty());
        assert!(sidebar_ids_from_settings(&settings, None).is_empty());
    }

    /// `open` records the pane it made, once, and a sidebar of a new server
    /// replaces the ids of the old one rather than inheriting them.
    #[test]
    fn recording_a_sidebar_keeps_one_copy_and_follows_the_server() {
        let mut settings = GlobalSettings::default();

        record_sidebar(&mut settings, "21", Some("wezterm:2"));
        record_sidebar(&mut settings, "21", Some("wezterm:2"));
        record_sidebar(&mut settings, "22", Some("wezterm:2"));
        assert_eq!(settings.sidebar_panes, vec!["21", "22"]);
        assert_eq!(settings.sidebar_boot_id.as_deref(), Some("wezterm:2"));

        record_sidebar(&mut settings, "3", Some("wezterm:7"));
        assert_eq!(settings.sidebar_panes, vec!["3"]);
        assert_eq!(settings.sidebar_boot_id.as_deref(), Some("wezterm:7"));
    }

    /// Killing a sidebar drops its id, so the record does not outlive it.
    #[test]
    fn forgetting_sidebars_keeps_the_ones_that_still_run() {
        let mut settings = GlobalSettings {
            sidebar_panes: ids(&["21", "22", "23"]).into_iter().collect(),
            sidebar_boot_id: Some("wezterm:2".to_string()),
            ..Default::default()
        };

        drop_sidebar_panes(&mut settings, &["21".to_string(), "23".to_string()]);
        assert_eq!(settings.sidebar_panes, vec!["22"]);
    }

    /// A sidebar quits when its tab holds nothing but it, and only then: the
    /// reading has to name this pane as the one tab's first pane.
    #[test]
    fn a_tab_holding_only_the_sidebar_is_a_last_pane() {
        let panes = vec![
            pane("1", 10, "default", "cmd.exe"),
            pane("2", 10, "default", "workmux-sidebar"),
            pane("3", 11, "default", "workmux-sidebar"),
        ];

        assert!(
            !sidebar_is_only_pane(&panes, "10", "2"),
            "the shell beside it is still there"
        );
        assert!(sidebar_is_only_pane(&panes, "11", "3"));
        assert!(
            !sidebar_is_only_pane(&panes, "11", "1"),
            "a pane of another tab is not one of this tab's panes"
        );
        assert!(
            !sidebar_is_only_pane(&[], "11", "3"),
            "a reading that names no panes is not an empty tab"
        );
    }

    /// `sidebar next` walks what the sidebar lists, so the session filter it
    /// applies has to be the sidebar's, not every workspace on the server.
    #[test]
    fn listing_agents_under_the_session_filter_keeps_the_workspace() {
        let agents = vec![
            agent("1", "ws-a"),
            agent("2", "ws-b"),
            agent("3", "ws-a"),
        ];

        assert_eq!(
            listed_pane_ids(agents.clone(), SidebarFilterMode::Session, "ws-a"),
            vec!["1".to_string(), "3".to_string()]
        );
        assert_eq!(
            listed_pane_ids(agents, SidebarFilterMode::None, "ws-a"),
            vec!["1".to_string(), "2".to_string(), "3".to_string()]
        );
    }

    /// A wake-up is taken once, not once per look: the loop polls on the change
    /// and then waits again with the token it just read remembered.
    #[test]
    fn a_wake_up_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wezterm__sock.refresh");
        let mut signal = RefreshSignal::at(Some(path.clone()));

        assert!(!signal.requested(), "no command has asked for anything yet");

        crate::util::write_atomic(&path, b"1").unwrap();
        assert!(signal.requested());
        assert!(
            !signal.requested(),
            "the same token is not a second request"
        );

        crate::util::write_atomic(&path, b"2").unwrap();
        assert!(signal.requested(), "a later token is a new request");
    }

    /// A sidebar with nothing to read never wakes and never fails: no state
    /// store, no token written yet.
    #[test]
    fn a_wake_up_that_is_not_there_is_not_a_request() {
        let dir = tempfile::tempdir().unwrap();

        assert!(
            !RefreshSignal::at(Some(dir.path().join("missing.refresh"))).requested(),
            "the file is written by the first command that changes state"
        );
        assert!(!RefreshSignal::at(None).requested());
    }

    /// The store writes the token the loop reads, and every write is a new one
    /// even though the file is the same file.
    #[test]
    fn a_signal_writes_a_token_the_sidebar_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_path(dir.path().to_path_buf()).unwrap();
        let path = store.refresh_signal_path("wezterm", "sock");
        let mut signal = RefreshSignal::at(Some(path.clone()));

        assert!(!path.exists(), "nothing is written until a command asks");
        assert!(!signal.requested());

        store.signal_sidebar_refresh("wezterm", "sock").unwrap();
        assert!(signal.requested());
        assert!(!signal.requested());

        store.signal_sidebar_refresh("wezterm", "sock").unwrap();
        assert!(signal.requested(), "a later write is a new request");
    }

    /// Two instances, and two backends, never share one wake-up -- and an
    /// instance that holds path separators names a file, not a directory.
    #[test]
    fn a_wake_up_names_the_instance_it_is_for() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::with_path(dir.path().to_path_buf()).unwrap();
        let path = store.refresh_signal_path("wezterm", r"\\?\C:\socket");

        assert!(path.starts_with(dir.path().join("runtime")));
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "wezterm__%5C%5C?%5CC%3A%5Csocket.refresh"
        );
        assert_ne!(path, store.refresh_signal_path("tmux", r"\\?\C:\socket"));
        assert_ne!(path, store.refresh_signal_path("wezterm", "socket"));
    }

    /// The sidebar is split off a pane, not off the whole tab: WezTerm's
    /// whole-tab split leaves the tab sized for the part it did not split.
    #[test]
    fn a_sidebar_is_a_split_of_a_pane() {
        assert_eq!(
            open_args(r"C:\workmux\workmux.exe", "12", SidebarPosition::Left, "30"),
            [
                "cli",
                "split-pane",
                "--pane-id",
                "12",
                "--left",
                "--cells",
                "30",
                "--",
                r"C:\workmux\workmux.exe",
                "_sidebar-run",
            ]
        );
        assert_eq!(
            open_args(r"C:\workmux\workmux.exe", "12", SidebarPosition::Top, "13")[4],
            "--top"
        );
    }

    /// A tab is split at the pane that reaches its edge: down the side that is
    /// the tallest pane, across the top the widest, whichever order they are
    /// listed in.
    #[test]
    fn a_sidebar_is_split_off_the_pane_that_reaches_the_tabs_edge() {
        // A tab held by a tall pane beside a stack of two short ones.
        let panes = vec![
            pane_of_extent("1", 10, "default", 60, 12),
            pane_of_extent("2", 10, "default", 60, 12),
            pane_of_extent("3", 10, "default", 30, 24),
        ];

        assert_eq!(
            plan_tabs(&panes, &ids(&[]), "default", SidebarPosition::Left),
            vec!["3".to_string()],
            "only the pane reaching the bottom of the tab carries a sidebar to it"
        );
        assert_eq!(
            plan_tabs(&panes, &ids(&[]), "default", SidebarPosition::Top),
            vec!["1".to_string()],
            "the widest panes tie, and the first listed one wins"
        );
    }
}
