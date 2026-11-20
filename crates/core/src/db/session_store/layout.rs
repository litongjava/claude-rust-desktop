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
const GLOBAL_VERSION: i32 = 1;
const SESSION_VERSION: i32 = 1;

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

thread_local! { static IN_SESSION_CALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

pub(super) fn allow_entry() -> Result<()> {
    ensure!(
        !IN_SESSION_CALLBACK.get(),
        "Cannot enter a store operation from a session transaction callback"
    );
    Ok(())
}

pub(super) fn callback<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_SESSION_CALLBACK.set(false);
        }
    }
    allow_entry()?;
    IN_SESSION_CALLBACK.set(true);
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
    let conn = Connection::open_with_flags(
        path,
        flags | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL",
    )?;
    Ok(conn)
}

pub(super) fn verify(conn: &Connection, kind: i32) -> Result<()> {
    let (app, version, journal): (i32, i32, String) = conn.query_row(
        "SELECT application_id,user_version,journal_mode FROM pragma_application_id,pragma_user_version,pragma_journal_mode",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    if app != kind {
        return Err(InvalidFormat(format!("Unexpected SQLite application ID {app}")).into());
    }
    if version != schema_version(kind) {
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
    let mut columns = conn.prepare_cached(
        "SELECT p.name FROM main.sqlite_schema AS t, pragma_table_info(t.name,'main') AS p
         WHERE t.type='table' AND t.name=?1",
    )?;
    for &(table, required) in tables {
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
    if kind == SESSION {
        let seal: String = conn.query_row(
            "SELECT sealed_operation FROM manifest WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if !canonical_uuid(&seal) {
            return Err(InvalidFormat("Invalid session seal".into()).into());
        }
    }
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
    populate(&tx)?;
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

pub(super) fn open_root(root: &Path) -> Result<String> {
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
            let uuid = read_global_uuid(&path)?;
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
    let uuid = read_global_uuid(&path)?;
    ensure!(
        fs::read_to_string(&marker)? == uuid,
        "Global store UUID changed"
    );
    Ok(uuid)
}

fn read_global_uuid(path: &Path) -> Result<String> {
    let conn = open(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    verify(&conn, GLOBAL)?;
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
