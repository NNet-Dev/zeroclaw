use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tokio::process::Command;
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{WorkspaceIsolationConfig, WorkspaceIsolationMode};

use super::types::AllocatedWorkspace;

pub(crate) struct WorktreeAllocator {
    parent_policy: Arc<SecurityPolicy>,
    cfg: WorkspaceIsolationConfig,
}

impl WorktreeAllocator {
    pub(crate) fn new(parent_policy: Arc<SecurityPolicy>, cfg: &WorkspaceIsolationConfig) -> Self {
        Self {
            parent_policy,
            cfg: cfg.clone(),
        }
    }

    pub(crate) async fn allocate(&self, task_id: &str) -> AllocatedWorkspace {
        if self.cfg.isolation != WorkspaceIsolationMode::WorktreePerTask {
            return AllocatedWorkspace::shared(self.parent_policy.workspace_dir.clone());
        }

        match self.try_allocate(task_id).await {
            Ok(root) => AllocatedWorkspace {
                root,
                mode: WorkspaceIsolationMode::WorktreePerTask,
                warning: None,
            },
            Err(error) => AllocatedWorkspace::shared_with_warning(
                self.parent_policy.workspace_dir.clone(),
                format!(
                    "workspace isolation fell back to shared workspace for subagent task {task_id}: {error}"
                ),
            ),
        }
    }

    async fn try_allocate(&self, task_id: &str) -> anyhow::Result<PathBuf> {
        ensure_git_worktree(&self.parent_policy.workspace_dir).await?;
        let worktree_root =
            resolve_worktree_root(&self.parent_policy.workspace_dir, &self.cfg.worktree_root)?;
        tokio::fs::create_dir_all(&worktree_root).await?;

        let live = count_live_worktrees(&worktree_root).await?;
        if live >= self.cfg.max_concurrent_worktrees {
            anyhow::bail!(
                "concurrent task worktree cap ({}) reached",
                self.cfg.max_concurrent_worktrees
            );
        }

        let target = worktree_root.join(sanitize_task_id(task_id));
        if !self.parent_policy.is_resolved_path_allowed(&target) {
            anyhow::bail!("worktree target is outside the parent's writable workspace");
        }
        if tokio::fs::try_exists(&target).await? {
            anyhow::bail!("worktree target already exists: {}", target.display());
        }

        let output = Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&target)
            .arg("HEAD")
            .current_dir(&self.parent_policy.workspace_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(target)
    }
}

fn resolve_worktree_root(workspace_dir: &Path, raw_root: &str) -> anyhow::Result<PathBuf> {
    let root = Path::new(raw_root);
    if root.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        anyhow::bail!("worktree_root must be relative and must not contain parent traversal");
    }
    Ok(workspace_dir.join(root))
}

async fn ensure_git_worktree(workspace_dir: &Path) -> anyhow::Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        anyhow::bail!("workspace is not a git worktree");
    }
    Ok(())
}

async fn count_live_worktrees(worktree_root: &Path) -> anyhow::Result<usize> {
    let mut count = 0;
    let mut entries = tokio::fs::read_dir(worktree_root).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            count += 1;
        }
    }
    Ok(count)
}

fn sanitize_task_id(task_id: &str) -> String {
    task_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_root_must_be_relative() {
        assert!(resolve_worktree_root(Path::new("/repo"), ".zc-tasks").is_ok());
        assert!(resolve_worktree_root(Path::new("/repo"), "../x").is_err());
        assert!(resolve_worktree_root(Path::new("/repo"), "/tmp/x").is_err());
    }
}
