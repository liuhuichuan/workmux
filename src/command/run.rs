//! Run a command in a worktree's tmux/wezterm window.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::config::SplitDirection;
use crate::multiplexer::{create_backend, detect_backend};
use crate::state::run::{RunSpec, cleanup_run, create_run, generate_run_id, read_result};
use crate::workflow;

/// The command the new pane runs, in the form the hand-off preserves.
///
/// A pane command reaches the terminal as one argument, and that trip escapes
/// any `"` in it: `cmd.exe` would then look for a program whose name carries
/// the backslashes. A script's path survives instead, because cmd accepts a
/// path with spaces when it is the whole command, so Windows writes the command
/// into a script in the run directory and hands the pane that path.
#[cfg(windows)]
fn pane_command(run_dir: &Path, exec_cmd: &str) -> Result<String> {
    let script = run_dir.join("run.cmd");
    std::fs::write(&script, format!("@echo off\r\n{exec_cmd}\r\n"))
        .context("Failed to write the run script")?;
    Ok(script.to_string_lossy().into_owned())
}

/// Unix hands the command over as it is: `sh -c` takes it verbatim.
#[cfg(unix)]
fn pane_command(_run_dir: &Path, exec_cmd: &str) -> Result<String> {
    Ok(exec_cmd.to_string())
}

pub fn run(
    worktree_name: &str,
    command_parts: Vec<String>,
    background: bool,
    keep: bool,
    timeout: Option<u64>,
) -> Result<()> {
    if command_parts.is_empty() {
        return Err(anyhow!("No command provided"));
    }

    let mux = create_backend(detect_backend());

    // Resolve worktree to agent pane (consistent with send/capture)
    let (worktree_path, agent) = workflow::resolve_worktree_agent(worktree_name, mux.as_ref())?;

    // Build command string (preserve argument boundaries via shell escaping)
    let command = command_parts
        .iter()
        .map(|s| crate::shell::snippet_quote(s))
        .collect::<Vec<_>>()
        .join(" ");

    // Generate run ID and create spec
    let run_id = generate_run_id();
    let spec = RunSpec {
        command: command.clone(),
        worktree_path: worktree_path.clone(),
    };
    let run_dir = create_run(&run_id, &spec)?;

    // Get path to current executable for _exec
    let exe_path = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "workmux".to_string());

    // Split pane with _exec command (pass absolute run_dir path)
    let run_dir_arg = run_dir.to_string_lossy();
    let exec_cmd = format!(
        "{} _exec --run-dir {}",
        crate::shell::snippet_quote(&exe_path),
        crate::shell::snippet_quote(&run_dir_arg)
    );
    let pane_command = pane_command(&run_dir, &exec_cmd)?;
    let new_pane_id = mux.split_pane(
        &agent.pane_id,
        &SplitDirection::Vertical,
        &worktree_path,
        None,
        Some(30), // 30% for the command pane
        Some(&pane_command),
    )?;

    if background {
        eprintln!("Started: {} (run_id: {})", command, run_id);
        eprintln!("Pane: {}", new_pane_id);
        eprintln!("Artifacts: {}", run_dir.display());
        return Ok(());
    }

    // Wait for completion, streaming output in real-time
    let start = Instant::now();
    let timeout_duration = timeout.map(Duration::from_secs);

    // Open files for streaming
    let stdout_path = run_dir.join("stdout");
    let stderr_path = run_dir.join("stderr");

    // Wait briefly for files to be created
    thread::sleep(Duration::from_millis(100));

    let mut stdout_file = File::open(&stdout_path).ok();
    let mut stderr_file = File::open(&stderr_path).ok();
    let mut stdout_pos: u64 = 0;
    let mut stderr_pos: u64 = 0;

    loop {
        // Check timeout
        if let Some(max_duration) = timeout_duration
            && start.elapsed() > max_duration
        {
            eprintln!("\nTimeout after {}s", timeout.unwrap());
            if keep {
                eprintln!("Artifacts kept at: {}", run_dir.display());
            } else {
                let _ = cleanup_run(&run_dir);
            }
            std::process::exit(124); // Standard timeout exit code
        }

        // Stream new stdout content
        if let Some(ref mut file) = stdout_file {
            stdout_pos = stream_new_content(file, stdout_pos, &mut io::stdout());
        }

        // Stream new stderr content
        if let Some(ref mut file) = stderr_file {
            stderr_pos = stream_new_content(file, stderr_pos, &mut io::stderr());
        }

        // Check if complete
        if let Some(result) = read_result(&run_dir)? {
            // Final flush of any remaining output
            if let Some(ref mut file) = stdout_file {
                stream_new_content(file, stdout_pos, &mut io::stdout());
            }
            if let Some(ref mut file) = stderr_file {
                stream_new_content(file, stderr_pos, &mut io::stderr());
            }

            // Cleanup unless --keep
            if keep {
                eprintln!("Artifacts kept at: {}", run_dir.display());
            } else {
                let _ = cleanup_run(&run_dir);
            }

            // Exit with command's exit code
            let exit_code = result.exit_code.unwrap_or(1);
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return Ok(());
        }

        thread::sleep(Duration::from_millis(50));
    }
}

/// Stream new content from file starting at given position, return new position.
fn stream_new_content<W: Write>(file: &mut File, pos: u64, out: &mut W) -> u64 {
    if file.seek(SeekFrom::Start(pos)).is_err() {
        return pos;
    }

    let mut reader = BufReader::new(file);
    let mut new_pos = pos;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let _ = out.write_all(line.as_bytes());
                let _ = out.flush();
                new_pos += n as u64;
            }
            Err(_) => break,
        }
    }

    new_pos
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows cannot hand the pane a command naming an absolute path, so the
    /// command goes into a script and the pane is given the script's path --
    /// bare, since a quoted one arrives with the quotes escaped.
    #[cfg(windows)]
    #[test]
    fn pane_command_writes_the_exec_command_to_a_script() {
        let dir = tempfile::tempdir().unwrap();
        let exec_cmd = r#""C:\Program Files\workmux.exe" _exec --run-dir "C:\runs\1""#;

        let command = pane_command(dir.path(), exec_cmd).unwrap();

        let script = dir.path().join("run.cmd");
        assert_eq!(command, script.to_string_lossy());
        assert!(!command.contains('"'));
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(
            content.contains(exec_cmd),
            "script lost the command: {content:?}"
        );
    }

    /// Unix hands the pane the command itself; `sh -c` takes it verbatim.
    #[cfg(unix)]
    #[test]
    fn pane_command_is_the_exec_command_itself() {
        let dir = tempfile::tempdir().unwrap();
        let exec_cmd = "/tmp/workmux _exec --run-dir /tmp/runs/1";
        assert_eq!(pane_command(dir.path(), exec_cmd).unwrap(), exec_cmd);
    }
}
