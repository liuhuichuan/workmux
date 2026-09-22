use anyhow::Result;
use std::path::Path;
use tracing::debug;

use crate::util::FileLock;

/// RAII guard that holds an exclusive advisory lock on a `.workmux.lock` file
/// in the git common directory. Serializes concurrent workmux processes that
/// write to `.git/config`.
pub struct GitConfigLock {
    _lock: FileLock,
}

impl GitConfigLock {
    /// Acquire an exclusive lock, blocking until available.
    pub fn acquire(git_common_dir: &Path) -> Result<Self> {
        let lock_path = git_common_dir.join(".workmux.lock");
        debug!(path = %lock_path.display(), "config_lock:acquiring");

        let lock = FileLock::acquire(&lock_path)?;

        debug!(path = %lock_path.display(), "config_lock:acquired");
        Ok(Self { _lock: lock })
    }
}
