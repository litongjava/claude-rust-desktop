//! Explicit per-session tool rules; plan mode remains read-only and deny wins.
use crate::session::Session;
use anyhow::Result;
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

fn words(argument: &str) -> Vec<&str> {
    let argument = argument.trim();
    let (first, rest) = argument
        .split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((argument, ""));
    if first.is_empty() {
        vec![]
    } else if rest.is_empty() {
        vec![first]
    } else {
        vec![first, rest]
    }
}

pub async fn handle(argument: &str, session: &mut Session) -> Result<()> {
    if let Some(action) = argument.trim().strip_prefix("auto-config").filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        let context = crate::services::context(&session.paths)?;
        let action = action.trim();
        let mut config = claude_core::permissions::auto_config::load(&context)?;
        match action {
            "" | "show" => {},
            "off" => {
                config.enabled = false;
                claude_core::permissions::auto_config::save(&context, &config)?;
                if session.permissions.get_mode() == claude_core::permissions::PermissionMode::Auto { session.set_permission_mode("ask").await?; }
            },
            value if value.starts_with("set ") => {
                config = serde_json::from_str(value[4..].trim())?;
                claude_core::permissions::auto_config::save(&context, &config)?;
            },
            _ => anyhow::bail!("Usage: /permissions auto-config [show|set JSON|off]"),
        }
        return crate::services::print(&config);
    }
    let words = words(argument);
    let mut rules = load(session)?;
    match words.as_slice() {
        [] | ["list"] => {},
        [mode] if claude_core::permissions::PermissionMode::parse(mode).is_ok() => session.set_permission_mode(mode).await?,
        [action @ ("allow" | "deny" | "ask" | "remove"), tool] => {
            claude_core::permissions::patterns::validate(tool)?;
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
        _ => anyhow::bail!("Usage: /permissions [list|ask|accept-edits|plan|bypass|dont-ask|auto|auto-config show|auto-config set JSON|auto-config off|allow RULE|deny RULE|ask RULE|remove RULE]. Examples: allow Bash(git status *), deny Read(.env), ask Edit(src/**). Enter rules without outer shell quotes."),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_command_preserves_spaces_in_rule_and_serialization() {
        assert_eq!(
            words(" allow Bash(git status *) "),
            vec!["allow", "Bash(git status *)"]
        );
        assert_eq!(
            words("deny Read(my files/**)"),
            vec!["deny", "Read(my files/**)"]
        );
        let rules = HashMap::from([
            ("Bash(git status *)".to_string(), PermissionAction::Allow),
            ("Read(my files/**)".to_string(), PermissionAction::Deny),
        ]);
        let restored: HashMap<String, PermissionAction> =
            serde_json::from_str(&serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!(rules, restored);
        for rule in restored.keys() {
            claude_core::permissions::patterns::validate(rule).unwrap();
        }
    }
}
