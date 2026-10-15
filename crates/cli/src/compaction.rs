use crate::{args::Cli, config};
use anyhow::Result;
use claude_core::{
    cache::FileCache,
    native_engine::provider_manager::ProviderManager,
    runtime::{self, RuntimePaths, SessionStore},
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub async fn run(cli: &Cli, paths: &RuntimePaths, id: &str, instructions: &str) -> Result<()> {
    let db = paths.open_database()?;
    let store = SessionStore::new(db.clone());
    let session = store.resume(id)?;
    let selected = config::select(
        cli,
        paths,
        session.model.as_deref(),
        session.provider.as_deref(),
    )?;
    claude_core::config::OrchestrationConfig::init_global()?;
    let manager =
        ProviderManager::in_memory(selected.providers, paths.data_dir.join("providers.json"));
    let engine = runtime::create_engine(
        paths,
        db.clone(),
        Arc::new(Mutex::new(manager)),
        runtime::default_permissions(),
        Arc::new(FileCache::new()),
    );
    tokio::select! {
        result = claude_core::services::compaction::compact(&engine, &db, id, &selected.provider_id, &selected.model, instructions) => crate::services::print(&result?),
        _ = tokio::signal::ctrl_c() => anyhow::bail!("Compaction cancelled; original history retained"),
    }
}
