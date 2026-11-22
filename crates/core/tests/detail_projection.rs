use anyhow::Result;
use deepseek_code_core::db::session_store::Stores;
use rusqlite::Connection;

#[test]
fn detail_projection() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let session = db.create("coherence", Default::default())?;
    let writer = Connection::open(root.path().join("global.db"))?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    session.write(|w| w.append("user", "first committed prompt"))?;
    let (row, messages) = db.conversation_detail("coherence")?;
    let blocked = row.unwrap();
    println!(
        "G writer held: message_count={}, messages={}, title={:?}, text={}",
        blocked.message_count,
        messages.len(),
        blocked.title,
        messages[0]["content"]
    );
    writer.execute_batch("ROLLBACK")?;
    let (row, messages) = db.conversation_detail("coherence")?;
    let after = row.unwrap();
    println!(
        "G writer released: message_count={}, messages={}, title={:?}",
        after.message_count,
        messages.len(),
        after.title
    );
    anyhow::ensure!(
        blocked.message_count == 1
            && blocked.title.as_deref() == Some("first committed prompt")
            && messages.len() == 1
            && after.message_count == 1
    );
    Ok(())
}
