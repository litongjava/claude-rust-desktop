//! Session-scoped plan documents. Approval is handled by the host, never inferred
//! from the existence of a document or from model-supplied permissions.
use anyhow::{ensure, Context, Result};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const MAX_PLAN_BYTES: usize = 256 * 1024;

pub fn path(root: &Path, session: &str) -> Result<PathBuf> {
    ensure!(
        !session.is_empty() && session.len() <= 100,
        "Invalid plan session ID"
    );
    let key: String = session.bytes().map(|byte| format!("{byte:02x}")).collect();
    let directory = root.join("plans");
    if let Ok(metadata) = std::fs::symlink_metadata(&directory) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Plan directory must be a real directory"
        );
    }
    let path = directory.join(format!("{key}.md"));
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Plan must be a regular file"
        );
        ensure!(
            metadata.len() <= MAX_PLAN_BYTES as u64,
            "Plan exceeds 256 KiB"
        );
    }
    Ok(path)
}

pub fn read(root: &Path, session: &str) -> Result<Option<String>> {
    let path = path(root, session)?;
    match std::fs::read_to_string(path) {
        Ok(content) => {
            ensure!(content.len() <= MAX_PLAN_BYTES, "Plan exceeds 256 KiB");
            Ok(Some(content))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn save(root: &Path, session: &str, content: &str) -> Result<PathBuf> {
    ensure!(!content.trim().is_empty(), "Plan cannot be empty");
    ensure!(content.len() <= MAX_PLAN_BYTES, "Plan exceeds 256 KiB");
    let path = path(root, session)?;
    let directory = path.parent().context("Missing plan directory")?;
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    Ok(path)
}

pub fn definitions() -> Vec<crate::tools::ToolDefinition> {
    use serde_json::json;
    vec![
        crate::tools::ToolDefinition {
            name: "EnterPlanMode".into(),
            description: "Enter read-only planning mode before investigating an implementation. Read and inspect files, then submit your Markdown plan with ExitPlanMode(plan). Project writes remain blocked until the user approves.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        crate::tools::ToolDefinition {
            name: "ExitPlanMode".into(),
            description: "Save a concrete Markdown plan and ask the user to approve leaving plan mode. Supply plan text, or omit it to review the existing session plan. Approval switches to ask permissions; it never grants blanket Bash/write access. Rejection keeps planning mode active.".into(),
            input_schema: json!({"type":"object","properties":{"plan":{"type":"string","description":"Complete Markdown implementation and verification plan"}},"additionalProperties":false}),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_documents_are_durable_isolated_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(root.path(), "one").unwrap().is_none());
        save(root.path(), "one", "# Plan\n检查 UTF-8").unwrap();
        assert_eq!(
            read(root.path(), "one").unwrap().unwrap(),
            "# Plan\n检查 UTF-8"
        );
        assert!(read(root.path(), "two").unwrap().is_none());
        save(root.path(), "one", "# Revised").unwrap();
        assert_eq!(read(root.path(), "one").unwrap().unwrap(), "# Revised");
        assert!(save(root.path(), "one", " ").is_err());
        assert!(save(root.path(), "one", &"x".repeat(MAX_PLAN_BYTES + 1)).is_err());
        assert!(path(root.path(), "../escape")
            .unwrap()
            .starts_with(root.path().join("plans")));
    }
}
