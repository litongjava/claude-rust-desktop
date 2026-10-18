use super::*;
use crate::services::hook_callbacks::{self, Host};
use futures::future::BoxFuture;
use std::sync::Arc;

#[test]
fn replacement_truthiness_matches_reference_and_is_post_only() {
    for value in [Value::Null, json!(false), json!(0), json!(-0.0), json!("")] {
        let outcome = parse_json_output(HookEvent::PostToolUse, json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":value}})).unwrap();
        assert!(outcome.updated_mcp_tool_output.is_none());
    }
    for value in [
        json!([]),
        json!({}),
        json!(true),
        json!(3),
        json!("redacted"),
    ] {
        let outcome = parse_json_output(HookEvent::PostToolUse, json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":value}})).unwrap();
        assert_eq!(outcome.updated_mcp_tool_output, Some(value));
    }
    assert!(parse_json_output(HookEvent::PreToolUse, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedMCPToolOutput":"not allowed"}})).is_err());
}

#[tokio::test]
async fn delayed_command_tail_cannot_overwrite_newer_callback_output() {
    struct Callback {
        root: PathBuf,
    }
    impl Host for Callback {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            input: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                assert_eq!(
                    input["tool_response"],
                    json!([{"type":"text","text":"original"}])
                );
                tokio::time::timeout(Duration::from_secs(15), async {
                    while !self.root.join("waiting").exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?;
                std::fs::write(self.root.join("release"), "yes")?;
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":"callback wins","additionalContext":"kept"}}),
                )
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Callback {
        root: root.path().to_path_buf(),
    });
    let callbacks = hook_callbacks::validate(
        &json!({"PostToolUse":[{"hookCallbackIds":["post"],"timeout":20}]}),
    )
    .unwrap();
    let _guard = hook_callbacks::install(&session, callbacks, host.clone());
    let (shell, first, tail) = if cfg!(windows) {
        ("powershell", "Write-Output '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedMCPToolOutput\":\"earlier command\"}}'", "[IO.File]::WriteAllText('waiting','yes'); while (!(Test-Path -LiteralPath 'release')) { Start-Sleep -Milliseconds 10 }; Write-Output '{}'")
    } else {
        ("sh", "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedMCPToolOutput\":\"earlier command\"}}'", "printf yes > waiting; while [ ! -f release ]; do sleep 0.01; done; printf '{}'")
    };
    let mut runner = HookRunner::default().with_session_callbacks(&session);
    runner.workspace = root.path().to_path_buf();
    runner.hooks = [first, tail]
        .into_iter()
        .map(|command| ActiveHook {
            event: HookEvent::PostToolUse,
            matcher: String::new(),
            command: command.into(),
            http: None,
            timeout: 20,
            source: root.path().into(),
            shell: Some(shell.into()),
            if_condition: None,
            once: false,
            status_message: None,
        })
        .collect();
    let outcome = runner
        .run(
            HookEvent::PostToolUse,
            &session,
            json!({"tool_name":"fixture","tool_response":[{"type":"text","text":"original"}]}),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.updated_mcp_tool_output,
        Some(json!("callback wins"))
    );
    assert_eq!(outcome.additional_context, vec!["kept"]);
}

#[tokio::test]
async fn callbacks_observe_original_response_and_last_completion_wins() {
    struct Callback {
        first: tokio::sync::Notify,
    }
    impl Host for Callback {
        fn invoke<'a>(
            &'a self,
            id: &'a str,
            input: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                assert_eq!(input["tool_response"], "original");
                if id == "last" {
                    self.first.notified().await;
                    tokio::task::yield_now().await;
                } else {
                    self.first.notify_one();
                }
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":id}}),
                )
            })
        }
    }
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Callback {
        first: Default::default(),
    });
    let callbacks =
        hook_callbacks::validate(&json!({"PostToolUse":[{"hookCallbackIds":["last","first"]}]}))
            .unwrap();
    let _guard = hook_callbacks::install(&session, callbacks, host.clone());
    let runner = HookRunner::default().with_session_callbacks(&session);
    let outcome = runner
        .run(
            HookEvent::PostToolUse,
            &session,
            json!({"tool_name":"fixture","tool_response":"original"}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.updated_mcp_tool_output, Some(json!("last")));
}
