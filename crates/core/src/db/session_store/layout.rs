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

// Lock files are permanent. Unlinking one would allow a second, independent lock.
pub(super) fn lock_keys(root: &Path, mut keys: Vec<&str>, exclusive: bool) -> Result<Vec<File>> {
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .map(|key| {
            let path = root.join("session-locks").join(key);
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            if exclusive {
                file.lock()
            } else {
                file.lock_shared()
            }
            .with_context(|| format!("acquiring lifecycle lock {}", path.display()))?;
            Ok(file)
        })
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
) -> Result<()> {
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
    Ok(())
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
    fs::create_dir_all(root.join("session-locks"))?;
    let locks = lock_keys(root, vec![".directory"], true)?;
    let path = root.join("global.db");
    let marker = root.join("session-locks/.initialized");
    if !path.try_exists()? {
        ensure!(
            !marker.try_exists()?,
            "Global directory database is missing"
        );
        // A fresh G is built privately too. A crash cannot expose empty SQLite
        // pages as an initialized directory. Only this reserved bootstrap path is retried.
        let bootstrap = root.join(".global-staging");
        if bootstrap.try_exists()? {
            fs::remove_dir_all(&bootstrap)?;
        }
        fs::create_dir(&bootstrap)?;
        for name in [
            "sessions/.staging",
            "sessions/.trash",
            "sessions/.quarantine",
        ] {
            fs::create_dir_all(root.join(name))?;
        }
        sync_dir(&root.join("sessions"))?;
        let staged = bootstrap.join("global.db");
        initialize(&staged, GLOBAL, |tx| {
            tx.execute(
                "INSERT INTO store_identity VALUES(1, ?1)",
                [Uuid::new_v4().to_string()],
            )?;
            Ok(())
        })?;
        checkpoint(&staged, GLOBAL)?;
        durable_move(&staged, &path)?;
        fs::remove_dir(bootstrap)?;
    }
    let conn = open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    verify(&conn, GLOBAL)?;
    integrity(&conn)?;
    let store: String = conn.query_row(
        "SELECT store_uuid FROM store_identity WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(canonical_uuid(&store), "Invalid store UUID");
    if marker.try_exists()? {
        ensure!(
            fs::read_to_string(&marker)? == store,
            "Global store UUID changed"
        );
    } else {
        let temp = marker.with_extension("new");
        fs::write(&temp, store.as_bytes())?;
        File::options().write(true).open(&temp)?.sync_all()?;
        durable_move(&temp, &marker)?;
    }
    drop(locks);
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
