//! Search reads metadata without visiting transcript overflow pages. Large
//! searchable values have independently keyed fragments so a cold continuation
//! never walks the prefix of a SQLite overflow chain.
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, DatabaseName};

pub(super) const BLOCK: usize = 64 * 1024;
pub(super) const PROJECTED_TOOLS: [&str; 8] = [
    "Bash",
    "TaskGet",
    "TaskOutput",
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "ApplyPatch",
];

/// DbManager holds BEGIN IMMEDIATE through schema creation, backfill and trigger installation.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    if conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='context_search_sources')",
        [], |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_context_search_messages;
         DROP INDEX IF EXISTS idx_context_search_tools;
         DROP INDEX IF EXISTS idx_context_search_boundaries;
         CREATE INDEX IF NOT EXISTS idx_message_history ON messages(conversation_id,sort_order);
         CREATE INDEX IF NOT EXISTS idx_message_boundary_order ON messages(conversation_id,sort_order)
             WHERE is_compact_boundary=1;
         CREATE TABLE context_search_sources(
             kind TEXT NOT NULL, id TEXT NOT NULL, owner TEXT NOT NULL, sort_order INTEGER NOT NULL,
             sequence INTEGER NOT NULL, label TEXT NOT NULL, stamp TEXT, boundary INTEGER,
             size INTEGER NOT NULL, chunked INTEGER NOT NULL, PRIMARY KEY(kind,id)
         ) WITHOUT ROWID;
         CREATE INDEX context_search_order ON context_search_sources(kind,owner,sort_order,sequence);
         CREATE INDEX context_search_boundary_order ON context_search_sources(owner,sort_order,sequence)
             WHERE kind='message' AND boundary=1;
         -- Keep fragment bytes out of the search key, including interior B-tree pages.
         -- A rowid table puts the composite key in a separate, compact index.
         CREATE TABLE context_search_chunks(
             kind TEXT NOT NULL, id TEXT NOT NULL, part INTEGER NOT NULL, body BLOB NOT NULL,
             PRIMARY KEY(kind,id,part),
             FOREIGN KEY(kind,id) REFERENCES context_search_sources(kind,id) ON DELETE CASCADE
         );"
    )?;
    let projected = PROJECTED_TOOLS.map(|name| format!("'{name}'")).join(",");
    let max = super::context_search_repo::MAX_CHUNK_BYTES;
    for (table, column, kind, fields, metadata, chunked) in [
        ("messages", "content", "message", "id,conversation_id,sort_order,role,created_at,is_compact_boundary,content",
         "conversation_id,sort_order,rowid,role,created_at,is_compact_boundary,length(CAST(content AS BLOB))",
         format!("role IN ('user','assistant') AND NOT coalesce(is_compact_boundary,0) AND length(CAST(content AS BLOB))>{BLOCK}")),
        ("tool_calls", "output", "tool", "id,message_id,sort_order,name,input,output",
         "message_id,sort_order,rowid,name,NULL,0,CASE WHEN typeof(output) IN ('text','blob') THEN length(CAST(output AS BLOB)) ELSE -1 END",
         format!("name!='ContextSearch' AND typeof(output) IN ('text','blob') AND (
                    (name COLLATE NOCASE NOT IN ({projected}) AND length(CAST(output AS BLOB))>{BLOCK})
                    OR (length(CAST(input AS BLOB))>4096 AND
                        (name COLLATE NOCASE NOT IN ({projected}) OR length(CAST(output AS BLOB))<={max}))
                  )")),
    ] {
        let insert = format!("INSERT INTO context_search_sources
            SELECT '{kind}',id,{metadata},coalesce(({chunked}),0) FROM {table}");
        conn.execute_batch(&insert)?;
        // Open each old value once and advance sequentially. Backfill occurs once
        // on upgrade; searches only open the small indexed fragments afterward.
        let mut old = conn.prepare("SELECT id,sequence FROM context_search_sources WHERE kind=? AND chunked=1")?;
        let mut rows = old.query([kind])?;
        let mut fragment = conn.prepare("INSERT INTO context_search_chunks VALUES (?1,?2,?3,?4)")?;
        let mut buf = vec![0; BLOCK];
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let value = conn.blob_open(DatabaseName::Main, table, column, row.get(1)?, true)?;
            for (part, offset) in (0..value.len()).step_by(BLOCK).enumerate() {
                let len = BLOCK.min(value.len() - offset);
                value.read_at_exact(&mut buf[..len], offset)?;
                fragment.execute(params![kind, id, part, &buf[..len]])?;
            }
        }
        let remove = |key: &str| format!(
            "DELETE FROM context_search_chunks WHERE kind='{kind}' AND id={key}.id;
             DELETE FROM context_search_sources WHERE kind='{kind}' AND id={key}.id;"
        );
        let save = format!("{insert} WHERE rowid=NEW.rowid;
            INSERT INTO context_search_chunks
            WITH RECURSIVE offsets(pos,size) AS (
                SELECT 0,size FROM context_search_sources WHERE kind='{kind}' AND id=NEW.id AND chunked=1 AND size>0
                UNION ALL SELECT pos+{BLOCK},size FROM offsets WHERE pos+{BLOCK}<size
            ) SELECT '{kind}',NEW.id,pos/{BLOCK},substr(CAST(NEW.{column} AS BLOB),pos+1,{BLOCK}) FROM offsets;");
        let old = remove("OLD");
        let new = remove("NEW");
        conn.execute_batch(&format!(
            "CREATE TRIGGER context_search_{table}_insert AFTER INSERT ON {table} BEGIN {new} {save} END;
             CREATE TRIGGER context_search_{table}_update AFTER UPDATE OF {fields},rowid,_rowid_,oid ON {table} BEGIN {old} {save} END;
             CREATE TRIGGER context_search_{table}_delete AFTER DELETE ON {table} BEGIN {old} END;"
        ))?;
    }
    Ok(())
}

/// Read only the intersecting fragments. Charge a whole fragment when just its
/// suffix or prefix is returned. Unfragmented values have a bounded starting
/// position in the source record (message content or a short tool input).
pub(super) fn read_bytes(
    conn: &Connection,
    kind: &str,
    id: &str,
    chunked: bool,
    start: usize,
    count: usize,
) -> Result<(Vec<u8>, usize)> {
    let mut charged = 0;
    let mut bytes = Vec::with_capacity(count);
    if count > 0 && chunked {
        let last = (start + count - 1) / BLOCK;
        let mut stmt = conn.prepare_cached(
            "SELECT part,body FROM context_search_chunks
                WHERE kind=?1 AND id=?2 AND part BETWEEN ?3 AND ?4 ORDER BY part",
        )?;
        let mut rows = stmt.query(params![kind, id, start / BLOCK, last])?;
        while let Some(row) = rows.next()? {
            let part: usize = row.get(0)?;
            let block = row.get_ref(1)?.as_blob()?;
            charged += block.len();
            let base = part * BLOCK;
            let lo = start.saturating_sub(base);
            let hi = (start + count - base).min(block.len());
            ensure!(lo <= hi, "Invalid ContextSearch fragment");
            bytes.extend_from_slice(&block[lo..hi]);
        }
        ensure!(bytes.len() == count, "Incomplete ContextSearch source");
    } else if count > 0 {
        let (table, column, lookup) = if kind == "tool" {
            (
                "tool_calls",
                "output",
                "SELECT rowid FROM tool_calls WHERE id=?",
            )
        } else {
            (
                "messages",
                "content",
                "SELECT rowid FROM messages WHERE id=?",
            )
        };
        let rowid = conn
            .prepare_cached(lookup)?
            .query_row([id], |row| row.get(0))?;
        let blob = conn.blob_open(DatabaseName::Main, table, column, rowid, true)?;
        bytes.resize(count, 0);
        blob.read_at_exact(&mut bytes, start)?;
        charged += count;
    }
    Ok((bytes, charged))
}
