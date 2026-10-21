//! Versioned upstream skill assets, loaded only when a skill is invoked.
//!
//! Each process publishes assets into a fresh private temporary directory. Read
//! serves only registered, exact asset names from the executable: registering a
//! bundle never grants access to its parent directory or write permissions.
use anyhow::{ensure, Context, Result};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

#[path = "bundled_manifest.rs"]
mod manifest;

struct Bundle {
    directory: tempfile::TempDir,
    files: HashMap<PathBuf, &'static [u8]>,
}

// Weak registry exposes published immutable bytes without keeping temporary
// files alive. The engine and its active children own the actual resources.
static REGISTRY: LazyLock<Mutex<Vec<Weak<Bundle>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Default)]
pub struct Resources {
    bundles: Mutex<HashMap<String, Arc<Bundle>>>,
}

pub fn supports(name: &str) -> bool {
    matches!(name, "claude-api" | "dataviz")
}

impl Resources {
    pub fn prepare(&self, name: &str, arguments: &str, workspace: &Path) -> Result<String> {
        ensure!(supports(name), "Unknown bundled resource skill");
        let mut bundles = self
            .bundles
            .lock()
            .map_err(|_| anyhow::anyhow!("Skill resource cache unavailable"))?;
        if !bundles.contains_key(name) {
            let directory = tempfile::Builder::new()
                .prefix(&format!("claude-skill-2.1.295-{name}-"))
                .tempdir()?;
            let mut files = HashMap::new();
            for &(skill, relative, bytes) in manifest::FILES {
                if skill != name {
                    continue;
                }
                let relative = Path::new(relative);
                ensure!(
                    relative
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                    "Invalid embedded asset path"
                );
                let target = directory.path().join(relative);
                std::fs::create_dir_all(target.parent().context("Missing asset parent")?)?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                file.write_all(bytes)?;
                files.insert(target, bytes);
            }
            let bundle = Arc::new(Bundle { directory, files });
            let mut registry = REGISTRY
                .lock()
                .map_err(|_| anyhow::anyhow!("Skill resource registry unavailable"))?;
            registry.retain(|entry| entry.strong_count() > 0);
            registry.push(Arc::downgrade(&bundle));
            bundles.insert(name.into(), bundle);
        }
        let bundle = &bundles[name];
        // Never silently execute a changed script on a subsequent invocation.
        // There are no overwrites or repairs of paths supplied by the workspace.
        for (path, bytes) in &bundle.files {
            let metadata = std::fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Bundled skill resource changed; restart the session"
            );
            ensure!(
                std::fs::read(path)?.as_slice() == *bytes,
                "Bundled skill resource changed; restart the session"
            );
        }
        let instructions = match name {
            "claude-api" => include_str!("bundled/resources/claude-api/INSTRUCTIONS.md"),
            "dataviz" => include_str!("bundled/resources/dataviz/INSTRUCTIONS.md"),
            _ => unreachable!(),
        };
        Ok(format!(
        "Base directory for this skill: {}\n\nBundled documentation snapshot: Claude Code 2.1.295. Resolve resource paths against the base directory. Use Read for referenced documents. Execute scripts only through normal Bash permissions; invocation itself executes no scripts. After restarting a worker, invoke this skill again to obtain its current resource directory.\n\n{instructions}\n\nCurrent workspace: {}\n\n## User Request\n\n{arguments}",
        bundle.directory.path().display(), workspace.display()
    ))
    }
}

/// Exact registered filenames only; no directory, glob, alias or traversal grant.
pub(crate) fn read(path: &Path) -> Option<&'static [u8]> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    REGISTRY
        .lock()
        .ok()?
        .iter()
        .filter_map(Weak::upgrade)
        .find_map(|bundle| bundle.files.get(path).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_resources_match_acquired_sha256_manifest() {
        let provenance: serde_json::Value =
            serde_json::from_str(include_str!("bundled/resources/manifest.json")).unwrap();
        let files = provenance["files"].as_array().unwrap();
        assert_eq!(files.len(), manifest::FILES.len());
        for entry in files {
            let (_, _, bytes) = manifest::FILES
                .iter()
                .find(|(name, path, _)| entry["skill"] == *name && entry["path"] == *path)
                .expect("Missing upstream asset");
            let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
            let hash: String = hash
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(hash, entry["sha256"].as_str().unwrap());
            assert_eq!(bytes.len() as u64, entry["bytes"].as_u64().unwrap());
        }
    }

    #[test]
    fn changed_files_are_not_repaired_or_served_as_new_instructions() {
        let resources = Resources::default();
        let workspace = tempfile::tempdir().unwrap();
        resources.prepare("dataviz", "", workspace.path()).unwrap();
        let path = resources.bundles.lock().unwrap()["dataviz"]
            .directory
            .path()
            .join("references/palette.md");
        let original = read(&path).unwrap();
        std::fs::write(&path, "modified externally").unwrap();
        assert_eq!(read(&path).unwrap(), original);
        assert!(resources
            .prepare("dataviz", "", workspace.path())
            .unwrap_err()
            .to_string()
            .contains("resource changed"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "modified externally"
        );
        drop(resources);
        assert!(read(&path).is_none());
        assert!(!path.exists());
    }

    #[test]
    fn materializes_complete_resources_lazily_and_keeps_arguments_literal() {
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        for (name, count, example) in [
            ("claude-api", 90, "shared/live-sources.md"),
            ("dataviz", 9, "scripts/validate_palette.js"),
        ] {
            let prompt = resources
                .prepare(
                    name,
                    "literal !`echo should-not-run` $ARGUMENTS 中文😀",
                    workspace.path(),
                )
                .unwrap();
            assert!(prompt.contains("literal !`echo should-not-run` $ARGUMENTS 中文😀"));
            assert!(!prompt.contains("official-skills-mq4AvY"));
            assert!(!prompt.contains("Describe the available local instructions only."));
            let bundles = resources.bundles.lock().unwrap();
            let bundle = &bundles[name];
            assert_eq!(bundle.files.len(), count);
            let path = bundle.directory.path().join(example);
            assert_eq!(std::fs::read(&path).unwrap(), bundle.files[&path]);
            let root = bundle.directory.path().to_path_buf();
            drop(bundles);
            assert!(read(&path).is_some());
            let response = crate::tools::execute_tool(
                "Read",
                serde_json::json!({"file_path":path,"limit":4}),
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
            assert!(response["content"].as_str().unwrap().contains("1\t"));
            assert!(response["lines"].as_u64().unwrap() > 4);
            assert!(read(&root.join("../unregistered.txt")).is_none());
            assert!(read(&root.join("not-in-manifest.md")).is_none());
            assert!(crate::tools::resolve_path(
                path.to_str().unwrap(),
                workspace.path().to_str().unwrap()
            )
            .is_err());
        }
        assert!(std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none());
        let directories: Vec<_> = resources
            .bundles
            .lock()
            .unwrap()
            .values()
            .map(|b| b.directory.path().to_path_buf())
            .collect();
        drop(resources);
        assert!(directories.iter().all(|path| !path.exists()));
    }
}
