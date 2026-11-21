//! The live provider selection of a CLI session. The snapshot crosses only the
//! authenticated worker socket. Durable state contains the generation, never keys.
pub mod store;
use super::provider_manager::{resolve_model_selection, Provider, ResolvedProvider};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    sync::{Arc, RwLock},
};

/// These two admissions already exist in pre-generation workers. A legacy
/// parent starts a TurnTimer for a new submission or async hook. Waking a
/// retained child requires committing a *new* inbox message. Gate those writes
/// in the same transaction that reserves the cutover, without disturbing the
/// admitted turn or consuming its pending feedback.
pub fn fence_legacy_admissions(conn: &rusqlite::Connection) -> Result<()> {
    crate::services::session_messages::schema(conn)?;
    conn.execute_batch("
        CREATE TRIGGER IF NOT EXISTS cli_pending_provider_turn BEFORE INSERT ON execution_turns
        WHEN EXISTS (SELECT 1 FROM cli_binding_generations
                     WHERE session_id=NEW.conversation_id AND pending IS NOT NULL)
        BEGIN SELECT RAISE(ABORT, 'Provider switch pending; legacy worker is retiring'); END;
        CREATE TRIGGER IF NOT EXISTS cli_pending_provider_inbox BEFORE UPDATE OF state ON session_inbox
        WHEN NEW.state='context_committed' AND EXISTS (
            SELECT 1 FROM cli_binding_generations g
            JOIN session_inbox_consumers c ON c.pid=g.legacy_pid AND c.session_id=NEW.recipient
            WHERE g.pending IS NOT NULL AND (g.session_id=NEW.recipient OR EXISTS (
                SELECT 1 FROM peripheral_settings WHERE key='delegation.'||NEW.recipient
                AND json_extract(value,'$.parent_session_id')=g.session_id)))
        BEGIN SELECT RAISE(ABORT, 'Provider switch pending; inbox retained'); END;
    ")?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BindingSnapshot {
    pub generation: u64,
    // A disconnected observer can display the durable selection even if the
    // original private launch inputs are unavailable. It cannot send requests.
    #[serde(skip)]
    pub observation_only: bool,
    pub provider_id: String,
    pub model: String,
    #[serde(with = "catalog")]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub environment: BTreeMap<String, Option<OsString>>,
}

impl std::fmt::Debug for BindingSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindingSnapshot")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

// ModelConfig deliberately omits request effort in persisted provider records.
// IPC carries it along with the other model options in this in-memory snapshot.
mod catalog {
    use super::*;
    #[derive(Serialize)]
    struct Out<'a> {
        #[serde(flatten)]
        provider: &'a Provider,
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        model_efforts: BTreeMap<&'a str, &'a str>,
    }
    #[derive(Deserialize)]
    struct In {
        #[serde(flatten)]
        provider: Provider,
        #[serde(default)]
        model_efforts: BTreeMap<String, String>,
    }
    pub(super) fn serialize<S: serde::Serializer>(
        providers: &[Provider],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(providers.iter().map(|provider| {
            Out {
                provider,
                model_efforts: provider
                    .models
                    .iter()
                    .filter_map(|m| Some((m.id.as_str(), m.effort.as_deref()?)))
                    .collect(),
            }
        }))
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Provider>, D::Error> {
        let providers = Vec::<In>::deserialize(deserializer)?;
        Ok(providers
            .into_iter()
            .map(|mut wire| {
                for model in &mut wire.provider.models {
                    model.effort = wire.model_efforts.remove(&model.id);
                }
                wire.provider
            })
            .collect())
    }
}

impl BindingSnapshot {
    pub fn resolve(&self, provider: &str, model: &str) -> Option<ResolvedProvider> {
        let provider = self
            .providers
            .iter()
            .find(|p| p.id == provider && p.enabled)?;
        Some(ResolvedProvider {
            model: resolve_model_selection(&provider.models, model)?,
            provider: provider.clone(),
        })
    }

    pub fn active(&self) -> Result<ResolvedProvider> {
        self.resolve(&self.provider_id, &self.model)
            .context("Selected provider/model is unavailable")
    }
}

#[derive(Clone, Default)]
pub struct SessionBinding {
    state: Arc<std::sync::OnceLock<Arc<RwLock<Arc<BindingSnapshot>>>>>,
    authority: Arc<RwLock<Option<BindingAuthority>>>,
}

#[derive(Clone)]
enum BindingAuthority {
    Session(crate::db::session_store::SessionDb),
    Shared(std::sync::Weak<crate::db::DbManager>, String),
}

impl BindingAuthority {
    fn version(&self) -> Result<(u64, bool)> {
        match self {
            Self::Session(shard) => {
                let observed = shard.observe_provider()?;
                Ok((observed.generation, observed.pending.is_some()))
            }
            Self::Shared(db, id) => {
                let db = db.upgrade().context("Session binding closed")?;
                Ok(db.with_conn(|c| c.query_row(
                    "SELECT COALESCE((SELECT generation FROM cli_binding_generations WHERE session_id=?1),0),
                     EXISTS(SELECT 1 FROM cli_binding_generations WHERE session_id=?1 AND pending IS NOT NULL)",
                    [id], |r| Ok((r.get(0)?,r.get(1)?))
                ))??)
            }
        }
    }
}

/// An open CLI session can only own an initialized handle. Both views share
/// the same lock, including when the persistent worker replaces its snapshot.
#[derive(Clone, Debug)]
pub struct ActiveBinding {
    handle: SessionBinding,
    value: Arc<RwLock<Arc<BindingSnapshot>>>,
}

impl ActiveBinding {
    pub fn snapshot(&self) -> Arc<BindingSnapshot> {
        self.value.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl std::ops::Deref for ActiveBinding {
    type Target = SessionBinding;
    fn deref(&self) -> &SessionBinding {
        &self.handle
    }
}

impl std::fmt::Debug for SessionBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionBinding")
            .field("generation", &self.read().map(|s| s.generation))
            .finish()
    }
}

impl SessionBinding {
    pub fn track(&self, db: &Arc<crate::db::DbManager>, session: &str) {
        *self.authority.write().unwrap_or_else(|e| e.into_inner()) =
            Some(BindingAuthority::Shared(Arc::downgrade(db), session.into()));
    }

    pub fn track_session(&self, session: crate::db::session_store::SessionDb) {
        *self.authority.write().unwrap_or_else(|e| e.into_inner()) = Some(BindingAuthority::Session(session));
    }

    fn check_generation(&self, expected: u64) -> Result<()> {
        ensure!(
            !self.read().is_some_and(|s| s.observation_only),
            "Session credentials are unavailable; explicitly select a provider before submitting"
        );
        let authority = self
            .authority
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(authority) = authority {
            let (generation,pending) = authority.version()?;
            ensure!(
                generation == expected,
                "Provider binding changed; resume the current session before submitting"
            );
            ensure!(
                !pending,
                "Provider switch pending; use /continue to observe the legacy job"
            );
        }
        Ok(())
    }

    pub fn read(&self) -> Option<Arc<BindingSnapshot>> {
        Some(
            self.state
                .get()?
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        )
    }

    pub fn initialized(&self) -> Result<ActiveBinding> {
        Ok(ActiveBinding {
            handle: self.clone(),
            value: self.state.get().context("Missing session binding")?.clone(),
        })
    }

    pub fn install(&self, next: BindingSnapshot) -> Result<()> {
        self.install_snapshot(next, false)
    }

    /// Only a frontend restoring a freshly read durable revision can move back
    /// to the active generation after an aborted legacy retirement.
    pub fn restore(&self, next: BindingSnapshot) -> Result<()> {
        let authority = self.authority.read().unwrap_or_else(|e|e.into_inner()).clone()
            .context("Missing binding authority")?;
        let (actual,_) = authority.version()?;
        ensure!(
            actual == next.generation,
            "Session binding changed during refresh"
        );
        self.install_snapshot(next, true)
    }

    fn install_snapshot(&self, mut next: BindingSnapshot, restoring: bool) -> Result<()> {
        // Catalog entries are data. A lease here would retain a previous session
        // or create a cycle back to this handle.
        for provider in &mut next.providers {
            provider.lease = None;
        }
        next.active()?;
        let next = Arc::new(next);
        let cell = self
            .state
            .get_or_init(|| Arc::new(RwLock::new(next.clone())));
        let mut state = cell.write().unwrap_or_else(|e| e.into_inner());
        ensure!(
            restoring || state.generation <= next.generation,
            "Provider binding changed; resume the current session before submitting"
        );
        if restoring || state.generation != next.generation || next.generation == 0 {
            *state = next;
        }
        Ok(())
    }
}

/// A cached resolution is valid only while the handle still has this generation.
#[derive(Clone, Debug)]
pub struct ProviderLease {
    handle: SessionBinding,
    generation: u64,
    pinned: Option<(String, String)>,
}

impl SessionBinding {
    pub fn resolve(&self, pinned: Option<(&str, &str)>) -> Option<ResolvedProvider> {
        self.resolve_in(self.read()?.as_ref(), pinned)
    }

    pub(super) fn resolve_in(
        &self,
        state: &BindingSnapshot,
        pinned: Option<(&str, &str)>,
    ) -> Option<ResolvedProvider> {
        let (id, model) = pinned.unwrap_or((&state.provider_id, &state.model));
        let mut provider = state.resolve(id, model)?;
        provider.provider.lease = Some(ProviderLease {
            handle: self.clone(),
            generation: state.generation,
            pinned: pinned.map(|(id, model)| (id.to_owned(), model.to_owned())),
        });
        Some(provider)
    }
}

impl ResolvedProvider {
    pub fn ensure_current(&self) -> Result<()> {
        if let Some(lease) = &self.provider.lease {
            lease.handle.check_generation(lease.generation)?;
            ensure!(
                lease
                    .handle
                    .read()
                    .is_some_and(|s| s.generation == lease.generation),
                "Provider binding changed while preparing the request; discarded stale request"
            );
        }
        Ok(())
    }

    pub fn pinned(&self, provider: &str, model: &str) -> Option<Self> {
        self.provider
            .lease
            .as_ref()?
            .handle
            .resolve(Some((provider, model)))
    }

    pub fn current(&self) -> Result<Self> {
        Ok(self.current_selection()?.0)
    }

    /// Preserve turn-specific options in their own generation. On rebind,
    /// also replace the raw selector (including a virtual context suffix).
    pub(super) fn current_selection(&self) -> Result<(Self, Option<String>)> {
        let Some(lease) = &self.provider.lease else {
            return Ok((self.clone(), None));
        };
        let state = lease.handle.read().context("Session binding unavailable")?;
        lease.handle.check_generation(state.generation)?;
        if state.generation == lease.generation {
            return Ok((self.clone(), None));
        }
        let pinned = lease.pinned.as_ref().map(|(p, m)| (p.as_str(), m.as_str()));
        let resolved = lease
            .handle
            .resolve_in(&state, pinned)
            .context("Provider unavailable in the current session binding")?;
        Ok((
            resolved,
            Some(pinned.map_or(state.model.as_str(), |(_, m)| m).to_owned()),
        ))
    }

    pub fn binding_snapshot(&self) -> Option<Arc<BindingSnapshot>> {
        self.provider.lease.as_ref()?.handle.read()
    }
}

pub(super) fn account_var_os(state: Option<&BindingSnapshot>, name: &str) -> Option<OsString> {
    match state {
        Some(state) => state.environment.get(name).cloned().flatten(),
        None => std::env::var_os(name),
    }
}

pub(super) fn account_var(state: Option<&BindingSnapshot>, name: &str) -> Result<String> {
    account_var_os(state, name)
        .and_then(|value| value.into_string().ok())
        .context("Account environment variable unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(generation: u64, active: &str) -> BindingSnapshot {
        let providers = ["a", "b"].map(|id| serde_json::from_value(json!({
            "id": id, "name": id, "base_url": format!("http://127.0.0.1/{id}/{generation}"),
            "api_key": format!("fixture-{id}-{generation}"), "enabled": true, "api_format": "openai",
            "models": [{"id":"m","name":"m","enabled":true,"supports_web_search":false,"max_tokens":generation+100}]
        })).unwrap()).into();
        BindingSnapshot {
            generation,
            observation_only: false,
            providers,
            provider_id: active.into(),
            model: "m".into(),
            environment: Default::default(),
        }
    }

    #[tokio::test]
    async fn binding_preserves_alias_and_virtual_context_across_generations() {
        use crate::native_engine::provider_manager::ProviderManager;
        let manager = ProviderManager::in_memory(Vec::new(), Default::default());
        let catalog = |generation| {
            let mut state = snapshot(generation, "a");
            state.providers[1].models[0].id = "deepseek-chat".into();
            state
        };
        manager.binding.install(catalog(1)).unwrap();
        let model = manager
            .resolve_provider("deepseek-v4-pro[1m]")
            .await
            .unwrap();
        manager.binding.install(catalog(2)).unwrap();
        let current = model.current().unwrap();
        assert_eq!(current.model.id, "deepseek-chat");
        assert_eq!(current.model.context_window, Some(1_000_000));
        assert_eq!(current.provider.api_key, "fixture-b-2");
    }

    #[test]
    fn pending_legacy_generation_rejects_new_turn_and_child_delivery_until_retirement() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(r#"
            CREATE TABLE execution_turns(conversation_id TEXT);
            CREATE TABLE cli_binding_generations(session_id TEXT,pending TEXT,legacy_pid INTEGER);
            CREATE TABLE peripheral_settings(key TEXT,value TEXT);
            INSERT INTO execution_turns VALUES('parent');
            INSERT INTO cli_binding_generations VALUES('parent','old-incarnation',123);
            INSERT INTO peripheral_settings VALUES('delegation.child','{"parent_session_id":"parent"}');
        "#).unwrap();
        fence_legacy_admissions(&conn).unwrap();
        conn.execute_batch(
            "
            INSERT INTO session_inbox_consumers VALUES('child','receiver',123);
            INSERT INTO session_inbox(id,sender,recipient,project_source,content,created_at)
            VALUES('message','sender','child','project','queued','now');
        ",
        )
        .unwrap();
        let new_turn = "INSERT INTO execution_turns VALUES('parent')";
        let new_delivery = "UPDATE session_inbox SET state='context_committed' WHERE id='message'";
        assert!(conn.execute(new_turn, []).is_err());
        assert!(conn.execute(new_delivery, []).is_err());
        conn.execute("INSERT INTO execution_turns VALUES('unrelated')", [])
            .unwrap();
        conn.execute("UPDATE cli_binding_generations SET pending=NULL", [])
            .unwrap();
        conn.execute(new_turn, []).unwrap();
        conn.execute(new_delivery, []).unwrap();
    }

    #[test]
    fn binding_wire_preserves_runtime_model_options_without_persisting_them() {
        let mut binding = snapshot(3, "a");
        binding.providers[0].models[0].effort = Some("high".into());
        let wire = serde_json::to_vec(&binding).unwrap();
        let received: BindingSnapshot = serde_json::from_slice(&wire).unwrap();
        assert_eq!(
            received.active().unwrap().model.effort.as_deref(),
            Some("high")
        );
        assert!(
            serde_json::to_value(&binding.providers[0]).unwrap()["models"][0]
                .get("effort")
                .is_none()
        );
    }

    #[test]
    fn generation_changes_refresh_active_and_pinned_consumers_and_reject_late_submits() {
        let handle = SessionBinding::default();
        handle.install(snapshot(1, "a")).unwrap();
        let active = handle.resolve(None).unwrap();
        let pinned = handle.resolve(Some(("a", "m"))).unwrap();

        for generation in 2..10 {
            let id = if generation % 2 == 0 { "b" } else { "a" };
            handle.install(snapshot(generation, id)).unwrap();
            for (cache, expected) in [(&active, id), (&pinned, "a")] {
                assert!(cache.ensure_current().is_err());
                let resolved = cache.current().unwrap();
                resolved.ensure_current().unwrap();
                assert_eq!(
                    resolved.provider.base_url,
                    format!("http://127.0.0.1/{expected}/{generation}")
                );
                assert_eq!(
                    resolved.provider.api_key,
                    format!("fixture-{expected}-{generation}")
                );
                assert_eq!(resolved.model.max_tokens, Some(generation as u32 + 100));
            }
            assert!(handle.install(snapshot(generation - 1, "a")).is_err());
            assert!(handle.install(snapshot(generation + 1, "missing")).is_err());
            assert_eq!(handle.read().unwrap().generation, generation);
        }
    }
}
