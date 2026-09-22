mod agent_display;
mod agent_identity;
mod agent_setup;
mod claude;
mod cli;
mod cmd;
mod command;
mod config;
mod creation_time;
mod frozen_config;
mod git;
mod github;
mod gitlab;
mod llm;
mod logger;
mod markdown;
mod multiplexer;
mod naming;
mod nerdfont;
mod prompt;
mod sandbox;
mod shell;
mod skills;
mod spinner;
mod state;
mod template;
#[cfg(test)]
mod test_support;
mod tips;
mod tmux_style;
mod ui;
mod util;
mod workflow;
mod xdg;

use anyhow::Result;
use tracing::{error, info};

// Windows gives the main thread a 1 MiB stack, but clap's generated
// `augment_subcommands` needs about 1.2 MiB of it in a debug build: without this
// worker the process dies in `__chkstk` with "thread 'main' has overflowed its
// stack" before it reads an argument. Unix keeps the plain `main` so signal and
// thread semantics stay untouched.
#[cfg(unix)]
fn main() -> Result<()> {
    real_main()
}

#[cfg(windows)]
fn main() -> Result<()> {
    run_on_worker_thread(real_main)
}

#[cfg(windows)]
fn run_on_worker_thread(body: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    let worker = std::thread::Builder::new()
        .stack_size(WINDOWS_MAIN_STACK_BYTES)
        .spawn(body)
        .expect("failed to spawn main worker thread");
    match worker.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(windows)]
const WINDOWS_MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

fn real_main() -> Result<()> {
    logger::init()?;
    let context = LogContext::current();
    let args = std::env::args().collect::<Vec<_>>();
    let deferred_cleanup_worker = args.get(1).is_some_and(|arg| arg == "_deferred-cleanup");
    info!(
        args = ?args,
        cwd = ?context.cwd,
        tmux_pane = ?context.tmux_pane,
        "workmux start"
    );

    match cli::run() {
        Ok(result) => {
            info!(
                cwd = ?context.cwd,
                tmux_pane = ?context.tmux_pane,
                "workmux finished successfully"
            );
            Ok(result)
        }
        Err(err) => {
            if !deferred_cleanup_worker {
                error!(error = ?err, "workmux failed");
            }
            Err(err)
        }
    }
}

struct LogContext {
    cwd: Option<std::path::PathBuf>,
    tmux_pane: Option<String>,
}

impl LogContext {
    fn current() -> Self {
        Self {
            cwd: std::env::current_dir().ok(),
            tmux_pane: std::env::var("TMUX_PANE").ok(),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    // The point of the worker is to hand the body a stack the caller does not
    // have, so the body must not run on the calling thread.
    #[test]
    fn the_worker_runs_the_body_on_a_thread_of_its_own() {
        let caller = std::thread::current().id();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = std::sync::Arc::clone(&seen);

        run_on_worker_thread(move || {
            *slot.lock().unwrap() = Some(std::thread::current().id());
            Ok(())
        })
        .expect("the worker should report the body's success");

        assert_ne!(
            Some(caller),
            *seen.lock().unwrap(),
            "the body ran on the caller's thread, so it got the main thread's stack"
        );
    }

    #[test]
    fn the_worker_hands_back_the_body_result_and_resumes_its_panic() {
        assert!(run_on_worker_thread(|| Ok(())).is_ok());
        assert!(run_on_worker_thread(|| anyhow::bail!("boom")).is_err());

        let panic = std::panic::catch_unwind(|| run_on_worker_thread(|| panic!("boom")));
        assert!(panic.is_err(), "the worker swallowed the body's panic");
    }

    // Measured on a debug build on 2026-09-23: a worker given the 1 MiB the main
    // thread gets still overflows inside `augment_subcommands`, and the process
    // then hangs in `__chkstk` after "thread 'main' has overflowed its stack".
    #[test]
    fn the_worker_stack_has_margin_over_the_main_thread() {
        assert!(
            WINDOWS_MAIN_STACK_BYTES >= 4 * 1024 * 1024,
            "a debug clap parse needs more than the main thread's 1 MiB, got {WINDOWS_MAIN_STACK_BYTES}"
        );
    }
}
