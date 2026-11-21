use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::im_permission_repo;
use crate::db::session_store::{ImPermissionChange, ImPermissions};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Open,
    Whitelist,
    PairingCode,
}

impl PermissionMode {
    pub fn as_str(&self) -> &str {
        match self {
            PermissionMode::Open => "open",
            PermissionMode::Whitelist => "whitelist",
            PermissionMode::PairingCode => "pairing",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(PermissionMode::Open),
            "whitelist" => Some(PermissionMode::Whitelist),
            "pairing" => Some(PermissionMode::PairingCode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPermission {
    pub id: String,
    pub platform: String,
    pub user_id: String,
    pub permission_mode: PermissionMode,
    pub is_allowed: bool,
    pub paired_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<im_permission_repo::ImUserPermissionRow> for UserPermission {
    fn from(row: im_permission_repo::ImUserPermissionRow) -> Self {
        Self {
            id: row.id,
            platform: row.platform,
            user_id: row.user_id,
            permission_mode: PermissionMode::from_str(&row.permission_mode)
                .unwrap_or(PermissionMode::Open),
            is_allowed: row.is_allowed,
            paired_code: row.paired_code,
            created_at: row.created_at.parse().unwrap_or_else(|_| Utc::now()),
            updated_at: row.updated_at.parse().unwrap_or_else(|_| Utc::now()),
        }
    }
}

pub struct PermissionManager {
    db: Arc<dyn ImPermissions>,
    platform_modes: Mutex<HashMap<String, PermissionMode>>,
}

impl PermissionManager {
    pub fn new(db: Arc<dyn ImPermissions>) -> Self {
        Self {
            db,
            platform_modes: Mutex::new(HashMap::new()),
        }
    }

    pub async fn set_permission_mode(&self, platform: &str, mode: PermissionMode) -> Result<()> {
        let mut modes = self.platform_modes.lock().await;
        modes.insert(platform.to_string(), mode.clone());
        Ok(())
    }

    pub async fn get_permission_mode(&self, platform: &str) -> PermissionMode {
        let modes = self.platform_modes.lock().await;
        modes.get(platform).cloned().unwrap_or(PermissionMode::Open)
    }

    async fn for_user<T: Send + 'static>(
        &self,
        platform: &str,
        user: &str,
        operation: impl FnOnce(&dyn ImPermissions, &str, &str) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = self.db.clone();
        let platform = platform.to_owned();
        let user = user.to_owned();
        tokio::task::spawn_blocking(move || operation(db.as_ref(), &platform, &user)).await?
    }

    pub async fn generate_pairing_code(&self, platform: &str, user: &str) -> Result<String> {
        self.for_user(platform, user, |db, platform, user| {
            let code = format!("{:06}", uuid::Uuid::new_v4().as_u128() % 1_000_000);
            db.change_im_permission(platform, user, ImPermissionChange::Pair(&code))?;
            Ok(code)
        })
        .await
    }
    pub async fn verify_pairing_code(
        &self,
        platform: &str,
        user: &str,
        code: &str,
    ) -> Result<bool> {
        let code = code.to_owned();
        self.for_user(platform, user, move |db, p, u| {
            Ok(db
                .im_user(p, u)?
                .is_some_and(|row| row.paired_code.as_deref() == Some(&code)))
        })
        .await
    }
    pub async fn add_to_whitelist(&self, platform: &str, user: &str) -> Result<()> {
        self.for_user(platform, user, |db, p, u| {
            db.change_im_permission(p, u, ImPermissionChange::Allow)
        })
        .await
    }
    pub async fn remove_from_whitelist(&self, platform: &str, user: &str) -> Result<()> {
        self.for_user(platform, user, |db, p, u| {
            db.change_im_permission(p, u, ImPermissionChange::Remove)
        })
        .await
    }
    pub async fn check_permission(&self, platform: &str, user: &str) -> Result<bool> {
        if self.get_permission_mode(platform).await == PermissionMode::Open {
            return Ok(true);
        }
        self.for_user(platform, user, |db, p, u| {
            Ok(db.im_user(p, u)?.is_some_and(|row| row.is_allowed))
        })
        .await
    }
    async fn list(&self, platform: &str, pending: bool) -> Result<Vec<UserPermission>> {
        let db = self.db.clone();
        let platform = platform.to_owned();
        let rows =
            tokio::task::spawn_blocking(move || db.list_im_users(&platform, pending)).await??;
        Ok(rows.into_iter().map(UserPermission::from).collect())
    }
    pub async fn list_permissions(&self, platform: &str) -> Result<Vec<UserPermission>> {
        self.list(platform, false).await
    }
    pub async fn get_pending_pairing_requests(
        &self,
        platform: &str,
    ) -> Result<Vec<UserPermission>> {
        self.list(platform, true).await
    }
    pub async fn approve_pairing_request(&self, platform: &str, user: &str) -> Result<()> {
        self.for_user(platform, user, |db, p, u| {
            db.change_im_permission(p, u, ImPermissionChange::Approve)
        })
        .await
    }
    pub async fn reject_pairing_request(&self, platform: &str, user: &str) -> Result<()> {
        self.remove_from_whitelist(platform, user).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pairing_decisions_survive_restart_and_are_scoped_to_the_platform() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stores = crate::db::session_store::Stores::open(root.path())?;
        let manager = PermissionManager::new(Arc::new(stores));
        manager
            .set_permission_mode("alpha", PermissionMode::PairingCode)
            .await?;
        let code = manager.generate_pairing_code("alpha", "user").await?;
        assert!(manager.verify_pairing_code("alpha", "user", &code).await?);
        assert!(!manager.verify_pairing_code("beta", "user", &code).await?);
        assert!(!manager.check_permission("alpha", "user").await?);
        manager.approve_pairing_request("alpha", "user").await?;
        drop(manager);
        let manager = PermissionManager::new(Arc::new(crate::db::session_store::Stores::open(
            root.path(),
        )?));
        manager
            .set_permission_mode("alpha", PermissionMode::PairingCode)
            .await?;
        assert!(manager.check_permission("alpha", "user").await?);
        manager.reject_pairing_request("alpha", "user").await?;
        assert!(!manager.check_permission("alpha", "user").await?);
        Ok(())
    }
}
