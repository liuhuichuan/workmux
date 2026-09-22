//! Input handling and exit checks shared by both sidebar event loops.
//!
//! The tmux client and the WezTerm pane render the same `SidebarApp`, so what a
//! keystroke means -- and when the sidebar has outlived its window -- belongs to
//! neither event loop in particular.

use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use super::app::{HostIdentity, SidebarApp};

/// What a frame has to do after an input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InputOutcome {
    pub render: bool,
    pub clear: bool,
}

impl InputOutcome {
    /// The event changed nothing the frame shows.
    const NOTHING: Self = Self {
        render: false,
        clear: false,
    };
    /// The event changed the list, so redraw it.
    const REDRAW: Self = Self {
        render: true,
        clear: false,
    };
    /// The viewport moved, so the cells already drawn have to go.
    const CLEAR: Self = Self {
        render: true,
        clear: true,
    };
}

/// Apply one terminal input event to the sidebar.
pub(super) fn apply_input(app: &mut SidebarApp, event: Event) -> InputOutcome {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key_press(app, key.code, key.modifiers);
            InputOutcome::REDRAW
        }
        // A pending quit prompt owns the pane; a click would only fight it.
        Event::Mouse(_) if app.pending_exit => InputOutcome::NOTHING,
        Event::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(idx) = app.hit_test(mouse.column, mouse.row) {
                        app.select_index(idx);
                        app.jump_to_selected();
                    }
                }
                MouseEventKind::ScrollUp => app.scroll_up(),
                MouseEventKind::ScrollDown => app.scroll_down(),
                _ => {}
            }
            InputOutcome::REDRAW
        }
        Event::Resize(cols, rows) => {
            app.on_resize_event(cols, rows);
            InputOutcome::CLEAR
        }
        _ => InputOutcome::NOTHING,
    }
}

fn handle_key_press(app: &mut SidebarApp, code: KeyCode, modifiers: KeyModifiers) {
    if app.pending_exit {
        if code == KeyCode::Char('y') {
            app.quit_reason = Some("confirmed user exit".to_string());
            app.should_quit = true;
        } else {
            app.pending_exit = false;
        }
        return;
    }

    match (code, modifiers) {
        (KeyCode::Char('q'), _)
        | (KeyCode::Esc, _)
        | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.pending_exit = true;
        }
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.next(),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.previous(),
        (KeyCode::Enter, _) => app.jump_to_selected(),
        (KeyCode::Char('G'), _) => app.select_last(),
        (KeyCode::Char('g'), _) => app.select_first(),
        (KeyCode::Char('v'), _) => app.toggle_layout_mode(),
        (KeyCode::Char('z'), _) => app.toggle_sleeping(),
        (KeyCode::Char('f'), _) => app.toggle_filter_mode(),
        _ => {}
    }
}

/// Watches for the sidebar becoming the only pane left in its window, the sign
/// that whoever opened it has closed everything it was monitoring.
pub(super) struct LastPaneCheck {
    grace_deadline: Option<Instant>,
    /// Panes in the sidebar's window, as the last snapshot counted them.
    pub(super) pane_count: Option<usize>,
}

impl LastPaneCheck {
    pub(super) fn new(grace_deadline: Instant) -> Self {
        Self {
            grace_deadline: Some(grace_deadline),
            pane_count: None,
        }
    }

    /// How long to wait for the startup recheck, if it is still pending.
    pub(super) fn timeout(&self, now: Instant) -> Option<Duration> {
        self.grace_deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Consume the startup deadline once, including when input wakes the loop.
    pub(super) fn grace_expired(&mut self, now: Instant) -> bool {
        if self.grace_deadline.is_some_and(|deadline| now >= deadline) {
            self.grace_deadline = None;
            true
        } else {
            false
        }
    }

    pub(super) fn should_exit(
        &self,
        identity: Option<&HostIdentity>,
        verify_live_panes: impl FnOnce(&str, &str) -> bool,
    ) -> bool {
        if self.grace_deadline.is_some() || self.pane_count.is_none_or(|count| count > 1) {
            return false;
        }
        let Some(identity) = identity else {
            return false;
        };
        verify_live_panes(&identity.window_id, &identity.pane_id)
    }
}

/// Leave the sidebar behind without shutting down the ones in other windows.
pub(super) fn quit_for_last_pane(app: &mut SidebarApp) {
    let window_id = app.host_window_id().unwrap_or("unknown");
    app.quit_reason = Some(format!(
        "last-pane: sidebar is sole pane in window {}",
        window_id
    ));
    app.quit_silent = true;
    app.should_quit = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::sidebar::app::TemplateError;
    use crossterm::event::KeyEvent;

    fn test_app() -> SidebarApp {
        SidebarApp::test_with_template_error(TemplateError {
            location: String::new(),
            message: String::new(),
        })
    }

    fn test_identity() -> HostIdentity {
        HostIdentity {
            session_name: "main".to_string(),
            session_id: "$1".to_string(),
            window_id: "@42".to_string(),
            pane_id: "%12".to_string(),
        }
    }

    fn press(app: &mut SidebarApp, code: KeyCode) {
        apply_input(app, Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn startup_recheck_exits_without_another_snapshot() {
        let identity = test_identity();
        let startup = Instant::now();
        let grace = Duration::from_secs(3);
        let mut check = LastPaneCheck::new(startup + grace);
        check.pane_count = Some(1);

        assert_eq!(check.timeout(startup), Some(grace));
        assert!(!check.grace_expired(startup + grace - Duration::from_nanos(1)));
        assert!(!check.should_exit(Some(&identity), |_, _| {
            panic!("startup grace must skip live verification")
        }));

        assert_eq!(check.timeout(startup + grace), Some(Duration::ZERO));
        assert!(check.grace_expired(startup + grace));
        assert!(check.should_exit(Some(&identity), |window, pane| {
            window == "@42" && pane == "%12"
        }));
        assert_eq!(check.timeout(startup + grace), None);
        assert!(!check.grace_expired(startup + grace + Duration::from_secs(1)));
    }

    #[test]
    fn startup_recheck_live_verifies_stale_snapshot_only_once() {
        let identity = test_identity();
        let deadline = Instant::now();
        let mut check = LastPaneCheck::new(deadline);
        check.pane_count = Some(1);
        let mut live_checks = 0;

        for now in [deadline, deadline + Duration::from_secs(60)] {
            if check.grace_expired(now) {
                assert!(!check.should_exit(Some(&identity), |_, _| {
                    live_checks += 1;
                    false
                }));
            }
        }
        assert_eq!(live_checks, 1);
        assert_eq!(check.timeout(deadline), None);
    }

    #[test]
    fn startup_recheck_uses_latest_snapshot_count() {
        let identity = test_identity();
        let deadline = Instant::now();
        let mut check = LastPaneCheck::new(deadline);
        check.pane_count = Some(1);
        check.pane_count = Some(2);
        assert!(check.grace_expired(deadline));
        assert!(!check.should_exit(Some(&identity), |_, _| {
            panic!("content pane created during grace must skip live verification")
        }));
        assert_eq!(check.timeout(deadline), None);

        // Snapshot-driven checks remain available after the one-shot deadline.
        check.pane_count = Some(1);
        assert!(check.should_exit(Some(&identity), |_, _| true));
    }

    #[test]
    fn last_pane_exit_requires_snapshot_and_live_confirmation() {
        let identity = test_identity();
        let deadline = Instant::now();
        let mut check = LastPaneCheck::new(deadline);
        assert!(check.grace_expired(deadline));

        for count in [None, Some(2)] {
            check.pane_count = count;
            assert!(!check.should_exit(Some(&identity), |_, _| {
                panic!("missing count or multiple panes must skip live verification")
            }));
        }
        check.pane_count = Some(1);
        assert!(!check.should_exit(None, |_, _| {
            panic!("missing identity must skip live verification")
        }));
        assert!(!check.should_exit(Some(&identity), |_, _| false));
        assert!(check.should_exit(Some(&identity), |window, pane| {
            window == "@42" && pane == "%12"
        }));
    }

    #[test]
    fn resize_requests_full_redraw() {
        let mut app = test_app();

        let outcome = apply_input(&mut app, Event::Resize(120, 3));

        assert!(outcome.render);
        assert!(outcome.clear);
    }

    #[test]
    fn q_q_does_not_quit_sidebar() {
        let mut app = test_app();

        press(&mut app, KeyCode::Char('q'));
        assert!(app.pending_exit);
        assert!(!app.should_quit);

        press(&mut app, KeyCode::Char('q'));
        assert!(!app.pending_exit);
        assert!(!app.should_quit);
    }

    #[test]
    fn y_confirms_pending_exit() {
        let mut app = test_app();

        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('y'));

        assert!(app.should_quit);
        assert_eq!(app.quit_reason.as_deref(), Some("confirmed user exit"));
    }
}
