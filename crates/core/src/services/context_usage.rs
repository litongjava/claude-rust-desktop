//! Local inspection of the last parent-session request; never prepares a new request.
use crate::{db::DbManager, native_engine::request_context::ContextUsage};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::Value;

#[derive(Debug)]
pub struct LastRequest {
    pub model: String,
    pub started_at_ms: Option<i64>,
    pub status: String,
    pub usage: Option<ContextUsage>,
    pub reported_input_tokens: Option<u64>,
}

pub fn last_request(db: &DbManager, session: &str) -> Result<Option<LastRequest>> {
    let raw = db.with_conn(|conn| conn.query_row(
        "SELECT t.requests_json FROM message_trajectory t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 ORDER BY m.sort_order DESC, m.created_at DESC LIMIT 1",
        [session], |row| row.get::<_, String>(0),
    ).optional())??;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let Ok(snapshots) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    let Some(snapshot) = snapshots.as_array().and_then(|items| {
        items
            .iter()
            .rev()
            .find(|s| s["source"] == "native_engine" || s.get("source").is_none())
    }) else {
        return Ok(None);
    };
    let usage = serde_json::from_value::<ContextUsage>(snapshot["contextUsage"].clone())
        .ok()
        .filter(|u| u.version == 1);
    // Anthropic input excludes cached input; normalized OpenAI input includes it.
    let raw_usage = &snapshot["usage"];
    let reported_input_tokens = raw_usage["prompt_tokens"].as_u64().or_else(|| {
        raw_usage["input_tokens"].as_u64().map(|input| {
            input
                .saturating_add(raw_usage["cache_read_input_tokens"].as_u64().unwrap_or(0))
                .saturating_add(
                    raw_usage["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
        })
    });
    Ok(Some(LastRequest {
        model: snapshot["model"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect(),
        started_at_ms: snapshot["startedAt"].as_i64(),
        status: match snapshot["status"].as_str() {
            Some("completed") => "completed",
            Some("error") => "error",
            Some("running") => "incomplete",
            Some("cancelled") => "cancelled",
            _ => "unknown",
        }
        .into(),
        usage,
        reported_input_tokens,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::message_repo,
        runtime::{RuntimePaths, SessionStore},
    };
    use serde_json::json;

    #[test]
    fn last_request_is_session_local_and_missing_metadata_stays_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "model", "provider").unwrap();
        assert!(last_request(&store.db, &parent).unwrap().is_none());
        let child = store.create(temp.path(), "child", "provider").unwrap();
        for (session, id, model, usage) in [
            (
                &parent,
                "p",
                "model",
                json!({"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}),
            ),
            (
                &child,
                "c",
                "child",
                json!({"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":40}}),
            ),
        ] {
            let message = store.append(session, "assistant", "private body").unwrap();
            store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &json!([{"id":id,"source":"native_engine","model":model,"usage":usage,"status":"completed"}]))).unwrap().unwrap();
        }
        let result = last_request(&store.db, &parent).unwrap().unwrap();
        assert_eq!(result.model, "model");
        assert_eq!(result.reported_input_tokens, Some(60));
        assert!(result.usage.is_none());
        assert_eq!(
            last_request(&store.db, &child)
                .unwrap()
                .unwrap()
                .reported_input_tokens,
            Some(100)
        );
        let message = store.append(&parent, "assistant", "new response").unwrap();
        store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &json!([{"id":"new","source":"native_engine","model":"new-model","contextUsage":{"bad":"PRIVATE"}}]))).unwrap().unwrap();
        let result = last_request(&store.db, &parent).unwrap().unwrap();
        assert_eq!(result.model, "new-model");
        assert!(result.usage.is_none());
        assert!(result.reported_input_tokens.is_none());
        store
            .db
            .with_conn(|conn| {
                conn.execute("DELETE FROM messages WHERE conversation_id=?1", [&parent])
            })
            .unwrap()
            .unwrap();
        assert!(last_request(&store.db, &parent).unwrap().is_none());
    }
}
