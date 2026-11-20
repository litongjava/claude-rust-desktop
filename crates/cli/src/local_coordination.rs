//! Read-only `dcl sessions live` inspection of the local coordination registry.
//!
//! Nothing here registers a participant, opens the conversation database or
//! contacts a provider: the command renders whatever the shared registry
//! already holds, and never repairs, deletes or rewrites it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use deepseek_code_core::runtime::RuntimePaths;
use deepseek_code_core::services::local_coordination::{
    data_namespace, escaped_native, load_settings, observe_workspace, registry_snapshot,
    CoordinationError, CoordinationPaths, InstanceView, NativePath, OwnerProof, ParticipantKind,
    ParticipantRole, RegistrationInput, RegistryGuard, RegistrySnapshot, Responsiveness,
    COORDINATION_DIR_ENV,
};
use deepseek_code_core::services::{background_sessions, session_worker};
use serde_json::{json, Value};

use crate::args::Cli;

/// How a `Session` acquires its registry incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistrationMode {
    /// The session registers itself from its settings snapshot.
    Automatic,
    /// A persistent worker host owns the only incarnation for this session.
    HostOwned,
}

/// How this launch decided the coordination root.
#[derive(Clone)]
enum RootSelection {
    /// Coordination is off and no explicit root was requested; no lookup ran.
    Deferred,
    /// The launch root was resolved and may be reused by every incarnation.
    Ready(CoordinationPaths),
    /// A required lookup failed; the failure was already reported once.
    Rejected,
}

/// One startup decision reused by every incarnation of a session.
///
/// The launch inputs are retained so a later incarnation can resolve once more
/// if the user opts in, without falling back to the current directory.
#[derive(Clone)]
pub(crate) struct CoordinationContext {
    launch_cwd: Option<PathBuf>,
    explicit: Option<std::ffi::OsString>,
    inherited: Option<std::ffi::OsString>,
    root: RootSelection,
}

impl CoordinationContext {
    /// The native value every host child must inherit from this launch.
    ///
    /// A deferred launch has nothing to export. A rejected lookup yields an
    /// empty native value: the resolver rejects an explicitly empty override, so
    /// children stay unavailable instead of falling back to another root.
    pub(crate) fn child_root(&self) -> Option<&std::ffi::OsStr> {
        match &self.root {
            RootSelection::Ready(paths) => Some(paths.root.as_os_str()),
            RootSelection::Deferred => None,
            RootSelection::Rejected => Some(std::ffi::OsStr::new("")),
        }
    }
}

/// The settings and root decision made once at the CLI launch boundary.
pub(crate) struct PreparedCoordination {
    pub(crate) context: CoordinationContext,
    pub(crate) enabled: bool,
}

/// Identifier prefix used in text output; a collision falls back to the full id.
const SHORT_ID: usize = 12;

/// Resolve the coordination root before `-C` is applied.
///
/// Precedence is the explicit flag, then the environment, then the shared home
/// root. The environment is read here rather than by the argument parser so the
/// order can be exercised by tests.
pub(crate) fn resolve_paths(cli: &Cli, launch_cwd: &Path) -> Result<CoordinationPaths> {
    let env = std::env::var_os(COORDINATION_DIR_ENV);
    // A path that cannot be resolved is reported with the same fixed category
    // as any other unavailable store; the resolver's text never reaches stderr.
    CoordinationPaths::resolve(
        cli.coordination_dir.as_deref().map(Path::new),
        env.as_deref(),
        dirs::home_dir().as_deref(),
        launch_cwd,
    )
    .map_err(|_| unavailable(None))
}

/// One fixed token per failure class. Raw error text, SQL, paths and I/O details
/// never reach the terminal, and the message says the state was not reset.
///
/// `None` covers failures outside the typed store (root resolution, launch
/// directory lookup): they are reported as the plain `unavailable` class with
/// no source chain, so no path text or diagnostic leaks.
pub(crate) fn unavailable(error: Option<&CoordinationError>) -> anyhow::Error {
    anyhow!(
        "Local coordination unavailable: {}. No state was reset.",
        category(error)
    )
}

/// One fixed token per failure class; `None` is the plain `unavailable` class.
fn category(error: Option<&CoordinationError>) -> &'static str {
    match error {
        Some(CoordinationError::Busy) => "busy",
        Some(CoordinationError::UnsupportedSchema(_)) => "unsupported_schema",
        Some(CoordinationError::Corrupt) => "corrupt",
        Some(CoordinationError::MissingDatabase) => "missing_database",
        Some(CoordinationError::DomainMismatch) => "domain_mismatch",
        // I/O, SQL and every other variant are one fixed class on purpose.
        _ => "unavailable",
    }
}

/// Best-effort registration failure: one bounded line, no raw error or path.
fn warn_registry(error: Option<&CoordinationError>) {
    eprintln!(
        "Local coordination registry unavailable: {}. Session continues without peer visibility.",
        category(error)
    );
}

/// Prepare the one coordination decision this launch uses everywhere.
///
/// Settings are read once, off the async scheduler. A deferred startup does no
/// coordination-root filesystem lookup at all; an enabled one or an explicit
/// CLI override resolves from the captured launch inputs, never from the
/// process's later current directory.
pub(crate) async fn prepare_startup(
    cli: &Cli,
    paths: &RuntimePaths,
    launch_cwd: Option<&Path>,
) -> PreparedCoordination {
    let context = CoordinationContext {
        launch_cwd: launch_cwd.map(Path::to_path_buf),
        explicit: cli.coordination_dir.clone(),
        // Captured before this launch exports anything to its children.
        inherited: std::env::var_os(COORDINATION_DIR_ENV),
        root: RootSelection::Deferred,
    };
    prepare_decision(paths.data_dir.clone(), context).await
}

/// Re-read the opt-in decision for a new incarnation of an already-started session.
///
/// The launch inputs are never replaced, so a root that was ready stays exactly
/// the same bytes and a deferred launch can resolve at most once, from the
/// directory it was launched in rather than the current workspace.
pub(crate) async fn prepare_next_incarnation(
    context: CoordinationContext,
    data_dir: PathBuf,
) -> PreparedCoordination {
    prepare_decision(data_dir, context).await
}

/// The one blocking preparation shared by startup and by every transition.
async fn prepare_decision(data_dir: PathBuf, context: CoordinationContext) -> PreparedCoordination {
    let fallback = CoordinationContext {
        launch_cwd: context.launch_cwd.clone(),
        explicit: context.explicit.clone(),
        inherited: context.inherited.clone(),
        root: RootSelection::Rejected,
    };
    let prepared = tokio::task::spawn_blocking(move || {
        let explicit = context.explicit.is_some();
        let settings = load_settings(&data_dir);
        let enabled = settings
            .as_ref()
            .is_ok_and(|settings| settings.enabled && settings.registry);
        // Invalid settings continue disabled; a separately valid explicit root
        // is still selected, so its children inherit exactly that root.
        let mut warn = settings.is_err();
        // A ready root is reused byte for byte and a rejection is sticky for the
        // launch. Only a deferred decision may resolve, and only once.
        let root = match context.root {
            RootSelection::Ready(paths) => RootSelection::Ready(paths),
            RootSelection::Rejected => RootSelection::Rejected,
            RootSelection::Deferred if !enabled && !explicit => RootSelection::Deferred,
            RootSelection::Deferred => match resolve_captured(&context) {
                Ok(paths) => RootSelection::Ready(paths),
                Err(_) => {
                    warn = true;
                    RootSelection::Rejected
                }
            },
        };
        (CoordinationContext { root, ..context }, enabled, warn)
    })
    .await;
    match prepared {
        Ok((context, enabled, warn)) => {
            if warn {
                warn_registry(None);
            }
            PreparedCoordination { context, enabled }
        }
        Err(_) => {
            // A failed join still leaves one bounded warning, once.
            warn_registry(None);
            PreparedCoordination {
                context: fallback,
                enabled: false,
            }
        }
    }
}

/// Publish a session's current native launch root at the CLI boundary.
///
/// A deferred launch exported nothing; the first committed transition that opts
/// in resolves the root, so the value is reapplied before any child starts.
pub(crate) fn export_session_child_root(root: Option<&std::ffi::OsStr>) {
    if let Some(value) = root {
        std::env::set_var(COORDINATION_DIR_ENV, value);
    }
}

/// Resolve the captured launch inputs without consulting the current directory.
fn resolve_captured(context: &CoordinationContext) -> Result<CoordinationPaths> {
    let launch_cwd = context
        .launch_cwd
        .as_deref()
        .ok_or_else(|| unavailable(None))?;
    CoordinationPaths::resolve(
        context.explicit.as_deref().map(Path::new),
        context.inherited.as_deref(),
        dirs::home_dir().as_deref(),
        launch_cwd,
    )
    .map_err(|_| unavailable(None))
}

/// Export the prepared native root for every known host child.
///
/// Only the CLI launch boundary calls this, and only an opted-in launch or an
/// explicit override exports; a deferred launch leaves the environment alone.
pub(crate) fn export_child_root(prepared: &PreparedCoordination) {
    if !(prepared.enabled || prepared.context.explicit.is_some()) {
        return;
    }
    if let Some(value) = prepared.context.child_root() {
        std::env::set_var(COORDINATION_DIR_ENV, value);
    }
}

/// Drop the original coordination override from a relaunch or handoff argv.
///
/// The replacement child inherits the exported native root instead, so a
/// relative launch override cannot be re-anchored at a different directory and
/// a native value never has to be decoded. Native bytes are inspected before
/// any conversion; everything after a standalone `--` is positional and kept
/// verbatim, even when it looks like this flag.
pub(crate) fn child_argv_without_coordination(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Vec<String>> {
    let mut kept: Vec<std::ffi::OsString> = Vec::new();
    let mut positional = false;
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        if positional {
            kept.push(arg);
            continue;
        }
        if arg == "--" {
            positional = true;
            kept.push(arg);
            continue;
        }
        let bytes = arg.as_encoded_bytes();
        if bytes == b"--coordination-dir" {
            // The split value belongs to this flag; a missing value is left for
            // the child's own parser rather than invented here.
            rest.next();
            continue;
        }
        if bytes.starts_with(b"--coordination-dir=") {
            continue;
        }
        kept.push(arg);
    }
    kept.into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow!("Child process arguments must be UTF-8"))
        })
        .collect()
}

/// Replace a session's incarnation after a successful resume or clear, using the
/// root the launch already resolved.
///
/// A deferred or rejected context has no usable root, so this returns without a
/// second lookup and without a second warning.
pub(crate) async fn register_with_context(
    context: &CoordinationContext,
    data_dir: PathBuf,
    session_id: String,
    workspace: PathBuf,
) -> Option<RegistryGuard> {
    let RootSelection::Ready(coordination) = context.root.clone() else {
        return None;
    };
    register_with_root(coordination, data_dir, session_id, workspace).await
}

/// Why a persistent host did or did not get a coordination root.
enum HostRoot {
    /// Coordination is off for the host's data directory.
    Disabled,
    /// Enabled, but the host's own identity is not a canonical UUID.
    Refused,
    Ready(CoordinationPaths),
}

/// Register the persistent worker host that owns a session's execution.
///
/// The host inherits the frontend's resolved absolute root through the
/// environment; an old worker may still belong to another root and is kept as
/// the verified working root for its own lifetime.
pub(crate) async fn register_worker(
    worker: &session_worker::Worker,
) -> Result<Option<RegistryGuard>> {
    let root = worker.root.clone();
    // A worker whose own identity is not a canonical UUID never joins; the host
    // keeps serving, so this is a bounded registry warning only.
    let canonical_id = canonical_uuid(&worker.id);
    // Settings and the inherited root are read on the blocking adapter, so the
    // async scheduler never performs configuration or path I/O for the host.
    let resolved = tokio::task::spawn_blocking(move || -> Result<HostRoot> {
        let settings = load_settings(&root).map_err(|_| unavailable(None))?;
        if !(settings.enabled && settings.registry) {
            return Ok(HostRoot::Disabled);
        }
        // Checked before any root, store or lease work.
        if !canonical_id {
            return Ok(HostRoot::Refused);
        }
        let launch_cwd = std::env::current_dir().map_err(|_| unavailable(None))?;
        CoordinationPaths::resolve(
            None,
            std::env::var_os(COORDINATION_DIR_ENV).as_deref(),
            dirs::home_dir().as_deref(),
            &launch_cwd,
        )
        .map(HostRoot::Ready)
        .map_err(|_| unavailable(None))
    })
    .await;
    let coordination = match resolved {
        Ok(Ok(HostRoot::Ready(coordination))) => coordination,
        Ok(Ok(HostRoot::Disabled)) => return Ok(None),
        Ok(Ok(HostRoot::Refused)) => {
            warn_registry(None);
            return Ok(None);
        }
        _ => {
            warn_registry(None);
            return Ok(None);
        }
    };
    let job_id = worker.active_job.clone().filter(|id| canonical_uuid(id));
    Ok(register_incarnation(
        coordination,
        worker.root.clone(),
        worker.session_id.clone(),
        worker.workspace.clone(),
        ParticipantKind::BackgroundAgent,
        ParticipantRole::Executor,
        Some(worker.id.clone()),
        job_id,
    )
    .await)
}

async fn register_with_root(
    coordination: CoordinationPaths,
    data_dir: PathBuf,
    session_id: String,
    workspace: PathBuf,
) -> Option<RegistryGuard> {
    // The validated control directory names a job only when it already proves
    // ownership of this session; the directory leaf is that job's UUID. The
    // lookup reads worker metadata, so it runs off the async scheduler.
    let lookup_data = data_dir.clone();
    let lookup_session = session_id.clone();
    let internal = match tokio::task::spawn_blocking(move || {
        background_sessions::worker_directory(&lookup_data, &lookup_session)
    })
    .await
    {
        Ok(Ok(dir)) => dir,
        _ => {
            warn_registry(None);
            return None;
        }
    };
    let (kind, role, job_id) = match internal {
        Some(dir) => (
            ParticipantKind::BackgroundAgent,
            ParticipantRole::Executor,
            dir.file_name()
                .and_then(|name| name.to_str())
                .filter(|name| canonical_uuid(name))
                .map(str::to_owned),
        ),
        None => (ParticipantKind::Cli, ParticipantRole::Frontend, None),
    };
    register_incarnation(
        coordination,
        data_dir,
        session_id,
        workspace,
        kind,
        role,
        None,
        job_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn register_incarnation(
    coordination: CoordinationPaths,
    data_dir: PathBuf,
    session_id: String,
    workspace: PathBuf,
    kind: ParticipantKind,
    role: ParticipantRole,
    worker_id: Option<String>,
    job_id: Option<String>,
) -> Option<RegistryGuard> {
    let now = wall_clock();
    let observed = match observe_workspace(&workspace, now).await {
        Ok(observed) => observed,
        Err(error) => {
            warn_registry(Some(&error));
            return None;
        }
    };
    let input = RegistrationInput {
        // Filled by the blocking call, which owns the data directory too.
        data_namespace_id: String::new(),
        session_id: Some(session_id),
        kind,
        role,
        parent_instance_id: None,
        worker_id,
        job_id,
        workspace: observed,
    };
    let registered = tokio::task::spawn_blocking(move || {
        let namespace = data_namespace(&data_dir)?;
        let input = RegistrationInput {
            data_namespace_id: namespace,
            ..input
        };
        RegistryGuard::register(coordination, input, now)
    })
    .await;
    match registered {
        Ok(Ok(guard)) => Some(guard),
        Ok(Err(error)) => {
            warn_registry(Some(&error));
            None
        }
        Err(_) => {
            warn_registry(None);
            None
        }
    }
}

fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn wall_clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Print the current registry state. `enabled=false` in settings does not apply:
/// this is read-only inspection and never starts coordination.
pub(crate) fn show_live(paths: &CoordinationPaths, json: bool) -> Result<()> {
    let snapshot =
        registry_snapshot(paths, wall_clock()).map_err(|error| unavailable(Some(&error)))?;
    if json {
        // `live_json` already ends with exactly one LF.
        print!("{}", live_json(&snapshot, &paths.root));
    } else {
        print!("{}", render_live(&snapshot, &paths.root));
    }
    Ok(())
}

fn native_path(path: &Path) -> NativePath {
    NativePath::from_path(path)
}

/// Lossless native bytes plus the escaped form for humans.
fn native_json(path: &NativePath) -> Value {
    json!({
        "encoding": path.encoding,
        "base64": base64(&path.bytes),
        "display": escaped_native(path),
    })
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Twelve hex characters of an identifier, unless that prefix is ambiguous.
fn short_for(ids: &[String], id: &str) -> String {
    let shorten =
        |value: &str| -> String { value.chars().filter(|c| *c != '-').take(SHORT_ID).collect() };
    let prefix = shorten(id);
    let collisions = ids.iter().filter(|other| shorten(other) == prefix).count();
    if collisions > 1 {
        id.to_owned()
    } else {
        prefix
    }
}

fn namespace_prefix(namespace: &str) -> String {
    namespace.chars().take(SHORT_ID).collect()
}

fn escaped_bytes(encoding: &str, bytes: Vec<u8>) -> String {
    escaped_native(&NativePath {
        encoding: encoding.to_owned(),
        bytes,
    })
}

fn owner_word(proof: OwnerProof) -> &'static str {
    match proof {
        OwnerProof::Alive => "alive",
        OwnerProof::Gone => "gone",
        OwnerProof::Unknown => "unknown",
    }
}

fn heartbeat_word(responsiveness: Responsiveness) -> &'static str {
    match responsiveness {
        Responsiveness::Responsive => "responsive",
        Responsiveness::Unresponsive => "unresponsive",
        Responsiveness::ClockUncertain => "clock_uncertain",
    }
}

fn instance_json(view: &InstanceView) -> Value {
    let registration = &view.registration;
    json!({
        "instance_id": registration.instance_id,
        "parent_instance_id": registration.parent_instance_id,
        "kind": registration.kind.as_str(),
        "role": registration.role.as_str(),
        "worker_id": registration.worker_id,
        "job_id": registration.job_id,
        "owner": {
            "pid": registration.owner.pid,
            "platform": registration.owner.platform,
            "boot_id": registration.owner.boot_id,
            "creation_id": registration.owner.creation_id,
            "generation": registration.owner.generation,
        },
        "workspace": native_json(&registration.workspace),
        // Null is the stored value for both a detached HEAD and an unavailable
        // branch probe; the JSON never invents a branch.
        "branch_ref": registration.branch_ref.as_ref().map(|bytes| native_json(&NativePath {
            encoding: registration.workspace.encoding.clone(),
            bytes: bytes.clone(),
        })),
        "repo_id": registration.repo_id,
        "worktree_id": registration.worktree_id,
        "activity": registration.activity.as_str(),
        "started_ms": registration.started_ms,
        "heartbeat_ms": registration.heartbeat_ms,
        "owner_proof": owner_word(view.owner_proof),
        "responsiveness": heartbeat_word(view.responsiveness),
    })
}

fn live_json(snapshot: &RegistrySnapshot, root: &Path) -> String {
    let groups: Vec<Value> = snapshot
        .groups
        .iter()
        .map(|group| {
            json!({
                "data_namespace_id": group.data_namespace_id,
                "session_id": group.session_id,
                "instances": group.instances.iter().map(instance_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    let value = json!({
        "format_version": 1,
        "domain_id": snapshot.domain_id,
        "root": native_json(&native_path(root)),
        "observed_ms": snapshot.observed_ms,
        "coverage": "cli_and_persistent_workers",
        "groups": groups,
        "truncated": snapshot.truncated,
    });
    let mut line = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    line.push('\n');
    line
}

/// A terminal-free rendering: one plain line per field, no control sequences.
pub(crate) fn render_live(snapshot: &RegistrySnapshot, root: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Local coordination: {}\n",
        escaped_native(&native_path(root))
    ));
    out.push_str("Coverage: participating CLI sessions and persistent workers.\n");
    if snapshot.groups.is_empty() {
        out.push_str("No live local sessions.\n");
    }

    let all_ids: Vec<String> = snapshot
        .groups
        .iter()
        .flat_map(|group| group.instances.iter())
        .map(|view| view.registration.instance_id.clone())
        .collect();
    let session_ids: Vec<String> = snapshot
        .groups
        .iter()
        .filter_map(|group| group.session_id.clone())
        .collect();

    for group in &snapshot.groups {
        let session = group
            .session_id
            .as_deref()
            .map(|session| short_for(&session_ids, session))
            .unwrap_or_else(|| "-".to_owned());
        out.push_str(&format!(
            "Session {session} [{}] \u{2014} {} participants\n",
            namespace_prefix(&group.data_namespace_id),
            group.instances.len()
        ));
        for view in &group.instances {
            let registration = &view.registration;
            let branch = match &registration.branch_ref {
                Some(bytes) => escaped_bytes(&registration.workspace.encoding, bytes.clone()),
                None => "unavailable".to_owned(),
            };
            out.push_str(&format!(
                "  {} {} pid={} {} owner={} heartbeat={} workspace={} branch={}\n",
                short_for(&all_ids, &registration.instance_id),
                registration.role.as_str(),
                registration.owner.pid,
                registration.activity.as_str(),
                owner_word(view.owner_proof),
                heartbeat_word(view.responsiveness),
                escaped_native(&registration.workspace),
                branch,
            ));
        }
    }
    if snapshot.truncated {
        out.push_str("More participants exist; output limited to 100.\n");
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn context(root: RootSelection) -> CoordinationContext {
        CoordinationContext {
            launch_cwd: None,
            explicit: None,
            inherited: None,
            root,
        }
    }

    /// Both coordination-flag forms are removed before decoding, positional text
    /// after `--` survives, and only the prepared root reaches a child.
    #[test]
    fn registry_child_argv_uses_exported_native_root_for_handoff() {
        let argv = vec![
            OsString::from("--data-dir"),
            OsString::from("/tmp/data"),
            OsString::from_vec(b"--coordination-dir".to_vec()),
            OsString::from_vec(b"/tmp/coord-\xff".to_vec()),
            OsString::from_vec(b"--coordination-dir=/tmp/coord-\xfe".to_vec()),
            OsString::from("--permission-mode"),
            OsString::from("ask"),
            OsString::from("--"),
            OsString::from("--coordination-dir=keep-me"),
            OsString::from("positional"),
        ];
        let filtered = child_argv_without_coordination(argv).expect("the remaining args are UTF-8");
        assert_eq!(
            filtered,
            vec![
                "--data-dir",
                "/tmp/data",
                "--permission-mode",
                "ask",
                "--",
                "--coordination-dir=keep-me",
                "positional",
            ],
            "flag order is preserved and only the override is dropped"
        );
        let separator = filtered.iter().position(|arg| arg == "--").unwrap();
        assert!(
            !filtered[..separator]
                .iter()
                .any(|arg| arg == "--coordination-dir" || arg.starts_with("--coordination-dir=")),
            "no original override survives before the separator"
        );

        // The existing launcher, handoff and restart builders keep that shape.
        let workspace = Path::new("/tmp/workspace");
        let session = "session-id";
        for args in [
            crate::remote_host::launch_args(&filtered),
            crate::remote_host::child_args(&filtered, None, session, workspace),
            crate::restart::args(&filtered, None, session, workspace),
        ] {
            assert!(args.iter().any(|arg| arg == "positional"));
            let boundary = args
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(args.len());
            assert!(
                !args[..boundary].iter().any(
                    |arg| arg == "--coordination-dir" || arg.starts_with("--coordination-dir=")
                ),
                "a builder must not reintroduce the original override"
            );
        }

        // A ready context exports its exact native bytes; a rejected lookup
        // exports an empty value the resolver refuses instead of a default root.
        let root = PathBuf::from(OsStr::from_bytes(b"/tmp/coord-\xff"));
        let ready = context(RootSelection::Ready(CoordinationPaths {
            root: root.clone(),
        }));
        assert_eq!(
            ready
                .child_root()
                .expect("ready exports its root")
                .as_bytes(),
            root.as_os_str().as_bytes()
        );
        let rejected = context(RootSelection::Rejected);
        let value = rejected
            .child_root()
            .expect("a rejected lookup still exports a value");
        assert!(value.as_bytes().is_empty());
        assert!(
            CoordinationPaths::resolve(
                None,
                Some(value),
                Some(Path::new("/tmp/home")),
                Path::new("/tmp"),
            )
            .is_err(),
            "an empty native override never resolves to a default root"
        );
        assert!(context(RootSelection::Deferred).child_root().is_none());
    }
}
