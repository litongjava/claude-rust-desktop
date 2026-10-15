//! Startup failures must remain parseable by automation without contacting an API.
use serde_json::Value;
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

async fn execute(args: &[&str]) -> std::process::Output {
    let temp = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_claude-code"));
    command
        .arg("--data-dir")
        .arg(temp.path().join("data"))
        .arg("--path")
        .arg(temp.path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "API_BASE_URL",
        "OPENAI_BASE_URL",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_MODEL",
        "RUST_LOG",
    ] {
        command.env_remove(key);
    }
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("CLI hung")
        .unwrap()
}

fn assert_failure(output: std::process::Output, message: &str) {
    assert!(!output.status.success());
    // Parsing the entire stdout also rejects duplicate final result records.
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(result["type"], "result");
    assert_eq!(result["session_id"], Value::Null);
    assert_eq!(result["result"], "");
    assert_eq!(result["is_error"], true);
    assert!(result["error"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains(message));
}

#[tokio::test]
async fn missing_prompt_is_one_json_failure() {
    for format in ["json", "stream-json"] {
        assert_failure(
            execute(&["--print", "--output-format", format]).await,
            "prompt",
        );
    }
}

#[tokio::test]
async fn missing_provider_is_one_json_failure() {
    for format in ["json", "stream-json"] {
        assert_failure(
            execute(&[
                "--provider",
                "missing-provider-fixture",
                "--output-format",
                format,
                "query",
                "-p",
                "hello",
            ])
            .await,
            "provider",
        );
    }
}

#[tokio::test]
async fn missing_resume_is_one_json_failure() {
    for format in ["json", "stream-json"] {
        assert_failure(
            execute(&[
                "--resume",
                "missing-session-fixture",
                "--output-format",
                format,
                "query",
                "-p",
                "hello",
            ])
            .await,
            "session",
        );
    }
}

#[tokio::test]
async fn management_and_text_failures_keep_stdout_empty() {
    for args in [
        vec!["--print"],
        vec![
            "--output-format",
            "json",
            "sessions",
            "show",
            "missing-session-fixture",
        ],
    ] {
        let result = execute(&args).await;
        assert!(!result.status.success());
        assert!(
            result.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
}
