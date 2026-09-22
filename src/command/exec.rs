//! Hidden `_exec` subcommand for running commands in worktree panes.
//!
//! This is invoked by `workmux run` in a split pane to execute the command
//! while capturing output to files.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};

use crate::state::run::{RunResult, read_spec, write_result};

pub fn run(run_dir: &Path) -> Result<()> {
    let result = try_run(run_dir);

    // If execution failed before writing result, write a failure marker
    // so the coordinator doesn't hang waiting forever
    if let Err(e) = &result {
        eprintln!("Execution failed: {:#}", e);
        let fail_result = RunResult {
            exit_code: Some(1),
            signal: None,
        };
        let _ = write_result(run_dir, &fail_result);
    }

    result
}

fn try_run(run_dir: &Path) -> Result<()> {
    let spec = read_spec(run_dir)?;

    let stdout_path = run_dir.join("stdout");
    let stderr_path = run_dir.join("stderr");

    // Open output files for appending
    let stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_path)
        .context("Failed to open stdout file")?;

    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .context("Failed to open stderr file")?;

    // Spawn the command through the platform shell.
    let argv = crate::shell::snippet_argv(&spec.command);
    let (program, args) = argv
        .split_first()
        .expect("snippet_argv always returns a program");
    let (flags, snippet) = args.split_at(args.len() - 1);
    let mut command = Command::new(program);
    command.args(flags);
    crate::shell::append_snippet(&mut command, program, &snippet[0]);
    let mut child = command
        .current_dir(&spec.worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn command")?;

    #[cfg(unix)]
    let child_pid = child.id();
    let running = Arc::new(AtomicBool::new(true));

    // Keep this process alive through an interrupt so the child's exit status
    // still reaches the `workmux run` that waits on the result file.
    //
    // Unix has to forward SIGINT itself. Windows delivers Ctrl+C to every
    // process attached to the console, so the child is interrupted without
    // help and absorbing the event here is all that is needed.
    #[cfg(unix)]
    {
        let r = running.clone();
        let _ = ctrlc::set_handler(move || {
            if r.load(Ordering::SeqCst) {
                unsafe {
                    libc::kill(child_pid as i32, libc::SIGINT);
                }
            }
        });
    }

    #[cfg(windows)]
    let _ = ctrlc::set_handler(|| {});

    // Take ownership of child's stdout/stderr
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    // Spawn thread to pump stdout (move owned handles into thread)
    let stdout_handle = thread::spawn(move || {
        pump_output(child_stdout, stdout_file, std::io::stdout());
    });

    // Spawn thread to pump stderr (move owned handles into thread)
    let stderr_handle = thread::spawn(move || {
        pump_output(child_stderr, stderr_file, std::io::stderr());
    });

    // Wait for child to complete
    let status = child.wait().context("Failed to wait for command")?;
    running.store(false, Ordering::SeqCst);

    // Wait for IO threads to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Write result
    let result = run_result(status);
    write_result(run_dir, &result)?;

    // Exit with same code as child
    std::process::exit(result.exit_code.unwrap_or(1));
}

/// The status of a finished child, in the shape `workmux run` reports.
///
/// Unix names the signal that killed the child. Windows has no signals: a
/// console process that dies from Ctrl+C exits with `STATUS_CONTROL_C_EXIT`,
/// which is what `SIGINT` means on Unix, so the two platforms agree on the
/// result.
fn run_result(status: ExitStatus) -> RunResult {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        RunResult {
            exit_code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(windows)]
    {
        /// `STATUS_CONTROL_C_EXIT`: the Windows counterpart of dying to
        /// `SIGINT`.
        const STATUS_CONTROL_C_EXIT: i32 = 0xC000_013A_u32 as i32;
        /// `SIGINT`, the signal Unix reports for an interrupt.
        const SIGINT: i32 = 2;

        if status.code() == Some(STATUS_CONTROL_C_EXIT) {
            RunResult {
                exit_code: None,
                signal: Some(SIGINT),
            }
        } else {
            RunResult {
                exit_code: status.code(),
                signal: None,
            }
        }
    }
}

fn pump_output<R: Read, F: Write, T: Write>(mut reader: R, mut file: F, mut terminal: T) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let data = &buf[..n];
                let _ = file.write_all(data);
                let _ = file.flush();
                let _ = terminal.write_all(data);
                let _ = terminal.flush();
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child killed by an interrupt is reported the way Unix reports it: no
    /// exit code, `SIGINT` as the signal.
    #[cfg(unix)]
    #[test]
    fn run_result_reports_the_signal_that_killed_the_child() {
        use std::os::unix::process::ExitStatusExt;

        let interrupted = run_result(ExitStatus::from_raw(2));
        assert_eq!(interrupted.exit_code, None);
        assert_eq!(interrupted.signal, Some(2));

        let completed = run_result(ExitStatus::from_raw(3 << 8));
        assert_eq!(completed.exit_code, Some(3));
        assert_eq!(completed.signal, None);
    }

    /// Windows has no signals, so the status a Ctrl+C'd console process exits
    /// with is translated into the same result.
    #[cfg(windows)]
    #[test]
    fn run_result_translates_the_windows_control_exit_status() {
        use std::os::windows::process::ExitStatusExt;

        let interrupted = run_result(ExitStatus::from_raw(0xC000_013A));
        assert_eq!(interrupted.exit_code, None);
        assert_eq!(interrupted.signal, Some(2));

        let completed = run_result(ExitStatus::from_raw(3));
        assert_eq!(completed.exit_code, Some(3));
        assert_eq!(completed.signal, None);
    }
}
