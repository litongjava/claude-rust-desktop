use anyhow::Result;
use deepseek_code_core::db::session_store::{CreateOptions, Stores};
use serde_json::json;

#[test]
fn combined_edit_preserves_history_on_failure_and_recovers_after_decision() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let id = r#"edit"with\escapes"#;
    let shard = stores.create(
        id,
        CreateOptions {
            title: Some("before".into()),
            ..Default::default()
        },
    )?;
    shard.write(|w| w.append("user", "keep me"))?;
    let fault = rusqlite::Connection::open(root.path().join("global.db"))?;
    fault.execute_batch(
        "CREATE TRIGGER reject_title BEFORE UPDATE OF title ON conversations
        BEGIN SELECT RAISE(ABORT,'injected title failure'); END",
    )?;
    let body = json!({"messages":[],"title":"after"});
    let err = shard.edit_conversation(&body).unwrap_err();
    assert!(format!("{err:#}").contains("injected title failure"));
    let (metadata, history) = stores.conversation_detail(id)?;
    assert_eq!(metadata.unwrap().title.as_deref(), Some("before"));
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["content"], "keep me");
    fault.execute_batch("DROP TRIGGER reject_title")?;
    drop(fault);
    let interrupted = stores.with_fault_hook(|phase| {
        anyhow::ensure!(
            format!("{phase:?}") != "EditMetadata",
            "interrupted after decision"
        );
        Ok(())
    });
    let e = interrupted
        .open_session(id)?
        .edit_conversation(&body)
        .unwrap_err();
    assert!(format!("{e:#}").contains("interrupted after decision"));
    drop((shard, interrupted, stores));
    let reopened = Stores::open(root.path())?;
    let (metadata, history) = reopened.conversation_detail(id)?;
    assert_eq!(metadata.unwrap().title.as_deref(), Some("after"));
    assert!(history.is_empty());
    Ok(())
}
