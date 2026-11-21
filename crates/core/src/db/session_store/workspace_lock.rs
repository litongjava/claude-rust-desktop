//! Workspace admission shares a filesystem lock with Super Agent cleanup across processes.
use super::Stores;
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::path::Path;

impl Stores {
    fn workspace_lock(&self, canonical: &Path, exclusive: bool) -> Result<File> {
        // Keep locks outside the workspace so Git can remove it without unlinking the lock.
        let locks = self.data_dir().join("workspace-locks");
        fs::create_dir_all(&locks)?;
        let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(locks.join(format!("{digest:x}")))?;
        if exclusive {
            crate::fs::try_lock_file(&file).context("Worktree has active workspace users")?;
        } else {
            // Never wait behind cleanup while holding a session or directory writer lock.
            #[cfg(target_os = "android")]
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared)
                .context("Worktree removal in progress")?;
            #[cfg(not(target_os = "android"))]
            file.try_lock_shared()
                .context("Worktree removal in progress")?;
        }
        Ok(file)
    }

    pub(crate) fn hold_worktree_removal(&self, path: &Path) -> Result<File> {
        // Cleanup also resumes after Git removed the leaf. Its recorded parent must still exist.
        let parent = path
            .parent()
            .context("Worktree has no parent")?
            .canonicalize()?;
        self.workspace_lock(
            &parent.join(path.file_name().context("Worktree has no name")?),
            true,
        )
    }

    pub(super) fn admit_workspace_path(&self, path: &Path) -> Result<Vec<File>> {
        let canonical = path.canonicalize()?;
        // A session in a subdirectory occupies its enclosing worktree too.
        let locks = canonical
            .ancestors()
            .map(|ancestor| self.workspace_lock(ancestor, false))
            .collect::<Result<_>>()?;
        ensure!(
            path.canonicalize()? == canonical && canonical.is_dir(),
            "Workspace changed during admission"
        );
        Ok(locks)
    }

    /// Read the final binding in the transaction that publishes or moves the session.
    /// Its caller carries the returned guards through commit, or through the execution lease.
    pub(super) fn admit_session_workspace(&self, c: &Connection, id: &str) -> Result<Vec<File>> {
        let (project, recorded): (Option<String>, Option<String>) = c.query_row(
            "SELECT coalesce(w.path,p.workspace_path),c.workspace_path FROM conversations c
             LEFT JOIN projects p ON p.id=c.project_id
             LEFT JOIN conversation_worktrees w ON w.conversation_id=c.id AND w.project_id=p.id
             WHERE c.id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut locks = Vec::new();
        if let Some(path) = &project {
            locks = self.admit_workspace_path(Path::new(path))?;
        }
        // A synced header can also carry a path from another machine.
        if let Some(path) = recorded.filter(|p| Some(p) != project.as_ref()) {
            let path = Path::new(&path);
            if path.is_absolute() && path.try_exists()? {
                locks.extend(self.admit_workspace_path(path)?);
            }
        }
        Ok(locks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_removal_excludes_moves_and_execution_at_the_canonical_path() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stores = Stores::open(&root.path().join("data"))?;
        let source = root.path().join("source");
        let worktree = root.path().join("worktree");
        fs::create_dir(&source)?;
        fs::create_dir(&worktree)?;
        let subdirectory = worktree.join("nested");
        fs::create_dir(&subdirectory)?;
        #[cfg(unix)]
        let target = {
            let alias = root.path().join("alias");
            std::os::unix::fs::symlink(&subdirectory, &alias)?;
            alias
        };
        #[cfg(not(unix))]
        let target = subdirectory;
        let create = |path: &Path| {
            stores.create_code_session(path, "model", "provider", &uuid::Uuid::new_v4().to_string())
        };
        let resident = create(&target)?;
        let moving = create(&source)?;
        let change = || {
            crate::services::session_workspace::change(
                &stores,
                moving.id(),
                &source,
                target.to_str().unwrap(),
                false,
            )
        };
        let removal = stores.hold_worktree_removal(&worktree)?;
        assert!(resident.acquire_execution().is_err());
        assert!(change().is_err());
        assert_eq!(moving.workspace()?, source.canonicalize()?);
        let elsewhere = moving.acquire_execution()?;
        drop((elsewhere, removal));
        assert_eq!(change()?, target.canonicalize()?);
        let lease = moving.acquire_execution()?;
        assert!(stores.hold_worktree_removal(&worktree).is_err());
        drop(lease);
        stores.hold_worktree_removal(&worktree)?;
        Ok(())
    }
}
