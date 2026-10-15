//! Session-local permission mode storage shared by CLI and its worker.
use crate::{
    db::DbManager,
    permissions::{PermissionManager, PermissionMode},
};
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
pub fn parse(value: &str) -> Result<PermissionMode> {
    Ok(match value {
        "ask" | "default" => PermissionMode::AskPermissions,
        "accept-edits" | "acceptEdits" => PermissionMode::AcceptEdits,
        "plan" => PermissionMode::PlanMode,
        "bypass" | "bypassPermissions" => PermissionMode::BypassPermissions,
        _ => bail!("Unknown permission mode: {value}"),
    })
}
pub fn name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AskPermissions => "ask",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::PlanMode => "plan",
        PermissionMode::BypassPermissions => "bypass",
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
    db.with_conn(|conn| conn.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("cli.permission-mode.{session}"), name(mode).into()]))??;
    manager.set_mode(mode);
    Ok(())
}
