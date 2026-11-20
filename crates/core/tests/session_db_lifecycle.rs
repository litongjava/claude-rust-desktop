use anyhow::{bail, Result};
use deepseek_code_core::db::session_store::{
    CreateOptions, FirstMessage, Phase, ProjectSelection, Stores,
};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const ID: &str = "12345678-abcd-4abc-89ef-123456789abc";
const CHILD_ENV: &str = "I46_LIFECYCLE_CHILD_DIR";
const WAIT: Duration = Duration::from_secs(20);

fn global(root: &Path) -> Connection {
    Connection::open_with_flags(root.join("global.db"), OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
}
fn shard(root: &Path) -> PathBuf {
    root.join("sessions").join(ID).join("session.db")
}
fn query<T: rusqlite::types::FromSql>(db: &Connection, sql: &str) -> T {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn first_message() -> FirstMessage {
    FirstMessage {
        text: "first line\n第二行".into(),
        typed: Some(serde_json::json!([{"type":"text","text":"第一"}])),
    }
}
fn first() -> CreateOptions {
    CreateOptions {
        first_message: Some(first_message()),
        ..Default::default()
    }
}
fn stop_at(stores: &Stores, expected: Phase) -> Stores {
    stores.with_fault_hook(move |phase| {
        if phase == expected {
            bail!("interrupt at {phase:?}")
        }
        Ok(())
    })
}
fn pending(root: &Path) -> i64 {
    query(
        &global(root),
        "SELECT count(*) FROM lifecycle WHERE quarantine_reason IS NULL",
    )
}
fn count(root: &Path, path: &str) -> usize {
    fs::read_dir(root.join(path)).unwrap().count()
}

#[test]
fn concurrent_first_open_shares_one_directory_and_pool() {
    let tmp = tempdir().unwrap();
    let gate = Arc::new(Barrier::new(8));
    let stores: Vec<_> = thread::scope(|scope| {
        let mut jobs = vec![];
        for _ in 0..8 {
            let gate = gate.clone();
            let root = tmp.path().join(".");
            jobs.push(scope.spawn(move || {
                gate.wait();
                Stores::open_for_test(&root).unwrap()
            }));
        }
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    for s in &stores {
        assert!(std::ptr::eq(s.global(), stores[0].global()));
    }
    assert_eq!(
        query::<i64>(&global(tmp.path()), "SELECT count(*) FROM store_identity"),
        1
    );
}

#[test]
fn two_creates_racing_for_one_id_publish_exactly_once() {
    let tmp = tempdir().unwrap();
    let stores = Stores::open_for_test(tmp.path()).unwrap();
    let gate = Barrier::new(2);
    let outcomes = thread::scope(|scope| {
        let jobs: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    gate.wait();
                    stores.create(ID, first())
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(outcomes.iter().filter(|s| s.is_ok()).count(), 1);
    assert_eq!(pending(tmp.path()), 0);
    assert_eq!(
        stores
            .open_session(ID)
            .unwrap()
            .read(|r| r.initial_message())
            .unwrap(),
        Some(first_message())
    );
}

#[test]
fn legacy_database_wal_and_shm_remain_untouched() {
    let tmp = tempdir().unwrap();
    let names = [
        "deepseek_code.db",
        "deepseek_code.db-wal",
        "deepseek_code.db-shm",
    ];
    for name in names {
        fs::write(tmp.path().join(name), name).unwrap();
    }
    #[cfg(target_os = "linux")]
    let watch = {
        use rustix::fs::inotify::*;
        let w = init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).unwrap();
        for name in names {
            add_watch(&w, tmp.path().join(name), WatchFlags::ALL_EVENTS).unwrap();
        }
        w
    };
    let run = || {
        let s = Stores::open_for_test(tmp.path()).unwrap();
        let session = s.create(ID, first()).unwrap();
        session.set_archived(true).unwrap();
        drop(s.open_session(ID).unwrap());
        session.delete().unwrap();
    };
    run();
    #[cfg(target_os = "linux")]
    {
        let mut buffer = [std::mem::MaybeUninit::uninit(); 512];
        let mut events = rustix::fs::inotify::Reader::new(watch, &mut buffer);
        assert!(
            matches!(events.next(), Err(rustix::io::Errno::AGAIN)),
            "legacy file was accessed"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in names {
            fs::set_permissions(tmp.path().join(name), fs::Permissions::from_mode(0)).unwrap();
        }
        run();
        for name in names {
            fs::set_permissions(tmp.path().join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    for name in names {
        assert_eq!(fs::read(tmp.path().join(name)).unwrap(), name.as_bytes());
    }
}

#[test]
fn exact_logical_ids_have_distinct_safe_storage_keys() {
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    for id in [
        ID,
        &ID.to_uppercase(),
        "../outside",
        "/absolute/path",
        "a\\..\\b",
        ".staging",
        "CON",
        "a\0b",
        "e\u{301}",
        "é",
        &"é".repeat(128),
    ] {
        let session = s.create(id, Default::default()).unwrap();
        let key = if id == ID {
            id.into()
        } else {
            format!("sha256-{:x}", Sha256::digest(id.as_bytes()))
        };
        let file = tmp.path().join("sessions").join(&key).join("session.db");
        assert!(tmp.path().join("session-locks").join(key).is_file());
        let c = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(query::<String>(&c, "SELECT id FROM manifest"), id);
        assert_eq!(
            s.open_session(id).unwrap().incarnation(),
            session.incarnation()
        );
    }
    for id in ["", &"é".repeat(129)] {
        assert!(s.create(id, Default::default()).is_err());
    }
    assert!(!tmp.path().join("outside").exists());
}

#[test]
fn archive_and_stale_handles_are_fenced_by_incarnation() {
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    let old = s.create(ID, first()).unwrap();
    let previous = old.incarnation().to_owned();
    old.set_archived(true).unwrap();
    old.set_archived(true).unwrap();
    assert!(s.global().conversation(ID).unwrap().unwrap().archived);
    let g = global(tmp.path());
    assert_eq!(query::<i64>(&g, "SELECT archived FROM conversations"), 1);
    assert_eq!(
        query::<i64>(&g, "SELECT metadata_revision FROM conversations"),
        1
    );
    assert_eq!(
        s.open_session(ID)
            .unwrap()
            .read(|r| r.initial_message())
            .unwrap(),
        Some(first_message())
    );
    old.set_archived(false).unwrap();
    assert_eq!(query::<i64>(&g, "SELECT archived FROM conversations"), 0);
    assert_eq!(old.write(|tx| tx.synchronous_for_test()).unwrap(), 1);
    old.delete().unwrap();
    assert!(s.open_session(ID).is_err());
    let new = s.create(ID, Default::default()).unwrap();
    assert_ne!(previous, new.incarnation());
    assert!(old.read(|r| r.initial_message()).is_err());
    assert!(old.write(|_| Ok(())).is_err());
    assert!(old.set_archived(true).is_err());
    assert!(old.delete().is_err());
    assert_eq!(new.read(|r| r.initial_message()).unwrap(), None);
    assert_eq!(
        query::<i64>(&g, "SELECT message_count FROM conversations"),
        0
    );
}

#[test]
fn invalid_session_files_fail_closed_without_recreation() {
    for damage in [
        "missing",
        "corrupt",
        "kind",
        "newer",
        "wal",
        "store",
        "id",
        "incarnation",
    ] {
        let tmp = tempdir().unwrap();
        let s = Stores::open_for_test(tmp.path()).unwrap();
        let h = s.create(ID, first()).unwrap();
        let path = shard(tmp.path());
        match damage {
            "missing" => fs::remove_file(&path).unwrap(),
            "corrupt" => fs::write(&path, "invalid SQLite").unwrap(),
            other => {
                let c = Connection::open(&path).unwrap();
                c.execute_batch(match other {
                    "kind" => "PRAGMA application_id=0x44434c47",
                    "newer" => "PRAGMA user_version=2",
                    "wal" => "PRAGMA journal_mode=delete",
                    "store" => "UPDATE manifest SET store_uuid='other'",
                    "id" => "UPDATE manifest SET id='other'",
                    _ => "UPDATE manifest SET incarnation='other'",
                })
                .unwrap();
            }
        }
        assert!(s.open_session(ID).is_err(), "{damage}");
        assert!(h.read(|_| Ok(())).is_err(), "{damage}");
        assert!(h.write(|_| Ok(())).is_err(), "{damage}");
        if damage == "missing" {
            assert!(!path.exists());
        }
    }
}

#[test]
fn invalid_global_files_fail_closed_without_reinitialization() {
    for damage in ["missing", "corrupt", "kind", "newer", "empty", "wal"] {
        let tmp = tempdir().unwrap();
        drop(Stores::open_for_test(tmp.path()).unwrap());
        let path = tmp.path().join("global.db");
        match damage {
            "missing" => fs::remove_file(&path).unwrap(),
            "corrupt" => fs::write(&path, "invalid global").unwrap(),
            "empty" => fs::write(&path, []).unwrap(),
            d => global(tmp.path())
                .execute_batch(match d {
                    "kind" => "PRAGMA application_id=0x44434c53",
                    "newer" => "PRAGMA user_version=2",
                    _ => "PRAGMA journal_mode=delete",
                })
                .unwrap(),
        }
        assert!(Stores::open_for_test(tmp.path()).is_err(), "{damage}");
        if damage == "missing" {
            assert!(!path.exists());
        }
    }
}

#[test]
fn recover_every_create_boundary() {
    use Phase::*;
    for phase in [
        BeforeCreateIntentCommit,
        CreateIntent,
        StageDirectory,
        BeforeSeal,
        Sealed,
        Checkpointed,
        Renamed,
        Installed,
        BeforePublishCommit,
        Published,
    ] {
        let tmp = tempdir().unwrap();
        let s = Stores::open_for_test(tmp.path()).unwrap();
        let result = stop_at(&s, phase).create(ID, first());
        assert!(
            format!("{:#}", result.err().unwrap()).contains("interrupt at"),
            "{phase:?}"
        );
        assert_eq!(
            query::<i64>(&global(tmp.path()), "SELECT count(*) FROM conversations"),
            i64::from(phase == Published)
        );
        drop(s);
        let reopened = Stores::open_for_test(tmp.path()).unwrap();
        assert_eq!(pending(tmp.path()), 0);
        assert_eq!(count(tmp.path(), "sessions/.staging"), 0);
        if matches!(
            phase,
            BeforeCreateIntentCommit | CreateIntent | StageDirectory | BeforeSeal
        ) {
            assert!(reopened.open_session(ID).is_err());
            assert!(!shard(tmp.path()).exists());
        } else {
            let h = reopened.open_session(ID).unwrap();
            assert_eq!(
                h.read(|r| r.initial_message()).unwrap(),
                Some(first_message())
            );
            assert_eq!(
                query::<String>(&global(tmp.path()), "SELECT incarnation FROM conversations"),
                h.incarnation()
            );
            assert_eq!(
                query::<i64>(
                    &global(tmp.path()),
                    "SELECT message_count FROM conversations"
                ),
                1
            );
            assert!(reopened.create(ID, Default::default()).is_err());
        }
    }
}

#[test]
fn recover_every_delete_boundary_without_resurrection() {
    use Phase::*;
    for phase in [
        BeforeDeleteIntentCommit,
        DeleteIntent,
        TrashRenamed,
        Trashed,
        Removed,
        Retired,
    ] {
        let tmp = tempdir().unwrap();
        let s = Stores::open_for_test(tmp.path()).unwrap();
        let crash = stop_at(&s, phase);
        let h = crash.create(ID, first()).unwrap();
        let old = h.incarnation().to_owned();
        assert!(format!("{:#}", h.delete().unwrap_err()).contains("interrupt at"));
        if phase == BeforeDeleteIntentCommit {
            drop((h, s, crash));
            let reopened = Stores::open_for_test(tmp.path()).unwrap();
            assert_eq!(reopened.open_session(ID).unwrap().incarnation(), old);
            assert_eq!(pending(tmp.path()), 0);
            continue;
        }
        assert!(s.open_session(ID).is_err());
        drop((h, s, crash));
        let reopened = Stores::open_for_test(tmp.path()).unwrap();
        assert!(reopened.open_session(ID).is_err());
        assert_eq!(pending(tmp.path()), 0);
        assert_eq!(count(tmp.path(), "sessions/.trash"), 0);
        assert!(!shard(tmp.path()).exists());
        assert_ne!(
            reopened
                .create(ID, Default::default())
                .unwrap()
                .incarnation(),
            old
        );
    }
}

#[test]
fn publication_rechecks_project_trust_and_quarantines_conflict() {
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    let workspace = tmp.path().join("project");
    fs::create_dir(&workspace).unwrap();
    s.create(
        "a",
        CreateOptions {
            project: Some(ProjectSelection::Workspace(workspace)),
            ..Default::default()
        },
    )
    .unwrap();
    let g = global(tmp.path());
    let project: String = query(&g, "SELECT project_id FROM conversations WHERE id='a'");
    assert!(stop_at(&s, Phase::Installed)
        .create(
            ID,
            CreateOptions {
                project: Some(ProjectSelection::Existing(project)),
                ..first()
            }
        )
        .is_err());
    g.execute("UPDATE project_folders SET trusted=0", [])
        .unwrap();
    drop(s);
    let reopened = Stores::open_for_test(tmp.path()).unwrap();
    assert!(reopened.open_session(ID).is_err());
    assert!(reopened.open_session("a").is_ok());
    assert!(!shard(tmp.path()).exists());
    assert_eq!(pending(tmp.path()), 0);
    assert_eq!(
        query::<i64>(
            &g,
            "SELECT count(*) FROM lifecycle WHERE quarantine_reason IS NOT NULL"
        ),
        1
    );
    assert_eq!(count(tmp.path(), "sessions/.quarantine"), 1);
}

#[test]
fn orphaned_session_never_becomes_visible() {
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    s.create(ID, first()).unwrap();
    global(tmp.path())
        .execute("DELETE FROM conversations", [])
        .unwrap();
    drop(s);
    let reopened = Stores::open_for_test(tmp.path()).unwrap();
    assert!(reopened.open_session(ID).is_err());
    assert!(!shard(tmp.path()).exists());
    assert_eq!(count(tmp.path(), "sessions/.quarantine"), 1);
}

#[test]
fn damaged_or_foreign_seals_are_quarantined_while_other_sessions_open() {
    for damage in ["bytes", "operation", "incarnation", "newer"] {
        let tmp = tempdir().unwrap();
        let s = Stores::open_for_test(tmp.path()).unwrap();
        s.create("other", first()).unwrap();
        assert!(stop_at(&s, Phase::Sealed).create(ID, first()).is_err());
        let stage = fs::read_dir(tmp.path().join("sessions/.staging"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("session.db");
        if damage == "bytes" {
            fs::write(&stage, "invalid sqlite").unwrap();
        } else {
            Connection::open(&stage)
                .unwrap()
                .execute_batch(match damage {
                    "operation" => "UPDATE manifest SET sealed_operation='foreign'",
                    "newer" => "PRAGMA user_version=2",
                    _ => "UPDATE manifest SET incarnation='foreign'",
                })
                .unwrap();
        }
        drop(s);
        let s = Stores::open_for_test(tmp.path()).unwrap();
        assert!(s.open_session(ID).is_err());
        assert!(s.open_session("other").is_ok());
        assert_eq!(pending(tmp.path()), 0);
        assert_eq!(count(tmp.path(), "sessions/.quarantine"), 1);
        assert_eq!(
            query::<i64>(
                &global(tmp.path()),
                "SELECT count(*) FROM lifecycle WHERE quarantine_reason IS NOT NULL"
            ),
            1
        );
    }
}

#[test]
fn journal_paths_and_cross_root_identity_are_checked_before_use() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let sa = Stores::open_for_test(a.path()).unwrap();
    let sb = Stores::open_for_test(b.path()).unwrap();
    let a_handle = sa.create(ID, first()).unwrap();
    let b_handle = sb.create(ID, first()).unwrap();
    // Match logical ID and incarnation so that only the store UUID distinguishes the files.
    let c = Connection::open(shard(a.path())).unwrap();
    c.execute(
        "UPDATE manifest SET incarnation=?1",
        [b_handle.incarnation()],
    )
    .unwrap();
    drop(c);
    fs::copy(shard(a.path()), shard(b.path())).unwrap();
    assert!(sb.open_session(ID).is_err());
    assert!(b_handle.write(|_| Ok(())).is_err());
    assert!(a_handle.read(|_| Ok(())).is_err());

    assert!(stop_at(&sa, Phase::CreateIntent)
        .create("pending", first())
        .is_err());
    global(a.path())
        .execute("UPDATE lifecycle SET operation='../outside'", [])
        .unwrap();
    fs::write(a.path().join("outside"), "sentinel").unwrap();
    assert!(Stores::open_for_test(a.path()).is_err());
    assert_eq!(fs::read(a.path().join("outside")).unwrap(), b"sentinel");
}

#[test]
fn session_capacity_is_process_wide_and_releases_after_snapshots() {
    let dirs = [tempdir().unwrap(), tempdir().unwrap()];
    let stores: Vec<_> = dirs
        .iter()
        .map(|p| Stores::open_for_test(p.path()).unwrap())
        .collect();
    let handles: Vec<_> = stores
        .iter()
        .map(|s| s.create(ID, first()).unwrap())
        .collect();
    let (entered, observed) = mpsc::channel();
    let mut releases = vec![];
    let mut jobs = vec![];
    for i in 0..17 {
        let (release, wait) = mpsc::channel();
        releases.push(release);
        let entered = entered.clone();
        let h = handles[i % 2].clone();
        jobs.push(thread::spawn(move || {
            h.read(|r| {
                assert_eq!(r.initial_message()?, Some(first_message()));
                entered.send(i)?;
                wait.recv_timeout(WAIT)?;
                Ok(())
            })
        }));
        if i < 16 {
            assert_eq!(observed.recv_timeout(WAIT).unwrap(), i);
        }
    }
    assert!(observed.recv_timeout(Duration::from_millis(200)).is_err());
    // G remains usable while the seventeenth S checkout is waiting.
    stores[0].global().with_read_for_test(|| ()).unwrap();
    releases[0].send(()).unwrap();
    assert_eq!(observed.recv_timeout(WAIT).unwrap(), 16);
    for release in &releases[1..] {
        release.send(()).unwrap();
    }
    for job in jobs {
        job.join().unwrap().unwrap();
    }
}

#[test]
fn global_pool_allows_four_concurrent_checkouts() {
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    let (entered, seen) = mpsc::channel();
    let mut jobs = vec![];
    let mut releases = vec![];
    for i in 0..5 {
        let (release, wait) = mpsc::channel();
        releases.push(release);
        let s = s.clone();
        let entered = entered.clone();
        jobs.push(thread::spawn(move || {
            s.global()
                .with_read_for_test(|| {
                    entered.send(i).unwrap();
                    wait.recv_timeout(WAIT).unwrap()
                })
                .unwrap()
        }));
        if i < 4 {
            assert_eq!(seen.recv_timeout(WAIT).unwrap(), i);
        }
    }
    assert!(seen.recv_timeout(Duration::from_millis(150)).is_err());
    releases[0].send(()).unwrap();
    assert_eq!(seen.recv_timeout(WAIT).unwrap(), 4);
    for release in &releases[1..] {
        release.send(()).unwrap();
    }
    for job in jobs {
        job.join().unwrap();
    }
}

fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.is_file() {
        assert!(
            start.elapsed() < WAIT,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct Worker(Child);
impl Worker {
    fn start(root: &Path, mode: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "subprocess", "--ignored", "--nocapture"])
                .env(CHILD_ENV, root)
                .env("I46_LIFECYCLE_ACTION", mode)
                .spawn()
                .unwrap(),
        )
    }
    fn kill_and_wait(&mut self) {
        self.0.kill().unwrap();
        assert!(!self.0.wait().unwrap().success());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "entry point for externally controlled lifecycle child processes"]
fn subprocess() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(CHILD_ENV).unwrap());
    let mode = std::env::var("I46_LIFECYCLE_ACTION")?;
    eprintln!(
        "child pid={} root={} action={mode}",
        std::process::id(),
        root.display()
    );
    if mode.starts_with("race") {
        fs::write(root.join(&mode), [])?;
        wait_file(&root.join("go"));
        let s = Stores::open_for_test(&root)?;
        let result = s.create(ID, first());
        fs::write(
            root.join(format!("{mode}.result")),
            if result.is_ok() {
                "created"
            } else {
                "rejected"
            },
        )?;
        return Ok(());
    }
    let s = Stores::open_for_test(&root)?;
    let hold = || -> Result<()> {
        fs::write(root.join("ready"), [])?;
        wait_file(&root.join("release"));
        Ok(())
    };
    match mode.as_str() {
        "snapshot" => s.open_session(ID)?.read(|r| {
            assert_eq!(r.initial_message()?, Some(first_message()));
            hold()
        })?,
        "before-seal" | "sealed-wal" | "installed" => {
            let phase = match mode.as_str() {
                "before-seal" => Phase::BeforeSeal,
                "sealed-wal" => Phase::Sealed,
                _ => Phase::Installed,
            };
            s.with_fault_hook(move |at| {
                if at == phase {
                    fs::write(root.join("ready"), [])?;
                    thread::sleep(WAIT);
                }
                Ok(())
            })
            .create(ID, first())?;
        }
        _ => bail!("unknown child action"),
    }
    Ok(())
}

#[test]
fn simultaneous_processes_bootstrap_and_race_create() {
    let tmp = tempdir().unwrap();
    let mut a = Worker::start(tmp.path(), "race-a");
    let mut b = Worker::start(tmp.path(), "race-b");
    wait_file(&tmp.path().join("race-a"));
    wait_file(&tmp.path().join("race-b"));
    fs::write(tmp.path().join("go"), []).unwrap();
    wait_file(&tmp.path().join("race-a.result"));
    wait_file(&tmp.path().join("race-b.result"));
    assert!(a.0.wait().unwrap().success());
    assert!(b.0.wait().unwrap().success());
    let mut outcomes = ["race-a.result", "race-b.result"]
        .map(|name| fs::read_to_string(tmp.path().join(name)).unwrap());
    outcomes.sort();
    assert_eq!(outcomes, ["created", "rejected"]);
    let s = Stores::open_for_test(tmp.path()).unwrap();
    assert_eq!(
        s.open_session(ID)
            .unwrap()
            .read(|r| r.initial_message())
            .unwrap(),
        Some(first_message())
    );
}

#[test]
fn killing_writer_releases_lock_and_replays_only_sealed_work() {
    for mode in ["before-seal", "sealed-wal", "installed"] {
        let tmp = tempdir().unwrap();
        let mut child = Worker::start(tmp.path(), mode);
        wait_file(&tmp.path().join("ready"));
        let lock = fs::File::open(tmp.path().join("session-locks").join(ID)).unwrap();
        assert!(matches!(
            lock.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert_eq!(pending(tmp.path()), 1);
        assert_eq!(
            query::<i64>(&global(tmp.path()), "SELECT count(*) FROM conversations"),
            0
        );
        if mode == "sealed-wal" {
            let stage = fs::read_dir(tmp.path().join("sessions/.staging"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert!(fs::metadata(stage.join("session.db-wal")).unwrap().len() > 32);
        }
        child.kill_and_wait();
        let s = Stores::open_for_test(tmp.path()).unwrap();
        assert_eq!(pending(tmp.path()), 0);
        if mode == "before-seal" {
            assert!(s.open_session(ID).is_err());
            s.create(ID, Default::default()).unwrap();
        } else {
            assert_eq!(
                s.open_session(ID)
                    .unwrap()
                    .read(|r| r.initial_message())
                    .unwrap(),
                Some(first_message())
            );
        }
    }
}

#[test]
fn active_child_snapshot_holds_off_delete_until_connection_closes() {
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    let h = s.create(ID, first()).unwrap();
    let mut child = Worker::start(tmp.path(), "snapshot");
    wait_file(&tmp.path().join("ready"));
    let (started, ready) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let job = thread::spawn(move || {
        started.send(()).unwrap();
        done.send(h.delete()).unwrap();
    });
    ready.recv_timeout(WAIT).unwrap();
    let start = Instant::now();
    assert!(result.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(
        query::<i64>(&global(tmp.path()), "SELECT count(*) FROM conversations"),
        1
    );
    fs::write(tmp.path().join("release"), []).unwrap();
    result.recv_timeout(WAIT).unwrap().unwrap();
    eprintln!(
        "pid={} root={} delete waited {:?} for child snapshot",
        std::process::id(),
        tmp.path().display(),
        start.elapsed()
    );
    job.join().unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(!shard(tmp.path()).exists());
}

#[cfg(windows)]
#[test]
fn windows_sharing_violation_preserves_tombstone_for_retry() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempdir().unwrap();
    let s = Stores::open_for_test(tmp.path()).unwrap();
    let h = s.create(ID, first()).unwrap();
    let file = fs::File::options()
        .read(true)
        .share_mode(3)
        .open(shard(tmp.path()))
        .unwrap();
    assert!(h.delete().is_err());
    assert!(s.open_session(ID).is_err());
    assert_eq!(pending(tmp.path()), 1);
    assert!(Stores::open_for_test(tmp.path())
        .unwrap()
        .open_session(ID)
        .is_err());
    drop(file);
    drop(Stores::open_for_test(tmp.path()).unwrap());
    assert_eq!(pending(tmp.path()), 0);
}

#[test]
fn supported_format_checks_columns_tables_and_live_seal() {
    for change in [
        "DROP TABLE conversations",
        "ALTER TABLE project_folders DROP COLUMN trusted",
    ] {
        let root = tempdir().unwrap();
        drop(Stores::open_for_test(root.path()).unwrap());
        global(root.path()).execute_batch(change).unwrap();
        assert!(Stores::open_for_test(root.path()).is_err(), "{change}");
    }
    for change in [
        "DROP TABLE message_user_content",
        "ALTER TABLE messages DROP COLUMN thinking",
        "UPDATE manifest SET sealed_operation=''",
        "UPDATE manifest SET sealed_operation='bogus'",
    ] {
        let root = tempdir().unwrap();
        let stores = Stores::open_for_test(root.path()).unwrap();
        let h = stores.create(ID, first()).unwrap();
        Connection::open(shard(root.path()))
            .unwrap()
            .execute_batch(change)
            .unwrap();
        assert!(stores.open_session(ID).is_err(), "{change}");
        assert!(h.read(|_| Ok(())).is_err(), "{change}");
        assert!(h.write(|_| Ok(())).is_err(), "{change}");
    }
}

#[test]
fn structural_damage_is_quarantined_before_publication_or_retirement() -> Result<()> {
    for (deleting, change) in [
        (false, "DROP TABLE manifest"),
        (false, "DROP TABLE message_user_content"),
        (false, "DELETE FROM messages"),
        (false, "DELETE FROM message_user_content"),
        (true, "DROP TABLE manifest"),
    ] {
        let root = tempdir()?;
        let stores = Stores::open_for_test(root.path())?;
        stores.create("healthy", first())?;
        let path = if deleting {
            stores.create(ID, first())?;
            assert!(stop_at(&stores, Phase::DeleteIntent)
                .open_session(ID)?
                .delete()
                .is_err());
            shard(root.path())
        } else {
            assert!(stop_at(&stores, Phase::Sealed).create(ID, first()).is_err());
            fs::read_dir(root.path().join("sessions/.staging"))?
                .next()
                .unwrap()?
                .path()
                .join("session.db")
        };
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA foreign_keys=OFF")?;
        db.execute_batch(change)?;
        drop((db, stores));
        for _ in 0..2 {
            let s = Stores::open_for_test(root.path())?;
            assert!(s.open_session(ID).is_err());
            s.open_session("healthy")?;
            assert_eq!(pending(root.path()), 0);
            assert_eq!(
                query::<i64>(
                    &global(root.path()),
                    "SELECT count(*) FROM lifecycle WHERE length(quarantine_reason)>0"
                ),
                1
            );
            assert_eq!(count(root.path(), "sessions/.quarantine"), 1);
            if deleting {
                assert!(s.create(ID, first()).is_err());
            }
        }
    }
    Ok(())
}
