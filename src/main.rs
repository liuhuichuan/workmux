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
    let worker = std::thread::Builder::new()
        .stack_size(WINDOWS_MAIN_STACK_BYTES)
        .spawn(real_main)
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
