//! A session that searched a tool before it ended must find it again after the
//! session is reopened, and no other session may.

use super::*;
use crate::db::session_store::Stores;
use crate::runtime::{RuntimePaths, SessionStore};

fn restore_executor(paths: &RuntimePaths, messages: Vec<Value>) -> ToolLoopExecutor {
    let _ = crate::config::OrchestrationConfig::init_global();
    let (tx, _rx) = mpsc::channel(64);
    ToolLoopExecutor::new(
        ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        },
        messages,
        None,
        256,
        tx,
        paths.data_dir.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(paths.data_dir.join("config.json"))
}

/// Stores one assistant tool call and its result the way the host does.
fn record_call(
    store: &SessionStore<Stores>,
    session: &str,
    assistant: &str,
    id: &str,
    name: &str,
    output: &str,
) {
    store
        .db
        .open_session(session)
        .unwrap()
        .write(|w| {
            w.stream_tools(
                assistant,
                "",
                &[crate::db::message_repo::ToolCallRow {
                    id: id.to_owned(),
                    message_id: assistant.to_owned(),
                    name: name.to_owned(),
                    input: Some("{}".into()),
                    output: Some(output.to_owned()),
                    is_error: false,
                    sort_order: 0,
                }],
                &Default::default(),
            )
        })
        .unwrap();
}

fn request_names(executor: &ToolLoopExecutor) -> Vec<String> {
    executor
        .tool_definitions()
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// The load order survives a resume: both the message history and the durable
/// rows append the loaded tools in the order their ToolLoad calls ran, not in
/// alphabetical order, so the request prefix after the resident tools stays
/// what the provider cached.
#[test]
fn a_resumed_session_keeps_the_loaded_tool_order() {
    let _ = crate::config::OrchestrationConfig::init_global();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(Arc::new(paths.open_stores().unwrap()));
    let session = store.create(dir.path(), "fixture", "fixture").unwrap();

    store.append(&session, "user", "Load both").unwrap();
    for (name, suffix) in [("WebFetch", "web"), ("ListDir", "dir")] {
        let assistant = uuid::Uuid::new_v4().to_string();
        record_call(
            &store,
            &session,
            &assistant,
            suffix,
            "ToolLoad",
            &json!({"success":true,"loaded_tools":[name],"tools":[]}).to_string(),
        );
    }

    let mut from_messages = restore_executor(
        &paths,
        store.model_history(&session, &ApiFormat::OpenAI).unwrap(),
    );
    from_messages.restore_loaded_tools();
    let sent = request_names(&from_messages);
    assert_eq!(&sent[sent.len() - 2..], ["WebFetch", "ListDir"], "{sent:?}");

    // The durable rows restore the same order even when the submitted display
    // history is empty.
    let mut from_rows = restore_executor(&paths, vec![])
        .with_conv_id(session.clone())
        .with_executor_history(store.db.executor_history(&session).unwrap())
        .unwrap();
    from_rows.restore_persisted_loaded_tools().unwrap();
    let sent = request_names(&from_rows);
    assert_eq!(&sent[sent.len() - 2..], ["WebFetch", "ListDir"], "{sent:?}");
}

#[test]
fn a_persisted_search_still_counts_after_a_restart_and_only_in_that_session() {
    let _ = crate::config::OrchestrationConfig::init_global();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(Arc::new(paths.open_stores().unwrap()));
    let session = store.create(dir.path(), "fixture", "fixture").unwrap();
    let marker = json!({"success":true,"loaded_tools":["ListDir"],"tools":[]}).to_string();
    let assistant = uuid::Uuid::new_v4().to_string();

    store
        .append(&session, "user", "List the fixture files")
        .unwrap();
    store.append(&session, "assistant", "").unwrap();
    record_call(
        &store, &session, &assistant, "search-1", "ToolLoad", &marker,
    );
    store.append(&session, "user", "Continue").unwrap();

    // Reopening the session restores the load from the stored call and result.
    let mut resumed = restore_executor(
        &paths,
        store.model_history(&session, &ApiFormat::OpenAI).unwrap(),
    );
    resumed.restore_loaded_tools();
    assert!(request_names(&resumed).contains(&"ListDir".to_owned()));

    // The Anthropic history shape restores it the same way.
    let mut anthropic = restore_executor(
        &paths,
        store
            .model_history(&session, &ApiFormat::Anthropic)
            .unwrap(),
    );
    anthropic.restore_loaded_tools();
    assert!(request_names(&anthropic).contains(&"ListDir".to_owned()));

    // A different session shares nothing with it.
    let other = store.create(dir.path(), "fixture", "fixture").unwrap();
    store
        .append(&other, "user", "List the fixture files")
        .unwrap();
    let mut fresh = restore_executor(
        &paths,
        store.model_history(&other, &ApiFormat::OpenAI).unwrap(),
    );
    fresh.restore_loaded_tools();
    assert!(!request_names(&fresh).contains(&"ListDir".to_owned()));

    // A failed search in the past grants nothing.
    let failed_session = store.create(dir.path(), "fixture", "fixture").unwrap();
    let failed_assistant = uuid::Uuid::new_v4().to_string();
    store.append(&failed_session, "user", "List").unwrap();
    store.append(&failed_session, "assistant", "").unwrap();
    record_call(
        &store,
        &failed_session,
        &failed_assistant,
        "search-2",
        "ToolLoad",
        &json!({"success":false,"loaded_tools":[],"tools":[]}).to_string(),
    );
    let mut rejected = restore_executor(
        &paths,
        store
            .model_history(&failed_session, &ApiFormat::OpenAI)
            .unwrap(),
    );
    rejected.restore_loaded_tools();
    assert!(!request_names(&rejected).contains(&"ListDir".to_owned()));

    // Compression can drop the record; the search then simply runs again.
    let summarized: Vec<Value> = store
        .model_history(&session, &ApiFormat::OpenAI)
        .unwrap()
        .into_iter()
        .filter(|message| message["role"] != "tool")
        .collect();
    let mut compacted = restore_executor(&paths, summarized);
    compacted.restore_loaded_tools();
    assert!(!request_names(&compacted).contains(&"ListDir".to_owned()));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (_, output, failed) = compacted
            .execute_tool_call("ToolLoad", &json!({"names":["ListDir"]}), "search-3")
            .await;
        assert!(!failed, "{output}");
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["loaded_tools"], json!(["ListDir"]));
        // The stored result carries the real schema, not a placeholder.
        assert!(value["tools"][0]["input_schema"].is_object(), "{output}");
        // And it becomes callable from the next request on.
        compacted.exposed_tools = Some(request_names(&compacted).into_iter().collect());
        let (_, output, failed) = compacted
            .execute_tool_call("ListDir", &json!({"path":"."}), "list")
            .await;
        assert!(!failed, "{output}");
    });
}

#[test]
fn executor_saves_hook_adjusted_mcp_to_its_admitted_occurrence() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let paths = RuntimePaths::new(dir.path().join("data"))?;
    let stores = paths.open_stores()?;
    let session = stores.create("owner", Default::default())?;
    let assistant = session.write(|w| w.append("assistant", ""))?;
    let executor = restore_executor(&paths, vec![])
        .with_conv_id("owner".into())
        .with_executor_history(stores.executor_history("owner")?)?
        .with_parent_message_id(assistant.clone());
    let mut content =
        McpOutput::from_transport(&json!({"content":"original","_meta":{"trace":17}}))?;
    content.replace(&json!("after hook"));
    executor.finish_mcp_output("repeat", "tool", "display", false, Some(content.clone()))?;
    let (value, metadata, _) = session
        .read(|r| r.mcp(&format!("{assistant}:repeat"), "display"))?
        .unwrap();
    assert_eq!(value, json!("after hook"));
    assert_eq!(metadata["_meta"]["trace"], 17);
    session.delete()?;
    stores.create("owner", Default::default())?;
    assert!(executor
        .finish_mcp_output("repeat", "tool", "display", false, Some(content))
        .is_err());
    assert!(stores
        .open_session("owner")?
        .read(|r| r.mcp(&format!("{assistant}:repeat"), "display"))?
        .is_none());
    Ok(())
}
