//! SQLite normally opens (and can create) WAL sidecars even for a read-only
//! connection. Use existing live WAL state or take a private copy of a cold DB.
use super::*;
use rusqlite::ffi;
use std::{ffi::c_void, path::PathBuf, ptr};

pub(super) struct Source {
    pub db: Connection,
    // The main-file SHARED lock keeps the last writer from unlinking the WAL
    // while SQLite opens or uses the live snapshot. Drop the connection first.
    _guard: Option<Connection>,
    _cold: Option<tempfile::TempDir>,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
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
            if !wal.try_exists()? {
                // Under SHARED, a writer can start and checkpoint, but cannot
                // remove its WAL. Recheck after copying before accepting bytes.
                let temp = tempfile::tempdir()?;
                let copy = temp.path().join("snapshot.db");
                let mut output = std::fs::File::create_new(&copy)?;
                let mut size = 0;
                check(io.xFileSize.context("Missing SQLite size method")?(
                    file, &mut size,
                ))?;
                let read = io.xRead.context("Missing SQLite read method")?;
                let mut buf = vec![0u8; 64 * 1024];
                let mut offset = 0;
                while offset < size {
                    let n = (size - offset).min(buf.len() as i64) as usize;
                    check(read(file, buf.as_mut_ptr().cast(), n as i32, offset))?;
                    output.write_all(&buf[..n])?;
                    offset += n as i64;
                }
                drop(output);
                if !wal.try_exists()? {
                    return Ok(Self {
                        db: layout::open(&copy, flags)?,
                        _guard: None,
                        _cold: Some(temp),
                    });
                }
            }
        }
        ensure!(
            std::path::Path::new(&shm).is_file(),
            "Session unavailable: missing live WAL index"
        );
        let mut uri = reqwest::Url::from_file_path(path)
            .map_err(|_| anyhow::anyhow!("Invalid database path"))?;
        uri.set_query(Some("mode=ro&readonly_shm=1"));
        let db = Connection::open_with_flags(uri.as_str(), flags | OpenFlags::SQLITE_OPEN_URI)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            db,
            _guard: Some(guard),
            _cold: None,
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
