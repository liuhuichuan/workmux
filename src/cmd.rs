use anyhow::{Context, Result, anyhow};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// How often a command with a deadline is checked while it runs.
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Duplicate the current process's stderr so it can be given to a child as stdout.
///
/// Unix duplicates the file descriptor; Windows duplicates the console handle.
#[cfg(unix)]
fn dup_stderr() -> std::io::Result<Stdio> {
    use std::os::fd::AsFd;
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
}

#[cfg(windows)]
fn dup_stderr() -> std::io::Result<Stdio> {
    use std::os::windows::io::AsHandle;
    std::io::stderr()
        .as_handle()
        .try_clone_to_owned()
        .map(Stdio::from)
}

/// A builder for executing shell commands with unified error handling
pub struct Cmd<'a> {
    command: &'a str,
    args: Vec<&'a str>,
    workdir: Option<&'a Path>,
    /// The deadline this command runs under, if any.
    pub(crate) timeout: Option<Duration>,
}

impl<'a> Cmd<'a> {
    /// Create a new command builder
    pub fn new(command: &'a str) -> Self {
        Self {
            command,
            args: Vec::new(),
            workdir: None,
            timeout: None,
        }
    }

    /// Give the command a deadline, after which it is killed and fails.
    ///
    /// A program that can wait forever on a service stops being a program
    /// workmux can report on: the caller waits with it. WezTerm's CLI is that
    /// program, so every call to it carries a deadline.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Add a single argument
    pub fn arg(mut self, arg: &'a str) -> Self {
        self.args.push(arg);
        self
    }

    /// Add multiple arguments
    pub fn args(mut self, args: &[&'a str]) -> Self {
        self.args.extend_from_slice(args);
        self
    }

    /// Set the working directory for the command
    pub fn workdir(mut self, path: &'a Path) -> Self {
        self.workdir = Some(path);
        self
    }

    /// Execute the command and return the output
    /// Returns an error if the command fails (non-zero exit code)
    pub fn run(self) -> Result<Output> {
        let Cmd {
            command,
            args,
            workdir,
            timeout,
        } = self;
        let workdir_display = workdir.map(|p| p.display().to_string());

        trace!(command, args = ?args, workdir = ?workdir_display, "cmd:run start");

        let mut cmd = if command == "git" {
            crate::git::unattended_git(workdir)?
        } else {
            let mut command = Command::new(command);
            if let Some(dir) = workdir {
                command.current_dir(dir);
            }
            command
        };
        cmd.args(&args);
        let display = format!("{} {}", command, args.join(" "));
        let output = match timeout {
            Some(timeout) => run_with_deadline(cmd, timeout, &display)?,
            None => cmd
                .output()
                .with_context(|| format!("Failed to execute command: {display}"))?,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            debug!(
                command,
                args = ?args,
                status = ?output.status.code(),
                stderr = %stderr.trim(),
                "cmd:run failure"
            );
            return Err(anyhow!(
                "Command failed: {} {}\n{}",
                command,
                args.join(" "),
                stderr.trim()
            ));
        }
        trace!(command, "cmd:run success");
        Ok(output)
    }

    /// Execute the command and return stdout as a trimmed string
    pub fn run_and_capture_stdout(self) -> Result<String> {
        let output = self.run()?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    /// Execute the command, returning Ok(true) if it succeeds, Ok(false) if it fails
    /// This is useful for commands that are used as checks (e.g., git rev-parse --verify)
    pub fn run_as_check(self) -> Result<bool> {
        let Cmd {
            command,
            args,
            workdir,
            timeout,
        } = self;
        let workdir_display = workdir.map(|p| p.display().to_string());
        trace!(command, args = ?args, workdir = ?workdir_display, "cmd:check start");

        let mut cmd = if command == "git" {
            crate::git::unattended_git(workdir)?
        } else {
            let mut command = Command::new(command);
            if let Some(dir) = workdir {
                command.current_dir(dir);
            }
            command
        };
        cmd.args(&args);
        let display = format!("{} {}", command, args.join(" "));
        let output = match timeout {
            Some(timeout) => run_with_deadline(cmd, timeout, &display)?,
            None => cmd
                .output()
                .with_context(|| format!("Failed to execute command: {display}"))?,
        };

        let success = output.status.success();
        trace!(command, success, "cmd:check result");
        Ok(success)
    }
}

/// Run a command to completion, killing it if it outlives `timeout`.
///
/// The pipes are drained on their own threads: a child that writes more than a
/// pipe holds would otherwise block on that write while this side waits for the
/// child to exit, which turns a deadline into a deadlock. A child killed at the
/// deadline is reaped before this returns, so the caller can retry.
fn run_with_deadline(mut cmd: Command, timeout: Duration, display: &str) -> Result<Output> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to execute command: {display}"))?;
    let stdout = child.stdout.take().map(reader_thread);
    let stderr = child.stderr.take().map(reader_thread);
    let deadline = Instant::now() + timeout;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("Command timed out after {timeout:?}: {display}"));
            }
            Ok(None) => std::thread::sleep(DEADLINE_POLL_INTERVAL),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to wait for command: {display}"));
            }
        }
    };

    Ok(Output {
        status,
        stdout: drain(stdout),
        stderr: drain(stderr),
    })
}

/// Read one of a child's pipes to the end, off the waiting thread.
fn reader_thread(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

fn drain(reader: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellOutput {
    Inherit,
    Silent,
    RedirectToStderr,
}

/// Helper to create a shell command with additional environment variables.
pub fn shell_command_with_env(
    hook_shell: Option<&[String]>,
    command: &str,
    workdir: &Path,
    env_vars: &[(&str, &str)],
) -> Result<()> {
    shell_command_with_env_mode(hook_shell, command, workdir, env_vars, ShellOutput::Inherit)
}

/// Run a lifecycle hook with additional environment variables and optional output inheritance.
/// The hook command is appended after all configured shell arguments.
pub fn shell_command_with_env_output(
    hook_shell: Option<&[String]>,
    command: &str,
    workdir: &Path,
    env_vars: &[(&str, &str)],
    inherit_output: bool,
) -> Result<()> {
    let output = if inherit_output {
        ShellOutput::Inherit
    } else {
        ShellOutput::Silent
    };
    shell_command_with_env_mode(hook_shell, command, workdir, env_vars, output)
}

pub fn shell_command_with_env_mode(
    hook_shell: Option<&[String]>,
    command: &str,
    workdir: &Path,
    env_vars: &[(&str, &str)],
    output: ShellOutput,
) -> Result<()> {
    let default_shell = crate::shell::default_hook_argv();
    let argv = hook_shell.unwrap_or(&default_shell);
    let (executable, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("'hook_shell' must contain an executable"))?;
    if executable.trim().is_empty() {
        return Err(anyhow!("'hook_shell' executable must not be empty"));
    }

    let mut cmd = Command::new(executable);
    cmd.args(args);
    crate::shell::append_snippet(&mut cmd, executable, command);
    cmd.current_dir(workdir);

    match output {
        ShellOutput::Inherit => {}
        ShellOutput::Silent => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        ShellOutput::RedirectToStderr => {
            let stdout = dup_stderr().context("Failed to redirect hook output to stderr")?;
            cmd.stdout(stdout).stderr(Stdio::inherit());
        }
    }

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let status = cmd.status().with_context(|| {
        format!(
            "Failed to execute lifecycle hook shell '{}': {}",
            executable, command
        )
    })?;

    if !status.success() {
        return Err(anyhow!(
            "Lifecycle hook command failed with exit code {} using '{}': {}",
            status.code().unwrap_or(-1),
            executable,
            command
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A command that stays up far longer than any test waits.
    fn slow_command() -> Cmd<'static> {
        if cfg!(windows) {
            Cmd::new("powershell").args(&["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
        } else {
            Cmd::new("sh").args(&["-c", "sleep 30"])
        }
    }

    fn echo_command() -> Cmd<'static> {
        if cfg!(windows) {
            Cmd::new("cmd").args(&["/C", "echo hello"])
        } else {
            Cmd::new("sh").args(&["-c", "echo hello"])
        }
    }

    fn failing_command() -> Cmd<'static> {
        if cfg!(windows) {
            Cmd::new("cmd").args(&["/C", "exit 3"])
        } else {
            Cmd::new("sh").args(&["-c", "exit 3"])
        }
    }

    /// A command that outlives its deadline is killed and reported, not waited
    /// on: `wezterm cli` against a mux that has stopped answering is what this
    /// deadline exists for.
    #[test]
    fn a_command_that_outlives_its_deadline_is_killed() {
        let started = Instant::now();
        let error = slow_command()
            .timeout(Duration::from_millis(200))
            .run()
            .unwrap_err()
            .to_string();
        let waited = started.elapsed();

        assert!(error.contains("timed out"), "{error}");
        assert!(
            waited < Duration::from_secs(15),
            "the deadline was not honored: {waited:?}"
        );
    }

    /// A deadline inside the command's runtime changes nothing about the run.
    #[test]
    fn a_command_inside_its_deadline_reports_what_it_wrote() {
        let output = echo_command()
            .timeout(Duration::from_secs(60))
            .run_and_capture_stdout()
            .unwrap();

        assert_eq!(output, "hello");
    }

    /// A deadline must not turn a failure into a timeout: the exit status is
    /// still what the caller reads.
    #[test]
    fn a_deadline_does_not_hide_a_failing_command() {
        let error = failing_command()
            .timeout(Duration::from_secs(60))
            .run()
            .unwrap_err()
            .to_string();

        assert!(error.contains("Command failed"), "{error}");
    }

    /// More output than the pipes hold must not deadlock the wait: the pipes
    /// are drained while the command runs, not after it exits.
    #[test]
    fn a_command_that_writes_more_than_a_pipe_holds_is_drained() {
        let cmd = if cfg!(windows) {
            Cmd::new("cmd").args(&[
                "/C",
                "for /L %i in (1,1,3000) do @echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            ])
        } else {
            Cmd::new("sh").args(&["-c", "yes x | head -c 200000"])
        };

        let output = cmd.timeout(Duration::from_secs(60)).run().unwrap();

        assert!(
            output.stdout.len() > 100_000,
            "read {} bytes of it",
            output.stdout.len()
        );
    }

    #[test]
    fn lifecycle_hook_default_is_the_platform_shell() {
        let temp = TempDir::new().unwrap();
        let output = temp.path().join("default-shell");
        // The default hook shell is `bash -c` on Unix and `cmd /C` on Windows,
        // so the snippet has to be spelled in that dialect.
        let command = if cfg!(windows) {
            format!("echo compatible > \"{}\"", output.display())
        } else {
            format!("printf compatible > '{}'", output.display())
        };

        shell_command_with_env(None, &command, temp.path(), &[]).unwrap();

        assert_eq!(
            std::fs::read_to_string(output).unwrap().trim(),
            "compatible"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_hook_uses_configured_executable_and_appends_command() {
        let temp = TempDir::new().unwrap();
        let output = temp.path().join("argv");
        let hook_shell = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '%s\\n' \"$0\" \"$1\" > \"$ARGV_OUTPUT\"".to_string(),
            "--configured-argument".to_string(),
        ];

        shell_command_with_env(
            Some(&hook_shell),
            "the hook command",
            temp.path(),
            &[("ARGV_OUTPUT", output.to_str().unwrap())],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "--configured-argument\nthe hook command\n"
        );
    }

    #[test]
    fn unavailable_lifecycle_hook_executable_is_named_in_error() {
        let hook_shell = vec!["/workmux/missing/hook-shell".to_string(), "-c".to_string()];
        let error = shell_command_with_env(
            Some(&hook_shell),
            "true",
            std::env::temp_dir().as_path(),
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("/workmux/missing/hook-shell"));
    }
}
