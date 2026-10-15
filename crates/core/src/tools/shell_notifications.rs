//! Durable, session-local notification claims for managed shell completion.
use anyhow::Result;
use std::path::Path;

pub fn collect_completed(root: &Path, session: &str) -> Result<Vec<String>> {
    let session = uuid::Uuid::parse_str(session)?.to_string();
    let path = root.join("tasks").join(format!("{session}.sqlite3"));
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut connection = rusqlite::Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_millis(100))?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS shell_notification_claims(task_id TEXT PRIMARY KEY)",
    )?;
    let tasks = {
        let mut statement = tx.prepare("SELECT tasks.id, json_extract(body,'$.subject'), json_extract(body,'$.status') FROM tasks WHERE NOT EXISTS (SELECT 1 FROM shell_notification_claims c WHERE c.task_id=tasks.id) AND json_extract(body,'$.metadata.managed_shell')=1 AND json_extract(body,'$.status') IN ('completed','failed','cancelled') ORDER BY tasks.rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut notices = Vec::new();
    for (id, subject, status) in tasks {
        let id = uuid::Uuid::parse_str(&id)?.to_string();
        if !root.join("shell-tasks").join(&session).join(&id).is_dir() {
            continue;
        }
        if tx.execute(
            "INSERT OR IGNORE INTO shell_notification_claims(task_id) VALUES(?1)",
            [&id],
        )? == 0
        {
            continue;
        }
        let title: String = subject
            .as_deref()
            .unwrap_or("Background shell")
            .chars()
            .filter(|ch| !ch.is_control())
            .take(160)
            .collect();
        notices.push(format!("Background shell {status}: {title} ({id}). Output: /tasks records or TaskOutput(task_id=\"{id}\", block=false)."));
    }
    tx.commit()?;
    Ok(notices)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_claim_is_durable_concurrent_and_session_local() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        for status in ["completed", "failed", "cancelled", "running"] {
            let task = crate::tools::tasks::execute_for_shell_owner(
                "TaskCreate",
                serde_json::json!({"subject":status,"metadata":{"managed_shell":true}}),
                root.path(),
                &session,
            )
            .unwrap();
            let id = task["id"].as_str().unwrap();
            std::fs::create_dir_all(root.path().join("shell-tasks").join(&session).join(id))
                .unwrap();
            crate::tools::tasks::execute_for_shell_owner(
                "TaskUpdate",
                serde_json::json!({"id":id,"status":status}),
                root.path(),
                &session,
            )
            .unwrap();
        }
        assert!(
            collect_completed(root.path(), &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .is_empty()
        );
        let notices = std::thread::scope(|scope| {
            let one = scope.spawn(|| collect_completed(root.path(), &session).unwrap());
            let two = scope.spawn(|| collect_completed(root.path(), &session).unwrap());
            [one.join().unwrap(), two.join().unwrap()].concat()
        });
        assert_eq!(notices.len(), 3);
        for status in ["completed", "failed", "cancelled"] {
            assert_eq!(
                notices
                    .iter()
                    .filter(|notice| notice.starts_with(&format!("Background shell {status}:")))
                    .count(),
                1
            );
        }
        assert!(collect_completed(root.path(), &session).unwrap().is_empty());
    }
}
