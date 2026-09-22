//! TUI event loop for the sidebar client.

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::cmd::Cmd;
use crate::multiplexer::{create_backend, detect_backend};
use crate::shell::shell_quote;

use super::app::SidebarApp;
use super::client;
use super::daemon_ctrl::ensure_daemon_running;
use super::input::{LastPaneCheck, quit_for_last_pane};
use super::panes::shutdown_all_sidebars;
use super::ui::render_sidebar;

/// Drop guard that restores terminal state on panic or early return.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

enum AppEvent {
    /// A new snapshot is available in the SnapshotHandle.
    SnapshotReady,
    /// A terminal input event (key press, resize, etc.).
    Input(Event),
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

/// Run the sidebar TUI (called by the hidden `_sidebar-run` command).
pub fn run_sidebar() -> Result<()> {
    let mux = create_backend(detect_backend());

    if !mux.is_running().unwrap_or(false) {
        tracing::info!("sidebar-run exiting: mux not running");
        return Ok(());
    }

    // Create app BEFORE entering raw mode: terminal_light::luma() queries
    // the terminal via stdin, which would race with the input reader thread.
    let mut app = SidebarApp::new_client(mux)?;
    let Some(host_identity) = app.host_identity().cloned() else {
        tracing::error!("sidebar-run exiting: host pane identity unavailable");
        return Ok(());
    };

    // Ensure daemon is running (may have auto-exited or crashed)
    let sock_path = ensure_daemon_running()?;

    // Setup terminal (raw mode required before spawning input thread)
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Channel for all events
    let (tx, rx) = mpsc::channel();

    // Snapshot receiver: overwrites latest, sends SnapshotReady wake via
    // a thin forwarding thread that converts () -> AppEvent::SnapshotReady
    let snapshot_handle = {
        let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);
        let event_tx = tx.clone();
        thread::spawn(move || {
            for () in wake_rx {
                if event_tx.send(AppEvent::SnapshotReady).is_err() {
                    break;
                }
            }
        });
        client::connect(&sock_path, wake_tx)
    };

    // Input reader thread (terminal is already in raw mode)
    spawn_input_thread(tx);

    let mut needs_render = true;
    let mut needs_clear = false;
    let startup = std::time::Instant::now();
    let startup_grace = Duration::from_secs(3);
    let mut last_pane_check = LastPaneCheck::new(startup + startup_grace);
    let mut last_refresh = std::time::Instant::now();

    loop {
        // Render before blocking (redraws only when state changed)
        if needs_render {
            if needs_clear {
                terminal.clear()?;
                needs_clear = false;
            }
            terminal.draw(|f| render_sidebar(f, &mut app))?;
            needs_render = false;
        }

        // Time-dependent content supplies its own refresh interval. Static or
        // hidden sidebars block until a snapshot or input wakes them.
        let refresh_interval = app
            .host_window_active()
            .then(|| app.refresh_interval())
            .flatten();
        let resize_timeout = app
            .resize_deadline
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
        let timeout = match (resize_timeout, refresh_interval) {
            (Some(resize), Some(refresh)) => {
                resize.min(refresh.saturating_sub(last_refresh.elapsed()))
            }
            (Some(resize), None) => resize,
            (None, Some(refresh)) => refresh.saturating_sub(last_refresh.elapsed()),
            (None, None) => Duration::from_secs(3600),
        };

        // The startup recheck has one deadline, even if snapshots are deduplicated.
        let timeout = last_pane_check
            .timeout(Instant::now())
            .map_or(timeout, |grace| timeout.min(grace));

        let first_event = match rx.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("sidebar-run exiting: event channel disconnected");
                break;
            }
        };

        // Process first event
        if let Some(ev) = first_event {
            process_event(
                ev,
                &mut app,
                &snapshot_handle,
                &mut last_pane_check,
                &mut needs_render,
                &mut needs_clear,
            );
        }

        // Drain all pending events to coalesce (avoids multiple redraws)
        while let Ok(ev) = rx.try_recv() {
            process_event(
                ev,
                &mut app,
                &snapshot_handle,
                &mut last_pane_check,
                &mut needs_render,
                &mut needs_clear,
            );
        }

        if last_pane_check.grace_expired(Instant::now())
            && last_pane_check.should_exit(app.host_identity(), sidebar_is_only_pane)
        {
            quit_for_last_pane(&mut app);
        }

        // Process any pending resize whose debounce has elapsed
        app.process_pending_resize(&startup, startup_grace);
        advance_refresh_if_due(
            &mut app,
            &mut last_refresh,
            refresh_interval,
            &mut needs_render,
        );

        if app.should_quit {
            tracing::info!(
                host_window = ?app.host_window_id(),
                quit_reason = app.quit_reason.as_deref().unwrap_or("unknown"),
                "sidebar-run quitting"
            );
            if app.quit_silent {
                schedule_pane_kill(&host_identity.pane_id);
            } else {
                shutdown_all_sidebars(&host_identity);
            }
            break;
        }
    }

    // _guard handles cleanup on drop (including the normal exit path)
    Ok(())
}

fn advance_refresh_if_due(
    app: &mut SidebarApp,
    last_refresh: &mut std::time::Instant,
    refresh_interval: Option<Duration>,
    needs_render: &mut bool,
) {
    let Some(refresh_interval) = refresh_interval else {
        *last_refresh = std::time::Instant::now();
        return;
    };
    if last_refresh.elapsed() >= refresh_interval {
        *last_refresh = std::time::Instant::now();
        app.tick();
        *needs_render = true;
    }
}

fn pane_kill_command(pane_id: &str) -> String {
    format!(
        "sleep 0.05; tmux kill-pane -t {} 2>/dev/null || true",
        shell_quote(pane_id)
    )
}

fn schedule_pane_kill(pane_id: &str) {
    let cmd = pane_kill_command(pane_id);
    let _ = Cmd::new("tmux").args(&["run-shell", "-b", &cmd]).run();
}

fn sole_pane_is_sidebar(output: &str, pane_id: &str) -> bool {
    let mut panes = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    panes.next() == Some(pane_id) && panes.next().is_none()
}

fn sidebar_is_only_pane(window_id: &str, pane_id: &str) -> bool {
    Cmd::new("tmux")
        .args(&["list-panes", "-t", window_id, "-F", "#{pane_id}"])
        .run_and_capture_stdout()
        .is_ok_and(|output| sole_pane_is_sidebar(&output, pane_id))
}

fn process_event(
    event: AppEvent,
    app: &mut SidebarApp,
    snapshot_handle: &client::SnapshotHandle,
    last_pane_check: &mut LastPaneCheck,
    needs_render: &mut bool,
    needs_clear: &mut bool,
) {
    match event {
        AppEvent::SnapshotReady => {
            if let Some(snapshot) = snapshot_handle.take() {
                last_pane_check.pane_count = app
                    .host_window_id()
                    .and_then(|window_id| snapshot.window_pane_counts.get(window_id))
                    .copied();
                if last_pane_check.should_exit(app.host_identity(), sidebar_is_only_pane) {
                    quit_for_last_pane(app);
                }
                app.apply_snapshot(snapshot);
                *needs_render = true;
            }
        }
        AppEvent::Input(event) => {
            let outcome = super::input::apply_input(app, event);
            *needs_render |= outcome.render;
            *needs_clear |= outcome.clear;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_kill_command_targets_captured_sidebar_pane() {
        assert_eq!(
            pane_kill_command("%12"),
            "sleep 0.05; tmux kill-pane -t '%12' 2>/dev/null || true"
        );
    }

    #[test]
    fn sole_pane_confirmation_requires_sidebar_identity() {
        assert!(sole_pane_is_sidebar("%12\n", "%12"));
        assert!(!sole_pane_is_sidebar("%12\n%13\n", "%12"));
        assert!(!sole_pane_is_sidebar("%13\n", "%12"));
        assert!(!sole_pane_is_sidebar("", "%12"));
    }
}
