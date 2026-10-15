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
        Ok(render_line(&String::from_utf8(bytes)?))
    })
    .await
    .context("Status line command timed out")?
}

/// Keep configured colors without allowing cursor, clipboard or terminal-mode
/// sequences. Removing only ESC leaves visible fragments such as `[32m`.
fn render_line(text: &str) -> String {
    enum State {
        Text,
        Escape,
        Intermediate,
        Csi(String),
        String,
        StringEscape,
    }
    let mut state = State::Text;
    let mut output = String::new();
    let mut styled = false;
    for ch in text.chars() {
        state = match state {
            State::Text => match ch {
                '\u{1b}' => State::Escape,
                '\u{9b}' => State::Csi(String::new()),
                '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => State::String,
                '\n' | '\r' | '\t' => {
                    if !output.ends_with(' ') {
                        output.push(' ');
                    }
                    State::Text
                }
                _ => {
                    if !ch.is_control()
                        && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                    {
                        output.push(ch);
                    }
                    State::Text
                }
            },
            State::Escape => match ch {
                '[' => State::Csi(String::new()),
                ']' | 'P' | 'X' | '^' | '_' => State::String,
                ' '..='/' => State::Intermediate,
                '\u{1b}' => State::Escape,
                _ => State::Text,
            },
            State::Intermediate => {
                if matches!(ch, ' '..='/') {
                    State::Intermediate
                } else {
                    State::Text
                }
            }
            State::Csi(mut parameters) => {
                if matches!(ch, '@'..='~') {
                    if ch == 'm'
                        && parameters.len() <= 128
                        && parameters
                            .bytes()
                            .all(|b| b.is_ascii_digit() || b == b';' || b == b':')
                    {
                        output.push_str("\x1b[");
                        output.push_str(&parameters);
                        output.push('m');
                        styled = true;
                    }
                    State::Text
                } else if ch == '\u{1b}' {
                    State::Escape
                } else {
                    // Once oversized, retain an invalid marker until the final byte.
                    if parameters.len() <= 128 {
                        parameters.push(ch);
                    }
                    State::Csi(parameters)
                }
            }
            State::String => match ch {
                '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::String,
            },
            State::StringEscape => match ch {
                '\\' | '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::String,
            },
        };
    }
    let mut output = output.trim().to_owned();
    if styled {
        output.push_str("\x1b[0m");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn colors_survive_without_terminal_commands_or_style_leaks() {
        assert_eq!(
            render_line("\x1b[38;2;20;200;90m模型 😀"),
            "\x1b[38;2;20;200;90m模型 😀\x1b[0m"
        );
        assert_eq!(
            render_line(
                "one\x1b[2J\x1b[?1049htwo\x1b]52;c;clipboard\x07\x1bPpayload\x1b\\\x1b(B\r\nthree"
            ),
            "onetwo three"
        );
        assert_eq!(
            render_line("\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(render_line("safe\x1b]unfinished"), "safe");
        assert_eq!(
            render_line("\u{9b}32mgreen\u{9b}0m"),
            "\x1b[32mgreen\x1b[0m\x1b[0m"
        );
        assert_eq!(
            render_line(&format!("\x1b[{}mplain", "1".repeat(129))),
            "plain"
        );
    }
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
        let color = if cfg!(windows) {
            "[Console]::Write(([char]27).ToString() + '[32mready')"
        } else {
            "printf '\\033[32mready'"
        };
        assert_eq!(
            run(color, cwd.path(), &serde_json::json!({}))
                .await
                .unwrap(),
            "\x1b[32mready\x1b[0m"
        );
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
