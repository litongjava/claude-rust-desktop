//! Session-owned native watches with stable filesystem snapshots. Native notifications
//! wake a bounded blocking sampler; idle watches never recursively poll. Missing
//! paths get a metadata-only fallback every two seconds. No global cwd or watch list.
use super::tool_hooks::{HookEvent, HookRunner};
use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};
use tokio::task::JoinHandle;

const STABILITY: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(200);
const MISSING_POLL: Duration = Duration::from_secs(2);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SCAN_ENTRIES: usize = 10_000;
struct Task {
    cancel: tokio::sync::oneshot::Sender<()>,
    handle: JoinHandle<()>,
}
impl Task {
    async fn stop(self) {
        let _ = self.cancel.send(());
        let _ = self.handle.await;
    }
}

type Stamp = (u64, Option<SystemTime>);
type Snapshot = BTreeMap<PathBuf, Stamp>;
fn registry() -> &'static Mutex<HashMap<String, Task>> {
    static TASKS: OnceLock<Mutex<HashMap<String, Task>>> = OnceLock::new();
    TASKS.get_or_init(Default::default)
}

/// Finish any bounded blocking scan, cancel pending hooks, and close the watcher.
pub async fn stop(session: &str) {
    let task = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session);
    if let Some(task) = task {
        task.stop().await;
    }
}

// Count directories and failed lookups too; traverse on the heap with a bounded queue.
fn snapshot_paths(paths: &[PathBuf], recursive: bool, limit: usize) -> (Snapshot, bool) {
    let mut snapshot = Snapshot::new();
    let mut queue: Vec<_> = paths.iter().take(limit).cloned().collect();
    let mut remaining = limit;
    let mut truncated = paths.len() > limit;
    while let Some(path) = queue.pop() {
        if remaining == 0 {
            truncated = true;
            break;
        }
        remaining -= 1;
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            snapshot.insert(path, (meta.len(), meta.modified().ok()));
        } else if recursive && meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries {
                    if queue.len() >= remaining {
                        truncated = true;
                        break;
                    }
                    if let Ok(entry) = entry {
                        queue.push(entry.path());
                    } else {
                        remaining = remaining.saturating_sub(1);
                    }
                }
            }
        }
    }
    (snapshot, truncated)
}

// notify's Windows Drop sends Stop but does not join its backend. The handler
// is destroyed after pending reads and watch handles have been closed.
struct Handler {
    wake: tokio::sync::mpsc::Sender<()>,
    closed: std::sync::mpsc::Sender<()>,
    paths: Vec<PathBuf>,
}
impl notify::EventHandler for Handler {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        match event {
            Ok(event)
                if !matches!(event.kind, notify::EventKind::Access(_))
                    && (event.paths.is_empty()
                        || event.paths.iter().any(|changed| {
                            self.paths
                                .iter()
                                .any(|path| changed.starts_with(path) || path.starts_with(changed))
                        })) =>
            {
                let _ = self.wake.try_send(());
            }
            Err(error) => tracing::warn!("FileChanged native watch: {error:#}"),
            _ => {}
        }
    }
}
impl Drop for Handler {
    fn drop(&mut self) {
        let _ = self.closed.send(());
    }
}
struct State {
    watcher: Option<RecommendedWatcher>,
    closed: std::sync::mpsc::Receiver<()>,
    wake: tokio::sync::mpsc::Receiver<()>,
    paths: Vec<PathBuf>,
    roots: BTreeSet<PathBuf>,
    committed: Snapshot,
    degraded: bool,
    fallback_paths: Vec<PathBuf>,
    missing: Vec<PathBuf>,
    last_missing_check: Instant,
    #[cfg(test)]
    scans: usize,
    pending: BTreeMap<PathBuf, (Option<Stamp>, Instant)>,
}
impl Drop for State {
    fn drop(&mut self) {
        drop(self.watcher.take());
        if self.closed.recv_timeout(CLOSE_TIMEOUT).is_err() {
            tracing::warn!("FileChanged watcher close acknowledgement timed out after 2 seconds");
        }
    }
}
impl State {
    fn new(paths: Vec<PathBuf>) -> Result<Self> {
        let (tx, wake) = tokio::sync::mpsc::channel(1);
        let (closed_tx, closed) = std::sync::mpsc::channel();
        let watcher = notify::recommended_watcher(Handler {
            wake: tx,
            closed: closed_tx,
            paths: paths.clone(),
        })?;
        let mut state = Self {
            watcher: Some(watcher),
            closed,
            wake,
            paths,
            roots: BTreeSet::new(),
            committed: Snapshot::new(),
            degraded: false,
            fallback_paths: Vec::new(),
            missing: Vec::new(),
            last_missing_check: Instant::now(),
            #[cfg(test)]
            scans: 0,
            pending: BTreeMap::new(),
        };
        state.refresh_roots()?;
        state.committed = state.snapshot(); // ignoreInitial, including directory descendants
        Ok(state)
    }
    fn snapshot(&mut self) -> Snapshot {
        self.snapshot_with_limit(MAX_SCAN_ENTRIES)
    }
    fn snapshot_with_limit(&mut self, limit: usize) -> Snapshot {
        #[cfg(test)]
        {
            self.scans += 1;
        }
        let paths = if self.degraded {
            self.fallback_paths.clone()
        } else {
            self.paths.clone()
        };
        let (mut snapshot, truncated) = snapshot_paths(&paths, !self.degraded, limit);
        if truncated {
            // Partial traversal cannot prove absence. Freeze discovery until reset.
            if !self.committed.is_empty() {
                snapshot = self.committed.clone();
            }
            if !self.degraded {
                self.fallback_paths = snapshot.keys().cloned().collect();
                tracing::warn!("FileChanged scan exceeds {MAX_SCAN_ENTRIES} entries; tracking known files only until watchPaths/cwd reset");
            }
            self.degraded = true;
        }
        snapshot
    }
    fn tick(&mut self, dirty: bool) -> Vec<(PathBuf, &'static str)> {
        let appeared =
            if !self.missing.is_empty() && self.last_missing_check.elapsed() >= MISSING_POLL {
                self.last_missing_check = Instant::now();
                self.missing.iter().any(|path| path.exists())
            } else {
                false
            };
        if !dirty && self.pending.is_empty() && !appeared {
            return vec![];
        }
        if let Err(error) = self.refresh_roots() {
            tracing::warn!("FileChanged watch: {error:#}");
        }
        self.sample()
    }
    fn refresh_roots(&mut self) -> Result<()> {
        let mut roots = BTreeSet::new();
        for path in &self.paths {
            let mut root = path.as_path();
            while !root.exists() {
                let Some(parent) = root.parent() else {
                    break;
                };
                root = parent;
            }
            // Retain a parent watch for file deletion/replacement; once a missing
            // target appears it also gets its own watch (directories recursive).
            roots.insert(root.to_owned());
            if let Some(parent) = root.parent() {
                roots.insert(parent.to_owned());
            }
        }
        for root in roots.difference(&self.roots) {
            let recursive = if self.paths.contains(root) && root.is_dir() {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            self.watcher.as_mut().unwrap().watch(root, recursive)?;
        }
        for root in self.roots.difference(&roots) {
            let _ = self.watcher.as_mut().unwrap().unwatch(root);
        }
        self.missing = self
            .paths
            .iter()
            .filter(|path| !path.exists())
            .cloned()
            .collect();
        self.roots = roots;
        Ok(())
    }
    fn sample(&mut self) -> Vec<(PathBuf, &'static str)> {
        let now = Instant::now();
        let current = self.snapshot();
        let paths: BTreeSet<_> = current
            .keys()
            .chain(self.committed.keys())
            .chain(self.pending.keys())
            .cloned()
            .collect();
        let mut events = vec![];
        for path in paths {
            let old = self.committed.get(&path).copied();
            let new = current.get(&path).copied();
            if old == new {
                self.pending.remove(&path);
                continue;
            }
            let pending = self.pending.entry(path.clone()).or_insert((new, now));
            if pending.0 != new {
                *pending = (new, now);
            }
            if now.duration_since(pending.1) < STABILITY {
                continue;
            }
            let event = match (old, new) {
                (None, Some(_)) => "add",
                (Some(_), None) => "unlink",
                _ => "change",
            };
            if let Some(stamp) = new {
                self.committed.insert(path.clone(), stamp);
            } else {
                self.committed.remove(&path);
            }
            self.pending.remove(&path);
            events.push((path, event));
        }
        events
    }
}

pub(crate) async fn start(
    session: &str,
    runner: HookRunner,
    mut dynamic: Vec<String>,
) -> Result<()> {
    stop(session).await;
    let paths = runner.file_watch_paths(session, &dynamic);
    if paths.is_empty() {
        return Ok(());
    }
    let mut state = tokio::task::spawn_blocking(move || State::new(paths)).await??;
    let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
    let session_id = session.to_owned();
    let task = tokio::spawn(async move {
        use futures::{stream::FuturesUnordered, StreamExt};
        let mut pending: FuturesUnordered<
            futures::future::BoxFuture<'_, Result<super::tool_hooks::HookOutcome>>,
        > = FuturesUnordered::new();
        let mut dirty = false;
        let mut interval = tokio::time::interval(POLL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = &mut cancelled => break,
                _ = interval.tick() => {
                    if dirty || !state.pending.is_empty() ||
                        (!state.missing.is_empty() && state.last_missing_check.elapsed() >= MISSING_POLL) {
                        let sampled = tokio::task::spawn_blocking(move || {
                            let events = state.tick(dirty);
                            (state, events)
                        }).await;
                        let Ok((next, events)) = sampled else { return; };
                        state = next;
                        dirty = false;
                        for (path, event) in events {
                            pending.push(Box::pin(runner.run_inner(HookEvent::FileChanged, &session_id, json!({"file_path":path,"event":event}))));
                        }
                    }
                }
                Some(result) = pending.next(), if !pending.is_empty() => {
                    match result {
                        Ok(outcome) => {
                            for message in outcome.warnings { tracing::warn!("FileChanged hook: {message}"); }
                            if !outcome.watch_paths.is_empty() {
                                let mut replacement = outcome.watch_paths;
                                replacement.sort(); replacement.dedup();
                                dynamic.sort(); dynamic.dedup();
                                if replacement != dynamic {
                                    let paths = runner.file_watch_paths(&session_id, &replacement);
                                    match tokio::task::spawn_blocking(move || State::new(paths)).await {
                                        Ok(Ok(next)) => {
                                            let old = std::mem::replace(&mut state, next);
                                            let _ = tokio::task::spawn_blocking(move || drop(old)).await;
                                            dynamic = replacement;
                                        }
                                        Ok(Err(error)) => tracing::warn!("FileChanged watch: {error:#}"),
                                        Err(error) => tracing::warn!("FileChanged watch task: {error:#}"),
                                    }
                                }
                            }
                        }
                        Err(error) => tracing::warn!("FileChanged hook: {error:#}"),
                    }
                }
                _ = state.wake.recv() => { dirty = true; }
            }
        }
        drop(pending);
        let _ = tokio::task::spawn_blocking(move || drop(state)).await;
    });
    let previous = registry().lock().unwrap_or_else(|e| e.into_inner()).insert(
        session.into(),
        Task {
            cancel,
            handle: task,
        },
    );
    if let Some(previous) = previous {
        previous.stop().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
