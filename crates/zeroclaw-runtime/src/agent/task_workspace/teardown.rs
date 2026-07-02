use std::path::Path;

use tokio::process::Command;

pub(crate) async fn release_worktree(workspace_dir: &Path, root: &Path) -> anyhow::Result<()> {
    let remove = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(root)
        .current_dir(workspace_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !remove.status.success() {
        anyhow::bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&remove.stderr).trim()
        );
    }

    let prune = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(workspace_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !prune.status.success() {
        anyhow::bail!(
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&prune.stderr).trim()
        );
    }

    Ok(())
}
