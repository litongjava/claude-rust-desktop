//! Installs the embedded bundled-skill tree into the data directory.
//!
//! The executable embeds the tree only as an install source. Files are written
//! to `<data>/skills/bundled/<skill>/...` on first start and refreshed after an
//! upgrade. A record beside the tree remembers the version and the digest of
//! every installed file, so a user-edited file is never overwritten and a file
//! removed from the tree is deleted only while it is still pristine.
use super::bundled_resources::FILES;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
};

const RECORD_FILE: &str = ".bundled-install.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct InstallRecord {
    version: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

/// `CARGO_PKG_VERSION` plus a digest of every embedded skill file.
static TREE_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = Sha256::new();
    for (skill, relative, bytes) in FILES {
        update_field(&mut hasher, skill.as_bytes());
        update_field(&mut hasher, relative.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    format!("{}+{}", env!("CARGO_PKG_VERSION"), hex(&hasher.finalize()))
});

fn update_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

/// One normal path component: no separators, traversal or drive prefixes.
fn valid_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\\', ':'])
}

/// Join a manifest-relative path under `root`, refusing a symlink in any
/// existing component. New components are fine; a symlink is never followed.
fn safe_target(root: &Path, skill: &str, relative: &str) -> Option<PathBuf> {
    if !valid_component(skill) {
        return None;
    }
    let mut path = root.to_path_buf();
    path.push(skill);
    if !safe_component(&path) {
        return None;
    }
    for part in relative.split('/') {
        if !valid_component(part) {
            return None;
        }
        path.push(part);
        if !safe_component(&path) {
            return None;
        }
    }
    Some(path)
}

fn safe_component(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => !metadata.file_type().is_symlink(),
        Err(error) => error.kind() == ErrorKind::NotFound,
    }
}

fn record_key(skill: &str, relative: &str) -> String {
    format!("{skill}/{relative}")
}

fn recorded_target(root: &Path, key: &str) -> Option<PathBuf> {
    let (skill, relative) = key.split_once('/')?;
    safe_target(root, skill, relative)
}

/// Install or refresh the embedded tree. A matching record short-circuits the
/// whole run; otherwise changed files are written, user edits are preserved and
/// obsolete pristine files are removed.
pub(super) fn install(bundled: &Path) -> Result<()> {
    std::fs::create_dir_all(bundled)?;
    let metadata = std::fs::symlink_metadata(bundled)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        tracing::warn!(
            module = "SkillsManager::install",
            path = %bundled.display(),
            "Skipped bundled skill installation: the bundled directory is not a real directory"
        );
        return Ok(());
    }

    let record_path = bundled.join(RECORD_FILE);
    let record = read_record(&record_path)?;
    if record.version.as_str() == TREE_VERSION.as_str() {
        return Ok(());
    }
    let recorded = record.files;

    let mut installed = BTreeMap::new();
    let mut embedded_keys = BTreeSet::new();
    for &(skill, relative, bytes) in FILES {
        let key = record_key(skill, relative);
        let digest = sha256(bytes);
        let Some(target) = safe_target(bundled, skill, relative) else {
            tracing::warn!(
                module = "SkillsManager::install",
                skill,
                relative,
                "Skipped bundled skill file: unsafe or symbolic-link path"
            );
            continue;
        };
        installed.insert(key.clone(), digest.clone());
        embedded_keys.insert(key.clone());
        install_file(&target, &key, bytes, &digest, &recorded)?;
    }

    for (key, previous) in &recorded {
        if embedded_keys.contains(key) {
            continue;
        }
        match recorded_target(bundled, key) {
            Some(target) => remove_if_pristine(bundled, &target, key, previous)?,
            None => tracing::info!(
                module = "SkillsManager::install",
                key,
                "Kept removed bundled skill entry: unsafe or symbolic-link path"
            ),
        }
    }

    write_record(
        &record_path,
        &InstallRecord {
            version: TREE_VERSION.clone(),
            files: installed,
        },
    )
}

fn install_file(
    target: &Path,
    key: &str,
    bytes: &[u8],
    digest: &str,
    recorded: &BTreeMap<String, String>,
) -> Result<()> {
    match std::fs::symlink_metadata(target) {
        Err(error) if error.kind() == ErrorKind::NotFound => write_atomic(target, bytes),
        Ok(metadata) if metadata.is_file() => {
            let current = match sha256_file(target) {
                Ok(current) => current,
                Err(error) => {
                    tracing::warn!(
                        module = "SkillsManager::install",
                        key,
                        path = %target.display(),
                        %error,
                        "Kept unreadable bundled skill file"
                    );
                    return Ok(());
                }
            };
            match recorded.get(key) {
                Some(previous) if *previous == current => {
                    if current == digest {
                        Ok(())
                    } else {
                        write_atomic(target, bytes)
                    }
                }
                Some(_) => {
                    tracing::info!(
                        module = "SkillsManager::install",
                        key,
                        path = %target.display(),
                        "Kept user-edited bundled skill file"
                    );
                    Ok(())
                }
                None => {
                    tracing::info!(
                        module = "SkillsManager::install",
                        key,
                        path = %target.display(),
                        "Kept existing bundled skill file without an install record"
                    );
                    Ok(())
                }
            }
        }
        Ok(_) => {
            tracing::info!(
                module = "SkillsManager::install",
                key,
                path = %target.display(),
                "Kept non-file entry at a bundled skill path"
            );
            Ok(())
        }
        Err(error) => {
            tracing::warn!(
                module = "SkillsManager::install",
                key,
                path = %target.display(),
                %error,
                "Kept bundled skill path that could not be inspected"
            );
            Ok(())
        }
    }
}

fn remove_if_pristine(root: &Path, target: &Path, key: &str, previous: &str) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            tracing::warn!(
                module = "SkillsManager::install",
                key,
                path = %target.display(),
                %error,
                "Kept removed bundled skill entry that could not be inspected"
            );
            return Ok(());
        }
    };
    if !metadata.is_file() {
        tracing::info!(
            module = "SkillsManager::install",
            key,
            path = %target.display(),
            "Kept removed bundled skill entry: no longer a regular file"
        );
        return Ok(());
    }
    let current = match sha256_file(target) {
        Ok(current) => current,
        Err(error) => {
            tracing::warn!(
                module = "SkillsManager::install",
                key,
                path = %target.display(),
                %error,
                "Kept removed bundled skill entry that could not be read"
            );
            return Ok(());
        }
    };
    if current != previous {
        tracing::info!(
            module = "SkillsManager::install",
            key,
            path = %target.display(),
            "Kept user-edited file from a removed bundled skill"
        );
        return Ok(());
    }
    std::fs::remove_file(target)?;
    prune_empty_parents(root, target);
    Ok(())
}

fn prune_empty_parents(root: &Path, path: &Path) {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root || !directory.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(directory).is_err() {
            break;
        }
        current = directory.parent();
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Bundled skill file has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Writing bundled skill file")?;
    Ok(())
}

fn read_record(path: &Path) -> Result<InstallRecord> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(InstallRecord::default()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        tracing::warn!(
            module = "SkillsManager::install",
            path = %path.display(),
            "Ignoring bundled skill install record that is not a regular file"
        );
        return Ok(InstallRecord::default());
    }
    match std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<InstallRecord>(&bytes).ok())
    {
        Some(record) => Ok(record),
        None => {
            tracing::warn!(
                module = "SkillsManager::install",
                path = %path.display(),
                "Ignoring unreadable bundled skill install record; existing files are kept"
            );
            Ok(InstallRecord::default())
        }
    }
}

fn write_record(path: &Path, record: &InstallRecord) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record)?;
    write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded(skill: &str, relative: &str) -> &'static [u8] {
        FILES
            .iter()
            .find(|(name, path, _)| *name == skill && *path == relative)
            .map(|(_, _, bytes)| *bytes)
            .expect("missing embedded fixture")
    }

    fn read_record(path: &Path) -> InstallRecord {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn write_record_fixture(root: &Path, record: &InstallRecord) {
        write_record(&root.join(RECORD_FILE), record).unwrap();
    }

    #[test]
    fn first_install_writes_every_file_and_the_record() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("skills/bundled");
        install(&bundled).unwrap();

        for &(skill, relative, bytes) in FILES {
            let path = bundled.join(skill).join(relative);
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{skill}/{relative}");
        }
        let record = read_record(&bundled.join(RECORD_FILE));
        assert!(record.version.starts_with(env!("CARGO_PKG_VERSION")));
        assert_eq!(record.files.len(), FILES.len());
        for &(skill, relative, bytes) in FILES {
            assert_eq!(
                record.files[&record_key(skill, relative)],
                sha256(bytes),
                "{skill}/{relative}"
            );
        }
    }

    #[test]
    fn second_run_with_the_same_version_is_a_no_op() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("skills/bundled");
        install(&bundled).unwrap();
        let record_before = std::fs::read(bundled.join(RECORD_FILE)).unwrap();
        let removed = bundled.join("code-review/SKILL.md");
        std::fs::remove_file(&removed).unwrap();

        // The recorded version still matches, so the missing file is not
        // rewritten; a run that ignored the record would restore it.
        install(&bundled).unwrap();

        assert!(!removed.exists());
        assert_eq!(
            std::fs::read(bundled.join(RECORD_FILE)).unwrap(),
            record_before
        );
    }

    #[test]
    fn upgrade_overwrites_untouched_files_and_keeps_edited_ones() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("skills/bundled");
        let untouched = bundled.join("code-review/SKILL.md");
        let edited = bundled.join("task-template/SKILL.md");
        std::fs::create_dir_all(untouched.parent().unwrap()).unwrap();
        std::fs::create_dir_all(edited.parent().unwrap()).unwrap();
        std::fs::write(&untouched, b"previous version").unwrap();
        std::fs::write(&edited, b"user edit").unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            "code-review/SKILL.md".to_string(),
            sha256(b"previous version"),
        );
        files.insert(
            "task-template/SKILL.md".to_string(),
            sha256(b"previous version"),
        );
        write_record_fixture(
            &bundled,
            &InstallRecord {
                version: "previous".into(),
                files,
            },
        );

        install(&bundled).unwrap();

        assert_eq!(
            std::fs::read(&untouched).unwrap(),
            embedded("code-review", "SKILL.md")
        );
        assert_eq!(std::fs::read(&edited).unwrap(), b"user edit");
        let record = read_record(&bundled.join(RECORD_FILE));
        assert_eq!(record.version.as_str(), TREE_VERSION.as_str());
        // The record describes what this release installs, even though the
        // edited file was kept as the user left it.
        assert_eq!(
            record.files["task-template/SKILL.md"],
            sha256(embedded("task-template", "SKILL.md"))
        );
    }

    #[test]
    fn removed_files_are_deleted_only_when_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("skills/bundled");
        std::fs::create_dir_all(bundled.join("removed")).unwrap();
        std::fs::create_dir_all(bundled.join("edited")).unwrap();
        std::fs::write(bundled.join("removed/notes.md"), b"old").unwrap();
        std::fs::write(bundled.join("edited/notes.md"), b"user edit").unwrap();
        let mut files = BTreeMap::new();
        files.insert("removed/notes.md".to_string(), sha256(b"old"));
        files.insert("edited/notes.md".to_string(), sha256(b"old"));
        write_record_fixture(
            &bundled,
            &InstallRecord {
                version: "previous".into(),
                files,
            },
        );

        install(&bundled).unwrap();

        assert!(!bundled.join("removed").exists());
        assert_eq!(
            std::fs::read(bundled.join("edited/notes.md")).unwrap(),
            b"user edit"
        );
        let record = read_record(&bundled.join(RECORD_FILE));
        assert!(!record.files.contains_key("removed/notes.md"));
        assert!(!record.files.contains_key("edited/notes.md"));
    }

    #[test]
    fn pre_existing_user_file_without_a_record_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("skills/bundled");
        let user_file = bundled.join("code-review/SKILL.md");
        std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
        std::fs::write(&user_file, b"user content").unwrap();

        install(&bundled).unwrap();

        assert_eq!(std::fs::read(&user_file).unwrap(), b"user content");
        let record = read_record(&bundled.join(RECORD_FILE));
        assert_eq!(
            record.files["code-review/SKILL.md"],
            sha256(embedded("code-review", "SKILL.md"))
        );
        // Other files are still installed beside the untouched file.
        assert_eq!(
            std::fs::read(bundled.join("task-template/SKILL.md")).unwrap(),
            embedded("task-template", "SKILL.md")
        );
    }
}
