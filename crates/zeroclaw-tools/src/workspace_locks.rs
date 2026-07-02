use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Per-path write locks shared by file mutation tools in one tool registry.
#[derive(Clone, Default)]
pub struct WorkspaceFileLocks {
    locks: Arc<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>>,
}

pub struct FileWriteGuard {
    _guard: OwnedMutexGuard<()>,
}

impl WorkspaceFileLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn lock_path(&self, path: &Path) -> FileWriteGuard {
        let key = normalized_lock_key(path);
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .expect("workspace file lock registry poisoned");
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        FileWriteGuard {
            _guard: lock.lock_owned().await,
        }
    }
}

fn normalized_lock_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_path_writers_serialize() {
        let locks = WorkspaceFileLocks::new();
        let path = PathBuf::from("/tmp/zeroclaw-lock-test");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        async fn writer(
            locks: WorkspaceFileLocks,
            path: PathBuf,
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        ) {
            let _guard = locks.lock_path(&path).await;
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            active.fetch_sub(1, Ordering::SeqCst);
        }

        tokio::join!(
            writer(
                locks.clone(),
                path.clone(),
                Arc::clone(&active),
                Arc::clone(&peak),
            ),
            writer(locks, path, active, Arc::clone(&peak)),
        );
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
}
