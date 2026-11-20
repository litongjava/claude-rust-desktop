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
        crate::db::write_tx(
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
                Ok(())
            },
        )
        .context("Saving session permission mode")
    })??;
    // Mirror the committed state only after the write succeeded.
    manager.commit_mode_update(mode, || Ok(()))?;
    Ok(())
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
