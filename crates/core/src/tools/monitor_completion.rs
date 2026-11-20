//! The Monitor owner retries only its selected terminal update.
use rusqlite::{params, Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::{value::RawValue, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

const RETRY_WINDOW: Duration = Duration::from_secs(10);
const BACKOFF: Duration = Duration::from_millis(100);

/// Keep the original JSON string token, including escapes without a Rust char equivalent.
#[derive(Serialize)]
struct TerminalOutput<'a> {
    #[serde(flatten)]
    outcome: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_output: Option<&'a RawValue>,
}

/// Try immediately so startup failures and uncontended exits are visible on
/// return. Only a busy writer needs to move its remaining work off the executor.
pub(super) fn record(db: PathBuf, id: &str, status: &'static str, output: Value) {
    let deadline = Instant::now() + RETRY_WINDOW;
    let write = move |id: &str, retrying: bool| -> rusqlite::Result<()> {
        let mut conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(Duration::ZERO)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Hold the write lock from reading the last edit through committing the outcome.
        let stored = tx
            .query_row(
                "SELECT body -> '$.status', body -> '$.updatedAt',
                    CASE WHEN ?2 AND json_type(body,'$.output')='text' THEN body -> '$.output' END
             FROM tasks WHERE id=?1",
                params![id, retrying],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_status, stored_time, annotation)) = stored else {
            return Ok(());
        };
        let decode_string = |encoded: Option<String>| {
            encoded.and_then(|json| serde_json::from_str::<String>(&json).ok())
        };
        let prior_status = decode_string(stored_status);
        let now = chrono::Utc::now();
        let updated_at = decode_string(stored_time)
            .filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok_and(|stored| stored >= now))
            .unwrap_or_else(|| now.to_rfc3339());
        let annotation = annotation
            .map(RawValue::from_string)
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
        let terminal_output = serde_json::to_string(&TerminalOutput {
            outcome: &output,
            task_output: annotation.as_deref(),
        })
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        // prior_status is the full decoded string from this same write transaction.
        tx.execute(
            "UPDATE tasks SET body=json_set(body,'$.status',?2,'$.output',?3,'$.updatedAt',?4)
             WHERE id=?1 AND json_extract(body,'$.metadata.monitor')=1
             AND (?5 IN ('running','in_progress') OR (?5='pending' AND ?2='failed'))",
            params![id, status, terminal_output, updated_at, prior_status],
        )?;
        tx.commit()
    };
    let Err(mut error) = write(id, false) else {
        return;
    };
    let id = id.to_owned();
    let mut retry = move || loop {
        if !matches!(
            error.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        ) {
            tracing::warn!(%id, %status, %error, "Monitor final status write failed");
            return;
        }
        std::thread::sleep(BACKOFF.min(deadline.saturating_duration_since(Instant::now())));
        if Instant::now() >= deadline {
            tracing::warn!(%id, %status, %error, "Monitor final status write exhausted its retry budget");
            return;
        }
        match write(&id, true) {
            Ok(()) => return,
            Err(next) => error = next,
        }
    };
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn_blocking(retry);
    } else {
        retry();
    }
}
