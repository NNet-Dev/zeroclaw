use crate::security::SecurityPolicy;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct ShellSession {
    cwd: Arc<Mutex<PathBuf>>,
    security: Arc<SecurityPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellSessionError {
    Poisoned,
    EscapesWorkspace(PathBuf),
    NotDirectory(PathBuf),
}

impl fmt::Display for ShellSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => write!(f, "shell cwd state is unavailable"),
            Self::EscapesWorkspace(path) => {
                write!(
                    f,
                    "cd target escapes the configured workspace: {}",
                    path.display()
                )
            }
            Self::NotDirectory(path) => {
                write!(f, "cd target is not a directory: {}", path.display())
            }
        }
    }
}

impl std::error::Error for ShellSessionError {}

impl ShellSession {
    pub(crate) fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            cwd: Arc::new(Mutex::new(security.workspace_dir.clone())),
            security,
        }
    }

    pub(crate) fn current_cwd(&self) -> Result<PathBuf, ShellSessionError> {
        self.cwd
            .lock()
            .map(|cwd| cwd.clone())
            .map_err(|_| ShellSessionError::Poisoned)
    }

    pub(crate) fn update_cwd_if_cd(
        &self,
        command: &str,
        succeeded: bool,
    ) -> Result<(), ShellSessionError> {
        if !succeeded {
            return Ok(());
        }

        let Some(target) = exact_relative_cd_target(command) else {
            return Ok(());
        };

        let current = self.current_cwd()?;
        let candidate = current.join(target);
        let resolved = candidate.canonicalize().unwrap_or(candidate);

        if !self.security.is_resolved_path_allowed(&resolved) {
            return Err(ShellSessionError::EscapesWorkspace(resolved));
        }
        if !resolved.is_dir() {
            return Err(ShellSessionError::NotDirectory(resolved));
        }

        *self.cwd.lock().map_err(|_| ShellSessionError::Poisoned)? = resolved;
        Ok(())
    }
}

fn exact_relative_cd_target(command: &str) -> Option<&Path> {
    let command = command.trim();
    if command.contains("&&")
        || command.contains(';')
        || command.contains('|')
        || command.contains('\n')
        || command.contains('\r')
    {
        return None;
    }

    let mut parts = command.split_whitespace();
    if parts.next()? != "cd" {
        return None;
    }
    let target = parts.next()?;
    if parts.next().is_some() || target.starts_with('~') {
        return None;
    }

    let path = Path::new(target);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }

    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn test_security(workspace: PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn exact_cd_updates_cwd_inside_workspace() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let subdir = tmp.path().join("crate");
        std::fs::create_dir(&subdir).expect("subdir");
        let session = ShellSession::new(test_security(tmp.path().to_path_buf()));

        session
            .update_cwd_if_cd("cd crate", true)
            .expect("cwd update");

        assert_eq!(session.current_cwd().expect("cwd"), subdir);
    }

    #[test]
    fn compound_cd_does_not_update_cwd() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let session = ShellSession::new(test_security(tmp.path().to_path_buf()));

        session
            .update_cwd_if_cd("cd crate && cargo test", true)
            .expect("compound command should be ignored");

        assert_eq!(session.current_cwd().expect("cwd"), tmp.path());
    }

    #[test]
    fn failed_cd_does_not_update_cwd() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let subdir = tmp.path().join("crate");
        std::fs::create_dir(&subdir).expect("subdir");
        let session = ShellSession::new(test_security(tmp.path().to_path_buf()));

        session
            .update_cwd_if_cd("cd crate", false)
            .expect("failed command should not update");

        assert_eq!(session.current_cwd().expect("cwd"), tmp.path());
    }

    #[test]
    fn escaping_cd_target_is_ignored_before_resolution() {
        assert!(exact_relative_cd_target("cd ..").is_none());
        assert!(exact_relative_cd_target("cd /tmp").is_none());
        assert!(exact_relative_cd_target("cd ~/src").is_none());
    }
}
