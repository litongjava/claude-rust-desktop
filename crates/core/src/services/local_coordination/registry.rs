//! Registry service: one fresh incarnation per lifetime, a bounded heartbeat,
//! and read-only snapshots that never mutate or recover anything.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;

use super::owner::{probe_owner, OwnerLease, OwnerProof};
use super::store::Store;
use super::types::{
    Activity, CoordResult, CoordinationError, Lifecycle, ParticipantKind, ParticipantRole,
    Registration,
};
use super::workspace::{observe_workspace_blocking, WorkspaceIdentity};
use super::CoordinationPaths;

/// The heartbeat interval: activity changes are coalesced into this write.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Oldest heartbeat still reported as responsive.
const RESPONSIVE_WINDOW_MS: i64 = 15_000;
/// The visible row cap; the query fetches one extra row as a lookahead.
const SNAPSHOT_LIMIT: usize = 100;
/// Bounded wait for one worker round trip.
const WORKER_REPLY: Duration = Duration::from_secs(5);

/// Everything a fresh incarnation registers, with the workspace already
/// observed by the caller.
#[derive(Clone, Debug)]
pub struct RegistrationInput {
    pub data_namespace_id: String,
    pub session_id: Option<String>,
    pub kind: ParticipantKind,
    pub role: ParticipantRole,
    pub parent_instance_id: Option<String>,
    pub worker_id: Option<String>,
    pub job_id: Option<String>,
    pub workspace: WorkspaceIdentity,
}

/// How recently the owner was heard from. This is display state only: it never
/// changes an owner proof and never authorizes reclaim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Responsiveness {
    Responsive,
    Unresponsive,
    ClockUncertain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceView {
    pub registration: Registration,
    pub owner_proof: OwnerProof,
    pub responsiveness: Responsiveness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGroup {
    pub data_namespace_id: String,
    pub session_id: Option<String>,
    pub instances: Vec<InstanceView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrySnapshot {
    pub domain_id: Option<String>,
    pub observed_ms: i64,
    pub groups: Vec<SessionGroup>,
    pub truncated: bool,
}

fn wall_clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn responsiveness(row: &Registration, now_ms: i64) -> Responsiveness {
    if now_ms < row.heartbeat_ms {
        // The recorded heartbeat is in the future, so the clocks disagree.
        Responsiveness::ClockUncertain
    } else if now_ms - row.heartbeat_ms <= RESPONSIVE_WINDOW_MS {
        Responsiveness::Responsive
    } else {
        Responsiveness::Unresponsive
    }
}

/// A read-only view of open instances, grouped by session.
///
/// Open rows are filtered before the row cap, so terminal rows can never hide a
/// newer live one. Rows whose owner is gone or unprovable stay visible; nothing
/// is recovered, closed or deleted here.
pub fn registry_snapshot(
    paths: &CoordinationPaths,
    now_ms: i64,
) -> CoordResult<RegistrySnapshot> {
    let Some(store) = Store::open_read(paths)? else {
        return Ok(RegistrySnapshot {
            domain_id: None,
            observed_ms: now_ms,
            groups: Vec::new(),
            truncated: false,
        });
    };
    let mut rows = store.open_rows(SNAPSHOT_LIMIT)?;
    let truncated = rows.len() > SNAPSHOT_LIMIT;
    rows.truncate(SNAPSHOT_LIMIT);

    let mut groups: Vec<SessionGroup> = Vec::new();
    for row in rows {
        let proof = probe_owner(paths, &row.instance_id, &row.owner);
        let view = InstanceView {
            responsiveness: responsiveness(&row, now_ms),
            owner_proof: proof,
            registration: row,
        };
        let key = view.registration.session_id.clone();
        let existing = key.as_ref().and_then(|session| {
            groups.iter_mut().find(|group| {
                group.session_id.as_deref() == Some(session.as_str())
                    && group.data_namespace_id == view.registration.data_namespace_id
            })
        });
        match (existing, key) {
            (Some(group), _) => group.instances.push(view),
            (None, session) => groups.push(SessionGroup {
                data_namespace_id: view.registration.data_namespace_id.clone(),
                session_id: session,
                instances: vec![view],
            }),
        }
    }

    Ok(RegistrySnapshot {
        domain_id: Some(store.domain_id().to_string()),
        observed_ms: now_ms,
        groups,
        truncated,
    })
}

/// One bounded command for the owned worker.
enum Command {
    /// Fenced workspace update with a synchronous reply.
    Update(WorkspaceIdentity, mpsc::Sender<CoordResult<()>>),
    /// Wakeup only: the worker closes and releases the lease on its way out.
    Stop,
}

/// The latest published state, kept in one short-critical-section slot.
///
/// Refresh requests are latest-value only: a path overwrites the previous one
/// instead of queueing work, so a burst of requests cannot delay the heartbeat.
struct Mailbox {
    activity: Activity,
    refresh: Option<PathBuf>,
    stopping: bool,
}

fn with_mailbox<T>(mailbox: &Mutex<Mailbox>, f: impl FnOnce(&mut Mailbox) -> T) -> T {
    // A poisoned mailbox only means another thread panicked; the value it holds
    // is still usable, so recovering keeps shutdown working.
    let mut guard = mailbox.lock().unwrap_or_else(|error| error.into_inner());
    f(&mut guard)
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
type Observer = Arc<dyn Fn(&Path, i64) -> CoordResult<WorkspaceIdentity> + Send + Sync>;

/// A live incarnation: its immutable lease, its owned heartbeat thread and the
/// latest activity it published.
///
/// The thread owns the store and the lease, so `Drop` only signals it: the final
/// close and the lease release happen on that thread and never wait on the
/// dropping thread. An abrupt process exit is handled by owner proof instead.
pub struct RegistryGuard {
    instance_id: String,
    commands: Option<mpsc::SyncSender<Command>>,
    mailbox: Arc<Mutex<Mailbox>>,
    finished: Option<oneshot::Receiver<CoordResult<()>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RegistryGuard {
    /// Open the store, publish the lease, then advertise the open row.
    ///
    /// The store must open first: on a fresh root, creating `owners/` before the
    /// store would leave orphan evidence that bootstrap refuses. If the insert
    /// fails the lease is dropped but its immutable record stays behind.
    pub fn register(
        paths: CoordinationPaths,
        input: RegistrationInput,
        now_ms: i64,
    ) -> CoordResult<Self> {
        register_with(
            paths,
            input,
            now_ms,
            HEARTBEAT_INTERVAL,
            Arc::new(wall_clock),
            Arc::new(observe_workspace_blocking),
        )
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Publish the latest activity; the next due tick persists it once.
    pub fn set_activity(&self, activity: Activity) {
        with_mailbox(&self.mailbox, |mailbox| mailbox.activity = activity);
    }

    /// Ask for a workspace refresh; only the latest path is observed, and this
    /// never runs Git or SQLite on the caller's thread.
    pub fn request_workspace_refresh(&self, cwd: PathBuf) {
        with_mailbox(&self.mailbox, |mailbox| {
            if !mailbox.stopping {
                mailbox.refresh = Some(cwd);
            }
        });
    }

    /// Fenced workspace update through the worker's own transaction.
    pub fn update_workspace(&self, workspace: WorkspaceIdentity) -> CoordResult<()> {
        let commands = self
            .commands
            .as_ref()
            .ok_or(CoordinationError::InvalidTransition)?;
        let (reply, outcome) = mpsc::channel();
        // A full slot means a command is already waiting; the caller retries
        // rather than queueing unbounded work behind an in-flight write.
        commands
            .try_send(Command::Update(workspace, reply))
            .map_err(|_| CoordinationError::Busy)?;
        outcome
            .recv_timeout(WORKER_REPLY)
            .map_err(|_| CoordinationError::Busy)?
    }

    /// Stop heartbeats, drain the worker, mark the row closed and release it.
    ///
    /// Awaiting through `self.finished.as_mut()` means a cancelled `close` can
    /// be repeated: the receiver is only taken once it resolves.
    pub async fn close(&mut self) -> CoordResult<()> {
        self.signal_stop();
        let Some(finished) = self.finished.as_mut() else {
            return Ok(());
        };
        let result = finished.await;
        self.finished = None;
        self.thread = None;
        result.map_err(|_| CoordinationError::InvalidTransition)?
    }

    /// Signal the worker exactly once: set the stopping flag, drop any pending
    /// refresh, and wake the worker if its bounded slot is free.
    fn signal_stop(&mut self) {
        let Some(commands) = self.commands.take() else {
            return;
        };
        with_mailbox(&self.mailbox, |mailbox| {
            mailbox.stopping = true;
            mailbox.refresh = None;
        });
        // A full slot already wakes the worker, so a rejected wakeup is fine.
        let _ = commands.try_send(Command::Stop);
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        // Bounded and never blocking: the worker closes the row and releases
        // the lease on its own stack after any in-flight write finishes.
        self.signal_stop();
    }
}

fn register_with(
    paths: CoordinationPaths,
    input: RegistrationInput,
    now_ms: i64,
    interval: Duration,
    clock: Clock,
    observer: Observer,
) -> CoordResult<RegistryGuard> {
    if !cfg!(target_os = "linux") {
        // No owner adapter exists here, so nothing is bootstrapped either.
        return Err(CoordinationError::UnsupportedPlatform);
    }
    let instance_id = uuid::Uuid::new_v4().to_string();
    let (commands, command_rx) = mpsc::sync_channel(1);
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = oneshot::channel();
    let mailbox = Arc::new(Mutex::new(Mailbox {
        activity: Activity::Idle,
        refresh: None,
        stopping: false,
    }));
    let worker_mailbox = mailbox.clone();
    let worker_instance = instance_id.clone();
    let worker_observer = observer.clone();
    // A real owned thread, not a Tokio blocking task: registration must work
    // without a runtime and the worker must outlive one.
    let thread = std::thread::Builder::new()
        .name("dcl-registry".into())
        .spawn(move || {
            let (mut store, lease) =
                match register_row(&paths, input, now_ms, &worker_instance) {
                    Ok(pair) => pair,
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                        return;
                    }
                };
            // The caller may have gone away; then close and release at once.
            let running = startup_tx.send(Ok(())).is_ok();
            let generation = lease.record().generation.clone();
            if running {
                worker_loop(
                    &mut store,
                    &worker_instance,
                    &generation,
                    &worker_mailbox,
                    command_rx,
                    interval,
                    &*clock,
                    &*worker_observer,
                );
            }
            // Closed first, then the lease is released: a bare Drop can
            // therefore finish after runtime shutdown but never resurrects.
            let finished = finish(&mut store, &worker_instance, &generation, &*clock);
            drop(lease);
            drop(store);
            let _ = done_tx.send(finished);
        })?;
    startup_rx
        .recv()
        .map_err(|_| CoordinationError::InvalidTransition)??;
    Ok(RegistryGuard {
        instance_id,
        commands: Some(commands),
        mailbox,
        finished: Some(done_rx),
        thread: Some(thread),
    })
}

/// The worker's startup: open the store, publish the lease, insert the row.
///
/// The store must exist before the lease: a fresh root that already holds an
/// `owners/` directory is orphan evidence and bootstrap refuses it.
fn register_row(
    paths: &CoordinationPaths,
    input: RegistrationInput,
    now_ms: i64,
    instance_id: &str,
) -> CoordResult<(Store, OwnerLease)> {
    let mut store = Store::open_rw(paths)?;
    let lease = OwnerLease::acquire(paths, instance_id)?;
    // A workspace that could not be persisted is rejected here, before the row
    // is advertised; the dropped lease leaves its immutable record behind.
    input.workspace.validate()?;
    let git_observed = input
        .workspace
        .repo_id
        .as_ref()
        .map(|_| input.workspace.observed_ms);
    let row = Registration {
        instance_id: instance_id.to_owned(),
        data_namespace_id: input.data_namespace_id,
        session_id: input.session_id,
        kind: input.kind,
        role: input.role,
        parent_instance_id: input.parent_instance_id,
        worker_id: input.worker_id,
        job_id: input.job_id,
        owner: lease.record().clone(),
        workspace: input.workspace.workspace,
        repo_id: input.workspace.repo_id,
        worktree_id: input.workspace.worktree_id,
        branch_ref: input.workspace.branch_ref,
        activity: Activity::Idle,
        activity_since_ms: now_ms,
        started_ms: now_ms,
        heartbeat_ms: now_ms,
        last_git_observed_ms: git_observed,
        lifecycle: Lifecycle::Open,
    };
    if let Err(error) = store.insert(&row) {
        drop(lease);
        return Err(error);
    }
    Ok((store, lease))
}

/// Mark the row closed. Called only after the worker loop has stopped, and just
/// before the lease is released, so a closed row never keeps a held lease.
fn finish(
    store: &mut Store,
    id: &str,
    generation: &str,
    clock: &(dyn Fn() -> i64 + Send + Sync),
) -> CoordResult<()> {
    // `Ok(false)` means the row was already terminal or another generation owns
    // it; the row is never reopened.
    store.close(id, generation, clock()).map(|_| ())
}

fn worker_loop(
    store: &mut Store,
    id: &str,
    generation: &str,
    mailbox: &Mutex<Mailbox>,
    commands: mpsc::Receiver<Command>,
    interval: Duration,
    clock: &(dyn Fn() -> i64 + Send + Sync),
    observe: &(dyn Fn(&Path, i64) -> CoordResult<WorkspaceIdentity> + Send + Sync),
) {
    // One monotonic deadline, advanced by whole intervals. A refresh command
    // never moves it, so a busy caller cannot starve the heartbeat.
    let mut next_tick = Instant::now() + interval;
    loop {
        if with_mailbox(mailbox, |mailbox| mailbox.stopping) {
            return;
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        match commands.recv_timeout(wait) {
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(Command::Update(workspace, reply)) => {
                if with_mailbox(mailbox, |mailbox| mailbox.stopping) {
                    return;
                }
                let outcome = store.update_workspace(id, generation, &workspace, clock());
                // A stale generation or terminal row is a no-op, not an error.
                let _ = reply.send(outcome.map(|_| ()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if with_mailbox(mailbox, |mailbox| mailbox.stopping) {
            return;
        }
        let now = Instant::now();
        if now < next_tick {
            continue;
        }
        // Skip any missed ticks without emitting a burst of writes.
        next_tick = std::cmp::max(next_tick + interval, now + interval);

        // Take the single pending path, then observe outside the lock.
        let pending = with_mailbox(mailbox, |mailbox| mailbox.refresh.take());
        let observed = pending.and_then(|path| {
            let at = clock();
            observe(&path, at)
                .ok()
                // A refresh that cannot be stored is discarded, not written.
                .filter(|workspace| workspace.validate().is_ok())
        });
        // Read the activity after observation so this tick reports the latest.
        let activity = with_mailbox(mailbox, |mailbox| mailbox.activity);
        let at = clock();
        // A refresh never replaces the heartbeat: both are one transaction.
        let _ = match observed.as_ref() {
            Some(workspace) => {
                store.heartbeat_with_workspace(id, generation, at, activity, Some(workspace))
            }
            None => store.heartbeat(id, generation, at, activity),
        };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::local_coordination::{NativePath, Registration};
    use std::path::Path;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Instant;

    fn paths_in(dir: &Path) -> CoordinationPaths {
        CoordinationPaths {
            root: dir.join("coord"),
        }
    }

    /// Registration-time observer for tests that never request a refresh.
    fn no_refresh() -> Observer {
        Arc::new(|_, _| Err(CoordinationError::WorkspaceUnavailable))
    }

    fn workspace() -> WorkspaceIdentity {
        WorkspaceIdentity {
            workspace: NativePath::from_path(Path::new("/workspace/example")),
            repo_id: None,
            worktree_id: None,
            branch_ref: None,
            observed_ms: 0,
        }
    }

    fn input(session: &str) -> RegistrationInput {
        RegistrationInput {
            data_namespace_id: "a".repeat(64),
            session_id: Some(session.into()),
            kind: ParticipantKind::Cli,
            role: ParticipantRole::Frontend,
            parent_instance_id: None,
            worker_id: None,
            job_id: None,
            workspace: workspace(),
        }
    }

    /// Wait for any row matching `predicate`, observed through a read-only
    /// connection while the guard's writer stays open.
    async fn wait_row(
        paths: &CoordinationPaths,
        limit: usize,
        predicate: impl Fn(&Registration) -> bool,
    ) -> Registration {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Some(store)) = Store::open_read(paths) {
                if let Ok(rows) = store.rows(limit) {
                    if let Some(row) = rows.into_iter().find(|row| predicate(row)) {
                        return row;
                    }
                }
            }
            assert!(Instant::now() < deadline, "the row did not reach the expected state");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A controllable monotonic wall clock for the worker; tests advance it
    /// explicitly, so no assertion depends on real elapsed time.
    fn fake_clock(start: i64) -> (Arc<AtomicI64>, Clock) {
        let value = Arc::new(AtomicI64::new(start));
        let read = value.clone();
        (
            value,
            Arc::new(move || read.load(Ordering::SeqCst)) as Clock,
        )
    }

    /// Each interval publishes exactly one bounded heartbeat that carries the
    /// latest activity, and close drains the worker before the row is read.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_guard_heartbeat_and_close() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let (clock, read) = fake_clock(1_000);
        let first_session = uuid::Uuid::new_v4().to_string();
        let mut guard = register_with(
            paths.clone(),
            input(&first_session),
            1_000,
            Duration::from_millis(20),
            read,
            no_refresh(),
        )?;
        let instance = guard.instance_id().to_string();

        let row = wait_row(&paths, 10, |row| row.heartbeat_ms == 1_000).await;
        assert_eq!(row.instance_id, instance);
        assert_eq!(row.activity, Activity::Idle);
        assert_eq!(row.lifecycle, Lifecycle::Open);

        // Advancing the clock produces the next interval's heartbeat once.
        clock.store(2_000, Ordering::SeqCst);
        let row = wait_row(&paths, 10, |row| row.heartbeat_ms == 2_000).await;
        assert_eq!(row.activity, Activity::Idle);

        // Ten activity changes coalesce into the next single heartbeat instead
        // of one write per change.
        for _ in 0..10 {
            guard.set_activity(Activity::Active);
        }
        let row = wait_row(&paths, 10, |row| row.activity == Activity::Active).await;
        assert!(row.heartbeat_ms >= 2_000);

        clock.store(3_000, Ordering::SeqCst);
        let row = wait_row(&paths, 10, |row| row.heartbeat_ms == 3_000).await;
        assert_eq!(row.activity, Activity::Active);
        let generation = row.owner.generation.clone();

        guard.close().await?;
        let closed = wait_row(&paths, 10, |row| row.lifecycle == Lifecycle::Closed).await;
        assert_eq!(closed.instance_id, instance);

        // A late write with the same generation cannot reopen the row.
        let mut store = Store::open_rw(&paths)?;
        assert!(!store.heartbeat(&instance, &generation, 4_000, Activity::Active)?);
        assert!(!store.close(&instance, &generation, 4_000)?);
        assert_eq!(store.rows(10)?[0].lifecycle, Lifecycle::Closed);

        // A new registration is a new incarnation with a new generation.
        let mut second = register_with(
            paths.clone(),
            input(&uuid::Uuid::new_v4().to_string()),
            5_000,
            Duration::from_millis(20),
            Arc::new(|| 5_000_i64) as Clock,
            no_refresh(),
        )?;
        let rows = store.rows(10)?;
        let other = rows
            .iter()
            .find(|row| row.instance_id == *second.instance_id())
            .expect("the second incarnation is registered");
        assert_ne!(other.instance_id, instance);
        assert_ne!(other.owner.generation, generation);
        second.close().await?;
        Ok(())
    }

    /// One persisted row with an explicit start time; the owner has no lease
    /// unless a guard published it, so probes report Unknown.
    fn registration_row(session: Option<&str>, started_ms: i64) -> Registration {
        let namespace = "a".repeat(64);
        Registration {
            instance_id: uuid::Uuid::new_v4().to_string(),
            data_namespace_id: namespace,
            session_id: session.map(Into::into),
            kind: ParticipantKind::Cli,
            role: ParticipantRole::Frontend,
            parent_instance_id: None,
            worker_id: None,
            job_id: None,
            owner: crate::services::local_coordination::OwnerRecord {
                platform: "linux".into(),
                pid: std::process::id(),
                boot_id: uuid::Uuid::new_v4().to_string(),
                creation_id: "1".into(),
                generation: uuid::Uuid::new_v4().to_string(),
            },
            workspace: NativePath::from_path(Path::new("/workspace/example")),
            repo_id: None,
            worktree_id: None,
            branch_ref: None,
            activity: Activity::Idle,
            activity_since_ms: started_ms,
            started_ms,
            heartbeat_ms: started_ms,
            last_git_observed_ms: None,
            lifecycle: Lifecycle::Open,
        }
    }

    fn seed(store: &mut Store, row: &Registration, close: bool) -> CoordResult<()> {
        store.insert(row)?;
        if close {
            assert!(store.close(&row.instance_id, &row.owner.generation, row.started_ms)?);
        }
        Ok(())
    }

    fn views(snapshot: &RegistrySnapshot) -> Vec<&InstanceView> {
        snapshot
            .groups
            .iter()
            .flat_map(|group| group.instances.iter())
            .collect()
    }

    fn view<'a>(snapshot: &'a RegistrySnapshot, instance: &str) -> &'a InstanceView {
        views(snapshot)
            .into_iter()
            .find(|view| view.registration.instance_id == instance)
            .expect("the instance is visible in the snapshot")
    }

    fn names(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Older terminal rows must not consume the cap that protects newer open
    /// ones, and the 101st open row is the truncation lookahead.
    #[test]
    fn local_coordination_live_cap_applies_after_lifecycle_filter() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;

        for index in 0..101 {
            seed(&mut store, &registration_row(None, index), true)?;
        }
        let live = registration_row(None, 10_000);
        seed(&mut store, &live, false)?;

        let snapshot = registry_snapshot(&paths, 20_000)?;
        let visible = views(&snapshot);
        assert_eq!(visible.len(), 1, "terminal rows must not consume the cap");
        assert!(!snapshot.truncated);
        assert_eq!(visible[0].registration.instance_id, live.instance_id);

        for index in 0..105 {
            seed(&mut store, &registration_row(None, 20_000 + index), false)?;
        }
        let snapshot = registry_snapshot(&paths, 40_000)?;
        let visible = views(&snapshot);
        assert_eq!(visible.len(), 100);
        assert!(snapshot.truncated, "the 101st open row is the lookahead");
        let newest = visible
            .iter()
            .map(|view| view.registration.started_ms)
            .max()
            .unwrap();
        assert_eq!(newest, 20_098, "the lookahead row must not be shown");
        Ok(())
    }

    /// Reads never create, mutate or recover anything, and unresponsive or
    /// unprovable owners stay visible for the user to see.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_snapshot_is_read_only() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());

        let absent = registry_snapshot(&paths, 1_000)?;
        assert!(absent.groups.is_empty());
        assert!(absent.domain_id.is_none());
        assert!(!paths.root.exists(), "a read must not create the root");

        let session = uuid::Uuid::new_v4().to_string();
        let mut guard = register_with(
            paths.clone(),
            input(&session),
            1_000,
            Duration::from_millis(20),
            Arc::new(|| 1_000_i64) as Clock,
            no_refresh(),
        )?;
        let unknown = registration_row(Some(&session), 500);
        {
            let mut store = Store::open_rw(&paths)?;
            seed(&mut store, &unknown, false)?;
        }

        let before_names = names(&paths.root);
        let before_rows = Store::open_read(&paths)?.unwrap().rows(10)?;

        // A live owner with an ancient heartbeat is unresponsive, not dead.
        let snapshot = registry_snapshot(&paths, 61_000)?;
        let live = view(&snapshot, guard.instance_id());
        assert_eq!(live.owner_proof, OwnerProof::Alive);
        assert_eq!(live.responsiveness, Responsiveness::Unresponsive);
        let missing = view(&snapshot, &unknown.instance_id);
        assert_eq!(missing.owner_proof, OwnerProof::Unknown);
        assert_eq!(missing.responsiveness, Responsiveness::Unresponsive);

        let again = registry_snapshot(&paths, 61_000)?;
        assert_eq!(views(&again).len(), views(&snapshot).len());
        assert_eq!(names(&paths.root), before_names, "a read must not add state");
        assert_eq!(
            Store::open_read(&paths)?.unwrap().rows(10)?,
            before_rows,
            "a read must not change rows or recover anything"
        );

        guard.close().await?;
        Ok(())
    }

    /// Wall-clock jumps only change the display estimate: the owner proof stays
    /// whatever the process evidence says.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_clock_jump_is_not_death() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let session = uuid::Uuid::new_v4().to_string();
        let mut guard = register_with(
            paths.clone(),
            input(&session),
            10_000,
            Duration::from_millis(20),
            Arc::new(|| 10_000_i64) as Clock,
            no_refresh(),
        )?;
        let instance = guard.instance_id().to_string();

        let current = registry_snapshot(&paths, 10_000)?;
        assert_eq!(view(&current, &instance).responsiveness, Responsiveness::Responsive);

        // A backward jump is clock uncertainty, not death.
        let backward = registry_snapshot(&paths, 5_000)?;
        let view_back = view(&backward, &instance);
        assert_eq!(view_back.responsiveness, Responsiveness::ClockUncertain);
        assert_eq!(view_back.owner_proof, OwnerProof::Alive);

        // A forward jump is only an old heartbeat.
        let forward = registry_snapshot(&paths, 10_000 + 3_600_000)?;
        let view_forward = view(&forward, &instance);
        assert_eq!(view_forward.responsiveness, Responsiveness::Unresponsive);
        assert_eq!(view_forward.owner_proof, OwnerProof::Alive);

        let store = Store::open_read(&paths)?.unwrap();
        let row = store
            .open_rows(10)?
            .into_iter()
            .find(|row| row.instance_id == instance)
            .expect("the row is still open");
        assert_eq!(row.lifecycle, Lifecycle::Open, "no cleanup or takeover happened");
        assert_eq!(row.owner.generation, view_forward.registration.owner.generation);

        guard.close().await?;
        Ok(())
    }

    /// A workspace identity with a distinct path per case.
    fn ws(path: &str, observed_ms: i64) -> WorkspaceIdentity {
        WorkspaceIdentity {
            workspace: NativePath::from_path(Path::new(path)),
            repo_id: None,
            worktree_id: None,
            branch_ref: None,
            observed_ms,
        }
    }

    /// One immediate, non-retrying read of an instance row.
    fn row_now(paths: &CoordinationPaths, instance: &str) -> Registration {
        Store::open_read(paths)
            .expect("the registry is readable")
            .expect("the registry exists")
            .rows(100)
            .expect("the rows decode")
            .into_iter()
            .find(|row| row.instance_id == instance)
            .expect("the instance row exists")
    }

    /// An observer that reports each path it takes and blocks until the test
    /// releases it with a result. A dropped release sender unblocks the worker
    /// as a fixture failure instead of parking it forever.
    fn gated_observer() -> (
        Observer,
        mpsc::Receiver<PathBuf>,
        mpsc::Sender<CoordResult<WorkspaceIdentity>>,
    ) {
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<CoordResult<WorkspaceIdentity>>();
        let release_rx = Mutex::new(release_rx);
        let observer: Observer = Arc::new(move |path: &Path, _at: i64| {
            let _ = entered.send(path.to_path_buf());
            release_rx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv()
                .unwrap_or(Err(CoordinationError::WorkspaceUnavailable))
        });
        (observer, entered_rx, release)
    }

    /// The next path the gated observer took, or a fixture failure.
    fn entered_path(entered: &mpsc::Receiver<PathBuf>) -> PathBuf {
        entered
            .recv_timeout(Duration::from_secs(20))
            .expect("the worker never observed a path")
    }

    /// A live guard plus a snapshot that must not change after a rejection.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_invalid_workspace_updates_leave_snapshot_unchanged(
    ) -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut guard = register_with(
            paths.clone(),
            RegistrationInput {
                workspace: ws("/workspace/start", 1_000),
                ..input(&uuid::Uuid::new_v4().to_string())
            },
            1_000,
            HEARTBEAT_INTERVAL,
            Arc::new(|| 1_000_i64) as Clock,
            no_refresh(),
        )?;
        let before = registry_snapshot(&paths, 1_000)?;

        let valid = ws("/workspace/start", 1_000);
        let mut cases: Vec<(&str, WorkspaceIdentity)> = Vec::new();
        let mut relative = valid.clone();
        relative.workspace = NativePath::from_path(Path::new("relative/path"));
        cases.push(("relative path", relative));
        let mut repo = valid.clone();
        repo.repo_id = Some("not-a-hash".into());
        cases.push(("repo id", repo));
        let mut worktree = valid.clone();
        worktree.worktree_id = Some("Z".repeat(64));
        cases.push(("worktree id", worktree));
        let mut empty_ref = valid.clone();
        empty_ref.branch_ref = Some(Vec::new());
        cases.push(("empty ref", empty_ref));
        let mut long_ref = valid.clone();
        long_ref.branch_ref = Some(vec![b'a'; 1025]);
        cases.push(("long ref", long_ref));
        let mut nul_ref = valid.clone();
        nul_ref.branch_ref = Some(b"refs/heads/a\0b".to_vec());
        cases.push(("NUL ref", nul_ref));
        let mut lf_ref = valid.clone();
        lf_ref.branch_ref = Some(b"refs/heads/a\nb".to_vec());
        cases.push(("LF ref", lf_ref));
        let mut negative = valid.clone();
        negative.observed_ms = -1;
        cases.push(("negative observation", negative));

        for (label, workspace) in cases {
            let error = guard
                .update_workspace(workspace)
                .expect_err("an unpersistable workspace must be rejected");
            assert!(
                matches!(error, CoordinationError::InvalidRecord(_)),
                "{label} must be an invalid record, got {error}"
            );
            assert_eq!(
                registry_snapshot(&paths, 1_000)?,
                before,
                "{label} changed the snapshot"
            );
        }

        // A valid change is accepted and becomes readable.
        let mut changed = valid.clone();
        changed.workspace = NativePath::from_path(Path::new("/workspace/changed"));
        guard.update_workspace(changed.clone())?;
        let row = row_now(&paths, guard.instance_id());
        assert_eq!(row.workspace, changed.workspace);

        // A stale generation is a no-op, not a write to someone else's row.
        let mut store = Store::open_rw(&paths)?;
        let stale = uuid::Uuid::new_v4().to_string();
        assert!(!store.update_workspace(
            guard.instance_id(),
            &stale,
            &ws("/workspace/stale", 9_000),
            9_000
        )?);
        assert_eq!(row_now(&paths, guard.instance_id()).workspace, changed.workspace);

        guard.close().await?;
        Ok(())
    }

    /// Registration persists the workspace's own observation time, while the
    /// incarnation times come from the registration clock.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_registration_uses_workspace_observation_time() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let observed = WorkspaceIdentity {
            repo_id: Some("a".repeat(64)),
            worktree_id: Some("b".repeat(64)),
            branch_ref: Some(b"refs/heads/main".to_vec()),
            observed_ms: 1_000,
            ..ws("/workspace/repo", 1_000)
        };
        let mut guard = register_with(
            paths.clone(),
            RegistrationInput {
                workspace: observed,
                ..input(&uuid::Uuid::new_v4().to_string())
            },
            2_000,
            HEARTBEAT_INTERVAL,
            Arc::new(|| 2_000_i64) as Clock,
            no_refresh(),
        )?;
        let row = row_now(&paths, guard.instance_id());
        assert_eq!(row.last_git_observed_ms, Some(1_000));
        assert_eq!(row.started_ms, 2_000);
        assert_eq!(row.heartbeat_ms, 2_000);
        guard.close().await?;

        // Without a repository there is no Git observation at all.
        let mut plain = register_with(
            paths.clone(),
            input(&uuid::Uuid::new_v4().to_string()),
            2_000,
            HEARTBEAT_INTERVAL,
            Arc::new(|| 2_000_i64) as Clock,
            no_refresh(),
        )?;
        assert_eq!(row_now(&paths, plain.instance_id()).last_git_observed_ms, None);
        plain.close().await?;
        Ok(())
    }

    /// A refresh request never replaces the heartbeat: whichever path is taken,
    /// the due tick still advances the heartbeat and the latest activity, and
    /// only the newest requested path is observed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_workspace_refresh_coalesces_and_preserves_heartbeat(
    ) -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let (observer, entered, release) = gated_observer();
        let (clock, read) = fake_clock(1_000);
        let mut guard = register_with(
            paths.clone(),
            RegistrationInput {
                workspace: ws("/workspace/a", 1_000),
                ..input(&uuid::Uuid::new_v4().to_string())
            },
            1_000,
            Duration::from_millis(20),
            read,
            observer,
        )?;

        // A is taken first; B and C arrive while it is still being observed.
        guard.request_workspace_refresh(PathBuf::from("/workspace/a"));
        assert_eq!(entered_path(&entered), PathBuf::from("/workspace/a"));
        guard.request_workspace_refresh(PathBuf::from("/workspace/b"));
        guard.request_workspace_refresh(PathBuf::from("/workspace/c"));
        guard.set_activity(Activity::Active);
        clock.store(2_000, Ordering::SeqCst);
        release
            .send(Ok(ws("/workspace/a", 2_000)))
            .expect("the worker is waiting for A");

        // Only the newest path survives, and A's own tick also beat.
        assert_eq!(entered_path(&entered), PathBuf::from("/workspace/c"));
        let row = row_now(&paths, guard.instance_id());
        assert_eq!(row.workspace, ws("/workspace/a", 2_000).workspace);
        assert_eq!(row.heartbeat_ms, 2_000);
        assert_eq!(row.activity, Activity::Active);

        // A failing observation discards its path but still advances the tick.
        guard.request_workspace_refresh(PathBuf::from("/workspace/d"));
        guard.set_activity(Activity::Idle);
        clock.store(3_000, Ordering::SeqCst);
        release
            .send(Err(CoordinationError::WorkspaceUnavailable))
            .expect("the worker is waiting for C");
        assert_eq!(entered_path(&entered), PathBuf::from("/workspace/d"));
        let row = row_now(&paths, guard.instance_id());
        assert_eq!(row.workspace, ws("/workspace/a", 2_000).workspace);
        assert_eq!(row.heartbeat_ms, 3_000);
        assert_eq!(row.activity, Activity::Idle);

        release
            .send(Ok(ws("/workspace/d", 3_000)))
            .expect("the worker is waiting for D");
        guard.close().await?;
        Ok(())
    }

    /// `close` drains an in-flight refresh before it returns: the tick commits
    /// its workspace and heartbeat, then the row is closed and the lease gone.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_coordination_close_drains_a_gated_inflight_refresh() -> CoordResult<()> {
        use std::future::Future;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let (observer, entered, release) = gated_observer();
        let mut guard = register_with(
            paths.clone(),
            RegistrationInput {
                workspace: ws("/workspace/before", 1_000),
                ..input(&uuid::Uuid::new_v4().to_string())
            },
            1_000,
            Duration::from_millis(20),
            Arc::new(|| 2_000_i64) as Clock,
            observer,
        )?;
        let instance = guard.instance_id().to_string();
        let record = row_now(&paths, &instance).owner;

        // Park the tick inside the observer, then start closing.
        guard.request_workspace_refresh(PathBuf::from("/workspace/gated"));
        assert_eq!(entered_path(&entered), PathBuf::from("/workspace/gated"));
        let mut close = Box::pin(guard.close());
        let first = close.as_mut().poll(&mut std::task::Context::from_waker(
            std::task::Waker::noop(),
        ));
        assert!(
            first.is_pending(),
            "close must wait for the in-flight observation"
        );

        // Release only after the poll asserted Pending, so a failure here still
        // lets the worker finish and the guard's Drop close the row.
        release
            .send(Ok(ws("/workspace/gated", 2_000)))
            .expect("the worker is waiting for the gated path");
        close.as_mut().await?;

        // One immediate read: no polling, retrying or sleeping.
        let row = row_now(&paths, &instance);
        assert_eq!(row.lifecycle, Lifecycle::Closed);
        assert_eq!(row.workspace, ws("/workspace/gated", 2_000).workspace);
        assert_eq!(row.heartbeat_ms, 2_000);
        assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);

        // A same-generation late heartbeat cannot resurrect the row.
        let mut store = Store::open_rw(&paths)?;
        assert!(!store.heartbeat(&instance, &record.generation, 3_000, Activity::Active)?);
        assert_eq!(row_now(&paths, &instance).lifecycle, Lifecycle::Closed);
        Ok(())
    }

    fn registration_input() -> RegistrationInput {
        RegistrationInput {
            workspace: ws("/workspace/runtime", 1_000),
            ..input(&uuid::Uuid::new_v4().to_string())
        }
    }

    /// The synchronous API needs no Tokio runtime: the worker is a real thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn local_coordination_register_without_runtime() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        // Registration happens on a thread with no runtime at all.
        let mut guard = RegistryGuard::register(paths.clone(), registration_input(), 1_000)?;
        let instance = guard.instance_id().to_string();

        let snapshot = registry_snapshot(&paths, 1_000)?;
        assert_eq!(snapshot.groups.len(), 1);
        let view = &snapshot.groups[0].instances[0];
        assert_eq!(view.registration.instance_id, instance);
        assert_eq!(view.owner_proof, OwnerProof::Alive);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(guard.close())?;
        assert_eq!(row_now(&paths, &instance).lifecycle, Lifecycle::Closed);
        assert_eq!(probe_owner(&paths, &instance, &view.registration.owner), OwnerProof::Unknown);
        Ok(())
    }

    /// Registration into a discarded runtime still works, because it never used
    /// that runtime's blocking pool.
    #[cfg(target_os = "linux")]
    #[test]
    fn local_coordination_register_on_stopped_runtime() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let stopped = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = stopped.handle().clone();
        drop(stopped);

        let entered = handle.enter();
        let mut guard =
            RegistryGuard::register(paths.clone(), registration_input(), 1_000)
                .expect("a stopped runtime must not block registration");
        let instance = guard.instance_id().to_string();
        let record = row_now(&paths, &instance).owner;
        assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Alive);
        drop(entered);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(guard.close())?;
        assert_eq!(row_now(&paths, &instance).lifecycle, Lifecycle::Closed);
        assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);
        Ok(())
    }

    /// A bare Drop still closes and releases while the process stays alive,
    /// even after the runtime that hosted the guard is gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn local_coordination_bare_drop_closes_and_releases_lease() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (instance, record) = runtime.block_on(async {
            let guard = RegistryGuard::register(paths.clone(), registration_input(), 1_000)?;
            let instance = guard.instance_id().to_string();
            let record = row_now(&paths, &instance).owner;
            Ok::<_, CoordinationError>((instance, record))
        })?;
        drop(runtime);

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let row = row_now(&paths, &instance);
            let proof = probe_owner(&paths, &instance, &record);
            if row.lifecycle == Lifecycle::Closed && proof == OwnerProof::Unknown {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a bare Drop never closed the row: {row:?} / {proof:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            paths
                .root
                .join("owners")
                .join(format!("{instance}.lock"))
                .exists(),
            "the immutable lease record stays behind"
        );
        Ok(())
    }

    /// Unsupported platforms refuse before bootstrapping anything.
    #[cfg(all(test, not(target_os = "linux")))]
    #[test]
    fn local_coordination_unsupported_registration_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let error = RegistryGuard::register(paths.clone(), registration_input(), 1_000)
            .err()
            .expect("an unsupported platform must not register");
        assert!(matches!(error, CoordinationError::UnsupportedPlatform));
        assert!(!paths.root.exists(), "nothing may be created");
    }
}
