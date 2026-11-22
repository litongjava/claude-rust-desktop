//! Directory-owned IM configuration and operational summaries.
use super::shared::DirectoryRepository;
use crate::{
    db::im_config_repo::{self as configs, ImConfigRow},
    im_integration::{ImErrorLogInfo, ImMessageStatsResult},
};
use anyhow::Result;
use rusqlite::params;

pub trait ImCatalog: Send + Sync {
    fn im_configs(&self) -> Result<Vec<ImConfigRow>>;
    fn save_im_config(
        &self,
        platform: &str,
        config: &str,
        status: &str,
        at: &str,
    ) -> Result<ImConfigRow>;
    fn disconnect_im_platform(&self, platform: &str, at: &str) -> Result<()>;
    fn im_stats(&self, platform: Option<&str>) -> Result<ImMessageStatsResult>;
    fn im_errors(&self, platform: Option<&str>) -> Result<Vec<ImErrorLogInfo>>;
}

impl<D: DirectoryRepository + Send + Sync> ImCatalog for D {
    fn im_configs(&self) -> Result<Vec<ImConfigRow>> {
        self.directory_read(configs::list_im_configs)
    }
    fn save_im_config(
        &self,
        platform: &str,
        config: &str,
        status: &str,
        at: &str,
    ) -> Result<ImConfigRow> {
        let id = uuid::Uuid::new_v4().to_string();
        self.directory_write(
            "save IM configuration",
            platform,
            super::WRITE_DEADLINE,
            |tx| {
                if let Some(existing) = configs::get_im_config_by_platform(tx, platform)? {
                    configs::update_im_config(tx, &existing.id, config, status, at)?;
                    Ok(ImConfigRow {
                        config_json: config.into(),
                        status: status.into(),
                        updated_at: at.into(),
                        ..existing
                    })
                } else {
                    configs::insert_im_config(tx, &id, platform, config, status, at, at)?;
                    Ok(ImConfigRow {
                        id: id.clone(),
                        platform: platform.into(),
                        config_json: config.into(),
                        status: status.into(),
                        created_at: at.into(),
                        updated_at: at.into(),
                    })
                }
            },
        )
    }
    fn disconnect_im_platform(&self, platform: &str, at: &str) -> Result<()> {
        self.directory_write(
            "disconnect IM platform",
            platform,
            super::WRITE_DEADLINE,
            |tx| {
                tx.execute(
                    "UPDATE im_configs SET status='disconnected', updated_at=?2 WHERE platform=?1",
                    params![platform, at],
                )?;
                Ok(())
            },
        )
    }
    fn im_stats(&self, platform: Option<&str>) -> Result<ImMessageStatsResult> {
        self.directory_read(|c| {
            Ok(c.query_row("SELECT COALESCE(SUM(message_count),0), COALESCE(SUM(CASE WHEN date=date('now') THEN message_count END),0),
                COALESCE(AVG(avg_response_time),0.0), (SELECT count(*) FROM im_sessions WHERE ?1 IS NULL OR platform=?1)
                FROM im_message_stats WHERE ?1 IS NULL OR platform=?1", [platform], |r| {
                    Ok(ImMessageStatsResult { platform: platform.map(str::to_owned), total_messages:r.get(0)?, active_today:r.get(1)?,
                        avg_response_time_ms:r.get::<_,f64>(2)? as u64, total_sessions:r.get(3)? })
                })?)
        })
    }
    fn im_errors(&self, platform: Option<&str>) -> Result<Vec<ImErrorLogInfo>> {
        self.directory_read(|c| {
            let mut q=c.prepare("SELECT id,platform,error_type,error_message,stack_trace,created_at FROM im_error_logs
                WHERE ?1 IS NULL OR platform=?1 ORDER BY created_at DESC LIMIT 100")?;
            let rows=q.query_map([platform],|r| Ok(ImErrorLogInfo { id:r.get(0)?,platform:r.get(1)?,error_type:r.get(2)?,error_message:r.get(3)?,stack_trace:r.get(4)?,created_at:r.get(5)? }))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}
