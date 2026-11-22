//! Host discovery and restart requests share a root. Tool outputs belong to their session.
use super::shared::DirectoryRepository;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::time::Duration;

pub trait McpControl {
    fn remove_mcp_host(&self, id: &str) -> Result<()>;
    fn heartbeat_mcp(&self, id: &str, pid: u32, expires: i64, servers: &str) -> Result<()>;
    fn mcp_advertisements(&self, now: i64) -> Result<Vec<(String, u32, String)>>;
    fn mcp_advertisement(&self, id: &str) -> Result<Option<(i64, String)>>;
    fn request_mcp_restart(&self, id: &str, host: &str, server: &str, expires: i64) -> Result<()>;
    fn claim_mcp_restart(&self, host: &str, now: i64) -> Result<Option<(String, String)>>;
    fn report_mcp_restart(&self, id: &str, error: Option<&str>) -> Result<usize>;
    fn mcp_restart_status(&self, id: &str) -> Result<(String, Option<String>)>;
}
const WAIT: Duration = Duration::from_millis(250);
impl<D: DirectoryRepository> McpControl for D {
    fn remove_mcp_host(&self, id: &str) -> Result<()> {
        self.directory_write("retire MCP host", id, WAIT, |c| {
            c.execute("DELETE FROM mcp_hosts WHERE id=?1", [id])?;
            Ok(())
        })
    }
    fn heartbeat_mcp(&self, id: &str, pid: u32, expires: i64, servers: &str) -> Result<()> {
        self.directory_write("renew MCP host",id,WAIT,|c|{
            c.execute("INSERT INTO mcp_hosts(id,pid,expires_at,servers) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET expires_at=excluded.expires_at,servers=excluded.servers",params![id,pid,expires,servers])?;
            Ok(())
        })
    }
    fn mcp_advertisements(&self, now: i64) -> Result<Vec<(String, u32, String)>> {
        self.directory_read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id,pid,servers FROM mcp_hosts WHERE expires_at>?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
    fn mcp_advertisement(&self, id: &str) -> Result<Option<(i64, String)>> {
        self.directory_read(|c| {
            Ok(c.query_row(
                "SELECT expires_at,servers FROM mcp_hosts WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
    }
    fn request_mcp_restart(&self, id: &str, host: &str, server: &str, expires: i64) -> Result<()> {
        self.directory_write("request MCP restart",host,Duration::from_secs(5),|c|{
            c.execute("INSERT INTO mcp_requests(id,host,server,expires_at,state) VALUES(?1,?2,?3,?4,'pending')",params![id,host,server,expires])?;Ok(())
        })
    }
    fn claim_mcp_restart(&self, host: &str, now: i64) -> Result<Option<(String, String)>> {
        let candidate:Option<(String,String)>=self.directory_read(|c|Ok(c.query_row("SELECT id,server FROM mcp_requests WHERE host=?1 AND state='pending' AND expires_at>?2 ORDER BY rowid LIMIT 1",params![host,now],|r|Ok((r.get(0)?,r.get(1)?))).optional()?))?;
        let Some((id, server)) = candidate else {
            return Ok(None);
        };
        let won=self.directory_write("claim MCP restart",host,WAIT,|c|Ok(c.execute("UPDATE mcp_requests SET state='running' WHERE id=?1 AND host=?2 AND state='pending' AND expires_at>?3",params![id,host,now])?==1))?;
        Ok(won.then_some((id, server)))
    }
    fn report_mcp_restart(&self, id: &str, error: Option<&str>) -> Result<usize> {
        let status = if error.is_some() {
            "failed"
        } else {
            "completed"
        };
        self.directory_write("finish MCP restart", id, WAIT, |c| {
            Ok(c.execute(
                "UPDATE mcp_requests SET state=?1,error=?2 WHERE id=?3",
                params![status, error, id],
            )?)
        })
    }
    fn mcp_restart_status(&self, id: &str) -> Result<(String, Option<String>)> {
        self.directory_read(|c| {
            Ok(c.query_row(
                "SELECT state,error FROM mcp_requests WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }
}
