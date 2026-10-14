//! Peripheral services shared by CLI and Desktop. Hosts supply credentials/devices.
pub mod lifecycle;
pub mod team;
pub mod team_server;
pub mod voice;

use crate::{db::DbManager, runtime::RuntimePaths};
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;

pub trait Credentials: Send + Sync {
    fn get(&self, name: &str) -> Result<String>;
}

pub struct EnvironmentCredentials;
impl Credentials for EnvironmentCredentials {
    fn get(&self, name: &str) -> Result<String> {
        let value =
            std::env::var(name).with_context(|| format!("Credential {name} is not configured"))?;
        ensure!(!value.trim().is_empty(), "Credential {name} is empty");
        Ok(value)
    }
}

#[derive(Clone)]
pub struct ServiceContext {
    pub paths: RuntimePaths,
    pub db: Arc<DbManager>,
    pub credentials: Arc<dyn Credentials>,
}
impl ServiceContext {
    pub fn open(paths: RuntimePaths, credentials: Arc<dyn Credentials>) -> Result<Self> {
        let db = paths.open_database()?;
        Ok(Self {
            paths,
            db,
            credentials,
        })
    }
    pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let value: Option<String> = self.db.with_conn(|c| {
            c.query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
        })??;
        value
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn save_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, &json],
            )
        })??;
        Ok(())
    }
}

pub fn validate_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(endpoint)?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Endpoint must not contain credentials, query or fragment"
    );
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Use HTTPS or loopback HTTP"
    );
    Ok(url)
}

/// Additive, versioned migrations run inside DbManager's transaction.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS peripheral_migrations(version INTEGER PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS peripheral_settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS service_leases(
            name TEXT PRIMARY KEY,owner TEXT NOT NULL,pid INTEGER NOT NULL,expires_at INTEGER NOT NULL,
            stop_requested INTEGER NOT NULL DEFAULT 0,state TEXT NOT NULL,error TEXT,completed INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS team_memories(
            team TEXT NOT NULL,id TEXT NOT NULL,title TEXT NOT NULL,content TEXT NOT NULL,tags TEXT NOT NULL,
            deleted INTEGER NOT NULL DEFAULT 0,revision INTEGER NOT NULL DEFAULT 0,dirty INTEGER NOT NULL DEFAULT 1,
            mutation TEXT NOT NULL,PRIMARY KEY(team,id));
        CREATE TABLE IF NOT EXISTS team_cursors(team TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS team_conflicts(
            team TEXT NOT NULL,id TEXT NOT NULL,remote TEXT NOT NULL,PRIMARY KEY(team,id));
        INSERT OR IGNORE INTO peripheral_migrations(version) VALUES(1);
        CREATE TABLE IF NOT EXISTS team_import_sources(team TEXT NOT NULL,id TEXT NOT NULL,source TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(team,id));
        INSERT OR IGNORE INTO peripheral_migrations(version) VALUES(2);
        CREATE TABLE IF NOT EXISTS team_bindings(team TEXT PRIMARY KEY,endpoint TEXT NOT NULL);
        INSERT OR IGNORE INTO peripheral_migrations(version) VALUES(3);")?;
    Ok(())
}
