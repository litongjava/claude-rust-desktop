use super::*;
use deepseek_code_core::db::session_store::Projects;
use rusqlite::ffi;
use std::{
    cell::RefCell,
    ffi::{c_char, c_void, CStr},
    sync::mpsc,
    time::Duration,
};

thread_local! {
    static PAUSE: RefCell<Option<(mpsc::Sender<()>,mpsc::Receiver<()>)>>=const {RefCell::new(None)};
}
// SQLite emits TRACE_ROW after it has read the row in S's data transaction.
unsafe extern "C" fn row(_: u32, _: *mut c_void, stmt: *mut c_void, _: *mut c_void) -> i32 {
    let ptr = ffi::sqlite3_sql(stmt.cast());
    if !ptr.is_null()
        && CStr::from_ptr(ptr).to_bytes()
            == b"SELECT content_revision,message_count,updated_at,last_turn_status FROM manifest"
    {
        PAUSE.with(|p| {
            if let Some((ready, go)) = p.take() {
                let _ = ready.send(());
                let _ = go.recv_timeout(Duration::from_secs(8));
            }
        });
    }
    0
}
unsafe extern "C" fn register(
    db: *mut ffi::sqlite3,
    _: *mut *const c_char,
    _: *const ffi::sqlite3_api_routines,
) -> i32 {
    if PAUSE.with(|p| p.borrow().is_some()) {
        ffi::sqlite3_trace_v2(
            db,
            ffi::SQLITE_TRACE_ROW as u32,
            Some(row),
            std::ptr::null_mut(),
        )
    } else {
        ffi::SQLITE_OK
    }
}

#[test]
fn project_removal_racing_a_committed_segment() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Stores::open(dir.path())?;
    let s = store.create(
        "timeline",
        CreateOptions {
            project: Some(ProjectSelection::Workspace(dir.path().into())),
            ..Default::default()
        },
    )?;
    let msg = s.write(|w| {
        let t = w.start_turn()?;
        let m = w.segment(&t)?;
        w.message_content(&m, "before")?;
        Ok(m)
    })?;
    let g = rusqlite::Connection::open(dir.path().join("global.db"))?;
    let project: String = g.query_row("SELECT project_id FROM conversations", [], |r| r.get(0))?;
    let (ready_tx, ready) = mpsc::channel();
    let (go, go_rx) = mpsc::channel();
    let path = dir.path().to_path_buf();
    ensure!(unsafe { ffi::sqlite3_auto_extension(Some(register)) } == ffi::SQLITE_OK);
    let export = std::thread::spawn(move || -> Result<Vec<u8>> {
        PAUSE.with(|p| p.replace(Some((ready_tx, go_rx))));
        let mut bytes = Vec::new();
        Stores::export_session(&path, "timeline", &mut bytes)?;
        Ok(bytes)
    });
    let updates = (|| -> Result<()> {
        ready.recv_timeout(Duration::from_secs(8))?;
        s.write(|w| w.message_content(&msg, "after"))?;
        store.delete_project(&project)?;
        Ok(())
    })();
    let _ = go.send(());
    let output = export.join().unwrap();
    unsafe { ffi::sqlite3_cancel_auto_extension(Some(register)) };
    updates?;
    let bytes = output?;
    let rows = decode(&bytes)?;
    let message = rows
        .iter()
        .find(|r| r["id"] == msg && r["type"] == "message")
        .unwrap();
    assert!(rows[0]["project_id"].is_null());
    assert_eq!(message["content"], "after");
    Ok(())
}

#[test]
fn inconsistent_projection_fails_before_the_header() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Stores::open(root.path())?;
    store.create("ahead", Default::default())?;
    let directory = rusqlite::Connection::open(root.path().join("global.db"))?;
    directory.execute(
        "UPDATE conversations SET content_revision=content_revision+100 WHERE id='ahead'",
        [],
    )?;
    let mut bytes = Vec::new();
    let err = Stores::export_session(root.path(), "ahead", &mut bytes).unwrap_err();
    assert!(err
        .to_string()
        .contains("consistent session export snapshot"));
    assert!(bytes.is_empty());
    Ok(())
}
