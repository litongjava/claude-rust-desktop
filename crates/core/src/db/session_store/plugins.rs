//! Plugin payloads are immutable. G serializes catalog changes using their digest.
use super::{layout, shared::DirectoryRepository, Stores, WRITE_DEADLINE};
use crate::{db::DbManager, services::plugins::Bundle};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};

pub trait PluginCatalog {
    fn plugins(&self) -> Result<Vec<(Bundle, bool, String)>>;
    fn active_plugin(&self, id: &str) -> Result<Bundle>;
    fn save_plugin(
        &self,
        bundle: &Bundle,
        source: &str,
        mode: PluginInstall,
    ) -> Result<(bool, bool)>;
    fn enable_plugin(&self, id: &str, enabled: bool) -> Result<()>;
    fn remove_plugin(&self, id: &str) -> Result<()>;
    fn backup_plugins(&self, root: &Path) -> Result<PathBuf>;
}
#[derive(Clone, Copy)]
pub enum PluginInstall {
    New,
    Update,
    Import { enabled: bool, preview: bool },
}

trait Payloads: DirectoryRepository {
    fn encode_plugin(&self, b: &Bundle) -> Result<String>;
    fn decode_plugin(&self, reference: &str) -> Result<Bundle>;
    fn backup(&self, root: &Path) -> Result<PathBuf>;
}
impl Payloads for Stores {
    fn encode_plugin(&self, b: &Bundle) -> Result<String> {
        self.put_payload(&serde_json::to_value(b)?)
    }
    fn decode_plugin(&self, reference: &str) -> Result<Bundle> {
        Ok(serde_json::from_value(self.payload(reference)?)?)
    }
    fn backup(&self, root: &Path) -> Result<PathBuf> {
        ensure!(
            root.canonicalize()? == self.data_dir(),
            "Plugin backup data root changed"
        );
        let backups = root.join("backups");
        std::fs::create_dir_all(&backups)?;
        let staged = tempfile::tempdir_in(&backups)?;
        let db = staged.path().join("global.db");
        self.directory_read(|c| {
            c.execute("VACUUM INTO ?1", [db.to_string_lossy().as_ref()])?;
            Ok(())
        })?;
        std::fs::File::options().write(true).open(&db)?.sync_all()?;
        let refs = {
            let c = layout::open(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut q =
                c.prepare("SELECT DISTINCT bundle FROM installed_plugins ORDER BY bundle")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let blobs = staged.path().join("global-blobs");
        std::fs::create_dir(&blobs)?;
        for digest in &refs {
            use sha2::{Digest, Sha256};
            ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid plugin digest"
            );
            let bytes = std::fs::read(root.join("global-blobs").join(digest))?;
            ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == *digest,
                "Plugin backup digest mismatch"
            );
            let path = blobs.join(digest);
            std::fs::write(&path, bytes)?;
            std::fs::File::options()
                .write(true)
                .open(path)?
                .sync_all()?;
        }
        layout::sync_dir(&blobs)?;
        let manifest = staged.path().join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(
                &serde_json::json!({"database":"global.db","tables":["installed_plugins"],"blobs":refs,"session_shards":[]}),
            )?,
        )?;
        std::fs::File::options()
            .write(true)
            .open(&manifest)?
            .sync_all()?;
        layout::sync_dir(staged.path())?;
        let dest = backups.join(format!("plugins-{}", uuid::Uuid::new_v4()));
        layout::durable_move(staged.path(), &dest)?;
        layout::sync_dir(&backups)?;
        layout::sync_dir(root)?;
        Ok(dest.join("manifest.json"))
    }
}
impl Payloads for DbManager {
    fn encode_plugin(&self, b: &Bundle) -> Result<String> {
        Ok(serde_json::to_string(b)?)
    }
    fn decode_plugin(&self, reference: &str) -> Result<Bundle> {
        Ok(serde_json::from_str(reference)?)
    }
    fn backup(&self, root: &Path) -> Result<PathBuf> {
        let dir = root.join("backups");
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("plugins-{}.sqlite3", uuid::Uuid::new_v4()));
        self.directory_read(|c| {
            c.execute("VACUUM INTO ?1", [file.to_string_lossy().as_ref()])?;
            Ok(())
        })?;
        Ok(file)
    }
}
impl<D: Payloads> PluginCatalog for D {
    fn plugins(&self) -> Result<Vec<(Bundle, bool, String)>> {
        let rows = self.directory_read(|c| {
            let mut q = c.prepare_cached(
                "SELECT bundle,enabled,source FROM installed_plugins ORDER BY id",
            )?;
            let rows = q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter()
            .map(|(s, e, src)| Ok((self.decode_plugin(&s)?, e, src)))
            .collect()
    }
    fn active_plugin(&self, id: &str) -> Result<Bundle> {
        let reference = self
            .directory_read(|c| {
                Ok(c.query_row(
                    "SELECT bundle FROM installed_plugins WHERE id=?1 AND enabled=1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })?
            .context("Plugin is disabled or not installed")?;
        self.decode_plugin(&reference)
    }
    fn save_plugin(&self, b: &Bundle, src: &str, mode: PluginInstall) -> Result<(bool, bool)> {
        let current = self.directory_read(|c| {
            Ok(c.query_row(
                "SELECT bundle,enabled FROM installed_plugins WHERE id=?1",
                [&b.id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .optional()?)
        })?;
        let enabled = match (mode, current.as_ref()) {
            (PluginInstall::New, None) => true,
            (PluginInstall::New, Some(_)) => anyhow::bail!("Plugin already installed; use update"),
            (PluginInstall::Update, None) => anyhow::bail!("Plugin not installed"),
            (PluginInstall::Update, Some((reference, enabled))) => {
                ensure!(
                    semver::Version::parse(&b.version)?
                        > semver::Version::parse(&self.decode_plugin(reference)?.version)?,
                    "Update must have a newer version"
                );
                *enabled
            }
            (PluginInstall::Import { .. }, Some((reference, enabled))) => {
                ensure!(
                    serde_json::to_value(self.decode_plugin(reference)?)?
                        == serde_json::to_value(b)?,
                    "Plugin ID conflicts with an installed package"
                );
                return Ok((true, *enabled));
            }
            (PluginInstall::Import { enabled, preview }, None) => {
                if preview {
                    return Ok((false, enabled));
                }
                enabled
            }
        };
        let reference = self.encode_plugin(b)?;
        self.directory_write("publish plugin",&b.id,WRITE_DEADLINE,|tx|{
            let updated=if let Some((prior,_))=&current{
                tx.execute("UPDATE installed_plugins SET bundle=?1,source=?2 WHERE id=?3 AND bundle=?4",params![reference,src,b.id,prior])?
            }else{
                tx.execute("INSERT OR IGNORE INTO installed_plugins(id,bundle,source,enabled) VALUES(?1,?2,?3,?4)",params![b.id,reference,src,enabled])?
            };
            ensure!(updated==1,"Plugin catalog changed during installation");
            Ok((false,enabled))
        })
    }
    fn enable_plugin(&self, id: &str, enabled: bool) -> Result<()> {
        self.directory_write("enable plugin", id, WRITE_DEADLINE, |tx| {
            ensure!(
                tx.execute(
                    "UPDATE installed_plugins SET enabled=?1 WHERE id=?2",
                    params![enabled, id]
                )? == 1,
                "Plugin not installed"
            );
            Ok(())
        })
    }
    fn remove_plugin(&self, id: &str) -> Result<()> {
        self.directory_write("remove plugin", id, WRITE_DEADLINE, |tx| {
            ensure!(
                tx.execute("DELETE FROM installed_plugins WHERE id=?1", [id])? == 1,
                "Plugin not installed"
            );
            Ok(())
        })
    }
    fn backup_plugins(&self, root: &Path) -> Result<PathBuf> {
        self.backup(root)
    }
}
