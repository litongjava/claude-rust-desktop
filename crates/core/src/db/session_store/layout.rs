use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::path::Path;
use std::sync::{Condvar, Mutex};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct InvalidFormat(pub(super) String);

pub(super) const GLOBAL: i32 = 0x4443_4c47; // DCLG
pub(super) const SESSION: i32 = 0x4443_4c53; // DCLS
pub(super) const GLOBAL_VERSION: i32 = 5;
pub(super) const SESSION_VERSION: i32 = 5;

pub(super) fn canonical_uuid(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

pub(super) fn storage_key(id: &str) -> Result<String> {
    ensure!(
        !id.is_empty() && id.len() <= 256,
        "Expected 1..256 bytes in session ID"
    );
    Ok(if canonical_uuid(id) {
        id.to_owned()
    } else {
        format!("sha256-{:x}", Sha256::digest(id.as_bytes()))
    })
}

pub(super) fn is_storage_key(key: &str) -> bool {
    canonical_uuid(key)
        || key.strip_prefix("sha256-").is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        })
}

thread_local! { static STORE_CALLBACK: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }

pub(super) fn allow_entry() -> Result<()> {
    if let Some(held) = STORE_CALLBACK.get() {
        anyhow::bail!("Cannot enter a store operation from a {held}");
    }
    Ok(())
}

pub(super) fn callback<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    with_checkout("session transaction callback", body)
}

pub(super) fn with_checkout<T>(held: &'static str, body: impl FnOnce() -> Result<T>) -> Result<T> {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STORE_CALLBACK.set(None);
        }
    }
    allow_entry()?;
    STORE_CALLBACK.set(Some(held));
    let _reset = Reset;
    body()
}

pub(super) fn lock_existing(path: &Path, exclusive: bool) -> Result<File> {
    allow_entry()?;
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Opening lifecycle lock {}", path.display()))?;
    if exclusive {
        file.lock()
    } else {
        file.lock_shared()
    }
    .with_context(|| format!("Acquiring lifecycle lock {}", path.display()))?;
    Ok(file)
}

pub(super) fn lock_keys(dir: &Path, mut keys: Vec<&str>, exclusive: bool) -> Result<Vec<File>> {
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .map(|key| lock_existing(&dir.join(key), exclusive))
        .collect()
}

pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    // Windows publishes namespace changes with a write-through move below.
    #[cfg(windows)]
    let _ = dir;
    Ok(())
}

pub(super) fn durable_move(from: &Path, to: &Path) -> Result<()> {
    rename(from, to)?;
    sync_dir(to.parent().unwrap())?;
    sync_dir(from.parent().unwrap())
}

pub(super) fn rename(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn MoveFileExW(old: *const u16, new: *const u16, flags: u32) -> i32;
        }
        let wide = |p: &Path| {
            p.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        let (source, dest) = (wide(from), wide(to));
        // Same-volume, no replace, MOVEFILE_WRITE_THROUGH.
        if unsafe { MoveFileExW(source.as_ptr(), dest.as_ptr(), 8) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(unix)]
    fs::rename(from, to)?;
    Ok(())
}

pub(super) fn open(path: &Path, flags: OpenFlags) -> Result<Connection> {
    open_timed(path, flags, 5000)
}
pub(super) fn open_timed(path: &Path, flags: OpenFlags, timeout_ms: u64) -> Result<Connection> {
    ensure!(
        fs::symlink_metadata(path)
            .with_context(|| format!("inspecting {}", path.display()))?
            .is_file(),
        "{} is not a regular file",
        path.display()
    );
    let conn = Connection::open_with_flags(
        path,
        flags | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(timeout_ms))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL")?;
    Ok(conn)
}

pub(super) fn file_identity(path: &Path) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(path)?;
        ensure!(meta.is_file(), "Expected regular database file");
        Ok((meta.dev(), meta.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
        };
        let file = File::open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
        Ok((
            u64::from(info.dwVolumeSerialNumber),
            u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
        ))
    }
}

/// Bounded checks for an admitted incarnation. Schema edits invalidate the
/// cached format check. Unsealed external history edits require revalidation.
pub(super) fn session_header(c: &Connection) -> Result<(i64, Option<[u8; 16]>)> {
    let (app,version,mode,schema):(i32,i32,String,i64)=c.query_row(
        "SELECT application_id,user_version,journal_mode,schema_version FROM pragma_application_id,pragma_user_version,pragma_journal_mode,pragma_schema_version", [],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(
        app == SESSION && version == SESSION_VERSION && mode == "wal",
        "Unsupported session header"
    );
    let (seal,edit):(String,Option<[u8;16]>)=c.query_row("SELECT sealed_operation,history_edit,updated_at,created_at,message_count,content_revision,last_turn_status
        FROM manifest JOIN projection_state USING(singleton) WHERE singleton=1", [], |r|{
        r.get::<_,String>(2)?; r.get::<_,String>(3)?; r.get::<_,i64>(4)?; r.get::<_,i64>(5)?; r.get::<_,Option<String>>(6)?;
        Ok((r.get(0)?,r.get(1)?))
    })?;
    ensure!(canonical_uuid(&seal), "Invalid session seal");
    Ok((schema, edit))
}

pub(super) fn verify(conn: &Connection, kind: i32) -> Result<()> {
    verify_version(conn, kind, schema_version(kind))
}

pub(super) fn verify_supported_session(c: &Connection) -> Result<()> {
    let version: i32 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        (1..=SESSION_VERSION).contains(&version),
        "Unsupported session schema version"
    );
    verify_version(c, SESSION, version)
}

fn additions(kind: i32) -> &'static str {
    if kind == GLOBAL {
        include_str!("delivery.sql")
    } else {
        include_str!("history.sql")
    }
}

pub(super) fn needs_upgrade(path: &Path, kind: i32) -> Result<bool> {
    let c = open(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(c.query_row("SELECT application_id=?1 AND user_version BETWEEN 1 AND ?2-1 FROM pragma_application_id,pragma_user_version", [kind, schema_version(kind)], |r| r.get(0))?)
}

// Caller owns the exclusive lifecycle lock and has checked the store identity.
pub(super) fn upgrade(path: &Path, kind: i32) -> Result<()> {
    let mut c = open(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        (1..schema_version(kind)).contains(&version),
        "Unsupported upgrade"
    );
    verify_version(&tx, kind, version)?;
    if version == 1 {
        tx.execute_batch(additions(kind))?;
    }
    if kind == GLOBAL && version < 3 {
        tx.execute_batch(include_str!("inbox_v3.sql"))?;
    }
    if (kind == GLOBAL && version < 4) || (kind == SESSION && version < 3) {
        install_state(&tx, kind)?;
    }
    if kind == SESSION {
        if version < 4 {
            install_session_v4(&tx)?;
            super::history::flush_summary(&tx)?;
        }
        let previous: i64 =
            tx.query_row("SELECT transcript_bytes FROM projection_state", [], |r| {
                r.get(0)
            })?;
        tx.execute_batch(include_str!("session_v5.sql"))?;
        tx.execute("UPDATE manifest SET content_revision=content_revision+1 WHERE (SELECT transcript_bytes FROM projection_state)<>?1",[previous])?;
    }
    if kind == GLOBAL {
        install_global_v5(&tx)?;
    }
    tx.pragma_update(None, "user_version", schema_version(kind))?;
    tx.commit()?;
    Ok(())
}

fn install_session_v4(c: &Connection) -> Result<()> {
    c.execute_batch(include_str!("projection.sql"))?;
    // A legacy timestamp cannot attest to bytes read before the upgrade.
    c.execute_batch("ALTER TABLE file_read_stamps ADD COLUMN digest BLOB")?;
    c.execute_batch(include_str!("compaction_target.sql"))?;
    Ok(())
}

fn install_global_v5(c: &Connection) -> Result<()> {
    c.execute_batch(include_str!("shared_domains.sql"))?;
    c.execute_batch(include_str!("global_v5.sql"))?;
    Ok(())
}

fn install_state(c: &Connection, kind: i32) -> Result<()> {
    if kind == GLOBAL {
        c.execute_batch(include_str!("summaries.sql"))?;
    } else {
        c.execute_batch(include_str!("state.sql"))?;
        crate::db::context_search_index::migrate(c)?;
    }
    Ok(())
}

fn verify_version(conn: &Connection, kind: i32, expected: i32) -> Result<()> {
    verify_structure(conn, kind, expected)?;
    if kind == SESSION {
        verify_history(conn)?;
    }
    Ok(())
}

pub(super) fn verify_structure(conn: &Connection, kind: i32, expected: i32) -> Result<()> {
    let (app, version, journal): (i32, i32, String) = conn.query_row(
        "SELECT application_id,user_version,journal_mode FROM pragma_application_id,pragma_user_version,pragma_journal_mode",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    if app != kind {
        return Err(InvalidFormat(format!("Unexpected SQLite application ID {app}")).into());
    }
    if version != expected {
        return Err(InvalidFormat(format!("Unsupported schema version {version}")).into());
    }
    if journal != "wal" {
        return Err(InvalidFormat("Expected WAL journal mode".into()).into());
    }
    let tables: &[(&str, &str)] = if kind == GLOBAL {
        &[
            ("store_identity", "singleton store_uuid"),
            ("projects", "id name description instructions workspace_path is_archived created_at updated_at"),
            ("project_folders", "project_id path trusted sort_order"),
            ("conversations", "id storage_key incarnation title model provider workspace_path project_id workspace_mode research_mode pinned archived metadata_revision created_at updated_at message_count content_revision last_turn_status first_prompt latest_prompt git_branch transcript_bytes"),
            ("lifecycle", "operation store_uuid id storage_key incarnation kind metadata quarantine_reason"),
        ]
    } else {
        &[
            ("manifest", "singleton store_uuid id incarnation sealed_operation created_at updated_at message_count content_revision last_turn_status"),
            ("messages", "id conversation_id role content thinking created_at is_compact_boundary sort_order"),
            ("message_user_content", "message_id content_json"),
        ]
    };
    let added: &[(&str, &str)] = if expected == 1 {
        &[]
    } else if kind == GLOBAL {
        &[
            ("conversation_private_workspace_bindings", "child_id parent_id parent_incarnation"),
            ("conversation_worktree_backends", "conversation_id backend metadata"),
            ("conversation_worktrees", "conversation_id project_id source_path repo_path path branch"),
            ("legacy_session_imports", "id digest project_id incarnation"),
            ("import_members", "storage_key batch operation digest abort_reason"),
            ("session_inbox", "id sender recipient project_source payload size created_at state generation read blocked_reason committed_at"),
            ("session_inbox_consumers", "session_id generation"),
            ("session_inbox_operations", "sender operation_id message_id"),
            ("session_input_queue", "id session_id request_id admission_digest payload mode revision state delivery_id target"),
            ("session_input_queue_control", "session_id paused"),
        ]
    } else {
        &[
            ("attachments", "id message_id file_name file_type mime_type file_size source"),
            ("code_diffs", "id conversation_id message_id file_path original_content modified_content diff_text status applied_at created_at"),
            ("delivery_generations", "delivery_id turn_id"),
            ("child_invocations", "attempt_key parent_message child_incarnation invocation"),
            ("delivery_receipts", "delivery_id digest effect_id kind"),
            ("execution_turns", "id conversation_id source_turn_id started_at ended_at elapsed_ms status"),
            ("mcp_tool_outputs", "conversation_id tool_use_id content_json metadata_json display_text is_error"),
            ("message_trajectory", "message_id requests_json"),
            ("message_turns", "message_id turn_id"),
            ("model_requests", "id message_id conversation_id request_number provider_id model response_model api_format status started_at_ms first_token_at_ms ended_at_ms total_duration_ms ttft_ms generation_ms throughput_tokens_per_second input_tokens output_tokens reasoning_tokens content_tokens total_tokens cache_read_tokens cache_write_tokens max_output_tokens effort finish_reason response_id timing_source error response_blocks_json usage_json"),
            ("native_boundaries", "message_id value"),
            ("compaction_state", "singleton local_count"),
            ("permission_approvals", "id conversation_id message_id tool_name action risk_level status user_decision decision_reason created_at decided_at"),
            ("request_blobs", "hash body"),
            ("session_agents", "conversation_id definition"),
            ("session_permission_authority", "id conversation_id source origin_id content_json"),
            ("tool_call_notes", "tool_call_id note"),
            ("tool_call_origins", "tool_call_id parent_tool_call_id sequence origin"),
            ("tool_call_timings", "tool_call_id duration_ms"),
            ("tool_calls", "id message_id name input output is_error sort_order"),
            ("tool_queued_inputs", "tool_call_id inputs_json"),
        ]
    };
    if kind == GLOBAL && expected >= 3 {
        conn.prepare("SELECT sender_incarnation FROM session_inbox LIMIT 0")?;
    }
    let state: &[(&str, &str)] = if kind == GLOBAL && expected >= 4 {
        &[
            ("global_settings", "domain name value"),
            (
                "conversations",
                "request_count input_tokens output_tokens total_tokens",
            ),
        ]
    } else if kind == SESSION && expected >= 3 {
        let search_objects: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE
             (type='index' AND tbl_name='context_search_sources'
              AND name IN ('context_search_order','context_search_boundary_order')) OR
             (type='trigger' AND tbl_name IN ('messages','tool_calls')
              AND name IN ('context_search_' || tbl_name || '_insert',
                           'context_search_' || tbl_name || '_update',
                           'context_search_' || tbl_name || '_delete'))",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            search_objects == 8,
            "Missing required session search index or trigger"
        );
        &[
            ("peripheral_settings", "key value"),
            ("file_read_stamps", "path modified_ns size"),
            ("compaction_request", "singleton id keep"),
            ("compaction_checkpoint", "fingerprint request_key summary"),
            ("approved_plan_boundaries", "message_id"),
            ("message_metadata", "message_id value"),
            ("request_metadata", "request_id value"),
            (
                "context_search_sources",
                "kind id owner sort_order sequence label stamp boundary size chunked",
            ),
            ("context_search_chunks", "kind id part body"),
        ]
    } else {
        &[]
    };
    let v4: &[(&str, &str)] = if kind == SESSION && expected >= 4 {
        let objects: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE
             (type='index' AND tbl_name='messages' AND name='projection_prompt_order') OR
             (type='trigger' AND (
                (tbl_name='messages' AND name IN ('projection_message_insert','projection_message_update','projection_message_delete','cancel_rewound_compaction'))
                OR (tbl_name='model_requests' AND name IN ('projection_request_insert','projection_request_update','projection_request_delete'))
                OR (tbl_name='message_user_content' AND name IN ('projection_typed_insert','projection_typed_update','projection_typed_delete'))))", [], |r|r.get(0))?;
        ensure!(
            objects == 11,
            "Missing required session summary index or trigger"
        );
        &[("projection_state","singleton transcript_bytes first_prompt latest_prompt prompts_dirty history_edit request_count input_tokens output_tokens total_tokens"),
          ("compaction_request","target_message"), ("compaction_cancellations","id reason"), ("file_read_stamps","digest"), ("request_metric_deltas","direction input output total")]
    } else {
        &[]
    };
    let mut columns = conn.prepare_cached(
        "SELECT p.name FROM main.sqlite_schema AS t, pragma_table_info(t.name,'main') AS p
         WHERE t.type='table' AND t.name=?1",
    )?;
    if kind == SESSION && expected >= 5 {
        let required: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema WHERE
            (type='index' AND tbl_name='execution_turns' AND name='idx_execution_turns_conversation') OR
            (type='trigger' AND tbl_name='messages' AND name IN ('transcript_thinking_insert','transcript_thinking_delete','transcript_thinking_update')) OR
            (type='trigger' AND tbl_name='tool_calls' AND name IN ('transcript_tool_insert','transcript_tool_delete','transcript_tool_update'))",
            [], |r|r.get(0))?;
        ensure!(
            required == 7,
            "Missing session turn index or transcript size trigger"
        );
    }
    let shared = if kind == GLOBAL && expected >= 5 {
        let indexes: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema WHERE tbl_name='conversations' AND
            ((type='index' AND name IN ('directory_order','directory_project_order','directory_activity')) OR
            (type='trigger' AND name='conversation_binding_changed'))", [], |r|r.get(0))?;
        ensure!(
            indexes == 4,
            "Missing global directory index or binding trigger"
        );
        super::global_format::TABLES
    } else {
        &[]
    };
    for &(table, required) in tables
        .iter()
        .chain(added)
        .chain(state)
        .chain(v4)
        .chain(shared)
    {
        let actual = columns
            .query_map([table], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in required.split_ascii_whitespace() {
            if !actual.iter().any(|col| col == name) {
                return Err(
                    InvalidFormat(format!("Missing required column {table}.{name}")).into(),
                );
            }
        }
    }
    Ok(())
}

fn verify_history(conn: &Connection) -> Result<()> {
    let (seal, count): (String, i64) = conn.query_row(
            "SELECT singleton,store_uuid,id,incarnation,sealed_operation,created_at,updated_at,
                    message_count,content_revision,last_turn_status FROM manifest WHERE singleton=1",
            [],
            |row| {
                for field in ["store_uuid", "id", "incarnation", "created_at", "updated_at"] {
                    row.get::<_, String>(field)?;
                }
                row.get::<_, i64>("singleton")?;
                row.get::<_, i64>("content_revision")?;
                row.get::<_, Option<String>>("last_turn_status")?;
                Ok((row.get("sealed_operation")?, row.get("message_count")?))
            },
        )?;
    if !canonical_uuid(&seal) {
        return Err(InvalidFormat("Invalid session seal".into()).into());
    }
    let first = super::SessionRead(conn)
        .initial_message()
        .context("decoding initial message")?;
    ensure!(
        count >= 0 && first.is_some() == (count > 0),
        "Initial message differs from manifest"
    );
    let detached: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM message_user_content t
             WHERE NOT EXISTS(SELECT 1 FROM messages m WHERE m.id=t.message_id))",
        [],
        |row| row.get(0),
    )?;
    ensure!(!detached, "Typed content has no matching message");
    Ok(())
}

fn schema_version(kind: i32) -> i32 {
    if kind == GLOBAL {
        GLOBAL_VERSION
    } else {
        SESSION_VERSION
    }
}

pub(super) fn initialize(
    path: &Path,
    kind: i32,
    populate: impl FnOnce(&Connection) -> Result<()>,
) -> Result<Connection> {
    File::create_new(path)?;
    let mut conn = open(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.execute_batch("PRAGMA journal_mode=WAL")?;
    if kind == SESSION {
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.pragma_update(None, "application_id", kind)?;
    tx.pragma_update(None, "user_version", schema_version(kind))?;
    tx.execute_batch(if kind == GLOBAL {
        include_str!("global.sql")
    } else {
        include_str!("session.sql")
    })?;
    tx.execute_batch(additions(kind))?;
    if kind == GLOBAL {
        tx.execute_batch(include_str!("inbox_v3.sql"))?;
    }
    install_state(&tx, kind)?;
    if kind == SESSION {
        install_session_v4(&tx)?;
        tx.execute_batch(include_str!("session_v5.sql"))?;
    }
    if kind == GLOBAL {
        install_global_v5(&tx)?;
    }
    populate(&tx)?;
    if kind == SESSION {
        super::history::flush_summary(&tx)?;
    }
    tx.commit()?;
    Ok(conn)
}

pub(super) fn checkpoint(path: &Path, kind: i32) -> Result<()> {
    let conn = open(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    verify(&conn, kind)?;
    if kind == SESSION {
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    let busy: i32 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    ensure!(busy == 0, "checkpoint blocked for {}", path.display());
    conn.close().map_err(|(_, e)| e)?;
    File::options()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()?;
    sync_dir(path.parent().unwrap())
}

pub(super) fn open_root(root: &Path) -> Result<(String, bool)> {
    let path = root.join("global.db");
    let stage = root.join(".global-staging");
    let locks = root.join("session-locks");
    let marker = locks.join(".initialized");
    if !path.try_exists()? || stage.try_exists()? {
        // This lock lives outside the removable namespace. Established v1 roots
        // do not need it unless finishing their own interrupted bootstrap.
        let bootstrap = root.join(".session-bootstrap");
        let may_start = !path.try_exists()? && !marker.try_exists()?;
        let guard = File::options()
            .read(true)
            .write(true)
            .create(may_start)
            .truncate(false)
            .open(&bootstrap)?;
        guard.lock()?;
        if !path.try_exists()? {
            ensure!(
                !marker.try_exists()?,
                "Global directory database is missing"
            );
            if stage.try_exists()? {
                // Only files owned by an interrupted bootstrap may be discarded.
                for name in [
                    "global.db",
                    "global.db-wal",
                    "global.db-shm",
                    "global.db-journal",
                    "store-uuid",
                ] {
                    let file = stage.join(name);
                    if file.try_exists()? {
                        fs::remove_file(file)?;
                    }
                }
            } else {
                ensure!(
                    !locks.try_exists()? && !root.join("sessions/.staging").try_exists()?,
                    "Missing global database for existing session layout"
                );
                fs::create_dir(&stage)?;
                sync_dir(root)?;
            }
            fs::create_dir_all(&locks)?;
            for name in [".directory", ".orphan-sweep"] {
                File::options()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(locks.join(name))?
                    .sync_all()?;
            }
            sync_dir(&locks)?;
            for name in [
                "sessions/.staging",
                "sessions/.trash",
                "sessions/.quarantine",
            ] {
                fs::create_dir_all(root.join(name))?;
            }
            sync_dir(&root.join("sessions"))?;
            let staged = stage.join("global.db");
            let uuid = Uuid::new_v4().to_string();
            initialize(&staged, GLOBAL, |tx| {
                tx.execute("INSERT INTO store_identity VALUES(1, ?1)", [&uuid])?;
                Ok(())
            })?;
            checkpoint(&staged, GLOBAL)?;
            let ticket = stage.join("store-uuid");
            fs::write(&ticket, &uuid)?;
            File::options().write(true).open(ticket)?.sync_all()?;
            sync_dir(&stage)?;
            sync_dir(root)?;
            rename(&staged, &path)?;
        }
        if stage.try_exists()? {
            // Replay both sides of the move, even when global.db already exists.
            sync_dir(root)?;
            sync_dir(&stage)?;
            let uuid = read_global_uuid(&path, global_version(&path)?)?;
            if !marker.try_exists()? {
                ensure!(
                    fs::read_to_string(stage.join("store-uuid"))? == uuid,
                    "Bootstrap identity changed"
                );
                let temp = locks.join(".initialized.new");
                fs::write(&temp, &uuid)?;
                File::options().write(true).open(&temp)?.sync_all()?;
                durable_move(&temp, &marker)?;
            }
            ensure!(
                fs::read_to_string(&marker)? == uuid,
                "Global store UUID changed"
            );
            sync_dir(&locks)?;
            let ticket = stage.join("store-uuid");
            if ticket.try_exists()? {
                fs::remove_file(ticket)?;
            }
            fs::remove_dir(stage)?;
            sync_dir(root)?;
        }
    }
    let mut upgraded = false;
    if global_version(&path)? < GLOBAL_VERSION {
        let _guard = lock_existing(&locks.join(".directory"), true)?;
        if global_version(&path)? < GLOBAL_VERSION {
            let c = open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let uuid: String = c.query_row(
                "SELECT store_uuid FROM store_identity WHERE singleton=1",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                fs::read_to_string(&marker)? == uuid,
                "Global store UUID changed"
            );
            drop(c);
            upgrade(&path, GLOBAL)?;
            upgraded = true;
        }
    }
    let uuid = read_global_uuid(&path, GLOBAL_VERSION)?;
    ensure!(
        fs::read_to_string(&marker)? == uuid,
        "Global store UUID changed"
    );
    Ok((uuid, upgraded))
}

fn global_version(path: &Path) -> Result<i32> {
    let c = open(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i32 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        (1..=GLOBAL_VERSION).contains(&version),
        "Unsupported global schema version"
    );
    Ok(version)
}
fn read_global_uuid(path: &Path, version: i32) -> Result<String> {
    let conn = open(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    verify_version(&conn, GLOBAL, version)?;
    integrity(&conn)?;
    let store: String = conn.query_row(
        "SELECT store_uuid FROM store_identity WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(canonical_uuid(&store), "Invalid store UUID");
    Ok(store)
}

// S capacity is shared by every root in the process. Future fork writers
// can reserve both connections in one wait, before opening either snapshot.
static AVAILABLE: Mutex<usize> = Mutex::new(16);
static RELEASED: Condvar = Condvar::new();
pub(super) struct Permits(usize);
impl Permits {
    pub(super) fn try_one() -> Option<Self> {
        let mut free = AVAILABLE.lock().unwrap();
        if *free == 0 {
            return None;
        }
        *free -= 1;
        Some(Self(1))
    }
    pub(super) fn take(count: usize) -> Self {
        let mut free = RELEASED
            .wait_while(AVAILABLE.lock().unwrap(), |free| *free < count)
            .unwrap();
        *free -= count;
        Self(count)
    }
}
impl Drop for Permits {
    fn drop(&mut self) {
        *AVAILABLE.lock().unwrap() += self.0;
        RELEASED.notify_all();
    }
}
pub(super) fn integrity(conn: &Connection) -> Result<()> {
    let result: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if result != "ok" {
        return Err(InvalidFormat(format!("SQLite integrity check failed: {result}")).into());
    }
    Ok(())
}
