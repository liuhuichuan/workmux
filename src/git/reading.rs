//! Repository facts workmux reads more than once in a run.
//!
//! Every question put to git is a process, measured at about 145 ms on this
//! machine, and one command asks the same question several times: a hook that
//! only records an agent's status resolves the repository root twice and lists
//! the worktrees twice, once per config load, which is 564 ms of git for an
//! answer that cannot have changed in between. A reading is therefore kept --
//! for a moment rather than for the run, because the processes that outlive a
//! command (a dashboard, a sidebar daemon) still have to see a worktree that
//! someone added by hand.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a reading of the repository is reused.
///
/// Longer than the several questions one command asks, shorter than the
/// shortest loop that reads one -- the dashboard refetches its worktrees every
/// two seconds.
const READING_AGE: Duration = Duration::from_secs(1);

/// A reading of the repository, and where it was taken from.
///
/// The place matters as much as the reading: the same question asked from
/// another directory has another answer. A caller that names no directory is
/// asking about its own, so the reading is keyed by the current directory and a
/// process that moves loses it.
#[derive(Default)]
pub(crate) struct Reading {
    held: Mutex<Option<Held>>,
}

struct Held {
    at: Instant,
    from: Option<PathBuf>,
    text: String,
}

impl Reading {
    /// The reading taken from `from`, or one taken now.
    pub(crate) fn get_or_take(
        &self,
        from: Option<PathBuf>,
        now: Instant,
        take: impl FnOnce() -> Result<String>,
    ) -> Result<String> {
        if let Some(text) = self.held(from.as_deref(), now) {
            return Ok(text);
        }
        let text = take()?;
        if let Ok(mut held) = self.held.lock() {
            *held = Some(Held {
                at: now,
                from,
                text: text.clone(),
            });
        }
        Ok(text)
    }

    /// Forget the reading: the next one is taken from git.
    pub(crate) fn forget(&self) {
        if let Ok(mut held) = self.held.lock() {
            *held = None;
        }
    }

    fn held(&self, from: Option<&Path>, now: Instant) -> Option<String> {
        let held = self.held.lock().ok()?;
        let held = held.as_ref()?;
        let fresh = now.saturating_duration_since(held.at) < READING_AGE;
        (fresh && held.from.as_deref() == from).then(|| held.text.clone())
    }
}

/// Where a question was asked from: the directory git should answer about.
pub(crate) fn asked_from(workdir: Option<&Path>) -> Option<PathBuf> {
    match workdir {
        Some(dir) => Some(dir.to_path_buf()),
        None => std::env::current_dir().ok(),
    }
}

/// The repository root each directory sits in.
static ROOTS: OnceLock<Reading> = OnceLock::new();

/// The worktrees, as `git worktree list --porcelain` prints them.
static WORKTREES: OnceLock<Reading> = OnceLock::new();

/// The workmux base each branch was created from, as `git config` prints them.
static BASES: OnceLock<Reading> = OnceLock::new();

pub(crate) fn roots() -> &'static Reading {
    ROOTS.get_or_init(Reading::default)
}

pub(crate) fn worktrees() -> &'static Reading {
    WORKTREES.get_or_init(Reading::default)
}

pub(crate) fn branch_bases() -> &'static Reading {
    BASES.get_or_init(Reading::default)
}

/// Forget what git was asked, for a command that changes the worktrees.
///
/// A worktree added, moved or pruned answers the same question differently, so
/// those commands drop what workmux holds rather than let the reads that follow
/// them answer from before.
pub fn forget_repository_readings() {
    roots().forget();
    worktrees().forget();
    branch_bases().forget();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A reading, and a counter of how often git was asked.
    fn reading() -> (Reading, Rc<Cell<usize>>) {
        (Reading::default(), Rc::new(Cell::new(0)))
    }

    fn answer(reads: &Rc<Cell<usize>>, text: &'static str) -> impl Fn() -> Result<String> + use<> {
        let reads = Rc::clone(reads);
        move || {
            reads.set(reads.get() + 1);
            Ok(text.to_string())
        }
    }

    /// A reading taken now answers the questions asked straight after it,
    /// without asking git again.
    #[test]
    fn a_reading_is_taken_once() {
        let (reading, reads) = reading();
        let take = answer(&reads, "/repo");
        let from = Some(PathBuf::from("/repo"));
        let now = Instant::now();

        assert_eq!(
            reading.get_or_take(from.clone(), now, &take).unwrap(),
            "/repo"
        );
        assert_eq!(
            reading
                .get_or_take(from, now + Duration::from_millis(100), &take)
                .unwrap(),
            "/repo"
        );

        assert_eq!(reads.get(), 1);
    }

    /// A command that changed the worktrees asks git again rather than answer
    /// from before the change.
    #[test]
    fn a_forgotten_reading_is_taken_again() {
        let (reading, reads) = reading();
        let take = answer(&reads, "/repo");
        let from = Some(PathBuf::from("/repo"));
        let now = Instant::now();

        reading.get_or_take(from.clone(), now, &take).unwrap();
        reading.forget();
        reading.get_or_take(from, now, &take).unwrap();

        assert_eq!(reads.get(), 2);
    }

    /// The same question asked from another directory has another answer, so a
    /// reading does not carry across.
    #[test]
    fn a_reading_does_not_carry_to_another_directory() {
        let (reading, reads) = reading();
        let take = answer(&reads, "/repo");
        let now = Instant::now();

        reading
            .get_or_take(Some(PathBuf::from("/one")), now, &take)
            .unwrap();
        reading
            .get_or_take(Some(PathBuf::from("/two")), now, &take)
            .unwrap();

        assert_eq!(reads.get(), 2);
    }

    /// A reading is a moment's answer, not the run's: a worktree added by hand
    /// while a dashboard is open still shows up.
    #[test]
    fn a_reading_goes_stale() {
        let (reading, reads) = reading();
        let take = answer(&reads, "/repo");
        let from = Some(PathBuf::from("/repo"));
        let now = Instant::now();

        reading.get_or_take(from.clone(), now, &take).unwrap();
        reading.get_or_take(from, now + READING_AGE, &take).unwrap();

        assert_eq!(reads.get(), 2);
    }

    /// A failed reading is nothing to hold on to.
    #[test]
    fn a_failed_read_is_not_held() {
        let (reading, reads) = reading();
        let failing = {
            let reads = Rc::clone(&reads);
            move || -> Result<String> {
                reads.set(reads.get() + 1);
                Err(anyhow::anyhow!("not a git repository"))
            }
        };
        let from = Some(PathBuf::from("/repo"));
        let now = Instant::now();

        assert!(reading.get_or_take(from.clone(), now, &failing).is_err());
        assert!(reading.get_or_take(from, now, &failing).is_err());

        assert_eq!(reads.get(), 2);
    }
}
