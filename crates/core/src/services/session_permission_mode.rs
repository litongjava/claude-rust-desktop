//! Session-local permission mode storage shared by CLI and its worker.
use crate::{
    db::DbManager,
    permissions::{PermissionManager, PermissionMode},
};
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
pub fn parse(value: &str) -> Result<PermissionMode> {
    PermissionMode::parse(value)
}
pub fn name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AskPermissions => "ask",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::PlanMode => "plan",
        PermissionMode::BypassPermissions => "bypass",
        PermissionMode::DontAsk => "dont-ask",
        PermissionMode::Auto => "auto",
    }
}
pub fn load(db: &DbManager, session: &str) -> Result<Option<PermissionMode>> {
    let value: Option<String> = db.with_conn(|conn| {
        conn.query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("cli.permission-mode.{session}")],
            |row| row.get(0),
        )
        .optional()
    })??;
    value.map(|value| parse(&value)).transpose()
}
pub fn set(
    db: &DbManager,
    session: &str,
    manager: &PermissionManager,
    mode: PermissionMode,
) -> Result<()> {
    db.with_conn(|conn| -> Result<()> {
        crate::db::write_tx_with_commit(
            conn,
            "session permission mode",
            crate::db::STARTUP_WRITE_DEADLINE,
            |tx| {
                if mode == PermissionMode::PlanMode {
                    let previous: Option<String> = tx.query_row(
                        "SELECT value FROM peripheral_settings WHERE key=?1",
                        [format!("cli.permission-mode.{session}")], |row| row.get(0),
                    ).optional()?;
                    // Preserve the origin across worker handoff and repeated plan entry.
                    if previous.as_deref() != Some("plan") {
                        let bypass = manager.bypass_available()
                            && (manager.get_mode() == PermissionMode::BypassPermissions
                                || previous.as_deref() == Some("bypass"));
                        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("plan.return-bypass.{session}"), bypass.to_string()])?;
                    }
                }
                tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("cli.permission-mode.{session}"), name(mode).into()])?;
                manager.commit_mode_update(mode, || Ok(tx.commit()?))
            },
        )
        .context("Saving session permission mode")
    })?
}

pub fn plan_returns_to_bypass(
    db: &DbManager,
    session: &str,
    manager: &PermissionManager,
) -> Result<bool> {
    if !manager.bypass_available() || manager.get_mode() != PermissionMode::PlanMode {
        return Ok(false);
    }
    let value: Option<String> = db.with_conn(|conn| {
        conn.query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("plan.return-bypass.{session}")],
            |row| row.get(0),
        )
        .optional()
    })??;
    Ok(value.as_deref() == Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support;
    use std::time::{Duration, Instant};

    #[test]
    fn concurrent_updates_publish_the_last_commit_under_policy_locks() {
        let temp = tempfile::tempdir().unwrap();
        let db = DbManager::new(temp.path().join("policy.sqlite")).unwrap();
        db.init().unwrap();
        let manager = crate::runtime::default_permissions();
        let session = "shared";
        let initial = PermissionMode::AskPermissions;
        set(&db, session, &manager, initial).unwrap();

        let start = std::sync::Barrier::new(3);
        std::thread::scope(|setters| {
            for mode in [PermissionMode::BypassPermissions, PermissionMode::DontAsk] {
                let gate = &start;
                let db = &db;
                let manager = &manager;
                setters.spawn(move || {
                    gate.wait();
                    set(db, session, manager, mode).unwrap();
                });
            }
            manager
                .commit_mode_update(initial, || {
                    start.wait();
                    // One setter must own the SQLite writer while it waits for
                    // these policy locks. Neither setter can have committed yet.
                    db.with_conn(|probe| -> Result<()> {
                        let _timeout = crate::db::BusyTimeoutGuard::within(probe, Duration::ZERO)?;
                        let until = Instant::now() + Duration::from_secs(3);
                        loop {
                            match crate::db::write_transaction(probe) {
                                Ok(tx) => tx.rollback()?,
                                Err(error)
                                    if error.sqlite_error_code()
                                        == Some(rusqlite::ErrorCode::DatabaseBusy) =>
                                {
                                    break
                                }
                                Err(error) => return Err(error.into()),
                            }
                            assert!(Instant::now() < until, "setter did not retain the writer");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        // A transient observation during the UPDATE is insufficient:
                        // the writer must still be unavailable while publication is blocked.
                        probe.busy_timeout(Duration::from_millis(150))?;
                        let blocked = crate::db::write_transaction(probe).unwrap_err();
                        assert_eq!(
                            blocked.sqlite_error_code(),
                            Some(rusqlite::ErrorCode::DatabaseBusy)
                        );
                        Ok(())
                    })??;
                    assert_eq!(load(&db, session)?, Some(initial));
                    Ok(())
                })
                .unwrap();
        });
        let stored = load(&db, session).unwrap().unwrap();
        assert_ne!(stored, initial);
        assert_eq!(manager.get_mode(), stored);
    }

    #[test]
    fn set_waits_for_another_writer_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("permissions.sqlite");
        let db = DbManager::new(path.clone()).unwrap();
        db.init().unwrap();
        let (holder, release) = test_support::hold_write_lock(path);
        let releaser = test_support::release_after(release, holder, Duration::from_secs(6));
        let manager = crate::runtime::default_permissions();
        let started = Instant::now();
        set(&db, "session", &manager, PermissionMode::AskPermissions).unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(5),
            "set returned after {:?}",
            started.elapsed()
        );
        releaser.join().unwrap();
        assert_eq!(
            load(&db, "session").unwrap(),
            Some(PermissionMode::AskPermissions)
        );
    }
}
