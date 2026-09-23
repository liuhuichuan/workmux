//! Pane handshake mechanisms for shell startup synchronization.
//!
//! Different backends use different mechanisms to ensure a shell is ready
//! before sending commands to a pane.

use anyhow::{Context, Result, anyhow};
#[cfg(unix)]
use nix::sys::stat::Mode;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, trace, warn};

use crate::cmd::Cmd;

/// Trait for pane handshake mechanisms.
///
/// A handshake ensures the shell has started in a pane before sending commands.
/// Different backends may use different mechanisms (tmux wait-for, named pipes, etc.)
pub trait PaneHandshake: Send {
    /// Returns a full shell command string that wraps the shell to signal readiness.
    /// Formatted for direct shell evaluation (e.g., `sh -c "..."`).
    /// Used by backends that need a single command string (tmux).
    fn wrapper_command(&self, shell: &str) -> String;

    /// Returns the command that starts the handshake wrapper around `shell`.
    ///
    /// Does NOT include `sh -c`/`cmd /C` wrapping -- the backend decides how to
    /// invoke it. Used by the shared `setup_panes` implementation, where each
    /// backend wraps the command appropriately for its CLI.
    fn script_content(&self, shell: &str) -> Result<String> {
        // Default: delegate to wrapper_command (backwards compat)
        Ok(self.wrapper_command(shell))
    }

    /// Waits for the handshake signal, consuming the handshake object.
    fn wait(self: Box<Self>) -> Result<()>;
}

/// Timeout for waiting for pane readiness (seconds)
const HANDSHAKE_TIMEOUT_SECS: u64 = 5;

/// Manages the tmux wait-for handshake protocol for pane synchronization.
///
/// This struct encapsulates the channel-based handshake mechanism that ensures
/// the shell is ready before sending commands. The handshake uses tmux's `wait-for`
/// feature with channel locking to synchronize between the process spawning the
/// pane and the shell that starts inside it.
///
/// # Protocol
/// 1. Lock a unique channel (on construction)
/// 2. Start the shell with a wrapper that unlocks the channel when ready
/// 3. Wait for the shell to signal readiness (wait blocks until unlock)
/// 4. Clean up the channel
pub struct TmuxHandshake {
    channel: String,
}

impl TmuxHandshake {
    /// Create a new handshake and lock the channel.
    ///
    /// The channel must be locked before spawning the pane to ensure we don't
    /// miss the signal even if the shell starts instantly.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let channel = format!("wm_ready_{}_{}", pid, nanos);

        // Lock the channel (ensures we don't miss the signal)
        Cmd::new("tmux")
            .args(&["wait-for", "-L", &channel])
            .run()
            .context("Failed to initialize wait channel")?;

        Ok(Self { channel })
    }
}

impl PaneHandshake for TmuxHandshake {
    /// Build a shell wrapper command that signals readiness.
    ///
    /// The wrapper briefly disables echo while signaling the channel, restores it,
    /// then exec's into the shell so the TTY starts in a normal state.
    ///
    /// We wrap in `sh -c "..."` with double quotes to ensure the command works when
    /// tmux's default-shell is a non-POSIX shell like nushell. Single-quote escaping
    /// (`'\''`) doesn't work reliably when nushell parses the command before passing
    /// it to sh.
    fn wrapper_command(&self, shell: &str) -> String {
        let escaped_shell = super::util::escape_for_sh_c_inner_single_quote(shell);
        format!(
            "sh -c \"stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l\"",
            self.channel, escaped_shell
        )
    }

    fn script_content(&self, shell: &str) -> Result<String> {
        Ok(format!(
            "stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l",
            self.channel, shell
        ))
    }

    /// Wait for the shell to signal it is ready, then clean up.
    ///
    /// This method consumes the handshake to ensure cleanup happens exactly once.
    /// Uses a polling loop with timeout to prevent indefinite hangs if the pane
    /// fails to start.
    fn wait(self: Box<Self>) -> Result<()> {
        debug!(channel = %self.channel, "tmux:handshake start");

        let mut child = std::process::Command::new("tmux")
            .args(["wait-for", "-L", &self.channel])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn tmux wait-for command")?;

        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        // Cleanup: unlock the channel we just re-locked
                        Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run()
                            .context("Failed to cleanup wait channel")?;
                        debug!(channel = %self.channel, "tmux:handshake success");
                        return Ok(());
                    } else {
                        // Attempt cleanup even on failure
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();
                        warn!(channel = %self.channel, status = ?status.code(), "tmux:handshake failed (wait-for error)");
                        return Err(anyhow!(
                            "Pane handshake failed - tmux wait-for returned error"
                        ));
                    }
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait(); // Ensure process is reaped

                        // Attempt cleanup
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();

                        warn!(
                            channel = %self.channel,
                            timeout_secs = HANDSHAKE_TIMEOUT_SECS,
                            "tmux:handshake timeout"
                        );
                        return Err(anyhow!(
                            "Pane handshake timed out after {}s - shell may have failed to start",
                            HANDSHAKE_TIMEOUT_SECS
                        ));
                    }
                    trace!(
                        channel = %self.channel,
                        elapsed_ms = start.elapsed().as_millis(),
                        "tmux:handshake waiting"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = Cmd::new("tmux")
                        .args(&["wait-for", "-U", &self.channel])
                        .run();
                    warn!(channel = %self.channel, error = %e, "tmux:handshake error");
                    return Err(anyhow!("Error waiting for pane handshake: {}", e));
                }
            }
        }
    }
}

/// Unix named pipe (FIFO) based handshake for backends without wait-for.
///
/// Used by WezTerm and other backends that don't have a built-in synchronization
/// mechanism like tmux's wait-for.
#[cfg(unix)]
pub struct UnixPipeHandshake {
    pipe_path: PathBuf,
}

#[cfg(unix)]
impl UnixPipeHandshake {
    /// Create a new pipe handshake.
    ///
    /// Creates a named pipe (FIFO) that the shell will write to when ready.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();

        let pipe_path = std::env::temp_dir().join(format!("workmux_pipe_{}_{}", pid, nanos));

        // Create FIFO with 0o600 permissions (owner read/write only)
        let mode = Mode::S_IRUSR | Mode::S_IWUSR;
        nix::unistd::mkfifo(&pipe_path, mode).context("Failed to create named pipe")?;

        Ok(Self { pipe_path })
    }
}

#[cfg(unix)]
impl PaneHandshake for UnixPipeHandshake {
    fn wrapper_command(&self, shell: &str) -> String {
        let escaped_shell = super::util::escape_for_sh_c_inner_single_quote(shell);
        format!(
            "sh -c 'echo ready > {}; exec '\\''{}'\\'' -l'",
            self.pipe_path.display(),
            escaped_shell
        )
    }

    fn script_content(&self, shell: &str) -> Result<String> {
        Ok(format!(
            "echo ready > {}; exec '{}' -l",
            self.pipe_path.display(),
            shell
        ))
    }

    fn wait(self: Box<Self>) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        const POLL_INTERVAL_MS: u64 = 50;

        // Open pipe for reading (non-blocking)
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.pipe_path)
            .context("Failed to open pipe for reading")?;

        let fd = file.as_raw_fd();
        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            // Check if data available via poll()
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let poll_timeout_ms = POLL_INTERVAL_MS as i32;
            let ret = unsafe { libc::poll(&mut pollfd, 1, poll_timeout_ms) };

            if ret > 0 && (pollfd.revents & libc::POLLIN) != 0 {
                // Data available - read and verify we got data
                let mut buf = [0u8; 64];
                let bytes_read =
                    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                let _ = std::fs::remove_file(&self.pipe_path);

                if bytes_read > 0 {
                    return Ok(());
                } else {
                    // EOF (0) or error (-1) - writer closed without sending data
                    return Err(anyhow!("Pipe closed without receiving handshake signal"));
                }
            }

            if start.elapsed() >= timeout {
                let _ = std::fs::remove_file(&self.pipe_path);
                return Err(anyhow!(
                    "Pane handshake timed out after {}s - shell may have failed to start",
                    HANDSHAKE_TIMEOUT_SECS
                ));
            }

            // Continue polling
        }
    }
}

#[cfg(unix)]
impl Drop for UnixPipeHandshake {
    fn drop(&mut self) {
        // Clean up the pipe file if it still exists
        let _ = std::fs::remove_file(&self.pipe_path);
    }
}

/// Marker-file handshake for Windows, where neither FIFOs nor `tmux wait-for`
/// exist.
///
/// `cmd.exe` rewrites the command line it is handed under `/C`, so a wrapper
/// that needs quoting (any path with a space) cannot be passed as an argument:
/// the quotes come back backslash-escaped and the wrapper never runs. The
/// wrapper therefore lives in a temporary `.cmd` file, which `cmd.exe` parses
/// with its ordinary rules, and the pane is started with that file's path.
///
/// The wrapper writes a marker file before handing the pane to the interactive
/// shell, and `wait` polls for that file. Signalling through the filesystem
/// keeps `wait` independent of the backend that owns the pane.
#[cfg(windows)]
pub struct MarkerFileHandshake {
    marker_path: PathBuf,
    script_path: PathBuf,
}

#[cfg(windows)]
impl MarkerFileHandshake {
    /// Create a new handshake with unique, not-yet-existing paths.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let stem = format!("workmux_ready_{}_{}", pid, nanos);

        let marker_path = std::env::temp_dir().join(format!("{stem}.marker"));
        let script_path = std::env::temp_dir().join(format!("{stem}.cmd"));
        // A leftover marker would report readiness before the pane ever starts.
        let _ = std::fs::remove_file(&marker_path);

        Ok(Self {
            marker_path,
            script_path,
        })
    }
}

#[cfg(windows)]
impl PaneHandshake for MarkerFileHandshake {
    fn wrapper_command(&self, shell: &str) -> String {
        // The path is left unquoted: `cmd.exe` accepts a bare path that contains
        // spaces, but rejects one whose quotes arrived backslash-escaped.
        format!("cmd /c {}", self.script_path.display())
    }

    /// Write the wrapper script and return the command that runs it.
    ///
    /// The returned command is a single unquoted path, so the backend can hand
    /// it to `cmd.exe /C` as one argument.
    fn script_content(&self, shell: &str) -> Result<String> {
        // Inside the script quoting is ordinary: only the command line is
        // rewritten by `cmd.exe`. The redirect follows `echo` with no space so
        // the marker file holds no leading blank.
        let shell_line = crate::shell::interactive_shell_argv(shell)
            .iter()
            .map(|word| format!("\"{word}\""))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "@echo off\r\necho ready> \"{}\"\r\n{}\r\n",
            self.marker_path.display(),
            shell_line
        );
        std::fs::write(&self.script_path, script).with_context(|| {
            format!(
                "Failed to write pane handshake script to {}",
                self.script_path.display()
            )
        })?;
        Ok(self.script_path.display().to_string())
    }

    fn wait(self: Box<Self>) -> Result<()> {
        debug!(marker = %self.marker_path.display(), "windows:handshake start");

        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            if self.marker_path.exists() {
                debug!(marker = %self.marker_path.display(), "windows:handshake success");
                return Ok(());
            }
            if start.elapsed() >= timeout {
                warn!(
                    marker = %self.marker_path.display(),
                    timeout_secs = HANDSHAKE_TIMEOUT_SECS,
                    "windows:handshake timeout"
                );
                return Err(anyhow!(
                    "Pane handshake timed out after {}s - shell may have failed to start",
                    HANDSHAKE_TIMEOUT_SECS
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(windows)]
impl Drop for MarkerFileHandshake {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker_path);
        let _ = std::fs::remove_file(&self.script_path);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::Command;

    /// The wrapper must survive the exact hand-off used for panes: a single
    /// `cmd.exe /C <script>` command line.
    ///
    /// The shell path deliberately contains a space: quoting it is what breaks
    /// once the wrapper is inlined into that command line instead of living in
    /// its own file.
    #[test]
    fn script_content_runs_through_a_cmd_command_line() {
        let temp = tempfile::tempdir().unwrap();
        let shell_dir = temp.path().join("shell dir");
        std::fs::create_dir_all(&shell_dir).unwrap();
        let shell = shell_dir.join("fake shell.cmd");
        std::fs::write(&shell, "@echo off\r\nexit /b 0\r\n").unwrap();

        let handshake = MarkerFileHandshake::new().unwrap();
        let marker_path = handshake.marker_path.clone();
        let script = handshake.script_content(shell.to_str().unwrap()).unwrap();

        let argv = crate::shell::snippet_argv(&script);
        let (program, args) = argv.split_first().unwrap();
        let output = Command::new(program).args(args).output().unwrap();

        assert!(
            output.status.success(),
            "wrapper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            marker_path.exists(),
            "wrapper did not write the readiness marker"
        );
    }

    /// The shell the pane is left with is the one the user's own profile
    /// describes, so it is started as a login shell. Windows panes run a POSIX
    /// shell whenever `$SHELL` names one, and both POSIX handshakes pass `-l`
    /// for exactly this reason; without it here the profile that sets a pane's
    /// PATH, aliases and environment never runs.
    #[test]
    fn script_content_starts_a_profile_reading_shell_as_a_login_shell() {
        let temp = tempfile::tempdir().unwrap();
        let shell = temp.path().join("bash.cmd");
        let arguments = temp.path().join("arguments.txt");
        std::fs::write(
            &shell,
            format!("@echo off\r\necho %* > \"{}\"\r\n", arguments.display()),
        )
        .unwrap();

        let handshake = MarkerFileHandshake::new().unwrap();
        let script = handshake.script_content(shell.to_str().unwrap()).unwrap();
        let argv = crate::shell::snippet_argv(&script);
        let (program, args) = argv.split_first().unwrap();
        let output = Command::new(program).args(args).output().unwrap();

        assert!(
            output.status.success(),
            "wrapper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let seen = std::fs::read_to_string(&arguments).unwrap();
        assert!(
            seen.trim().trim_matches('"') == "-l",
            "the shell was not started as a login shell: {seen:?}"
        );
    }
}
