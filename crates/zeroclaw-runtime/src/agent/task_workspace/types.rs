use std::path::PathBuf;

use zeroclaw_config::schema::WorkspaceIsolationMode;

#[derive(Debug, Clone)]
pub(crate) struct AllocatedWorkspace {
    pub(crate) root: PathBuf,
    pub(crate) mode: WorkspaceIsolationMode,
    pub(crate) warning: Option<String>,
}

impl AllocatedWorkspace {
    pub(crate) fn shared(root: PathBuf) -> Self {
        Self {
            root,
            mode: WorkspaceIsolationMode::Shared,
            warning: None,
        }
    }

    pub(crate) fn shared_with_warning(root: PathBuf, warning: String) -> Self {
        Self {
            root,
            mode: WorkspaceIsolationMode::Shared,
            warning: Some(warning),
        }
    }
}
