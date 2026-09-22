use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::Serialize;
use tabled::{
    Table, Tabled,
    settings::{Padding, Style, object::Columns},
};

use crate::git;
use crate::multiplexer::{AgentPane, AgentStatus, create_backend, detect_backend_strict};
use crate::state::StateStore;
use crate::util;
use crate::workflow;

#[derive(Serialize)]
struct StatusEntry {
    worktree: String,
    branch: String,
    project: Option<String>,
    project_path: Option<PathBuf>,
    status: String,
    elapsed_secs: Option<u64>,
    title: Option<String>,
    pane_id: String,
    workdir: PathBuf,
    agent_kind: Option<String>,
    session: Option<String>,
    window_name: Option<String>,
    updated_ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<GitInfo>,
}

#[derive(Serialize)]
struct StatusContext {
    backend: String,
    instance: String,
}

#[derive(Serialize)]
struct StatusScope {
    repository: Option<PathBuf>,
    all: bool,
    targets: Vec<String>,
}

#[derive(Serialize)]
struct StatusTargetError {
    target: String,
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct StatusOutput {
    context: StatusContext,
    scope: StatusScope,
    state_files_total: usize,
    state_files_invalid: usize,
    state_files_invalid_unattributed: usize,
    state_files_invalid_matching_context: usize,
    state_files_matching_context: usize,
    reconciled_agent_count: usize,
    agents: Vec<StatusEntry>,
    target_errors: Vec<StatusTargetError>,
}

#[derive(Serialize, Clone)]
struct GitInfo {
    has_staged: bool,
    has_unstaged: bool,
    has_unmerged_commits: bool,
}

struct StatusRow {
    worktree: String,
    status: String,
    elapsed: String,
    git: String,
    title: String,
}

impl Tabled for StatusRow {
    const LENGTH: usize = 5;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        vec![
            Cow::Borrowed(&self.worktree),
            Cow::Borrowed(&self.status),
            Cow::Borrowed(&self.elapsed),
            Cow::Borrowed(&self.git),
            Cow::Borrowed(&self.title),
        ]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            Cow::Borrowed("WORKTREE"),
            Cow::Borrowed("STATUS"),
            Cow::Borrowed("ELAPSED"),
            Cow::Borrowed("GIT"),
            Cow::Borrowed("TITLE"),
        ]
    }
}

fn git_label(git: &Option<GitInfo>) -> String {
    let Some(g) = git else {
        return "-".to_string();
    };
    let mut parts = Vec::new();
    if g.has_staged {
        parts.push("staged");
    }
    if g.has_unstaged {
        parts.push("unstaged");
    }
    if g.has_unmerged_commits {
        parts.push("unmerged");
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(",")
    }
}

fn status_label(status: Option<AgentStatus>) -> String {
    match status {
        Some(AgentStatus::Working) => "working".to_string(),
        Some(AgentStatus::Waiting) => "waiting".to_string(),
        Some(AgentStatus::Done) => "done".to_string(),
        None => "-".to_string(),
    }
}

fn optional_name(name: &str) -> Option<String> {
    (!name.is_empty()).then(|| name.to_string())
}

fn normalized_branch(branch: String) -> String {
    if branch.is_empty() {
        "(detached)".to_string()
    } else {
        branch
    }
}

fn status_entry(
    agent: &AgentPane,
    worktree: String,
    branch: String,
    now: u64,
    git: Option<GitInfo>,
) -> StatusEntry {
    let (project, project_path) = git::project_identity(&agent.path);
    StatusEntry {
        worktree,
        branch,
        project,
        project_path,
        status: status_label(agent.status),
        elapsed_secs: agent.status_ts.map(|ts| now.saturating_sub(ts)),
        title: agent.pane_title.clone(),
        pane_id: agent.pane_id.clone(),
        workdir: agent.path.clone(),
        agent_kind: agent.agent_kind.clone(),
        session: optional_name(&agent.session),
        window_name: optional_name(&agent.window_name),
        updated_ts: agent.updated_ts,
        git,
    }
}

/// Repository facts every worktree of one repository shares an answer to.
///
/// The default branch, the ref to compare against, and the branches not merged
/// into it are the repository's, not the worktree's that asks for them. Asking
/// per worktree spent four git processes on them per worktree -- a process
/// measured about 140 ms here -- so a status covering twenty worktrees paid for
/// the same answer twenty times.
#[derive(Default)]
struct RepositoryBranches {
    unmerged: HashMap<PathBuf, HashSet<String>>,
}

impl RepositoryBranches {
    /// The branches not merged into the base of `worktree`'s repository.
    fn unmerged_in(&mut self, worktree: &Path) -> Result<&HashSet<String>> {
        self.held_or_resolved(repository_root(worktree), resolve_unmerged)
    }

    /// The answer for `repository`, resolved by `resolve` when it is not held.
    ///
    /// Split out from `unmerged_in` so that asking twice can be counted, which
    /// is the whole reason for holding the answer at all.
    fn held_or_resolved(
        &mut self,
        repository: PathBuf,
        resolve: impl FnOnce(&Path) -> Result<HashSet<String>>,
    ) -> Result<&HashSet<String>> {
        match self.unmerged.entry(repository) {
            Entry::Occupied(held) => Ok(held.into_mut()),
            Entry::Vacant(slot) => {
                let resolved = resolve(slot.key())?;
                Ok(slot.insert(resolved))
            }
        }
    }
}

/// The repository a worktree belongs to, read off the worktree's `.git` entry.
///
/// A linked worktree's `.git` file names the administrative directory git keeps
/// for it: `<repo>/.git/worktrees/<name>`, or `<bare>/worktrees/<name>` in a
/// bare repository. Either way the repository is what holds that pair -- the
/// `.git`'s parent when there is a `.git`, and the directory itself when the
/// repository is bare. A main worktree has a `.git` directory of its own, and
/// is the repository.
fn repository_root(worktree: &Path) -> PathBuf {
    let Some(admin) = git::linked_worktree_admin_dir(worktree) else {
        return worktree.to_path_buf();
    };
    let worktrees = admin.parent();
    let above = worktrees.and_then(Path::parent);
    match above.and_then(Path::file_name) {
        Some(name) if name == OsStr::new(".git") => above.and_then(Path::parent),
        _ => above,
    }
    .map(Path::to_path_buf)
    .unwrap_or_else(|| worktree.to_path_buf())
}

/// The branches of `repository` that are not merged into its base.
fn resolve_unmerged(repository: &Path) -> Result<HashSet<String>> {
    let main = git::get_default_branch_in(Some(repository))?;
    let base = git::get_merge_base_in(Some(repository), &main)?;
    git::get_unmerged_branches_in(Some(repository), &base)
}

/// Compute git info for a worktree path.
///
/// Runs git commands with the worktree's directory as the working dir, so it
/// works correctly for cross-project agents. The repository-wide questions go
/// through `branches`, which answers each repository once.
fn compute_git_info(
    wt_path: &Path,
    branch: &str,
    branches: &mut RepositoryBranches,
) -> Result<GitInfo> {
    let has_staged = git::has_staged_changes(wt_path)?;
    let has_unstaged = git::has_unstaged_changes(wt_path)?;
    let unmerged = branches.unmerged_in(wt_path)?;

    Ok(GitInfo {
        has_staged,
        has_unstaged,
        has_unmerged_commits: unmerged.contains(branch),
    })
}

pub fn run(worktrees: &[String], json: bool, all: bool, show_git: bool) -> Result<()> {
    let mux = create_backend(detect_backend_strict()?);
    let store = StateStore::open_read_only()?;
    let mut report = store.load_reconciled_agent_report(mux.as_ref())?;
    let agent_panes = std::mem::take(&mut report.agents);
    let invalid_state_failures =
        report.state_files_invalid_matching_context + report.state_files_invalid_unattributed;
    if invalid_state_failures > 0 {
        return Err(anyhow!(
            "{} invalid agent state file(s) for {} instance {} or with unattributable filenames; inspect the workmux agents state directory",
            invalid_state_failures,
            report.backend,
            report.instance
        ));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let reconciled_agent_count = agent_panes.len();
    let mut entries: Vec<StatusEntry> = Vec::new();
    let mut target_errors = Vec::new();
    let repository;
    let mut branches = RepositoryBranches::default();

    if worktrees.is_empty() {
        if !all && git::get_repo_root_if_present()?.is_some() {
            let all_worktrees = git::list_worktrees()?;
            repository = Some(git::get_main_worktree_root()?);

            for (wt_path, branch) in &all_worktrees {
                let matching = workflow::match_agents_to_worktree(&agent_panes, wt_path);
                if matching.is_empty() {
                    continue;
                }
                let worktree_name = wt_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let git_info = if show_git {
                    let unmerged = branches.unmerged_in(wt_path)?;
                    Some(GitInfo {
                        has_staged: git::has_staged_changes(wt_path)?,
                        has_unstaged: git::has_unstaged_changes(wt_path)?,
                        has_unmerged_commits: unmerged.contains(branch),
                    })
                } else {
                    None
                };

                entries.extend(matching.into_iter().map(|agent| {
                    status_entry(
                        agent,
                        worktree_name.clone(),
                        branch.clone(),
                        now,
                        git_info.clone(),
                    )
                }));
            }
        } else {
            repository = None;
            for agent in &agent_panes {
                let worktree_path =
                    workflow::find_worktree_root(&agent.path).unwrap_or_else(|| agent.path.clone());
                let worktree_name = worktree_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let branch = match git::get_branch_for_worktree(&worktree_path) {
                    Ok(branch) => normalized_branch(branch),
                    Err(error) if show_git => return Err(error),
                    Err(_) => worktree_name.clone(),
                };
                let git_info = if show_git {
                    Some(compute_git_info(&worktree_path, &branch, &mut branches)?)
                } else {
                    None
                };
                entries.push(status_entry(agent, worktree_name, branch, now, git_info));
            }
        }
    } else {
        repository = if git::get_repo_root_if_present()?.is_some() {
            Some(git::get_main_worktree_root()?)
        } else {
            None
        };
        for name in worktrees {
            let (wt_path, matching) =
                match workflow::resolve_worktree_agents_from_snapshot(name, &agent_panes) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        target_errors.push(StatusTargetError {
                            target: name.clone(),
                            code: "target_resolution_failed",
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
            if matching.is_empty() {
                continue;
            }
            let worktree_name = wt_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string();
            let branch = match git::get_branch_for_worktree(&wt_path) {
                Ok(branch) => branch,
                Err(error) if show_git => return Err(error),
                Err(_) => worktree_name.clone(),
            };
            let git_info = if show_git {
                Some(compute_git_info(&wt_path, &branch, &mut branches)?)
            } else {
                None
            };

            entries.extend(matching.iter().map(|agent| {
                status_entry(
                    agent,
                    worktree_name.clone(),
                    branch.clone(),
                    now,
                    git_info.clone(),
                )
            }));
        }
    }

    let target_failure_count = target_errors.len();
    if json {
        let output = StatusOutput {
            context: StatusContext {
                backend: report.backend,
                instance: report.instance,
            },
            scope: StatusScope {
                repository,
                all,
                targets: worktrees.to_vec(),
            },
            state_files_total: report.state_files_total,
            state_files_invalid: report.state_files_invalid,
            state_files_invalid_unattributed: report.state_files_invalid_unattributed,
            state_files_invalid_matching_context: report.state_files_invalid_matching_context,
            state_files_matching_context: report.state_files_matching_context,
            reconciled_agent_count,
            agents: entries,
            target_errors,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        for error in &target_errors {
            eprintln!("{}: {}", error.target, error.message);
        }

        if entries.is_empty() && target_errors.is_empty() {
            if report.state_files_total > 0 && report.state_files_matching_context == 0 {
                eprintln!(
                    "{} agent state file(s) exist, but none match {} instance {}",
                    report.state_files_total, report.backend, report.instance
                );
            } else if reconciled_agent_count > 0 {
                eprintln!(
                    "{reconciled_agent_count} tracked agent(s) exist outside the requested scope"
                );
            }
            println!("No active agents");
        } else if !entries.is_empty() {
            let rows: Vec<StatusRow> = entries
                .iter()
                .map(|e| {
                    let worktree = if e.branch != e.worktree {
                        format!("{} ({})", e.worktree, e.branch)
                    } else {
                        e.worktree.clone()
                    };
                    StatusRow {
                        worktree,
                        status: e.status.clone(),
                        elapsed: e
                            .elapsed_secs
                            .map(util::format_elapsed_secs)
                            .unwrap_or("-".to_string()),
                        git: git_label(&e.git),
                        title: e.title.clone().unwrap_or("-".to_string()),
                    }
                })
                .collect();

            let mut table = Table::new(rows);
            table
                .with(Style::blank())
                .modify(Columns::new(..), Padding::new(0, 1, 0, 0));
            if !show_git {
                table.with(tabled::settings::Remove::column(
                    tabled::settings::location::ByColumnName::new("GIT"),
                ));
            }
            println!("{table}");
        }
    }

    if target_failure_count > 0 {
        return Err(anyhow!(
            "failed to resolve {target_failure_count} status target(s)"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn git_info_fails_for_non_repository_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut branches = RepositoryBranches::default();

        assert!(compute_git_info(dir.path(), "feature", &mut branches).is_err());
    }

    /// Two worktrees of one repository ask the repository's questions, and the
    /// answer is the repository's, so only the first worktree pays for it.
    #[test]
    fn a_repository_is_resolved_once_however_many_worktrees_ask() {
        let mut branches = RepositoryBranches::default();
        let resolutions = Rc::new(Cell::new(0));
        let resolve = |repository: &Path| -> Result<HashSet<String>> {
            resolutions.set(resolutions.get() + 1);
            Ok(HashSet::from([repository.display().to_string()]))
        };

        let from_one = branches
            .held_or_resolved(PathBuf::from("/repo"), &resolve)
            .unwrap()
            .clone();
        let from_two = branches
            .held_or_resolved(PathBuf::from("/repo"), &resolve)
            .unwrap()
            .clone();

        assert_eq!(resolutions.get(), 1);
        assert_eq!(from_one, from_two);
    }

    /// Two repositories are two answers, so the second one is asked for.
    #[test]
    fn another_repository_is_resolved_separately() {
        let mut branches = RepositoryBranches::default();
        let resolutions = Rc::new(Cell::new(0));
        let resolve = |repository: &Path| -> Result<HashSet<String>> {
            resolutions.set(resolutions.get() + 1);
            Ok(HashSet::from([repository.display().to_string()]))
        };

        let from_one = branches
            .held_or_resolved(PathBuf::from("/one"), &resolve)
            .unwrap()
            .clone();
        let from_two = branches
            .held_or_resolved(PathBuf::from("/two"), &resolve)
            .unwrap()
            .clone();

        assert_eq!(resolutions.get(), 2);
        assert_eq!(from_one, HashSet::from(["/one".to_string()]));
        assert_eq!(from_two, HashSet::from(["/two".to_string()]));
    }

    /// A main worktree has the repository's `.git` directory itself, so it is
    /// the repository.
    #[test]
    fn a_main_worktree_is_its_own_repository() {
        let dir = tempfile::tempdir().unwrap();
        test_support::init_repo(dir.path());

        assert_eq!(repository_root(dir.path()), dir.path());
    }

    /// A linked worktree is not a repository: the administrative directory it
    /// points at is held inside the repository that added it.
    #[test]
    fn a_linked_worktree_reports_the_repository_that_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let repository = dir.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        test_support::init_repo(&repository);
        let worktree = dir.path().join("worktree");
        test_support::run_git(
            &repository,
            &["worktree", "add", worktree.to_str().unwrap()],
        );

        assert_eq!(repository_root(&worktree), repository);
    }

    /// A bare repository keeps its worktrees where a main repository keeps
    /// `.git`, and is still the repository of every worktree it added.
    #[test]
    fn a_worktree_of_a_bare_repository_reports_the_bare_repository() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        test_support::init_repo(&source);
        let bare = dir.path().join("bare");
        test_support::run_git(
            dir.path(),
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let worktree = dir.path().join("worktree");
        test_support::run_git(&bare, &["worktree", "add", worktree.to_str().unwrap()]);

        assert_eq!(repository_root(&worktree), bare);
    }
}
