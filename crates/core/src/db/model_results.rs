//! The only model reader that can fall back to a complete legacy result.
use crate::native_engine::tool_loop::{project_for_model, Recovery};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub(crate) fn data_root(conn: &Connection) -> Result<Option<PathBuf>> {
    let bytes: Vec<u8> = conn.query_row(
        "SELECT CAST(file AS BLOB) FROM pragma_database_list WHERE name='main'",
        [],
        |r| r.get(0),
    )?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(not(unix))]
    let file = PathBuf::from(String::from_utf8(bytes)?);
    if file.as_os_str().is_empty() {
        return Ok(None);
    }
    let parent = file.parent();
    Ok(if file.file_name() == Some("session.db".as_ref())
        && parent.and_then(Path::parent).and_then(Path::file_name) == Some("sessions".as_ref())
    {
        parent.and_then(Path::parent).and_then(Path::parent)
    } else {
        parent
    }
    .map(Path::to_owned))
}

pub(crate) struct ModelText {
    pub text: String,
    pub preview: Option<String>,
    pub recovery: Option<Recovery>,
}

impl ModelText {
    pub fn new(
        name: &str,
        raw: &str,
        root: Option<&Path>,
        session: &str,
        id: Option<&str>,
        cwd: &Path,
    ) -> Self {
        if let Some(mut view) = project_for_model(name, raw, root, session) {
            view.text
                .push_str(&crate::native_engine::tool_loop::projected_command_notes(
                    name, raw, root, id, cwd,
                ));
            Self {
                text: view.text,
                preview: None,
                recovery: Some(view.recovery),
            }
        } else {
            let preview =
                crate::native_engine::tool_loop::ordinary_preview(name, raw, id, root, cwd);
            Self {
                text: raw.into(),
                preview: (preview != raw).then_some(preview),
                recovery: None,
            }
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ModelCall {
    pub id: String,
    pub message_id: String,
    pub name: String,
    pub input: Option<String>,
    #[serde(rename = "output")]
    pub text: Option<String>,
    pub is_error: bool,
    pub sort_order: i64,
    #[serde(skip)]
    pub preview: Option<String>,
}

pub(crate) fn calls(conn: &Connection, message: &str, session: &str) -> Result<Vec<ModelCall>> {
    let mut query=conn.prepare_cached("SELECT id,message_id,name,input,model_output,model_preview,is_error,sort_order,
        CASE WHEN model_output IS NULL THEN output END FROM tool_calls WHERE message_id=? ORDER BY sort_order,rowid")?;
    let mut rows = query.query([message])?;
    let mut calls = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(2)?;
        let mut text = row.get(4)?;
        let mut preview = row.get(5)?;
        if let Some(old) = row.get::<_, Option<String>>(8)? {
            let view = ModelText::new(
                &name,
                &old,
                data_root(conn)?.as_deref(),
                session,
                Some(&row.get::<_, String>(0)?),
                Path::new("."),
            );
            text = Some(view.text);
            preview = view.preview;
        }
        calls.push(ModelCall {
            id: row.get(0)?,
            message_id: row.get(1)?,
            name,
            input: row.get(3)?,
            text,
            preview,
            is_error: row.get(6)?,
            sort_order: row.get(7)?,
        });
    }
    Ok(calls)
}

/// A protocol ID is only authoritative for its own call in this session.
pub(crate) fn provider_content(
    conn: &Connection,
    session: &str,
    id: &str,
    name: &str,
    text_only: bool,
) -> Result<Option<Value>> {
    let mut query=conn.prepare_cached("SELECT coalesce(t.model_preview,t.model_output),
            CASE WHEN t.model_output IS NULL THEN t.output END, side.model_content_json,side.tool_use_id
        FROM tool_calls t JOIN messages m ON m.id=t.message_id
        LEFT JOIN mcp_tool_outputs side ON side.conversation_id=m.conversation_id AND side.tool_use_id=t.id AND side.display_text=t.output
        WHERE t.id=?1 AND t.name=?2 AND m.conversation_id=?3")?;
    let mut rows = query.query(rusqlite::params![id, name, session])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    if row.get::<_, Option<String>>(3)?.is_some() {
        // A legacy sidecar was restored by the protocol history reader.
        let model: Option<String> = row.get(2)?;
        return model
            .map(|model| {
                let content = serde_json::from_str(&model)?;
                Ok(if text_only {
                    Value::String(crate::native_engine::mcp_output::openai_text(&content))
                } else {
                    content
                })
            })
            .transpose();
    }
    if let Some(model) = row.get::<_, Option<String>>(0)? {
        return Ok(Some(Value::String(model)));
    }
    Ok(if let Some(old) = row.get::<_, Option<String>>(1)? {
        let view = ModelText::new(
            name,
            &old,
            data_root(conn)?.as_deref(),
            session,
            Some(id),
            Path::new("."),
        );
        Some(Value::String(view.preview.unwrap_or(view.text)))
    } else {
        None
    })
}

pub(crate) fn notification(
    conn: &Connection,
    session: &str,
    role: &str,
    raw: &str,
) -> Result<Option<ModelText>> {
    if role != "user" || !crate::native_engine::tool_loop::monitor_notification(raw) {
        return Ok(None);
    }
    let view = project_for_model("Monitor", raw, data_root(conn)?.as_deref(), session);
    let (text, recovery) = view
        .map(|v| (v.text, Some(v.recovery)))
        .unwrap_or_else(|| (raw.to_owned(), None));
    Ok(Some(ModelText {
        text,
        preview: None,
        recovery,
    }))
}

pub(crate) fn message_text(
    conn: &Connection,
    row: &crate::db::message_repo::MessageRow,
) -> Result<String> {
    if row.role == "user" {
        let text: Option<String> = conn
            .query_row(
                "SELECT model_content FROM messages WHERE id=?",
                [&row.id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(text) = text {
            return Ok(text);
        }
        if let Some(view) = notification(conn, &row.conversation_id, &row.role, &row.content)? {
            return Ok(view.text);
        }
    }
    Ok(row.content.clone())
}

pub(crate) fn search_text<'a>(name: &str, text: &'a str) -> Option<Cow<'a, str>> {
    let Some(name) = super::context_search_repo::projection_name(name) else {
        return Some(Cow::Borrowed(text));
    };
    let prefix = text.strip_prefix("Error: ").unwrap_or(text);
    if prefix.trim_start().starts_with('{') {
        let parsed = serde_json::Deserializer::from_str(prefix)
            .into_iter::<Value>()
            .next();
        match parsed {
            Some(Ok(value)) if name == "Read" && value.get("image").is_some() => {
                return value["content"]
                    .as_str()
                    .map(|display| Cow::Owned(display.to_owned()));
            }
            Some(Ok(_)) => {}
            _ => return None,
        }
    }
    Some(Cow::Owned(
        crate::native_engine::tool_loop::model_tool_output(name, text),
    ))
}

pub(crate) fn legacy_search(
    conn: &Connection,
    session: &str,
    name: &str,
    id: &str,
    notification: bool,
) -> Result<ModelText> {
    let query = if notification {
        "SELECT CAST(content AS BLOB) FROM messages WHERE id=?"
    } else {
        "SELECT CAST(output AS BLOB) FROM tool_calls WHERE id=?"
    };
    let bytes: Vec<u8> = conn.query_row(query, [id], |r| r.get(0))?;
    let raw = String::from_utf8_lossy(&bytes);
    if notification && !crate::native_engine::tool_loop::monitor_notification(&raw) {
        return Ok(ModelText {
            text: raw.into_owned(),
            preview: None,
            recovery: None,
        });
    }
    let mut view = ModelText::new(
        name,
        &raw,
        data_root(conn)?.as_deref(),
        session,
        Some(id),
        Path::new("."),
    );
    if view.recovery.is_none() && !notification {
        if let Some(text) = search_text(name, &raw) {
            view.text = text.into_owned();
        }
    }
    Ok(view)
}

pub(crate) fn messages(
    conn: &Connection,
    session: &str,
) -> Result<Vec<crate::db::message_repo::MessageRow>> {
    let mut query = conn.prepare_cached(
        "SELECT id,conversation_id,role,coalesce(model_content,content),thinking,created_at,
                is_compact_boundary,sort_order,model_content IS NULL
         FROM messages WHERE conversation_id=? ORDER BY sort_order,rowid",
    )?;
    let mut rows = query.query([session])?;
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        let mut message = crate::db::message_repo::row_to_message(row)?;
        if row.get::<_, bool>(8)? {
            if let Some(view) = notification(conn, session, &message.role, &message.content)? {
                message.content = view.text;
            }
        }
        messages.push(message);
    }
    Ok(messages)
}
