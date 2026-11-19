//! Keyset pages over the durable transcript. Payloads are read through SQLite
//! bounded primary values or directly addressed fragments of a large value.
use super::context_search_index::{BLOCK, PROJECTED_TOOLS};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_ROWS: usize = 25_000;
pub(crate) const MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_SCAN_BYTES: usize = 24 * 1024 * 1024;
// A literal is at most 512 bytes. Leave space for a split UTF-8 character.
const OVERLAP_BYTES: usize = 516;

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Range {
    #[default]
    All,
    Before,
    After,
}

#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Key {
    order: i64,
    sequence: i64,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Position {
    message: Key,
    part: Part,
}

#[derive(Clone, Deserialize, Serialize)]
enum Part {
    Start,
    Tool { key: Key, offset: usize },
    Text { offset: usize },
}

#[derive(Serialize)]
pub(crate) struct Location {
    pub message_id: String,
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

pub(crate) struct Source {
    pub position: Position,
    pub location: Location,
    pub text: String,
    pub overlap: usize,
    pub invalid_utf8: bool,
    pub preceded: bool,
    pub truncated: bool,
}

pub(crate) struct Transcript {
    pub boundary_id: Option<String>,
    pub sources: Vec<Source>,
    pub next: Option<Position>,
    pub skipped_structured_sources: usize,
    #[cfg(test)]
    pub bytes_read: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Boundary<'a> {
    Latest,
    Pinned(Option<&'a str>),
}

pub(crate) struct Selection<'a> {
    pub range: Range,
    pub boundary: Boundary<'a>,
    pub role: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub position: Option<&'a Position>,
}

pub(crate) fn projection_name(name: &str) -> Option<&'static str> {
    PROJECTED_TOOLS
        .into_iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
}

fn find_boundary(
    conn: &Connection,
    session: &str,
    boundary: Boundary<'_>,
) -> Result<Option<(Key, String)>> {
    let read = |row: &rusqlite::Row<'_>| {
        Ok((
            Key {
                order: row.get(0)?,
                sequence: row.get(1)?,
            },
            row.get(2)?,
        ))
    };
    match boundary {
        Boundary::Latest => Ok(conn
            .query_row(
                "SELECT sort_order,sequence,id FROM context_search_sources INDEXED BY context_search_boundary_order
             WHERE owner=?1 AND kind='message' AND boundary=1
             ORDER BY sort_order DESC,sequence DESC LIMIT 1",
                [session],
                read,
            )
            .optional()?),
        Boundary::Pinned(None) => Ok(None),
        Boundary::Pinned(Some(id)) => {
            let boundary = conn
                .query_row(
                    "SELECT sort_order,sequence,id FROM context_search_sources
                 WHERE id=?2 AND owner=?1 AND kind='message' AND boundary=1",
                    params![session, id],
                    read,
                )
                .optional()?;
            ensure!(
                boundary.is_some(),
                "Unknown compaction boundary in this session"
            );
            Ok(boundary)
        }
    }
}

pub(crate) fn read(
    conn: &Connection,
    session: &str,
    selection: Selection<'_>,
) -> Result<Transcript> {
    let tx = conn.unchecked_transaction()?;
    let mut page = Page::new(&tx)?;
    let boundary = find_boundary(&tx, session, selection.boundary)?;
    page.transcript.boundary_id = boundary.as_ref().map(|(_, id)| id.clone());
    if selection.range != Range::Before || boundary.is_some() {
        read_page(
            &tx,
            session,
            &selection,
            boundary.as_ref().map(|(key, _)| key),
            &mut page,
        )?;
    }
    tx.commit()?;
    #[cfg(test)]
    {
        page.transcript.bytes_read = page.payload_read;
    }
    Ok(page.transcript)
}

fn read_page(
    conn: &Connection,
    session: &str,
    selection: &Selection<'_>,
    boundary: Option<&Key>,
    page: &mut Page,
) -> Result<()> {
    let initial = selection.position;
    let lower = boundary.filter(|_| selection.range == Range::After);
    let before = boundary.filter(|_| selection.range == Range::Before);
    let upper = match (initial.map(|position| &position.message), before) {
        (Some(anchor), Some(before)) if anchor < before => Some((anchor, "<=")),
        (_, Some(before)) => Some((before, "<")),
        (Some(anchor), None) => Some((anchor, "<=")),
        (None, None) => None,
    };
    // Both keys are explicit index columns, so pages also seek directly among
    // rows with the same sort order, which can occur in imported transcripts.
    let upper_clause = upper.map_or(String::new(), |(_, op)| {
        format!("AND (sort_order,sequence){op}(?2,?3)")
    });
    let lower_clause = if lower.is_some() {
        "AND (sort_order,sequence)>(?4,?5)"
    } else {
        ""
    };
    let mut messages = conn.prepare(&format!(
        "SELECT sort_order,sequence,id,label,stamp,boundary,size,chunked
         FROM context_search_sources INDEXED BY context_search_order WHERE kind='message' AND owner=?1 {upper_clause} {lower_clause}
         ORDER BY sort_order DESC,sequence DESC LIMIT ?6"
    ))?;
    let mut rows = messages.query(params![
        session,
        upper.map(|(key, _)| key.order),
        upper.map(|(key, _)| key.sequence),
        lower.map(|key| key.order),
        lower.map(|key| key.sequence),
        MAX_ROWS + 1,
    ])?;
    let tools_available = selection.role.is_none_or(|role| role == "tool");
    const TOOL_ROWS: &str = "SELECT sort_order,sequence,id,label,size,chunked,
                    EXISTS(SELECT 1 FROM tool_call_origins o WHERE o.tool_call_id=s.id)
             FROM context_search_sources s INDEXED BY context_search_order WHERE kind='tool' AND owner=?1";
    const TOOL_ORDER: &str = "ORDER BY sort_order DESC,sequence DESC LIMIT ?4";
    let mut tools = if tools_available {
        Some(conn.prepare_cached(&format!("{TOOL_ROWS} {TOOL_ORDER}"))?)
    } else {
        None
    };
    let mut resumed_tools = tools_available
        .then(|| {
            conn.prepare_cached(&format!(
                "{TOOL_ROWS} AND (sort_order,sequence)<=(?2,?3) {TOOL_ORDER}"
            ))
        })
        .transpose()?;
    let mut turn = conn.prepare_cached(
        "SELECT tr.id FROM message_turns mt
         JOIN execution_turns tr ON tr.id=mt.turn_id AND tr.conversation_id=?2
         WHERE mt.message_id=?1",
    )?;
    while let Some(row) = rows.next()? {
        let message = Key {
            order: row.get(0)?,
            sequence: row.get(1)?,
        };
        let start = initial
            .filter(|p| p.message == message)
            .map_or(&Part::Start, |p| &p.part);
        if !page.visit_row(conn, || Position {
            message: message.clone(),
            part: start.clone(),
        }) {
            break;
        }
        if row.get::<_, Option<bool>>(5)?.unwrap_or(false) {
            continue;
        }
        let id: String = row.get(2)?;
        let role: String = row.get(3)?;
        let timestamp: String = row.get(4)?;
        let mut location = |tool: Option<(String, String)>| -> Result<Location> {
            let turn_id = turn
                .query_row(params![id, session], |row| row.get(0))
                .optional()?;
            let (role, tool_name, tool_call_id) = match tool {
                Some((name, id)) => ("tool".to_owned(), Some(name), Some(id)),
                None => (role.clone(), None, None),
            };
            Ok(Location {
                message_id: id.clone(),
                turn_id,
                timestamp: timestamp.clone(),
                role,
                tool_name,
                tool_call_id,
            })
        };

        let (tool_stmt, upper) = match start {
            Part::Start => (tools.as_mut(), None),
            Part::Tool { key, .. } => (resumed_tools.as_mut(), Some(key)),
            Part::Text { .. } => (None, None),
        };
        if let Some(stmt) = tool_stmt {
            let mut tool_rows = stmt.query(params![
                id,
                upper.map(|key| key.order),
                upper.map(|key| key.sequence),
                page.rows_left + 1
            ])?;
            while let Some(tool) = tool_rows.next()? {
                let key = Key {
                    order: tool.get(0)?,
                    sequence: tool.get(1)?,
                };
                let offset = match start {
                    Part::Tool {
                        key: anchor,
                        offset,
                    } if &key == anchor => *offset,
                    _ => 0,
                };
                let position = || Position {
                    message: message.clone(),
                    part: Part::Tool {
                        key: key.clone(),
                        offset,
                    },
                };
                if !page.visit_row(conn, position) {
                    return Ok(());
                }
                let name: String = tool.get(3)?;
                if name == "ContextSearch"
                    || selection
                        .tool_name
                        .is_some_and(|filter| !name.eq_ignore_ascii_case(filter))
                    || tool.get::<_, i64>(4)? < 0
                    || tool.get::<_, bool>(6)?
                {
                    continue;
                }
                let location = location(Some((name, tool.get(2)?)))?;
                if !page.read_source(conn, tool.get(4)?, tool.get(5)?, position(), location)? {
                    return Ok(());
                }
            }
        }
        if selection.tool_name.is_none()
            && matches!(role.as_str(), "user" | "assistant")
            && selection.role.is_none_or(|filter| filter == role)
        {
            let offset = match start {
                Part::Text { offset } => *offset,
                _ => 0,
            };
            let location = location(None)?;
            let position = Position {
                message,
                part: Part::Text { offset },
            };
            if !page.read_source(conn, row.get(6)?, row.get(7)?, position, location)? {
                break;
            }
        }
    }
    Ok(())
}

struct Page {
    rows_left: usize,
    payload_read: usize,
    initial_misses: u32,
    page_size: usize,
    transcript: Transcript,
}

impl Page {
    fn new(conn: &Connection) -> Result<Self> {
        Ok(Self {
            rows_left: MAX_ROWS,
            payload_read: 0,
            initial_misses: cache_misses(conn),
            page_size: conn.query_row("PRAGMA page_size", [], |r| r.get(0))?,
            transcript: Transcript {
                boundary_id: None,
                sources: Vec::new(),
                next: None,
                skipped_structured_sources: 0,
                #[cfg(test)]
                bytes_read: 0,
            },
        })
    }

    // SQLite can read overflow bytes directly or through its pager. Charge
    // requested bytes and pager misses conservatively, including metadata and
    // index pages. Reserve space for a B-tree seek and partial end fragments.
    fn available(&self, conn: &Connection) -> usize {
        let misses = cache_misses(conn).wrapping_sub(self.initial_misses) as usize;
        let spent = misses
            .saturating_mul(self.page_size)
            .saturating_add(self.payload_read);
        MAX_SCAN_BYTES
            .saturating_sub(spent)
            .saturating_sub(32 * self.page_size + 2 * BLOCK)
            / 2
    }

    fn visit_row(&mut self, conn: &Connection, position: impl FnOnce() -> Position) -> bool {
        if self.rows_left == 0 || self.available(conn) == 0 {
            self.transcript.next = Some(position());
            return false;
        }
        self.rows_left -= 1;
        true
    }

    fn read_source(
        &mut self,
        conn: &Connection,
        len: usize,
        chunked: bool,
        position: Position,
        location: Location,
    ) -> Result<bool> {
        let (kind, offset) = match position.part {
            Part::Tool { offset, .. } => ("tool", offset),
            Part::Text { offset } => ("message", offset),
            Part::Start => unreachable!("source has a selected part"),
        };
        let structured = location
            .tool_name
            .as_deref()
            .and_then(projection_name)
            .is_some();
        ensure!(
            !structured || offset == 0,
            "Invalid structured result cursor"
        );
        if structured && len > MAX_CHUNK_BYTES {
            self.transcript.skipped_structured_sources += 1;
            return Ok(true);
        }
        if offset > 0 && offset >= len {
            // The source may have been replaced between pages.
            return Ok(true);
        }
        let start = offset.saturating_sub(OVERLAP_BYTES);
        let count = (len - start).min(MAX_CHUNK_BYTES).min(self.available(conn));
        let truncated = start + count < len;
        if truncated && (structured || count <= offset - start + 4) {
            self.transcript.next = Some(position);
            return Ok(false);
        }
        let id = location
            .tool_call_id
            .as_deref()
            .unwrap_or(&location.message_id);
        let (bytes, charged) =
            super::context_search_index::read_bytes(conn, kind, id, chunked, start, count)?;
        self.payload_read += charged;
        let mut head = 0;
        if start > 0 {
            while head < (offset - start).min(3) && bytes[head] & 0xc0 == 0x80 {
                head += 1;
            }
        }
        let mut end = count;
        if truncated {
            if let Some(chunk) = bytes[head..].utf8_chunks().last() {
                let invalid = chunk.invalid();
                if std::str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none()) {
                    end -= invalid.len();
                }
            }
        }
        let overlap = String::from_utf8_lossy(&bytes[head..offset - start]).len();
        let text = String::from_utf8_lossy(&bytes[head..end]);
        let invalid_utf8 = matches!(text, std::borrow::Cow::Owned(_));
        if truncated {
            let offset = start + end;
            let part = match &position.part {
                Part::Tool { key, .. } => Part::Tool {
                    key: key.clone(),
                    offset,
                },
                Part::Text { .. } => Part::Text { offset },
                Part::Start => unreachable!(),
            };
            self.transcript.next = Some(Position {
                message: position.message.clone(),
                part,
            });
        }
        self.transcript.sources.push(Source {
            position,
            location,
            text: text.into_owned(),
            overlap,
            invalid_utf8,
            preceded: start + head > 0,
            truncated,
        });
        Ok(!truncated)
    }
}

/// Each scan has exclusive use of its pooled connection. The SQLite handle
/// lives at least as long as the reference and both output pointers are valid.
fn cache_misses(conn: &Connection) -> u32 {
    let mut misses = 0;
    let mut highwater = 0;
    unsafe {
        rusqlite::ffi::sqlite3_db_status(
            conn.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
            &mut misses,
            &mut highwater,
            0,
        );
    }
    misses as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{context_search_index, schema::SCHEMA_SQL};

    #[test]
    fn context_search_cold_reads_skip_large_rows_and_seek_directly_to_a_deep_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy.db");
        let mut writer = Connection::open(&path).unwrap();
        let source: String = writer
            .query_row("SELECT sqlite_source_id()", [], |r| r.get(0))
            .unwrap();
        eprintln!("SQLite {} {source}", rusqlite::version());
        writer.execute_batch(SCHEMA_SQL).unwrap();
        writer
            .execute_batch(
                "INSERT INTO conversations(id,created_at,updated_at) VALUES('s','now','now')",
            )
            .unwrap();
        for n in 0..4 {
            writer
                .execute(
                    "INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order)
                VALUES(?1,'s','assistant',CAST(zeroblob(?2) AS TEXT),'now',?3)",
                    params![format!("old-{n}"), 32 * 1024 * 1024, n],
                )
                .unwrap();
        }
        let marker = "deep-visible-marker";
        let deep = 4 * MAX_CHUNK_BYTES - 3 * OVERLAP_BYTES;
        let value = "x".repeat(deep) + marker;
        writer
            .execute(
                "INSERT INTO tool_calls(id,message_id,name,input,output,sort_order)
            VALUES('large','old-3','FixtureTool',zeroblob(2000000),?1,0)",
                [&value],
            )
            .unwrap();
        let tx = writer.transaction().unwrap();
        context_search_index::migrate(&tx).unwrap();
        tx.commit().unwrap();

        let cold_page = |role, position: Option<&Position>| {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA mmap_size=0; PRAGMA cache_size=-256;")
                .unwrap();
            let page_size: usize = db.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
            let misses = cache_misses(&db);
            #[cfg(target_os = "linux")]
            let before = thread_read_bytes();
            let result = read(
                &db,
                "s",
                Selection {
                    range: Range::All,
                    boundary: Boundary::Latest,
                    role: Some(role),
                    tool_name: None,
                    position,
                },
            )
            .unwrap();
            let page_bytes = (cache_misses(&db) - misses) as usize * page_size;
            // Direct overflow reads can bypass the pager. On Linux also measure
            // bytes returned by read/pread on this test thread, with mmap off.
            #[cfg(target_os = "linux")]
            let observed = thread_read_bytes() - before;
            #[cfg(not(target_os = "linux"))]
            let observed = page_bytes;
            eprintln!("cold {role} query: {observed} read bytes, {page_bytes} pager-miss bytes");
            assert!(observed <= MAX_SCAN_BYTES, "{observed}");
            (result, observed)
        };
        let (skipped, skipped_bytes) = cold_page("user", None);
        assert!(skipped.sources.is_empty() && skipped.next.is_none());
        assert!(skipped_bytes < 256 * 1024, "{skipped_bytes}");
        writer
            .execute(
                "INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order)
            VALUES('new','s','user',?1,'now',5)",
                [&value],
            )
            .unwrap();
        for role in ["tool", "user"] {
            let mut page = cold_page(role, None).0;
            for _ in 1..4 {
                page = cold_page(role, Some(&page.next.unwrap())).0;
            }
            let cursor = page.next.unwrap();
            assert!(
                matches!(cursor.part, Part::Tool { offset, .. } | Part::Text { offset } if offset == deep)
            );
            let (last, deep_bytes) = cold_page(role, Some(&cursor));
            assert!(last.next.is_none());
            assert!(last.sources[0].text.ends_with(marker));
            assert!(deep_bytes < 256 * 1024, "{deep_bytes}");
        }

        // Try to read 24 MiB of eligible data plus its metadata on one page.
        // Follow every cursor and account for the intentional overlap exactly once.
        let payload = "p".repeat(MAX_SCAN_BYTES / 4);
        writer
            .execute(
                "UPDATE messages SET content=?1 WHERE role='assistant'",
                [&payload],
            )
            .unwrap();
        let mut consumed = 0;
        let mut cursor = None;
        for turn in 0..10 {
            let (page, _) = cold_page("assistant", cursor.as_ref());
            consumed += page
                .sources
                .iter()
                .map(|source| source.text.len() - source.overlap)
                .sum::<usize>();
            cursor = page.next;
            if turn == 0 {
                assert!(cursor.is_some());
            }
            if cursor.is_none() {
                break;
            }
        }
        assert!(cursor.is_none());
        assert_eq!(consumed, MAX_SCAN_BYTES);

        // Exercise incremental maintenance as well as the legacy backfill.
        writer
            .execute(
                "UPDATE tool_calls SET output=?1 WHERE id='large'",
                ["replacement"],
            )
            .unwrap();
        assert_eq!(cold_page("tool", None).0.sources[0].text, "replacement");
        writer
            .execute("DELETE FROM tool_calls WHERE id='large'", [])
            .unwrap();
        assert!(cold_page("tool", None).0.sources.is_empty());
        assert_eq!(
            writer
                .query_row(
                    "SELECT count(*) FROM context_search_chunks WHERE kind='tool'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    fn thread_read_bytes() -> usize {
        std::fs::read_to_string("/proc/thread-self/io")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("rchar: "))
            .unwrap()
            .parse()
            .unwrap()
    }
}
