//! The database is the serialization point for frontend binding mutations.
//! Only a digest and public model options are durable. Runtime credentials stay in memory.
use crate::{db::DbManager, native_engine::session_binding::BindingSnapshot};
use anyhow::{Context, Result};
use rusqlite::Connection;

pub struct Observed {
    pub generation: u64,
    pub provider: String,
    pub model: String,
    pub pending: Option<String>,
    pub digest: Option<String>,
    options: Option<String>,
}

pub fn schema(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS cli_binding_generations(
        session_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, pending TEXT, legacy_pid INTEGER,
        target_provider TEXT, target_model TEXT);
        CREATE TABLE IF NOT EXISTS cli_binding_failures(
        session_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, requested TEXT NOT NULL, restored TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS cli_binding_reservations(
        session_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE, generation INTEGER NOT NULL,
        requester TEXT NOT NULL, effort TEXT, requested TEXT NOT NULL, restored TEXT,
        PRIMARY KEY(session_id,generation));
        CREATE TABLE IF NOT EXISTS cli_binding_revisions(
        session_id TEXT NOT NULL, generation INTEGER NOT NULL, previous INTEGER NOT NULL,
        digest TEXT NOT NULL, options TEXT NOT NULL, PRIMARY KEY(session_id,generation));",
    )?;
    Ok(())
}

pub(crate) fn read(c: &Connection, id: &str) -> Result<Observed> {
    Ok(c.query_row(
        "SELECT COALESCE(g.generation,0),
        COALESCE(CASE WHEN g.pending IS NOT NULL THEN g.target_provider END,c.provider,''),
        COALESCE(CASE WHEN g.pending IS NOT NULL THEN g.target_model END,c.model,''),
        g.pending,r.digest,r.options FROM conversations c
        LEFT JOIN cli_binding_generations g ON g.session_id=c.id
        LEFT JOIN cli_binding_revisions r ON r.session_id=g.session_id AND r.generation=g.generation
        WHERE c.id=?1",
        [id],
        |r| {
            Ok(Observed {
                generation: r.get(0)?,
                provider: r.get(1)?,
                model: r.get(2)?,
                pending: r.get(3)?,
                digest: r.get(4)?,
                options: r.get(5)?,
            })
        },
    )?)
}

pub fn observe(db: &DbManager, id: &str) -> Result<Observed> {
    db.with_conn(|c| {
        schema(c)?;
        read(c, id)
    })?
}

pub fn activation_failure(db: &DbManager, id: &str) -> Result<Option<(u64, String, String)>> {
    use rusqlite::OptionalExtension;
    Ok(db.with_conn(|c| {
        c.query_row(
            "SELECT generation,requested,restored FROM cli_binding_failures WHERE session_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
    })??)
}

/// Prefer this frontend's queued rollback over the latest public notice
/// displayed to reconnecting terminals. Another writer can supersede only the latter.
pub fn frontend_failure(
    c: &Connection,
    id: &str,
    requester: &str,
) -> Result<Option<(u64, String, String, bool)>> {
    use rusqlite::OptionalExtension;
    Ok(c.query_row("SELECT generation,requested,restored,1 AS addressed FROM cli_binding_reservations
        WHERE session_id=?1 AND requester=?2 AND restored IS NOT NULL
        UNION ALL SELECT generation,requested,restored,0 FROM cli_binding_failures WHERE session_id=?1
        ORDER BY addressed DESC,generation LIMIT 1", (id,requester),
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?)
}

pub fn acknowledge(c: &Connection, id: &str, requester: &str, generation: u64) -> Result<()> {
    c.execute(
        "DELETE FROM cli_binding_reservations WHERE session_id=?1 AND requester=?2
        AND generation=?3 AND restored IS NOT NULL",
        (id, requester, generation),
    )?;
    Ok(())
}

/// Retiring the frontend drops its private delivery address. A reserved
/// picker edit still belongs to the worker and must finish activation.
pub fn release_requester(c: &Connection, requester: &str) -> Result<()> {
    if !c.query_row(
        "SELECT EXISTS(SELECT 1 FROM cli_binding_reservations WHERE requester=?1)",
        [requester],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    let tx = crate::db::write_transaction(c)?;
    tx.execute(
        "UPDATE cli_binding_reservations SET requester='' WHERE requester=?1
        AND effort IS NOT NULL AND restored IS NULL",
        [requester],
    )?;
    tx.execute(
        "DELETE FROM cli_binding_reservations WHERE requester=?1",
        [requester],
    )?;
    tx.commit()?;
    Ok(())
}

fn digest(value: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn binding_value(state: &BindingSnapshot) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(state)?;
    value
        .as_object_mut()
        .context("Invalid binding")?
        .remove("generation");
    Ok(value)
}

pub fn fingerprint(state: &BindingSnapshot) -> Result<String> {
    let mut value = binding_value(state)?;
    let catalog = value["providers"]
        .as_array_mut()
        .context("Missing provider catalog")?;
    // Provider identity is the stable ID. Presentation labels are not transport inputs.
    catalog.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    for provider in catalog {
        provider
            .as_object_mut()
            .context("Invalid provider")?
            .remove("name");
    }
    Ok(format!("v2:{}", digest(&value)?))
}

impl Observed {
    pub fn matches(&self, state: &BindingSnapshot) -> Result<bool> {
        Ok(match self.digest.as_deref() {
            None => true,
            Some(saved) if saved.starts_with("v2:") => saved == fingerprint(state)?,
            Some(saved) => saved == digest(&binding_value(state)?)?,
        })
    }

    pub fn restore_options(&self, state: &mut BindingSnapshot) -> Result<()> {
        if let Some(options) = &self.options {
            let (saved, effort): (
                crate::native_engine::provider_manager::ModelConfig,
                Option<String>,
            ) = serde_json::from_str(options)?;
            let model = state
                .providers
                .iter_mut()
                .find(|p| p.id == self.provider)
                .and_then(|p| p.models.iter_mut().find(|m| m.id == saved.id))
                .context("Saved model unavailable")?;
            *model = saved;
            model.effort = effort;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub enum Write {
    Published(u64),
    Unchanged,
    Conflict,
}

pub fn publish(
    c: &Connection,
    id: &str,
    expected: u64,
    state: &BindingSnapshot,
    legacy: Option<(&str, Option<u32>)>,
) -> Result<Write> {
    publish_choice(c, id, expected, state, legacy, "", None)
}

/// Names the binding write boundary for lock-timeout diagnostics.
pub const PUBLISH_LABEL: &str = "session binding update";

/// The inner option represents the picker's model-default effort. An absent
/// outer option leaves the settings untouched, including for non-picker writes.
///
/// A resume can carry an explicit `--model`/`--provider`, so this startup write
/// shares [`crate::db::STARTUP_WRITE_DEADLINE`] and waits for another host's
/// writer instead of failing at the pooled connection's 5 s busy_timeout.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum BindingStore {
    Monolithic,
    Directory,
}

pub fn publish_choice(
    c: &Connection,
    id: &str,
    expected: u64,
    state: &BindingSnapshot,
    legacy: Option<(&str, Option<u32>)>,
    requester: &str,
    picker_effort: Option<Option<&str>>,
) -> Result<Write> {
    publish_choice_with_deadline(
        c,
        id,
        expected,
        state,
        legacy,
        requester,
        picker_effort,
        crate::db::STARTUP_WRITE_DEADLINE,
    )
}

fn publish_choice_with_deadline(
    c: &Connection,
    id: &str,
    expected: u64,
    state: &BindingSnapshot,
    legacy: Option<(&str, Option<u32>)>,
    requester: &str,
    picker_effort: Option<Option<&str>>,
    deadline: std::time::Duration,
) -> Result<Write> {
    crate::db::write_tx(c, PUBLISH_LABEL, deadline, |tx| {
        publish_choice_in(
            tx,
            id,
            expected,
            state,
            legacy,
            requester,
            picker_effort,
            BindingStore::Monolithic,
        )
    })
}

pub(crate) fn publish_choice_in(
    tx: &Connection,
    id: &str,
    expected: u64,
    state: &BindingSnapshot,
    legacy: Option<(&str, Option<u32>)>,
    requester: &str,
    picker_effort: Option<Option<&str>>,
    store: BindingStore,
) -> Result<Write> {
    // Every retried attempt re-reads the generation inside its own
    // transaction, so the compare-and-set cannot publish over a newer writer.
    let mut result = publish_in(tx, id, expected, state, legacy, store)?;
    match result {
        Write::Published(generation) if legacy.is_some() => {
            tx.execute("INSERT INTO cli_binding_reservations(session_id,generation,requester,effort,requested)
                    VALUES(?1,?2,?3,?4,?5)", (id,generation,requester,picker_effort.map(|e| e.unwrap_or("default")),
                    format!("{}::{}",state.provider_id,state.model)))?;
        }
        Write::Unchanged if picker_effort.is_some() && read(tx, id)?.pending.is_some() => {
            result = Write::Conflict;
        }
        Write::Unchanged | Write::Published(_) => {
            if let Some(effort) = picker_effort {
                apply_effort(tx, id, effort, store)?;
            }
        }
        Write::Conflict => {}
    }
    Ok(result)
}

fn apply_effort(c: &Connection, id: &str, effort: Option<&str>, store: BindingStore) -> Result<()> {
    if store == BindingStore::Directory {
        c.execute("INSERT INTO cli_binding_efforts(session_id,revision,effort) VALUES(?1,1,?2)
                   ON CONFLICT(session_id) DO UPDATE SET revision=revision+1,effort=excluded.effort", (id,effort))?;
        return Ok(());
    }
    let changed = c.execute(
        "INSERT INTO peripheral_settings(key,value) VALUES(?1,json_object('effort',?2))
        ON CONFLICT(key) DO UPDATE SET value=json_set(peripheral_settings.value,'$.effort',?2)",
        (format!("cli.session.{id}"), effort),
    )?;
    anyhow::ensure!(changed == 1, "Model picker effort was not saved");
    Ok(())
}

fn publish_in(
    tx: &Connection,
    id: &str,
    expected: u64,
    state: &BindingSnapshot,
    legacy: Option<(&str, Option<u32>)>,
    store: BindingStore,
) -> Result<Write> {
    if store == BindingStore::Monolithic {
        schema(tx)?;
    }
    let current = read(tx, id)?;
    if current.generation != expected {
        return Ok(Write::Conflict);
    }
    anyhow::ensure!(
        !state.observation_only,
        "Unavailable credentials cannot be published as a new binding"
    );
    let digest = fingerprint(state)?;
    if current.digest.is_some()
        && current.matches(state)?
        && current.provider == state.provider_id
        && current.model == state.model
    {
        return Ok(Write::Unchanged);
    }
    if current.pending.is_some() {
        return Ok(Write::Conflict);
    }
    if legacy.is_some() {
        if store == BindingStore::Monolithic {
            crate::native_engine::session_binding::fence_legacy_admissions(tx)?;
        }
        tx.execute_batch("SAVEPOINT validate_pending_provider")?;
    }
    let changed = tx.execute(
        "UPDATE conversations SET provider=?1,model=?2 WHERE id=?3",
        [&state.provider_id, &state.model, id],
    )?;
    anyhow::ensure!(changed == 1, "Provider selection was not applied");
    if legacy.is_some() {
        tx.execute_batch(
            "ROLLBACK TO validate_pending_provider; RELEASE validate_pending_provider",
        )?;
    }
    // A retired or rejected reservation must never acquire the same version as a later writer.
    let latest: u64 = tx.query_row(
        "SELECT COALESCE(MAX(generation),0) FROM cli_binding_revisions WHERE session_id=?1",
        [id],
        |r| r.get(0),
    )?;
    let next = expected
        .max(latest)
        .checked_add(1)
        .context("Binding generation exhausted")?;
    let active = state.active()?;
    let model = active
        .provider
        .models
        .iter()
        .find(|m| m.id == active.model.id)
        .context("Missing selected model")?;
    let options = serde_json::to_string(&(model, &model.effort))?;
    tx.execute("INSERT INTO cli_binding_revisions(session_id,generation,previous,digest,options) VALUES(?1,?2,?3,?4,?5)",(id,next,expected,&digest,options))?;
    tx.execute("INSERT INTO cli_binding_generations(session_id,generation,pending,legacy_pid,target_provider,target_model)
        VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id) DO UPDATE SET generation=excluded.generation,
        pending=excluded.pending,legacy_pid=excluded.legacy_pid,target_provider=excluded.target_provider,target_model=excluded.target_model",
        (id,next,legacy.map(|w|w.0),legacy.and_then(|w|w.1),&state.provider_id,&state.model))?;
    // Keep the active revision and, during retirement, its rollback target.
    // A failed reservation remains the high-water mark until the next publish.
    tx.execute("DELETE FROM cli_binding_revisions WHERE session_id=?1 AND generation<?2 AND generation!=?3",
        (id,next,if legacy.is_some(){expected}else{next}))?;
    tx.execute("DELETE FROM cli_binding_failures WHERE session_id=?1", [id])?;
    Ok(Write::Published(next))
}

/// Called inside the same transaction which allocates the cleared conversation.
pub fn publish_clear(
    tx: &Connection,
    source: &str,
    child: &str,
    state: &BindingSnapshot,
) -> Result<u64> {
    let parent = read(tx, source)?;
    anyhow::ensure!(
        parent.generation == state.generation && parent.pending.is_none(),
        "Clear did not apply: the source binding changed or is pending. Retry the command."
    );
    match publish_in(tx, child, 0, state, None, BindingStore::Monolithic)? {
        Write::Published(generation) => Ok(generation),
        _ => anyhow::bail!("Clear did not apply: the new session binding changed"),
    }
}

/// The worker incarnation and generation must both match the reservation.
/// On activation failure undo its selector write and restore the previous generation
/// with a conditional compensating transaction before returning the original error.
pub fn activate(c: &Connection, id: &str, worker: &str, expected: u64) -> Result<()> {
    activate_on(c, id, worker, expected, BindingStore::Monolithic)
}

pub(crate) fn activate_on(
    c: &Connection,
    id: &str,
    worker: &str,
    expected: u64,
    store: BindingStore,
) -> Result<()> {
    if store == BindingStore::Monolithic {
        schema(c)?;
    }
    let attempt = (|| -> Result<()> {
        let tx = crate::db::write_transaction(c)?;
        let owns: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cli_binding_generations
            WHERE session_id=?1 AND pending=?2 AND generation=?3)",
            (id, worker, expected),
            |r| r.get(0),
        )?;
        if !owns {
            return Ok(());
        }
        let changed = tx.execute(
            "UPDATE conversations SET (provider,model)=(SELECT target_provider,target_model
            FROM cli_binding_generations WHERE session_id=?1) WHERE id=?1",
            [id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "Reserved provider selection was not activated"
        );
        let cleared = tx.execute(
            "UPDATE cli_binding_generations SET pending=NULL,legacy_pid=NULL
            WHERE session_id=?1 AND pending=?2 AND generation=?3",
            (id, worker, expected),
        )?;
        anyhow::ensure!(cleared == 1, "Activated reservation was not cleared");
        use rusqlite::OptionalExtension;
        let effort: Option<String> = tx
            .query_row(
                "SELECT effort FROM cli_binding_reservations
            WHERE session_id=?1 AND generation=?2 AND effort IS NOT NULL",
                (id, expected),
                |r| r.get(0),
            )
            .optional()?;
        if let Some(effort) = effort {
            apply_effort(
                &tx,
                id,
                (effort != "default").then_some(effort.as_str()),
                store,
            )?;
        }
        tx.execute(
            "DELETE FROM cli_binding_reservations WHERE session_id=?1 AND generation=?2",
            (id, expected),
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = attempt {
        // Also handles RAISE(ROLLBACK) and commit failures. The first transaction
        // is gone; a newer reservation cannot be touched by this compensation.
        let tx = crate::db::write_transaction(c)?;
        tx.execute("INSERT OR REPLACE INTO cli_binding_failures(session_id,generation,requested,restored)
            SELECT g.session_id,g.generation,g.target_provider||'::'||g.target_model,COALESCE(c.provider||'::'||c.model,'previous session default')
            FROM cli_binding_generations g JOIN conversations c ON c.id=g.session_id
            WHERE g.session_id=?1 AND g.pending=?2 AND g.generation=?3", (id, worker, expected))?;
        tx.execute("UPDATE cli_binding_generations AS g SET generation=COALESCE(
            (SELECT previous FROM cli_binding_revisions r WHERE r.session_id=g.session_id AND r.generation=g.generation),MAX(g.generation-1,0)),
            pending=NULL,legacy_pid=NULL,(target_provider,target_model)=(SELECT provider,model FROM conversations WHERE id=?1)
            WHERE session_id=?1 AND pending=?2 AND generation=?3",(id,worker,expected))?;
        tx.execute("UPDATE cli_binding_reservations SET effort=NULL,
            restored=(SELECT restored FROM cli_binding_failures WHERE session_id=?1 AND generation=?2)
            WHERE session_id=?1 AND generation=?2 AND EXISTS
            (SELECT 1 FROM cli_binding_failures WHERE session_id=?1 AND generation=?2)", (id,expected))?;
        tx.execute("DELETE FROM cli_binding_reservations WHERE session_id=?1 AND generation=?2 AND requester=''",(id,expected))?;
        tx.commit()?;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn database() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE conversations(id TEXT PRIMARY KEY,provider TEXT,model TEXT);
            INSERT INTO conversations VALUES('session','a','m');
            CREATE TABLE execution_turns(conversation_id TEXT);
            CREATE TABLE peripheral_settings(key TEXT PRIMARY KEY,value TEXT);",
        )
        .unwrap();
        schema(&c).unwrap();
        c
    }

    fn snapshot(provider: &str) -> BindingSnapshot {
        BindingSnapshot {
            generation: 0,
            observation_only: false,
            provider_id: provider.into(),
            model: "m".into(),
            environment: Default::default(),
            providers: vec![serde_json::from_value(json!({
                "id":provider,"name":provider,"enabled":true,"api_format":"openai",
                "base_url":format!("http://127.0.0.1/{provider}"),"api_key":"fixture",
                "models":[{"id":"m","name":"m","enabled":true,"supports_web_search":false}]
            }))
            .unwrap()],
        }
    }

    #[test]
    fn revision_matches_stable_provider_ids_and_rejects_a_changed_key() {
        let c = database();
        let mut a = snapshot("a");
        a.providers.extend(snapshot("b").providers);
        publish(&c, "session", 0, &a, None).unwrap();
        let row = read(&c, "session").unwrap();
        let old_digest = digest(&binding_value(&a).unwrap()).unwrap();
        let mut previous_format = read(&c, "session").unwrap();
        previous_format.digest = Some(old_digest);
        assert!(previous_format.matches(&a).unwrap());
        a.providers.reverse();
        a.providers[0].name = "New label".into();
        assert!(row.matches(&a).unwrap());
        a.providers[1].api_key = "other-fixture-key".into();
        assert!(!row.matches(&a).unwrap());
        assert!(!previous_format.matches(&a).unwrap());
    }

    #[test]
    fn stale_writer_cannot_replace_a_newer_binding() {
        let c = database();
        let a = snapshot("a");
        let b = snapshot("b");
        assert_eq!(
            publish(&c, "session", 0, &a, None).unwrap(),
            Write::Published(1)
        );
        assert_eq!(
            publish(&c, "session", 1, &b, None).unwrap(),
            Write::Published(2)
        );
        assert_eq!(
            publish(&c, "session", 1, &a, None).unwrap(),
            Write::Conflict
        );
        let authoritative = read(&c, "session").unwrap();
        assert_eq!(
            (authoritative.generation, authoritative.provider.as_str()),
            (2, "b")
        );
        assert_eq!(authoritative.digest.unwrap(), fingerprint(&b).unwrap());
    }

    #[test]
    fn identical_effective_binding_keeps_generation() {
        let c = database();
        let mut a = snapshot("a");
        assert_eq!(
            publish(&c, "session", 0, &a, None).unwrap(),
            Write::Published(1)
        );
        a.generation = 1;
        assert_eq!(
            publish(&c, "session", 1, &a, None).unwrap(),
            Write::Unchanged
        );
        a.providers[0].models[0].fast = true;
        assert_eq!(
            publish(&c, "session", 1, &a, None).unwrap(),
            Write::Published(2)
        );
        a.providers[0].api_key = "rotated-fixture".into();
        assert_eq!(
            publish(&c, "session", 2, &a, None).unwrap(),
            Write::Published(3)
        );
    }

    #[test]
    fn plain_observation_preserves_reserved_binding() {
        let c = database();
        publish(&c, "session", 0, &snapshot("b"), Some(("worker", Some(7)))).unwrap();
        let observer = read(&c, "session").unwrap();
        assert_eq!(observer.generation, 1);
        assert_eq!(observer.provider, "b");
        assert_eq!(observer.pending.as_deref(), Some("worker"));
        assert_eq!(
            publish(&c, "session", 1, &snapshot("a"), None).unwrap(),
            Write::Conflict
        );
        assert_eq!(
            c.query_row("SELECT provider FROM conversations", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "a"
        );
        activate(&c, "session", "worker", 1).unwrap();
        let active = read(&c, "session").unwrap();
        assert_eq!(
            (active.generation, active.provider.as_str(), active.pending),
            (1, "b", None)
        );
    }

    #[test]
    fn pending_picker_applies_effort_only_at_activation_even_after_requester_exit() {
        let c = database();
        publish_choice(
            &c,
            "session",
            0,
            &snapshot("b"),
            Some(("worker", Some(7))),
            "picker",
            Some(Some("low")),
        )
        .unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM peripheral_settings", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0
        );
        release_requester(&c, "picker").unwrap();
        activate(&c, "session", "worker", 1).unwrap();
        let effort: String = c
            .query_row(
                "SELECT json_extract(value,'$.effort') FROM peripheral_settings
            WHERE key='cli.session.session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(effort, "low");
        assert_eq!(
            c.query_row("SELECT count(*) FROM cli_binding_reservations", [], |r| r
                .get::<_, u32>(
                0
            ))
            .unwrap(),
            0
        );
    }

    #[test]
    fn requester_receives_rollback_once_after_a_newer_selection() {
        let c = database();
        let b = snapshot("b");
        assert_eq!(
            publish_choice(
                &c,
                "session",
                0,
                &b,
                Some(("worker", Some(7))),
                "chooser",
                Some(Some("low"))
            )
            .unwrap(),
            Write::Published(1)
        );
        c.execute_batch(
            "CREATE TRIGGER reject BEFORE UPDATE OF model ON conversations
            BEGIN SELECT RAISE(ABORT,'rejected activation'); END;",
        )
        .unwrap();
        assert!(activate(&c, "session", "worker", 1).is_err());
        c.execute_batch("DROP TRIGGER reject").unwrap();
        assert_eq!(
            publish(&c, "session", 0, &snapshot("c"), None).unwrap(),
            Write::Published(2)
        );
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM peripheral_settings WHERE key='cli.session.session'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            0
        );
        assert!(frontend_failure(&c, "session", "peer").unwrap().is_none());
        acknowledge(&c, "session", "peer", 1).unwrap();
        assert_eq!(
            frontend_failure(&c, "session", "chooser").unwrap(),
            Some((1, "b::m".into(), "a::m".into(), true))
        );
        acknowledge(&c, "session", "chooser", 1).unwrap();
        assert!(frontend_failure(&c, "session", "chooser")
            .unwrap()
            .is_none());
    }

    #[test]
    fn ignored_activation_clears_reservation_and_keeps_previous_selector() {
        let c = database();
        publish(&c, "session", 0, &snapshot("b"), Some(("worker", Some(7)))).unwrap();
        c.execute_batch("CREATE TRIGGER ignore_selector BEFORE UPDATE OF model ON conversations BEGIN SELECT RAISE(IGNORE); END;").unwrap();
        assert!(activate(&c, "session", "worker", 1).is_err());
        let restored = read(&c, "session").unwrap();
        assert_eq!(
            (
                restored.generation,
                restored.provider.as_str(),
                restored.pending
            ),
            (0, "a", None)
        );
    }

    #[test]
    fn failed_activation_restores_prior_binding_and_never_reuses_reserved_generation() {
        let c = database();
        let a = snapshot("a");
        publish(&c, "session", 0, &a, None).unwrap();
        publish(&c, "session", 1, &snapshot("b"), Some(("worker", Some(7)))).unwrap();
        c.execute_batch(
            "CREATE TRIGGER reject_activation BEFORE UPDATE OF model ON conversations
            WHEN NEW.provider='b' BEGIN SELECT RAISE(ABORT,'activation fault'); END;",
        )
        .unwrap();
        assert!(activate(&c, "session", "worker", 2)
            .unwrap_err()
            .to_string()
            .contains("activation fault"));
        let restored = read(&c, "session").unwrap();
        assert_eq!(
            (
                restored.generation,
                restored.provider.as_str(),
                restored.pending
            ),
            (1, "a", None)
        );
        assert_eq!(restored.digest.unwrap(), fingerprint(&a).unwrap());
        c.execute_batch("DROP TRIGGER reject_activation").unwrap();
        assert_eq!(
            publish(&c, "session", 1, &snapshot("b"), None).unwrap(),
            Write::Published(3)
        );
        assert_eq!(
            publish(&c, "session", 2, &a, None).unwrap(),
            Write::Conflict
        );
    }
}
