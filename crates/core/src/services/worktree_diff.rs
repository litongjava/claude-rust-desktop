//! Read-only, bounded Git diff snapshots for terminal and Desktop adapters.
use anyhow::{ensure, Context, Result};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

async fn git(workspace: &Path, args: &[&str]) -> Result<(bool, String)> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(["--no-pager", "-C"])
        .arg(workspace)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Starting git diff")?;
    let _tree = super::tool_hooks::ProcessTree::new(child.id().context("Git exited")?)?;
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("Git stdout unavailable")?
            .take(262145)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 262144 {
            return Ok((
                true,
                format!(
                    "{}\n[Diff truncated at 256 KiB]",
                    String::from_utf8_lossy(&bytes[..262144])
                ),
            ));
        }
        Ok((
            child.wait().await?.success(),
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    })
    .await
    .context("Git diff timed out")?
}

pub async fn snapshot(workspace: &Path, staged: bool) -> Result<String> {
    ensure!(
        git(workspace, &["rev-parse", "--show-toplevel"]).await?.0,
        "Diff requires a Git working tree"
    );
    let has_head = git(workspace, &["rev-parse", "--verify", "HEAD"]).await?.0;
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--color=never",
        "--submodule=short",
    ];
    if staged {
        args.push("--cached");
    } else if has_head {
        args.push("HEAD");
    }
    let (ok, mut text) = git(workspace, &args).await?;
    ensure!(ok, "Git could not produce a diff");
    if !staged && !has_head {
        args.push("--cached");
        let (ok, cached) = git(workspace, &args).await?;
        ensure!(ok, "Git could not produce the staged diff");
        text.push_str(&cached);
    }
    if !staged {
        let (ok, untracked) = git(
            workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )
        .await?;
        ensure!(ok, "Git could not list untracked files");
        for path in untracked.split('\0').filter(|path| !path.is_empty()) {
            text.push_str(&format!("\nUntracked file: {path}\n"));
        }
    }
    if text.is_empty() {
        text = "No changes.".into();
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn includes_staged_and_working_edits_without_external_diff() {
        let root = tempfile::tempdir().unwrap();
        for args in [
            &["init"][..],
            &["config", "user.name", "Fixture"],
            &["config", "user.email", "fixture@example.invalid"],
        ] {
            assert!(git(root.path(), args).await.unwrap().0);
        }
        std::fs::write(root.path().join("file.txt"), "before\n").unwrap();
        assert!(git(root.path(), &["add", "file.txt"]).await.unwrap().0);
        assert!(
            git(root.path(), &["commit", "-m", "fixture"])
                .await
                .unwrap()
                .0
        );
        std::fs::write(root.path().join("file.txt"), "staged\n").unwrap();
        assert!(git(root.path(), &["add", "file.txt"]).await.unwrap().0);
        std::fs::write(root.path().join("file.txt"), "working\n").unwrap();
        std::fs::write(root.path().join("new.txt"), "new\n").unwrap();
        let full = snapshot(root.path(), false).await.unwrap();
        assert!(full.contains("+working"));
        assert!(full.contains("Untracked file: new.txt"));
        let staged = snapshot(root.path(), true).await.unwrap();
        assert!(staged.contains("+staged"));
        assert!(!staged.contains("+working"));
    }
}
