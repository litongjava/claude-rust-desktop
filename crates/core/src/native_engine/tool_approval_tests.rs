use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn typed_approval_validates_before_committing_and_uses_effective_input() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    for case in [
        "replacement",
        "empty",
        "invalid",
        "outside",
        "interrupt",
        "plan",
        "deny",
    ] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (tx, mut events) = mpsc::channel(16);
        let mut executor = ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        );
        let manager = Arc::new(crate::permissions::PermissionManager::new(Arc::new(
            crate::permissions::AuditLogger::new(16),
        )));
        manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
        executor = executor.with_permission_manager(manager.clone());
        let mailbox = executor.permission_mailbox.clone();
        let waiters = executor.get_answer_waiters();
        let task = tokio::spawn(async move {
            let result = executor
                .execute_ask_user_confirmation(
                    "Write",
                    &json!({"file_path":"original.txt","content":"original"}),
                    "write-1",
                    "approve",
                )
                .await;
            (
                result,
                executor.ensure_permission_continuation().is_err(),
                executor.approved_call.is_none(),
            )
        });
        let EngineEvent::ToolPermission { request_id, .. } = events.recv().await.unwrap() else {
            panic!()
        };
        if case == "plan" {
            manager.set_mode(crate::permissions::PermissionMode::PlanMode);
        }
        if case == "deny" {
            manager.set_tool_permission(
                "Write",
                crate::permissions::ToolPermission {
                    tool_name: "Write".into(),
                    allowed: false,
                    requires_confirmation: false,
                    scope: crate::permissions::PermissionScope::Global,
                    level: crate::permissions::PermissionLevel::Execute,
                },
            );
        }
        let committed = Arc::new(AtomicBool::new(false));
        let flag = committed.clone();
        let updated = match case {
            "replacement" => json!({"file_path":"replacement.txt","content":"replacement"}),
            "empty" | "plan" | "deny" => json!({}),
            "outside" => {
                json!({"file_path":outside.path().join("outside.txt"),"content":"outside"})
            }
            _ => json!({"file_path":"invalid.txt"}),
        };
        let response = if case == "interrupt" {
            PermissionResponse::Deny {
                message: "STOP_SENTINEL".into(),
                interrupt: true,
            }
        } else {
            PermissionResponse::Allow {
                updated_input: Some(updated),
                commit: Some(Box::new(move || {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                })),
            }
        };
        crate::native_engine::permission_response::respond(&mailbox, &request_id, response)
            .await
            .unwrap();
        let ((actual, output, failed), interrupted, cleared) = task.await.unwrap();
        let allowed = matches!(case, "replacement" | "empty");
        assert_eq!(failed, !allowed, "{case}: {output}");
        assert_eq!(committed.load(Ordering::SeqCst), allowed);
        assert_eq!(interrupted, case == "interrupt");
        assert!(cleared);
        if allowed {
            let context = crate::permissions::PermissionContext {
                tool_name: "Write".into(),
                tool_input: json!({"file_path":"another.txt","content":"no grant"}),
                conversation_id: String::new(),
                user_id: None,
                workspace_path: Some(root.path().to_string_lossy().into_owned()),
            };
            assert!(
                matches!(
                    manager.check_permission(&context),
                    PermissionResult::RequiresConfirmation(_)
                ),
                "one-shot approval must not grant other calls"
            );
        }
        assert!(mailbox.lock().await.is_empty());
        assert!(waiters.lock().await.is_empty());
        assert!(!outside.path().join("outside.txt").exists());
        assert_eq!(root.path().join("original.txt").exists(), case == "empty");
        if case == "replacement" {
            assert_eq!(actual["file_path"], "replacement.txt");
            assert_eq!(
                std::fs::read_to_string(root.path().join("replacement.txt")).unwrap(),
                "replacement"
            );
        }
        assert!(crate::native_engine::permission_response::respond(
            &mailbox,
            &request_id,
            PermissionResponse::Deny {
                message: "late".into(),
                interrupt: false
            }
        )
        .await
        .is_err());
    }
}

#[test]
fn approved_paths_resolve_symlinks_before_parent_traversal() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cwd = root.path().to_str().unwrap();
    assert!(validate_workspace_path("missing/../../outside.txt", cwd, &[]).is_err());
    // Windows resolves missing/.. to the existing parent; Unix rejects the unresolved ancestor.
    #[cfg(unix)]
    assert!(validate_workspace_path("missing/../inside.txt", cwd, &[]).is_err());
    assert!(validate_workspace_path("missing/nested/file.txt", cwd, &[]).is_ok());
    std::fs::create_dir(outside.path().join("nested")).unwrap();
    let link = root.path().join("link");
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path().join("nested"))
        .output()
        .unwrap()
        .status
        .success());
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path().join("nested"), &link).unwrap();
    #[cfg(unix)]
    assert!(validate_workspace_path("link/../escape.txt", cwd, &[]).is_err());
    assert!(validate_workspace_path("link/../../escape.txt", cwd, &[]).is_err());
    assert!(validate_workspace_path("link/new/file.txt", cwd, &[]).is_err());
    assert!(
        validate_workspace_path("link/new/file.txt", cwd, &[outside.path().to_path_buf()]).is_ok()
    );
}
