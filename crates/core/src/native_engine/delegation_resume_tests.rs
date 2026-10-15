use super::*;
use crate::native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider};
use crate::services::session_messages as mail;
use axum::{extract::State, routing::post, Json, Router};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Endpoint {
    requests: Mutex<Vec<Value>>,
    block: AtomicBool,
    json_mode: AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
async fn endpoint() -> (String, Arc<Endpoint>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Endpoint::default());
    let app=Router::new().route("/v1/chat/completions",post(|State(state):State<Arc<Endpoint>>,Json(body):Json<Value>|async move {
        state.requests.lock().unwrap().push(body);
        if state.block.swap(false,Ordering::SeqCst) {state.started.notify_one();state.release.notified().await;}
        ([("content-type","text/event-stream")],format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":if state.json_mode.load(Ordering::Relaxed){"{\"ok\":true}"}else{"child complete"}},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})))
    })).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state, server)
}
fn owner(
    context: &ServiceContext,
    id: &str,
    cwd: &std::path::Path,
    url: &str,
    registry: OwnedChildren,
) -> ToolLoopExecutor {
    let provider=ResolvedProvider {
        provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":url,"api_key":"memory-only-resume-secret","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(500);
    ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"owner next turn"})],
        None,
        128,
        tx,
        cwd.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id(id.into())
    .with_search_config_path(context.paths.data_dir.join("settings.json"))
    .with_owned_children(registry)
}
fn fixture() -> (
    tempfile::TempDir,
    ServiceContext,
    SessionStore,
    String,
    std::path::PathBuf,
) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    let context = mail::context(&temp.path().join("data")).unwrap();
    let store = SessionStore::new(context.db.clone());
    let parent = store.create(&cwd, "fixture", "fixture").unwrap();
    (temp, context, store, parent, cwd)
}
#[tokio::test]
async fn explicit_resume_sendmessage_and_safe_boundary_keep_original_child_provider() {
    let (_temp, context, store, parent, cwd) = fixture();
    let (a, api_a, server_a) = endpoint().await;
    let (b, api_b, server_b) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let first = owner(&context, &parent, &cwd, &a, registry.clone());
    let result = first
        .execute_delegated(&json!({"prompt":"original child"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap().to_owned();
    drop(first);
    {
        let retained = registry.lock().unwrap();
        let entry = retained.get(&child).unwrap();
        assert!(entry.executor.messages.is_empty());
        assert!(entry.executor.message_inbox.is_none());
        assert!(entry.executor.execution_lease.is_none());
    }
    let mut next = owner(&context, &parent, &cwd, &b, registry.clone());
    let resumed = next
        .execute_delegated(&json!({"resume":child,"prompt":"explicit continuation"}))
        .await
        .unwrap();
    assert_eq!(resumed["session_id"], child);
    assert_eq!(resumed["resumed_in_owner_turn"], true);
    let (_, output, failed) = next
        .execute_tool_call(
            "SendMessage",
            &json!({"operation":"send","to":child,"content":"MESSAGE_WAKE_MARKER"}),
            "send-owned",
        )
        .await;
    assert!(!failed, "{output}");
    assert!(output.contains("resumed_in_owner_turn"), "{output}");
    assert_eq!(
        mail::inbox(&context, &child, false).unwrap()[0].state,
        "context_committed"
    );
    let peer = store.fork(&parent).unwrap();
    mail::send(&context, &peer, &child, "BOUNDARY_WAKE_MARKER").unwrap();
    next.execute().await.unwrap();
    assert_eq!(api_a.requests.lock().unwrap().len(), 4);
    assert_eq!(api_b.requests.lock().unwrap().len(), 1);
    let requests = api_a.requests.lock().unwrap();
    assert!(requests[1].to_string().contains("original child"));
    assert!(requests[1].to_string().contains("explicit continuation"));
    assert!(requests[2].to_string().contains("MESSAGE_WAKE_MARKER"));
    assert!(requests[3].to_string().contains("BOUNDARY_WAKE_MARKER"));
    drop(requests);
    assert_eq!(
        store
            .history(&child)
            .unwrap()
            .iter()
            .filter(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("BOUNDARY_WAKE_MARKER")))
            .count(),
        1
    );
    let resumed: Value = context
        .setting(&format!("delegation.{child}"))
        .unwrap()
        .unwrap();
    assert_eq!(resumed["status"], "completed");
    server_a.abort();
    server_b.abort();
}
#[tokio::test]
async fn resume_cancellation_releases_ownership_and_never_automatically_restarts() {
    let (_temp, context, _store, parent, cwd) = fixture();
    let (a, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let first = owner(&context, &parent, &cwd, &a, registry.clone());
    let result = first
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap().to_owned();
    drop(first);
    api.block.store(true, Ordering::SeqCst);
    let next = owner(&context, &parent, &cwd, &a, registry.clone());
    let child_copy = child.clone();
    let task = tokio::spawn(async move {
        next.execute_delegated(&json!({"resume":child_copy,"prompt":"wait"}))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), api.started.notified())
        .await
        .unwrap();
    let other = owner(&context, &parent, &cwd, &a, registry.clone());
    assert!(other
        .resume_owned_child(&child, Some("duplicate"), false)
        .await
        .is_err());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let saved: Value = context
        .setting(&format!("delegation.{child}"))
        .unwrap()
        .unwrap();
    assert_eq!(saved["status"], "cancelled");
    assert!(!mail::receiver_active(&context.paths.data_dir, &child).unwrap());
    drop(mail::ExecutionLease::acquire(&context.paths.data_dir, &child).unwrap());
    mail::send(&context, &parent, &child, "must remain queued").unwrap();
    other.resume_pending_owned_children().await;
    assert!(other
        .resume_owned_child(&child, Some("explicit does not rearm cancellation"), false)
        .await
        .is_err());
    assert_eq!(api.requests.lock().unwrap().len(), 2);
    assert_eq!(mail::pending_count(&context, &child).unwrap(), 1);
    api.release.notify_one();
    server.abort();
}
#[tokio::test]
async fn foreign_owner_changed_policy_changed_trust_and_lost_configuration_make_no_request() {
    let (_temp, context, store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let first = owner(&context, &parent, &cwd, &url, registry.clone());
    let result = first
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap().to_owned();
    let foreign = store.fork(&parent).unwrap();
    assert!(owner(&context, &foreign, &cwd, &url, registry.clone())
        .resume_owned_child(&child, Some("foreign"), false)
        .await
        .is_err());
    let mut changed = owner(&context, &parent, &cwd, &url, registry.clone());
    changed.allowed_tools = Some(vec!["Read".into()]);
    assert!(changed
        .resume_owned_child(&child, Some("policy changed"), false)
        .await
        .is_err());
    let missing = owner(&context, &parent, &cwd, &url, Default::default());
    assert!(missing
        .resume_owned_child(&child, Some("lost"), false)
        .await
        .unwrap_err()
        .to_string()
        .contains("needs_resume"));
    context.db.with_conn(|conn|conn.execute("UPDATE project_folders SET trusted=0 WHERE project_id=(SELECT project_id FROM conversations WHERE id=?1)",[&child])).unwrap().unwrap();
    assert!(first
        .resume_owned_child(&child, Some("trust revoked"), false)
        .await
        .is_err());
    assert_eq!(api.requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn typed_research_stages_are_not_registered_for_automatic_resume() {
    let (_temp, context, _store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    api.json_mode.store(true, Ordering::Relaxed);
    let registry: OwnedChildren = Default::default();
    let executor = owner(&context, &parent, &cwd, &url, registry.clone());
    let result=executor.execute_delegated_typed(&json!({"prompt":"typed phase"}),Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false})),Some(vec!["Read".into()])).await.unwrap();
    assert_eq!(result["resume_available"], false);
    assert!(registry.lock().unwrap().is_empty());
    assert!(executor
        .resume_owned_child(
            result["session_id"].as_str().unwrap(),
            Some("must not restart fixed phase"),
            false
        )
        .await
        .is_err());
    assert_eq!(api.requests.lock().unwrap().len(), 1);
    fn no_secret(path: &std::path::Path) {
        for item in std::fs::read_dir(path).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                no_secret(&path);
            } else {
                assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap())
                    .contains("memory-only-resume-secret"));
            }
        }
    }
    no_secret(&context.paths.data_dir);
    server.abort();
}

#[tokio::test]
async fn unchanged_mode_keeps_resume_but_mode_away_and_back_invalidates_snapshot() {
    use crate::permissions::PermissionMode;
    let (_temp, context, _store, parent, cwd) = fixture();
    let (url, api, server) = endpoint().await;
    let registry: OwnedChildren = Default::default();
    let permissions = crate::runtime::default_permissions();
    permissions.set_mode(PermissionMode::AskPermissions);
    let executor =
        owner(&context, &parent, &cwd, &url, registry).with_permission_manager(permissions.clone());
    let result = executor
        .execute_delegated(&json!({"prompt":"original"}))
        .await
        .unwrap();
    let child = result["session_id"].as_str().unwrap();
    permissions.set_mode(PermissionMode::AskPermissions);
    executor
        .resume_owned_child(child, Some("same mode is stable"), false)
        .await
        .unwrap();
    permissions.set_mode(PermissionMode::PlanMode);
    permissions.set_mode(PermissionMode::AskPermissions);
    assert!(executor
        .resume_owned_child(child, Some("mode changed while idle"), false)
        .await
        .unwrap_err()
        .to_string()
        .contains("policy changed"));
    assert_eq!(api.requests.lock().unwrap().len(), 2);
    server.abort();
}

#[test]
fn private_session_execution_lease_rejects_other_engine_until_drop() {
    let (_temp, context, _store, parent, _cwd) = fixture();
    context
        .db
        .with_conn(|conn| {
            conn.execute(
                "UPDATE conversations SET workspace_mode='chat',project_id=NULL WHERE id=?1",
                [&parent],
            )
        })
        .unwrap()
        .unwrap();
    let lease = mail::ExecutionLease::acquire(&context.paths.data_dir, &parent)
        .unwrap()
        .unwrap();
    assert!(mail::ExecutionLease::acquire(&context.paths.data_dir, &parent).is_err());
    drop(lease);
    assert!(
        mail::ExecutionLease::acquire(&context.paths.data_dir, &parent)
            .unwrap()
            .is_some()
    );
}
