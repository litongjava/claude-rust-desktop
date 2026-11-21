//! SQLite normally opens (and can create) WAL sidecars even for a read-only
//! connection. Use existing live WAL state or take a private copy of a cold DB.
use super::*;
use rusqlite::{
    backup::{Backup, StepResult},
    ffi,
};
use std::{ffi::c_void, path::PathBuf, ptr};

pub(super) struct Source {
    pub db: Connection,
    // The main-file SHARED lock keeps the last writer from unlinking the WAL
    // while SQLite opens or uses the live snapshot. Drop the connection first.
    _guard: Option<Connection>,
}

impl Source {
    pub fn open(path: &Path, cancel: &dyn Fn() -> Result<()>) -> Result<Self> {
        cancel()?;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        ensure!(
            std::fs::symlink_metadata(path)?.is_file(),
            "Expected a regular database file"
        );
        let path = path.canonicalize()?;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        let mut shm = path.as_os_str().to_owned();
        shm.push("-shm");
        let guard = Connection::open_with_flags(&path, flags)?;
        let mut uri = reqwest::Url::from_file_path(&path)
            .map_err(|_| anyhow::anyhow!("Invalid database path"))?;
        // Acquire SQLite's native main-file lock without starting the pager
        // (a normal SELECT would already open/create the WAL). The private
        // connection owns the sqlite3_file and releases its lock on close.
        let mut file: *mut ffi::sqlite3_file = ptr::null_mut();
        unsafe {
            check(ffi::sqlite3_file_control(
                guard.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                &mut file as *mut _ as *mut c_void,
            ))?;
            ensure!(!file.is_null(), "Missing SQLite main file");
            let io = &*(*file).pMethods;
            check(io.xLock.context("Missing SQLite lock method")?(
                file,
                ffi::SQLITE_LOCK_SHARED,
            ))?;
            // A private SQLite temp database has its own journal mode. Check
            // the original database header's read/write format versions here.
            let mut versions = [0u8; 2];
            let read = io.xRead.context("Missing SQLite read method")?;
            check(read(file, versions.as_mut_ptr().cast(), 2, 18))?;
            ensure!(versions == [2, 2], "Expected WAL journal mode");
        }
        if !wal.try_exists()? {
            // A writer can start and checkpoint under SHARED, but it cannot
            // unlink its WAL. Discard the private pages if a WAL appeared.
            uri.set_query(Some("mode=ro&immutable=1"));
            let offline =
                Connection::open_with_flags(uri.as_str(), flags | OpenFlags::SQLITE_OPEN_URI)?;
            // An empty filename requests SQLite's anonymous, delete-on-close
            // temporary database. It is reclaimed even if the exporter dies.
            let mut copy = Connection::open("")?;
            let copied = (|| -> Result<()> {
                let backup = Backup::new(&offline, &mut copy)?;
                loop {
                    cancel()?;
                    match backup.step(64)? {
                        StepResult::Done => return Ok(()),
                        StepResult::More => (),
                        _ => anyhow::bail!("Could not snapshot closed export database"),
                    }
                }
            })();
            if !wal.try_exists()? {
                copied?;
                return Ok(Self {
                    db: copy,
                    _guard: None,
                });
            }
            cancel()?;
        }
        ensure!(
            std::path::Path::new(&shm).is_file(),
            "Session unavailable: missing live WAL index"
        );
        uri.set_query(Some("mode=ro&readonly_shm=1"));
        let db = Connection::open_with_flags(uri.as_str(), flags | OpenFlags::SQLITE_OPEN_URI)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            db,
            _guard: Some(guard),
        })
    }
}

fn check(code: i32) -> Result<()> {
    ensure!(
        code == ffi::SQLITE_OK,
        "Cannot read SQLite export source: {}",
        ffi::Error::new(code)
    );
    Ok(())
}
