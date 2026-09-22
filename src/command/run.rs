//! Run a command in a worktree's tmux/wezterm window.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::config::SplitDirection;
use crate::multiplexer::{create_backend, detect_backend};
use crate::state::run::{RunSpec, cleanup_run, create_run, generate_run_id, read_result};
use crate::workflow;

/// The command the new pane runs: this executable, told to execute the spec.
///
/// The words are handed over as the pane's own program and arguments rather
/// than as a command string. A command string has to survive a trip that
/// escapes the quotes in it, which is why a Windows pane command naming an
/// absolute path used to be written into a script and the script's path handed
/// over instead -- and a script is a batch file, which a Ctrl+C stops at
/// "Terminate batch job (Y/N)?" rather than exiting. Arguments are not re-read
/// on the way, so they need no such care.
fn pane_argv(exe: &str, run_dir: &Path) -> Vec<String> {
    vec![
        exe.to_string(),
        "_exec".to_string(),
        "--run-dir".to_string(),
        run_dir.to_string_lossy().into_owned(),
    ]
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

    // Split pane, running _exec on the absolute path of the run directory
    let new_pane_id = mux.split_pane_argv(
        &agent.pane_id,
        &SplitDirection::Vertical,
        &worktree_path,
        None,
        Some(30), // 30% for the command pane
        &pane_argv(&exe_path, &run_dir),
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

/// Copy the bytes a run's command has written since `pos`, and report the
/// position they were copied up to.
///
/// Bytes, not lines: a command writes in the console's code page, so the output
/// of `ping` on a Chinese Windows is GBK, which is not UTF-8. A reader that
/// decodes such output as lines stops at the first byte it cannot read, and
/// stops there for good -- every later look from the same position fails the
/// same way -- so the rest of the run's output is dropped in silence.
fn stream_new_content<W: Write>(file: &mut File, pos: u64, out: &mut W) -> u64 {
    if file.seek(SeekFrom::Start(pos)).is_err() {
        return pos;
    }

    let mut new_pos = pos;
    let mut buffer = [0u8; 8192];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let _ = out.write_all(&buffer[..n]);
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

    /// The pane is handed this executable and its arguments, so the run
    /// directory can name a path with spaces in it without being quoted for
    /// anyone on the way.
    #[test]
    fn the_run_pane_runs_this_executable_on_the_run_directory() {
        assert_eq!(
            pane_argv(
                r"C:\Program Files\workmux.exe",
                Path::new(r"C:\Temp\workmux runs\42")
            ),
            [
                r"C:\Program Files\workmux.exe",
                "_exec",
                "--run-dir",
                r"C:\Temp\workmux runs\42",
            ]
        );
    }

    /// A command's output reaches the caller as the bytes it wrote: a Windows
    /// command writes in the console's code page, so the output of `ping` on a
    /// Chinese Windows is GBK, which is not UTF-8.
    #[test]
    fn output_that_is_not_utf8_is_streamed_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        // `ping 127.0.0.1` on a Chinese Windows, in GBK.
        let ping = b"\xd5\xfd\xd4\xda Ping 127.0.0.1\r\n\xbb\xd8\xb8\xb4: \xd7\xd6\xbd\xda=32\r\n";
        std::fs::write(&path, ping).unwrap();

        let mut file = File::open(&path).unwrap();
        let mut streamed = Vec::new();
        let pos = stream_new_content(&mut file, 0, &mut streamed);

        assert_eq!(streamed.as_slice(), ping.as_slice());
        assert_eq!(pos, ping.len() as u64);
    }

    /// Output written after an earlier look is still streamed, and a later look
    /// resumes where the last one stopped rather than repeating it.
    #[test]
    fn output_written_later_is_streamed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "first\n").unwrap();

        let mut file = File::open(&path).unwrap();
        let mut streamed = Vec::new();
        let pos = stream_new_content(&mut file, 0, &mut streamed);
        assert_eq!(streamed.as_slice(), b"first\n".as_slice());

        // The run's pane appends to the same file while it is being read.
        {
            let mut appender = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            appender.write_all(b"second\n").unwrap();
        }

        let pos = stream_new_content(&mut file, pos, &mut streamed);
        assert_eq!(streamed.as_slice(), b"first\nsecond\n".as_slice());
        assert_eq!(pos, 13);

        // Nothing was written since: the position holds still and no line is
        // repeated.
        let pos = stream_new_content(&mut file, pos, &mut streamed);
        assert_eq!(streamed.as_slice(), b"first\nsecond\n".as_slice());
        assert_eq!(pos, 13);
    }
}
