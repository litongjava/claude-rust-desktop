use claude_desktop_tauri_lib::db::{DbManager, message_repo};
use serde_json::json;

#[test]
fn request_snapshots_survive_reopen_and_follow_message_deletion() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("trajectory.db");
    let snapshot = json!([{"id":"request-1", "number":1, "systemPrompt":"actual injected project instructions", "systemMessages":[{"role":"system","content":"retrieved memory"}], "tools":[{"name":"Read","input_schema":{"type":"object"}}]}]);
    {
        let db = DbManager::new(path.clone())?;
        db.init()?;
        db.with_conn(|conn| -> anyhow::Result<()> {
            conn.execute("INSERT INTO conversations (id, created_at, updated_at) VALUES ('conv', 'now', 'now')", [])?;
            message_repo::insert_message(conn, "assistant", "conv", "assistant", "", None, "now", false, 0)?;
            message_repo::save_request_snapshots(conn, "assistant", &snapshot)?;
            // A snapshot exists even before any answer is produced.
            assert_eq!(message_repo::get_request_snapshots(conn, "assistant")?, snapshot);
            message_repo::update_message_content(conn, "assistant", "answer")?;
            Ok(())
        })??;
    }
    let db = DbManager::new(path)?;
    db.init()?;
    db.with_conn(|conn| -> anyhow::Result<()> {
        assert_eq!(message_repo::get_request_snapshots(conn, "assistant")?, snapshot);
        assert_eq!(message_repo::get_message(conn, "assistant")?.unwrap().content, "answer");
        message_repo::delete_message(conn, "assistant")?;
        assert_eq!(message_repo::get_request_snapshots(conn, "assistant")?, json!([]));
        message_repo::insert_message(conn, "next", "conv", "assistant", "", None, "now", false, 1)?;
        message_repo::save_request_snapshots(conn, "next", &snapshot)?;
        conn.execute("DELETE FROM conversations WHERE id = 'conv'", [])?;
        assert_eq!(message_repo::get_request_snapshots(conn, "next")?, json!([]));
        Ok(())
    })??;
    Ok(())
}
