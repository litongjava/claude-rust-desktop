//! Bounded user-configured status-line commands with owned process-tree cleanup.
use anyhow::{ensure, Context, Result};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn run(script: &str, workspace: &Path, payload: &serde_json::Value) -> Result<String> {
    let payload = serde_json::to_vec(payload)?;
    ensure!(payload.len() <= 65536, "Status line input exceeds 64 KiB");
    let utf8_script = format!("[Console]::InputEncoding = [Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; {script}");
    let mut command = if cfg!(windows) {
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &utf8_script,
        ]);
        command
    } else {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", script]);
        command
    };
    command
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Starting status line command")?;
    let _tree =
        super::tool_hooks::ProcessTree::new(child.id().context("Status line process exited")?)?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut stdin = child
            .stdin
            .take()
            .context("Status line stdin unavailable")?;
        stdin.write_all(&payload).await?;
        drop(stdin);
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("Status line stdout unavailable")?
            .take(4097)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 4096, "Status line output exceeds 4096 bytes");
        ensure!(child.wait().await?.success(), "Status line command failed");
        Ok(String::from_utf8(bytes)?
            .chars()
            .filter(|ch| !ch.is_control())
            .collect())
    })
    .await
    .context("Status line command timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn status_command_receives_json_and_is_bounded() {
        let cwd = tempfile::tempdir().unwrap();
        let echo = if cfg!(windows) {
            "$x = [Console]::In.ReadToEnd() | ConvertFrom-Json; [Console]::Write($x.session_id)"
        } else {
            "cat"
        };
        assert!(run(
            echo,
            cwd.path(),
            &serde_json::json!({"session_id":"fixture中文😀"})
        )
        .await
        .unwrap()
        .contains("fixture中文😀"));
        let excess = if cfg!(windows) {
            "[Console]::Write(('x' * 5000))"
        } else {
            "head -c 5000 /dev/zero"
        };
        assert!(run(excess, cwd.path(), &serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("4096"));
    }

    #[tokio::test]
    async fn status_command_timeout_and_input_limit_are_enforced() {
        let cwd = tempfile::tempdir().unwrap();
        let script = if cfg!(windows) {
            "Start-Sleep -Seconds 10"
        } else {
            "sleep 10"
        };
        let start = std::time::Instant::now();
        assert!(run(script, cwd.path(), &serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(run(
            "must-not-run",
            cwd.path(),
            &serde_json::json!({"data":"x".repeat(65537)})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("64 KiB"));
    }
}
