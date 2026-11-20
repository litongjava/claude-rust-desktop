use super::*;
use crate::db::message_repo;
use rusqlite::{params_from_iter, types::Value};
use serde_json::Value as Json;

#[derive(Clone, Copy, Debug)]
pub enum Branch<'a> {
    Full,
    Through {
        message: Option<&'a str>,
        index: Option<usize>,
    },
    Clear,
    Child,
}

impl SessionDb {
    pub fn branch(&self, destination: &str, boundary: Branch<'_>) -> Result<SessionDb> {
        let dest = storage_key(destination)?;
        ensure!(dest != self.key, "Source and destination must differ");
        let s = &self.stores;
        let _locks = s.group_access(vec![(&self.key, false, false), (&dest, true, true)])?;
        self.check_directory()?;
        let (row, revision) = s.global().read(|c| {
            let row = crate::db::conversation_repo::get_conversation(c, &self.id)?
                .context("Source unavailable")?;
            let revision = c.query_row(
                "SELECT metadata_revision FROM conversations WHERE id=?1",
                [&self.id],
                |r| r.get(0),
            )?;
            if matches!(boundary, Branch::Child) {
                crate::native_engine::workspace::resolve_workspace(
                    c,
                    &s.root.path.join("workspaces"),
                    &self.id,
                    None,
                )?;
            }
            Ok((row, revision))
        })?;
        let options = CreateOptions {
            title: Some(format!(
                "{} (branch)",
                row.title.as_deref().unwrap_or("Conversation")
            )),
            model: row.model,
            provider: row.provider,
            workspace_mode: Some(row.workspace_mode),
            research: row.research_mode,
            project: row.project_id.map(ProjectSelection::Existing),
            ..Default::default()
        };
        s.create_locked(
            destination,
            options,
            Seed::Fork(self, boundary),
            Some((
                self.id.clone(),
                self.incarnation.clone(),
                revision,
                matches!(boundary, Branch::Child),
            )),
        )
    }
}

struct Copier<'a> {
    src: &'a Connection,
    dst: &'a Connection,
    id: &'a str,
}
impl Copier<'_> {
    fn mapped(&self, kind: &str, old: &str) -> Result<Option<String>> {
        Ok(self
            .dst
            .query_row(
                "SELECT new FROM copied_ids WHERE kind=?1 AND old=?2",
                [kind, old],
                |r| r.get(0),
            )
            .optional()?)
    }
    fn assign(&self, kind: &str, old: &str) -> Result<String> {
        if let Some(id) = self.mapped(kind, old)? {
            return Ok(id);
        }
        let id = Uuid::new_v4().to_string();
        self.dst
            .execute("INSERT INTO copied_ids VALUES(?1,?2,?3)", [kind, old, &id])?;
        Ok(id)
    }
    fn remap_blocks(&self, blocks: &mut Json) -> Result<()> {
        if let Some(items) = blocks.as_array_mut() {
            for block in items {
                for key in ["id", "tool_use_id", "toolCallId", "parentToolCallId"] {
                    if let Some(old) = block[key].as_str() {
                        if let Some(new) = self.mapped("tool", old)? {
                            block[key] = Json::String(new)
                        }
                    }
                }
            }
        }
        Ok(())
    }
    // Names and column rules below are fixed schema identifiers. Each cursor
    // holds one row; the potentially large ID map belongs to the private stage.
    fn rows(
        &self,
        table: &str,
        owner: Option<(&str, &str)>,
        identity: Option<(&str, &str)>,
        links: &[(&str, &str)],
    ) -> Result<()> {
        let mut q = self
            .src
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
        let columns: Vec<String> = q.column_names().into_iter().map(str::to_owned).collect();
        let idx = |name: &str| {
            columns
                .iter()
                .position(|c| c == name)
                .context("Missing copy column")
        };
        let marks = vec!["?"; columns.len()].join(",");
        let mut insert = self.dst.prepare(&format!(
            "INSERT OR REPLACE INTO {table}({}) VALUES({marks})",
            columns.join(",")
        ))?;
        let mut rows = q.query([])?;
        while let Some(row) = rows.next()? {
            let mut values = (0..columns.len())
                .map(|i| row.get::<_, Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some((col, kind)) = owner {
                let old: String = row.get(idx(col)?)?;
                let Some(new) = self.mapped(kind, &old)? else {
                    continue;
                };
                values[idx(col)?] = Value::Text(new);
            }
            if let Some((col, kind)) = identity {
                values[idx(col)?] =
                    Value::Text(self.assign(kind, &row.get::<_, String>(idx(col)?)?)?);
            }
            for &(col, kind) in links {
                let old: String = row.get(idx(col)?)?;
                values[idx(col)?] = Value::Text(
                    self.mapped(kind, &old)?
                        .context("Unselected copy reference")?,
                );
            }
            if let Ok(i) = idx("conversation_id") {
                values[i] = Value::Text(self.id.into())
            }
            if table == "model_requests" {
                let i = idx("response_blocks_json")?;
                let mut blocks: Json = serde_json::from_str(&row.get::<_, String>(i)?)?;
                self.remap_blocks(&mut blocks)?;
                values[i] = Value::Text(blocks.to_string());
            }
            if table == "session_permission_authority" {
                values[idx("id")?] = Value::Null
            }
            insert.execute(params_from_iter(values))?;
        }
        Ok(())
    }
    fn run(&self, boundary: Branch<'_>) -> Result<()> {
        self.dst.execute_batch(
            "CREATE TEMP TABLE copied_ids(kind TEXT,old TEXT,new TEXT,PRIMARY KEY(kind,old))",
        )?;
        self.rows("session_agents", None, None, &[])?;
        if matches!(boundary, Branch::Child | Branch::Full) {
            self.rows("session_permission_authority", None, None, &[])?;
        }
        if matches!(boundary, Branch::Clear | Branch::Child) {
            return Ok(());
        }
        let limit = match boundary {
            Branch::Through { message, index } => {
                let by_id=message.map(|id| self.src.query_row("SELECT n FROM (SELECT id,row_number() OVER(ORDER BY sort_order,rowid)-1 n FROM messages) WHERE id=?1",[id],|r|r.get::<_,usize>(0)).optional()).transpose()?.flatten();
                Some(by_id.or(index).context("Missing branch boundary")?)
            }
            _ => None,
        };
        let mut q=self.src.prepare("SELECT id,conversation_id,role,content,thinking,created_at,is_compact_boundary,sort_order FROM messages ORDER BY sort_order,rowid")?;
        let mut count = 0;
        for row in q.query_map([], message_repo::row_to_message)? {
            let m = row?;
            let id = self.assign("message", &m.id)?;
            message_repo::insert_message(
                self.dst,
                &id,
                self.id,
                &m.role,
                &m.content,
                m.thinking.as_deref(),
                &m.created_at,
                m.is_compact_boundary,
                m.sort_order,
            )?;
            count += 1;
            if limit == Some(count - 1) {
                break;
            }
        }
        ensure!(
            limit.is_none_or(|i| i < count),
            "Branch boundary outside history"
        );
        let mut turns = self.src.prepare(
            "SELECT id,started_at,ended_at,elapsed_ms,status FROM execution_turns ORDER BY rowid",
        )?;
        let mut rows = turns.query([])?;
        while let Some(r) = rows.next()? {
            let old: String = r.get(0)?;
            if limit.is_some() {
                let mut selected = false;
                for message in crate::db::turn_repo::turn_messages(self.src, &old)? {
                    if self.mapped("message", &message)?.is_some() {
                        selected = true;
                        break;
                    }
                }
                if !selected {
                    continue;
                }
            }
            self.dst.execute("INSERT INTO execution_turns(id,conversation_id,source_turn_id,started_at,ended_at,elapsed_ms,status) VALUES(?1,?2,?3,?4,?5,?6,CASE ?7 WHEN 'running' THEN 'cancelled' ELSE ?7 END)",params![self.assign("turn",&old)?,self.id,old,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,String>(4)?])?;
        }
        let message = Some(("message_id", "message"));
        self.rows("message_turns", message, None, &[("turn_id", "turn")])?;
        self.rows("tool_calls", message, Some(("id", "tool")), &[])?;
        let tool = Some(("tool_call_id", "tool"));
        self.rows(
            "tool_call_origins",
            tool,
            None,
            &[("parent_tool_call_id", "tool")],
        )?;
        for table in ["tool_queued_inputs", "tool_call_notes", "tool_call_timings"] {
            self.rows(table, tool, None, &[])?
        }
        self.rows("mcp_tool_outputs", Some(("tool_use_id", "tool")), None, &[])?;
        for table in ["message_user_content", "native_boundaries"] {
            self.rows(table, message, None, &[])?
        }
        for table in ["permission_approvals", "code_diffs", "attachments"] {
            self.rows(table, message, Some(("id", table)), &[])?
        }
        let mut trajectories = self
            .src
            .prepare("SELECT message_id FROM message_trajectory ORDER BY rowid")?;
        for old in trajectories.query_map([], |r| r.get::<_, String>(0))? {
            let old = old?;
            let Some(new) = self.mapped("message", &old)? else {
                continue;
            };
            let mut requests = message_repo::get_request_snapshots(self.src, &old)?;
            if let Some(items) = requests.as_array_mut() {
                for request in items {
                    if let Some(id) = request["id"].as_str() {
                        request["id"] = Json::String(self.assign("request", id)?);
                    }
                    self.remap_blocks(&mut request["responseBlocks"])?;
                }
            }
            message_repo::save_request_snapshots(self.dst, &new, &requests)?;
        }
        self.rows("model_requests", message, Some(("id", "request")), &[])?;
        super::super::history::advance(self.dst)?;
        Ok(())
    }
}
pub(super) fn copy_snapshot(
    source: &SessionDb,
    dst: &Connection,
    id: &str,
    boundary: Branch<'_>,
) -> Result<()> {
    let path = source
        .stores
        .sessions()
        .join(&source.key)
        .join("session.db");
    let mut src = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tx = src.transaction()?;
    layout::verify(&tx, SESSION)?;
    verify_manifest(
        &tx,
        source.stores.global().store_uuid(),
        &source.id,
        &source.incarnation,
    )?;
    Copier { src: &tx, dst, id }.run(boundary)
}
