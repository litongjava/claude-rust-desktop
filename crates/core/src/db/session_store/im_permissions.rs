use super::shared::DirectoryRepository;
use crate::db::im_permission_repo::{self as repo, ImUserPermissionRow};
use anyhow::Result;

pub enum ImPermissionChange<'a> {
    Pair(&'a str),
    Allow,
    Approve,
    Remove,
}
pub trait ImPermissions: Send + Sync {
    fn im_user(&self, platform: &str, user: &str) -> Result<Option<ImUserPermissionRow>>;
    fn list_im_users(&self, platform: &str, pending: bool) -> Result<Vec<ImUserPermissionRow>>;
    fn change_im_permission(
        &self,
        platform: &str,
        user: &str,
        change: ImPermissionChange<'_>,
    ) -> Result<()>;
}
impl<D: DirectoryRepository + Send + Sync> ImPermissions for D {
    fn im_user(&self, platform: &str, user: &str) -> Result<Option<ImUserPermissionRow>> {
        self.directory_read(|c| repo::get_im_user_permission_by_platform_user(c, platform, user))
    }
    fn list_im_users(&self, platform: &str, pending: bool) -> Result<Vec<ImUserPermissionRow>> {
        self.directory_read(|c| {
            if pending {
                repo::list_pending_im_user_permissions(c, platform)
            } else {
                repo::list_im_user_permissions_by_platform(c, platform)
            }
        })
    }
    fn change_im_permission(
        &self,
        platform: &str,
        user: &str,
        change: ImPermissionChange<'_>,
    ) -> Result<()> {
        self.directory_write(
            "change IM permission",
            platform,
            super::WRITE_DEADLINE,
            |c| {
                if matches!(change, ImPermissionChange::Remove) {
                    return repo::delete_im_user_permission_by_platform_user(c, platform, user);
                }
                let row = repo::get_im_user_permission_by_platform_user(c, platform, user)?;
                let at = chrono::Utc::now().to_rfc3339();
                let (mode, allowed, code) = match change {
                    ImPermissionChange::Pair(code) => ("pairing", false, Some(code)),
                    ImPermissionChange::Allow => ("whitelist", true, None),
                    ImPermissionChange::Approve => {
                        let Some(row) = row else {
                            return Ok(());
                        };
                        return repo::update_im_user_permission(
                            c,
                            &row.id,
                            "pairing",
                            true,
                            row.paired_code.as_deref(),
                            &at,
                        );
                    }
                    ImPermissionChange::Remove => unreachable!(),
                };
                match row {
                    Some(row) => {
                        repo::update_im_user_permission(c, &row.id, mode, allowed, code, &at)
                    }
                    None => repo::insert_im_user_permission(
                        c,
                        &uuid::Uuid::new_v4().to_string(),
                        platform,
                        user,
                        mode,
                        allowed,
                        code,
                        &at,
                        &at,
                    ),
                }
            },
        )
    }
}
