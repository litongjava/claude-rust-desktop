//! Explicitly scoped file memory. No default paths, engine hooks, or migration.
//!
//! Hosts must resolve trusted roots and enforce their permission/Plan/role policy
//! before opening a store. Possessing a scope key is not user authorization.
mod format;
mod index;
mod name_fold;
mod safe_dir;
mod scope;
mod store;
#[cfg(test)]
mod tests;

pub use format::{MemoryDocument, MemoryKind, MAX_FILE_BYTES, MAX_FRONTMATTER_BYTES};
pub use index::{IndexContext, IndexLimits};
pub use scope::MemoryScope;
pub use store::{Diagnostic, Entry, ExpectedVersion, FileMemoryStore, Snapshot, StoreOptions};

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum MemoryFileError {
    #[error("Invalid memory: {0}")]
    Invalid(String),
    #[error("Unsafe memory path: {0}")]
    UnsafePath(String),
    #[error("Memory changed since read: {0}")]
    Conflict(String),
    #[error("Memory journal conflicts with disk: {0}")]
    RecoveryConflict(String),
    #[error("Memory store is busy")]
    Busy,
    #[error("Memory operation cancelled before commit")]
    Cancelled,
    #[error("Memory storage is unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MemoryFileError>;

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid(message: impl Into<String>) -> MemoryFileError {
    MemoryFileError::Invalid(message.into())
}

fn safe_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 180
        || name == "."
        || name.contains("..")
        || name.ends_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(MemoryFileError::UnsafePath(name.into()));
    }
    let stem = name.split('.').next().unwrap().to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return Err(MemoryFileError::UnsafePath(name.into()));
    }
    Ok(())
}

fn memory_filename(name: &str) -> Result<()> {
    safe_component(name)?;
    if name.eq_ignore_ascii_case("MEMORY.md")
        || !name.ends_with(".md")
        || name.starts_with('.')
        || name.bytes().any(|b| b.is_ascii_uppercase())
    {
        return Err(MemoryFileError::UnsafePath(name.into()));
    }
    Ok(())
}
