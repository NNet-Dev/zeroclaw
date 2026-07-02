mod allocator;
mod teardown;
mod types;

use std::sync::Arc;

use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{WorkspaceIsolationConfig, WorkspaceIsolationMode};

use allocator::WorktreeAllocator;
pub(crate) use types::AllocatedWorkspace;

pub(crate) struct TaskWorkspace {
    parent_policy: Arc<SecurityPolicy>,
    allocator: WorktreeAllocator,
}

impl TaskWorkspace {
    pub(crate) fn new(parent_policy: Arc<SecurityPolicy>, cfg: &WorkspaceIsolationConfig) -> Self {
        Self {
            allocator: WorktreeAllocator::new(Arc::clone(&parent_policy), cfg),
            parent_policy,
        }
    }

    pub(crate) async fn allocate(&self, task_id: &str) -> AllocatedWorkspace {
        self.allocator.allocate(task_id).await
    }

    pub(crate) async fn release(&self, workspace: &AllocatedWorkspace) {
        if workspace.mode != WorkspaceIsolationMode::WorktreePerTask {
            return;
        }
        if let Err(error) =
            teardown::release_worktree(&self.parent_policy.workspace_dir, &workspace.root).await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "workspace": workspace.root.display().to_string(),
                        "error": error.to_string(),
                    })),
                "task workspace release failed"
            );
        }
    }

    pub(crate) fn child_policy_for(&self, workspace: &AllocatedWorkspace) -> Arc<SecurityPolicy> {
        if workspace.mode != WorkspaceIsolationMode::WorktreePerTask {
            return Arc::clone(&self.parent_policy);
        }

        let mut child = (*self.parent_policy).clone();
        child.workspace_dir = workspace.root.clone();
        child.allowed_roots.clear();
        child.allowed_roots_read_only.clear();
        child.allowed_roots_write_only.clear();
        Arc::new(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use zeroclaw_config::schema::WorkspaceIsolationMode;

    #[test]
    fn child_policy_rebases_to_worktree_and_drops_extra_roots() {
        let parent = Arc::new(SecurityPolicy {
            workspace_dir: PathBuf::from("/repo"),
            allowed_roots: vec![PathBuf::from("/repo/shared")],
            allowed_roots_read_only: vec![PathBuf::from("/repo/readonly")],
            allowed_roots_write_only: vec![PathBuf::from("/repo/writeonly")],
            ..SecurityPolicy::default()
        });
        let task_workspace = TaskWorkspace::new(
            Arc::clone(&parent),
            &WorkspaceIsolationConfig {
                isolation: WorkspaceIsolationMode::WorktreePerTask,
                ..WorkspaceIsolationConfig::default()
            },
        );
        let allocated = AllocatedWorkspace {
            root: PathBuf::from("/repo/.zc-tasks/task-1"),
            mode: WorkspaceIsolationMode::WorktreePerTask,
            warning: None,
        };

        let child = task_workspace.child_policy_for(&allocated);

        assert_eq!(child.workspace_dir, allocated.root);
        assert!(child.allowed_roots.is_empty());
        assert!(child.allowed_roots_read_only.is_empty());
        assert!(child.allowed_roots_write_only.is_empty());
    }
}
