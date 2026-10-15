//! Explicit per-session tool rules; plan mode remains read-only and deny wins.
use crate::session::Session;
use anyhow::{ensure, Result};
use claude_core::permissions::rules::PermissionAction;
use std::collections::HashMap;

pub fn load(session: &Session) -> Result<HashMap<String, PermissionAction>> {
    session.store.db.with_conn(|conn| -> Result<_> {
        let mut statement = conn.prepare("SELECT value FROM peripheral_settings WHERE key=?1")?;
        let mut rows = statement.query([format!("cli.permissions.{}", session.id)])?;
        Ok(match rows.next()? {
            Some(row) => serde_json::from_str(&row.get::<_, String>(0)?)?,
            None => HashMap::new(),
        })
    })?
}

pub async fn handle(argument: &str, session: &mut Session) -> Result<()> {
    let words: Vec<_> = argument.split_whitespace().collect();
    let mut rules = load(session)?;
    match words.as_slice() {
        [] | ["list"] => {},
        [mode @ ("ask" | "accept-edits" | "plan" | "bypass")] => session.set_permission_mode(mode).await?,
        [action @ ("allow" | "deny" | "ask" | "remove"), tool] => {
            ensure!(tool.chars().all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | ':')), "Use an exact tool name; shell/path patterns are not supported here");
            match *action {
                "remove" => { rules.remove(*tool); },
                "allow" => { rules.insert((*tool).into(), PermissionAction::Allow); },
                "deny" => { rules.insert((*tool).into(), PermissionAction::Deny); },
                "ask" => { rules.insert((*tool).into(), PermissionAction::Confirm); },
                _ => unreachable!(),
            }
            let value = serde_json::to_string(&rules)?;
            session.store.db.with_conn(|conn| conn.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("cli.permissions.{}",session.id),value]))??;
        }
        _ => anyhow::bail!("Usage: /permissions [list|ask|accept-edits|plan|bypass|allow TOOL|deny TOOL|ask TOOL|remove TOOL]"),
    }
    session
        .permissions
        .set_session_rules(&session.id, rules.clone());
    eprintln!("Permission mode: {}", session.permission_mode());
    for (tool, action) in rules
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>()
    {
        eprintln!("  {tool}: {action:?}");
    }
    Ok(())
}
