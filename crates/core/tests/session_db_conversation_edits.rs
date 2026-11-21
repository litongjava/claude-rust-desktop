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

#[test]
fn sync_reconciles_changed_user_display_and_preserves_unchanged_parts() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let shard = stores.create("typed", Default::default())?;
    let typed = json!([{"type":"text","text":"before"}]);
    shard.write(|w| w.append_user("typed-user", &typed))?;
    let mut rows = stores.sync_messages("typed")?;
    stores.sync_conversation("typed", None, &rows)?;
    assert_eq!(stores.conversation_detail("typed")?.1[0]["content"], typed);
    rows[0].content = "after".into();
    stores.sync_conversation("typed", None, &rows)?;
    stores.sync_conversation("typed", None, &rows)?;
    assert_eq!(
        stores.conversation_detail("typed")?.1[0]["content"],
        "after"
    );
    Ok(())
}

#[test]
fn synced_message_ids_can_be_restored_only_in_the_receiving_shard() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let a = Stores::open(source.path())?;
    a.create("shared", Default::default())?
        .write(|w| w.append("user", "from another device"))?;
    let b = Stores::open(destination.path())?;
    let target = b.create("shared", Default::default())?;
    let rows = a.sync_messages("shared")?;
    b.sync_conversation("shared", None, &rows)?;
    let snapshot = b.conversation_detail("shared")?.1;
    target.edit_conversation(&json!({"messages":[]}))?;
    target.edit_conversation(&json!({"messages":snapshot}))?;
    assert_eq!(b.sync_messages("shared")?[0].id, rows[0].id);
    let other = b.create("different", Default::default())?;
    let unqualified = json!({"messages":[{"id":rows[0].id,"role":"user","content":"foreign"}]});
    assert!(other.edit_conversation(&unqualified).is_err());
    Ok(())
}

#[test]
fn pin_completes_while_execution_is_held() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let shard = stores.create("running", Default::default())?;
    let lease = shard.acquire_execution()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let writer =
        std::thread::spawn(move || tx.send(shard.edit_conversation(&json!({"pinned":true}))));
    let result = rx.recv_timeout(std::time::Duration::from_secs(2));
    drop(lease);
    writer.join().unwrap().ok();
    result??;
    assert!(stores.conversation_metadata("running")?.unwrap().pinned);
    Ok(())
}
