//! Explicit per-turn trusted directories; never shared across sessions or threads.
use std::{
    future::Future,
    path::{Path, PathBuf},
};
tokio::task_local! { static ALLOWED: Vec<PathBuf>; }

pub(crate) async fn scope<F: Future>(directories: Vec<PathBuf>, future: F) -> F::Output {
    let roots = directories
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    ALLOWED.scope(roots, future).await
}

pub(super) fn allows(canonical: &Path) -> bool {
    ALLOWED
        .try_with(|roots| roots.iter().any(|root| canonical.starts_with(root)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn roots_are_scoped_and_do_not_escape_to_other_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let accepted = root.clone();
        scope(vec![root.clone()], async move {
            assert!(allows(&accepted));
            assert!(!tokio::spawn(async move { allows(&accepted) })
                .await
                .unwrap());
        })
        .await;
        assert!(!allows(&root));
    }
}
