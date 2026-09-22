//! The processes a WezTerm pane runs, as Windows reports them.
//!
//! WezTerm publishes no process for a pane: `wezterm cli list` answers with a
//! title, a cwd, and a `tty_name` that is null on Windows, where the Unix shape
//! of the WezTerm backend asks `ps -t <tty>`. That left a pane's pid and its
//! command unreadable on Windows, and with them the agent state built from
//! them: `pane_pid` was always zero, and `agent_identity` could only classify a
//! Windows pane by its title.
//!
//! The processes are read from the process table instead. A pane's processes are
//! found from the mux down: WezTerm starts a pane's own process, so a process
//! whose parent is the mux is the root of a pane, and the command a pane runs is
//! the first process below that root which is not a shell.
//!
//! ToolHelp reads the table in-process, in single-digit milliseconds. The WMI
//! query that would otherwise answer this (`Get-CimInstance Win32_Process`) was
//! measured at 980 ms, which is a whole pane listing on its own, and the sidebar
//! repeats one every second.
//!
//! Finding the root of a *given* pane needs a process inside that pane, and
//! `WEZTERM_PANE` names the pane a process runs in. So workmux that runs inside
//! a pane walks up from itself to the pane's root and writes it down
//! (`remember_root`); later runs -- `status`, the sidebar, `reap-agents` -- find
//! the pane's processes through that note.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::state::PaneKey;

/// One process, as the process table lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinProcess {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

/// Names of the processes WezTerm starts panes from.
const MUXES: &[&str] = &["wezterm.exe", "wezterm-gui.exe", "wezterm-mux-server.exe"];

/// Names that are a shell rather than the command a pane is running.
///
/// A pane's process is a shell even when it is running an agent: workmux starts
/// the shell through a handshake script and types the agent's command into it.
const SHELLS: &[&str] = &[
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "bash.exe",
    "sh.exe",
    "zsh.exe",
    "fish.exe",
    "nu.exe",
    "wsl.exe",
    "conhost.exe",
    "openconsole.exe",
];

/// How far a walk follows parents or children before giving up.
///
/// Real trees are a handful of processes deep. The bound is what keeps a
/// recycled id, or a pair of processes naming each other, from looping.
const WALK_LIMIT: usize = 32;

/// Every process on the machine, read in one snapshot.
pub fn processes() -> Result<Vec<WinProcess>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: the snapshot is owned here and closed before returning, and the
    // entry is a plain struct the API asks to be sized before the first call.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(anyhow!(
                "ToolHelp refused a process snapshot: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut processes = Vec::new();
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            processes.push(WinProcess {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                name: exe_name(&entry.szExeFile),
            });
            more = Process32NextW(snapshot, &mut entry) != 0;
        }

        CloseHandle(snapshot);
        Ok(processes)
    }
}

/// The name in a `PROCESSENTRY32W`: UTF-16 up to its null.
fn exe_name(field: &[u16]) -> String {
    let end = field
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}

/// The pane root that `pid` runs in: the process WezTerm started for the pane.
///
/// Following parents up from a process inside a pane reaches the process the
/// pane is rooted at -- the one whose own parent is the mux. A process with no
/// such ancestor, one running outside a pane, has no pane root.
pub fn pane_root(processes: &[WinProcess], pid: u32) -> Option<u32> {
    let mut current = pid;
    for _ in 0..WALK_LIMIT {
        let process = find(processes, current)?;
        let parent = find(processes, process.parent)?;
        if parent.pid == process.pid {
            return None;
        }
        if is_mux(&parent.name) {
            return Some(process.pid);
        }
        current = parent.pid;
    }
    None
}

/// The command a pane is running: the first process below its root that is not
/// a shell, or the root's shell when it is running nothing.
///
/// Descending through shells walks past the ones workmux started (the handshake
/// wrapper, the pane's shell) to the command itself, which is what tmux reports
/// as `pane_current_command`. A shell with nothing below it is the command,
/// which is what makes an agent's exit readable: the pane's command changes
/// from the agent back to the shell that started it.
pub fn foreground(processes: &[WinProcess], root: u32) -> Option<&WinProcess> {
    foreground_ignoring(processes, root, self_image_name())
}

/// `foreground`, with the name workmux's own runs carry given explicitly.
fn foreground_ignoring<'a>(
    processes: &'a [WinProcess],
    root: u32,
    self_name: Option<&str>,
) -> Option<&'a WinProcess> {
    let mut current = find(processes, root)?;
    for _ in 0..WALK_LIMIT {
        if !is_shell(&current.name) {
            break;
        }
        let Some(child) = running_child(processes, current.pid, self_name) else {
            break;
        };
        current = child;
    }
    Some(current)
}

/// The child of `pid` a shell is waiting on: a command over a nested shell, and
/// among equals the one started last.
///
/// Windows hands out process ids in order, so the largest id is the newest
/// child -- the command the shell started most recently.
///
/// A run of workmux's own is not that command: the status hook runs inside the
/// pane it reports on, and it lives only for the hook. Counting it would make
/// the pane look like it had changed command every time its status was set, and
/// the agent that set it would be read as gone.
fn running_child<'a>(
    processes: &'a [WinProcess],
    pid: u32,
    self_name: Option<&str>,
) -> Option<&'a WinProcess> {
    processes
        .iter()
        .filter(|process| process.parent == pid && !is_own_run(&process.name, self_name))
        .max_by_key(|process| (!is_shell(&process.name), process.pid))
}

/// Whether `name` is the name one of workmux's own runs carries.
fn is_own_run(name: &str, self_name: Option<&str>) -> bool {
    self_name.is_some_and(|own| name.eq_ignore_ascii_case(own))
}

/// The image name this process was started from.
///
/// workmux runs from one image whatever it is asked to do, so the name of the
/// process asking is the name of every run of it, hooks included.
fn self_image_name() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        std::env::current_exe().ok().and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
    })
    .as_deref()
}

fn find(processes: &[WinProcess], pid: u32) -> Option<&WinProcess> {
    processes.iter().find(|process| process.pid == pid)
}

fn is_mux(name: &str) -> bool {
    MUXES.iter().any(|mux| name.eq_ignore_ascii_case(mux))
}

fn is_shell(name: &str) -> bool {
    SHELLS.iter().any(|shell| name.eq_ignore_ascii_case(shell))
}

/// The command name workmux stores for a process.
///
/// The process table names images, so a Windows command arrives as `node.exe`
/// where the Unix backends report `node`. The suffix is dropped to keep the
/// stored command, and the agent classification that reads it, the same on both.
pub fn command_name(name: &str) -> &str {
    match name.len().checked_sub(4).and_then(|at| name.get(at..)) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".exe") => &name[..name.len() - 4],
        _ => name,
    }
}

/// The process a pane is rooted at, written down for the runs that cannot see
/// the pane from the inside.
///
/// Keyed by the pane, like the agent state beside it: a note for a pane that
/// was never seen inside reads as no root, and a note naming a process that is
/// gone is dropped by its reader.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RootNote {
    pid: u32,
}

/// Write down the root process of one pane.
pub fn remember_root(key: &PaneKey, pid: u32) -> Result<()> {
    remember_root_in(&notes_dir()?, key, pid)
}

/// The root process remembered for one pane, if one was.
pub fn remembered_root(key: &PaneKey) -> Option<u32> {
    read_root_note(&note_path(&notes_dir().ok()?, key)).map(|note| note.pid)
}

/// Drop a pane's note: its root is gone, so there is nothing left to find.
pub fn forget_root(key: &PaneKey) {
    if let Ok(dir) = notes_dir() {
        let _ = std::fs::remove_file(note_path(&dir, key));
    }
}

fn notes_dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_dir()?.join("pane-procs"))
}

fn note_path(dir: &Path, key: &PaneKey) -> PathBuf {
    dir.join(key.to_filename())
}

fn remember_root_in(dir: &Path, key: &PaneKey, pid: u32) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = note_path(dir, key);
    let note = serde_json::to_string(&RootNote { pid })?;
    std::fs::write(&path, note).with_context(|| format!("Failed to write {}", path.display()))
}

/// A note, or nothing: a note that cannot be read is not worth failing a
/// listing over, and the pane's next run inside itself writes it again.
fn read_root_note(path: &Path) -> Option<RootNote> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent: u32, name: &str) -> WinProcess {
        WinProcess {
            pid,
            parent,
            name: name.to_string(),
        }
    }

    fn pane_key(pane_id: &str) -> PaneKey {
        PaneKey {
            backend: "wezterm".to_string(),
            instance: "gui-sock-37828".to_string(),
            pane_id: pane_id.to_string(),
        }
    }

    /// A pane's root is the process the mux started, however deep inside the
    /// pane the process that asks is.
    #[test]
    fn a_pane_root_is_the_process_the_mux_started() {
        let processes = vec![
            process(1, 0, "wezterm-gui.exe"),
            process(2, 1, "cmd.exe"),
            process(3, 2, "powershell.exe"),
            process(4, 3, "workmux.exe"),
            // The console host is started for the same pane, beside its shell.
            process(5, 1, "OpenConsole.exe"),
            // A process that is not in a pane at all.
            process(9, 8, "explorer.exe"),
        ];

        assert_eq!(pane_root(&processes, 4), Some(2));
        assert_eq!(pane_root(&processes, 3), Some(2));
        // The pane's own process is its root.
        assert_eq!(pane_root(&processes, 2), Some(2));
        assert_eq!(pane_root(&processes, 1), None);
        assert_eq!(pane_root(&processes, 9), None);
        assert_eq!(pane_root(&processes, 99), None);
    }

    /// The command a pane runs is the process below the shells workmux started,
    /// and a shell running nothing is its own command.
    #[test]
    fn the_command_is_the_process_below_the_shells() {
        let agent = vec![
            process(1, 0, "wezterm-gui.exe"),
            process(2, 1, "cmd.exe"),
            process(3, 2, "powershell.exe"),
            process(4, 3, "node.exe"),
            // A tool the agent runs is below the agent, not the pane's command.
            process(5, 4, "node_repl.exe"),
        ];
        assert_eq!(
            foreground(&agent, 2).map(|process| process.name.as_str()),
            Some("node.exe")
        );

        // The agent has exited: the shell is what the pane is back to.
        let idle = vec![
            process(1, 0, "wezterm-gui.exe"),
            process(2, 1, "cmd.exe"),
            process(3, 2, "powershell.exe"),
        ];
        assert_eq!(
            foreground(&idle, 2).map(|process| process.name.as_str()),
            Some("powershell.exe")
        );

        // A pane whose own process is the command, as a hand-spawned one is.
        let direct = vec![process(1, 0, "wezterm-gui.exe"), process(2, 1, "PING.EXE")];
        assert_eq!(
            foreground(&direct, 2).map(|process| process.name.as_str()),
            Some("PING.EXE")
        );
    }

    /// A shell waiting on a command reports that command, not a shell nested
    /// inside it.
    #[test]
    fn a_command_outranks_a_nested_shell() {
        let processes = vec![
            process(1, 0, "wezterm-gui.exe"),
            process(2, 1, "cmd.exe"),
            process(3, 2, "cmd.exe"),
            process(9, 2, "ping.exe"),
        ];

        assert_eq!(
            foreground(&processes, 2).map(|process| process.name.as_str()),
            Some("ping.exe")
        );
    }

    /// Processes that name each other end the walk instead of looping.
    #[test]
    fn a_walk_ends_on_a_cycle() {
        let processes = vec![process(5, 6, "cmd.exe"), process(6, 5, "powershell.exe")];

        assert_eq!(pane_root(&processes, 5), None);
        assert!(foreground(&processes, 5).is_some());
    }

    /// A status hook runs inside the pane it reports on, and it is not the
    /// command that pane is running.
    #[test]
    fn a_status_hook_is_not_the_command_a_pane_runs() {
        // The pane is back at its prompt: the shell is what it runs, and the
        // hook is only the child it started.
        let hook_alone = vec![
            process(1, 0, "wezterm-gui.exe"),
            process(2, 1, "cmd.exe"),
            process(3, 2, "bin-workmux.exe"),
        ];
        assert_eq!(
            foreground_ignoring(&hook_alone, 2, Some("bin-workmux.exe"))
                .map(|process| command_name(&process.name)),
            Some("cmd")
        );

        // An agent is running: the hook must not stand in for it, even though
        // the hook is the younger of the shell's two children.
        let agent_and_hook = vec![
            process(1, 0, "wezterm-gui.exe"),
            process(2, 1, "cmd.exe"),
            process(3, 2, "node.exe"),
            process(4, 2, "bin-workmux.exe"),
        ];
        assert_eq!(
            foreground_ignoring(&agent_and_hook, 2, Some("bin-workmux.exe"))
                .map(|process| command_name(&process.name)),
            Some("node")
        );

        // An image name is compared without regard to case, and a run of
        // another workmux is not this one.
        assert!(is_own_run("BIN-WORKMUX.EXE", Some("bin-workmux.exe")));
        assert!(!is_own_run("bin-workmux.exe", Some("workmux.exe")));
        assert!(!is_own_run("bin-workmux.exe", None));
    }

    /// The process table names images, so a command carries a suffix the Unix
    /// backends never report.
    #[test]
    fn a_command_drops_its_image_suffix() {
        assert_eq!(command_name("node.exe"), "node");
        assert_eq!(command_name("codex.EXE"), "codex");
        assert_eq!(command_name("PING.EXE"), "PING");
        assert_eq!(command_name("wezterm"), "wezterm");
        assert_eq!(command_name("exe"), "exe");
        // Not a suffix, and never cut through the middle of a character.
        assert_eq!(command_name("x.exes"), "x.exes");
        assert_eq!(command_name("h\u{e9}"), "h\u{e9}");
    }

    /// The table answers, and a process can see itself and its parent in it.
    #[test]
    fn the_process_table_lists_this_process() {
        let processes = processes().expect("the process table is readable");
        let me = processes
            .iter()
            .find(|process| process.pid == std::process::id())
            .expect("this process is listed");

        assert!(!me.name.is_empty());
        assert!(
            processes.iter().any(|process| process.pid == me.parent),
            "the parent of this process is listed too: {me:?}"
        );
    }

    /// A note names a pane's root, per pane, and a note that cannot be read
    /// answers no root rather than an error.
    #[test]
    fn a_note_remembers_a_panes_root() {
        let dir = tempfile::tempdir().unwrap();
        let key = pane_key("45");

        assert_eq!(read_root_note(&note_path(dir.path(), &key)), None);
        remember_root_in(dir.path(), &key, 8700).unwrap();
        assert_eq!(
            read_root_note(&note_path(dir.path(), &key)).map(|note| note.pid),
            Some(8700)
        );
        assert_eq!(
            read_root_note(&note_path(dir.path(), &pane_key("46"))),
            None
        );

        std::fs::write(note_path(dir.path(), &key), "{").unwrap();
        assert_eq!(read_root_note(&note_path(dir.path(), &key)), None);
    }
}
