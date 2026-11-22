//! Provider publication and the S transactions that admit work share this short-lived fence.
//! Delivery ownership can outlive several turns and therefore keeps a separate lifecycle lock.
use super::{layout, SessionDb};
use anyhow::{ensure, Context, Result};
use std::{
    fs::{self, File},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn path_for(shard: &Path) -> PathBuf {
    shard.with_added_extension("admission")
}

// Called only under the exclusive lifecycle lock, during initial build or pre-v8 upgrade.
pub(super) fn install(shard: &Path) -> Result<()> {
    let path = path_for(shard);
    match File::create_new(&path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            open(shard)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    layout::sync_dir(path.parent().context("Missing session directory")?)
}
pub(super) fn open(shard: &Path) -> Result<File> {
    let path = path_for(shard);
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Expected a regular provider admission lock"
    );
    Ok(File::options().read(true).write(true).open(path)?)
}
impl SessionDb {
    pub(super) fn provider_fence(&self, exclusive: bool, deadline: Duration) -> Result<File> {
        layout::allow_entry()?;
        let lock = open(&self.stores.sessions().join(&self.key).join("session.db"))?;
        let started = Instant::now();
        loop {
            let result = if exclusive {
                lock.try_lock()
            } else {
                lock.try_lock_shared()
            };
            match result {
                Ok(()) => return Ok(lock),
                Err(fs::TryLockError::WouldBlock) if started.elapsed() < deadline => {
                    std::thread::sleep(
                        Duration::from_millis(10).min(deadline.saturating_sub(started.elapsed())),
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Provider admission lock for {} after {}ms: {e}",
                        self.id(),
                        started.elapsed().as_millis()
                    ))
                }
            }
        }
    }
}
