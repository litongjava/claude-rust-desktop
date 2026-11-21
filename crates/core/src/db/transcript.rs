//! The conversation detail and snapshot replacement contracts shared by hosts.
pub fn enriched_messages(
    conn: &rusqlite::Connection,
    id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let messages = super::message_repo::get_messages_by_conversation(conn, id)?;
    messages
        .iter()
        .map(|msg| {
            let mut value = serde_json::to_value(msg)?;
            value["content"] = super::message_repo::user_content_for_display(conn, msg)?;
            value["requestSnapshots"] =
                super::message_repo::get_request_snapshots(conn, &msg.id)
                    .unwrap_or_else(|_| serde_json::json!([]));
            if let Some(turn) = crate::db::turn_repo::get_message_turn(conn, &msg.id)?
            {
                value["turn"] = serde_json::to_value(turn)?;
            }
            let tool_calls = super::message_repo::get_tool_calls_for_message(conn, &msg.id)?;
            if !tool_calls.is_empty() {
                value["tool_calls"] = serde_json::json!(tool_calls
                    .iter()
                    .map(|tc| {
                        let input = tc
                            .input
                            .as_ref()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .unwrap_or(serde_json::json!({}));
                        let mcp_result = crate::services::subagents::typed_mcp_result(
                            conn, id, &tc.id, tc.output.as_deref(), tc.is_error,
                        )?;
                        Ok::<_, anyhow::Error>(serde_json::json!({
                            "id": tc.id.strip_prefix(&format!("{}:", msg.id)).unwrap_or(&tc.id),
                            "name": tc.name,
                            "input": input,
                            "result": tc.output,
                            "mcp_result": mcp_result,
                            "status": if tc.is_error { "error" } else if tc.output.is_none() { "running" } else { "done" },
                        }))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?);
            }
            Ok(value)
        })
        .collect()
}

/// Apply a client snapshot inside the caller's write transaction. Retained rows
/// keep their trajectory, turn links and tool occurrences.
pub fn replace_messages(
    tx: &rusqlite::Connection,
    id: &str,
    messages: &[serde_json::Value],
) -> anyhow::Result<()> {
    let existing = super::message_repo::get_messages_by_conversation(tx, id)?;
    let mut retained = std::collections::HashSet::new();
    for (idx, msg) in messages.iter().enumerate() {
        let msg_id = msg
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        anyhow::ensure!(retained.insert(msg_id.clone()), "Duplicate message ID");
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let previous = super::message_repo::get_message(tx, &msg_id)?;
        if let Some(previous) = &previous {
            anyhow::ensure!(
                previous.conversation_id == id && previous.role == role,
                "Message ID belongs to a different conversation or role"
            );
        }
        let now = chrono::Utc::now().to_rfc3339();
        let content = msg.get("content").cloned().unwrap_or(serde_json::json!(""));
        if role == "user" {
            super::message_repo::save_user_message(tx, &msg_id, id, &content, &now, idx as i64)?;
        } else {
            let content = content
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| content.to_string());
            if previous.is_some() {
                tx.execute(
                    "UPDATE messages SET content = ?1, sort_order = ?2 WHERE id = ?3",
                    rusqlite::params![content, idx as i64, msg_id],
                )?;
            } else {
                super::message_repo::insert_message(
                    tx, &msg_id, id, role, &content, None, &now, false, idx as i64,
                )?;
            }
        }
    }
    for message in existing {
        if !retained.contains(&message.id) {
            super::message_repo::delete_message(tx, &message.id)?;
        }
    }
    Ok(())
}
