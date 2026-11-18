//! Linux owner leases: immutable incarnations and process-level proof.
//!
//! The ignored `lc_process_helper` below is the fixture process: it publishes
//! leases, records its identity, and (where required) forks a child that keeps
//! an inherited descriptor open behind a pipe gate.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use deepseek_code_core::services::local_coordination::{
    data_namespace, observe_workspace, probe_owner, registry_snapshot, CoordinationError,
    CoordinationPaths, OwnerLease, OwnerProof, OwnerRecord, ParticipantKind, ParticipantRole,
    RegistrationInput, RegistryGuard,
};
use rustix::fd::OwnedFd;

const HELPER: &str = "lc_process_helper";
const STARTUP: Duration = Duration::from_secs(30);
const CLEANUP: Duration = Duration::from_secs(10);

fn paths_in(fixture: &Path) -> CoordinationPaths {
    CoordinationPaths {
        root: fixture.join("coord"),
    }
}

/// A direct child this test owns, reaped on every path including unwinding.
///
/// The guard keeps the `Child` until a status has been collected, so `wait`
/// always runs and never targets a numeric PID that could have been reused.
struct OwnedChild {
    child: Option<Child>,
}

impl OwnedChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    /// Kill and reap through the owned handle, preserving a status that already
    /// exists. `wait` runs even when the kill races a normal exit.
    fn terminate_and_reap(&mut self) -> std::io::Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            return Err(std::io::Error::other("the guarded child is already reaped"));
        };
        if let Some(status) = child.try_wait()? {
            self.child = None;
            return Ok(status);
        }
        let _ = child.kill();
        let status = child.wait();
        if status.is_ok() {
            self.child = None;
        }
        status
    }

    /// Wait for a normal helper exit within `deadline`; the Drop fallback still
    /// ends the helper if the wait fails.
    fn wait_success(&mut self, deadline: Instant) -> std::io::Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            return Err(std::io::Error::other("the guarded child is already reaped"));
        };
        loop {
            if let Some(status) = child.try_wait()? {
                self.child = None;
                if !status.success() {
                    return Err(std::io::Error::other(format!(
                        "helper exited unsuccessfully: {status}"
                    )));
                }
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::other("helper did not exit in time"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        // Best effort, and never panics during unwinding.
        let _ = self.terminate_and_reap();
    }
}

fn spawn_helper(mode: &str, fixture: &Path, instance: &str) -> OwnedChild {
    spawn_helper_with(mode, fixture, instance, &[])
}

fn spawn_helper_with(
    mode: &str,
    fixture: &Path,
    instance: &str,
    extra: &[(&str, &str)],
) -> OwnedChild {
    let nonce = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    std::fs::write(fixture.join("nonce"), &nonce).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(HELPER)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("DCL_LC_PROC_MODE", mode)
        .env("DCL_LC_PROC_DIR", fixture)
        .env("DCL_LC_PROC_NONCE", &nonce)
        .env("DCL_LC_PROC_INSTANCE", instance)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for (key, value) in extra {
        command.env(key, value);
    }
    OwnedChild::new(command.spawn().unwrap())
}

fn wait_ready(child: &mut OwnedChild, fixture: &Path) {
    let deadline = Instant::now() + STARTUP;
    loop {
        if fixture.join("ready").exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("helper exited before publishing its lease: {status}");
        }
        if Instant::now() >= deadline {
            panic!("helper did not publish its lease in time");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_record(fixture: &Path) -> OwnerRecord {
    let bytes = std::fs::read(fixture.join("record.json")).expect("the helper recorded its owner");
    serde_json::from_slice(&bytes).expect("the helper record is valid")
}

fn read_pid(fixture: &Path, name: &str) -> u32 {
    std::fs::read_to_string(fixture.join(name))
        .expect("the helper published a PID")
        .trim()
        .parse()
        .expect("the published PID is numeric")
}

fn read_ticks(fixture: &Path, name: &str) -> u64 {
    std::fs::read_to_string(fixture.join(name))
        .expect("the helper published creation ticks")
        .trim()
        .parse()
        .expect("the published ticks are numeric")
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Create a coordination root and its private owners directory exactly as the
/// product's preparation does, for fixtures that publish leases by hand.
fn prepare_root(fixture: &Path) -> PathBuf {
    let root = fixture.join("coord");
    std::fs::create_dir_all(root.join("owners")).unwrap();
    set_mode(&root, 0o700);
    set_mode(&root.join("owners"), 0o700);
    root
}

fn write_lease(root: &Path, instance: &str, record: &OwnerRecord) {
    let lease = serde_json::json!({
        "version": 1,
        "instance_id": instance,
        "owner": serde_json::to_value(record).unwrap(),
    });
    let path = root.join("owners").join(format!("{instance}.lock"));
    std::fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
    set_mode(&path, 0o600);
}

/// Publish a lease for `record` and hold its exclusive lock in this process, as
/// a live owner would. The inode and 0600 mode are what a probe observes.
fn write_held_lease(root: &Path, instance: &str, record: &OwnerRecord) -> std::fs::File {
    write_lease(root, instance, record);
    let path = root.join("owners").join(format!("{instance}.lock"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    file
}

fn entries(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_lease_unchanged(
    lease: &Path,
    owners: &Path,
    meta: &std::fs::Metadata,
    bytes: &[u8],
    names: &[String],
) {
    use std::os::unix::fs::MetadataExt;
    let now = std::fs::metadata(lease).unwrap();
    assert_eq!(
        (now.dev(), now.ino()),
        (meta.dev(), meta.ino()),
        "the lease inode changed"
    );
    assert_eq!(now.mode() & 0o7777, meta.mode() & 0o7777, "the lease mode changed");
    assert_eq!(now.mode() & 0o7777, 0o600, "the lease is not private");
    assert_eq!(std::fs::read(lease).unwrap(), bytes, "the lease bytes changed");
    assert_eq!(entries(owners), names, "the owners entry set changed");
}

/// The parent PID (field 4) and creation ticks (field 22) of
/// `/proc/<pid>/stat`; `None` once the process is gone.
fn stat_tail(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ppid = fields.get(1)?.parse::<u32>().ok()?;
    let start = fields.get(19)?.parse::<u64>().ok()?;
    Some((ppid, start))
}

/// Field 22 of `/proc/<pid>/stat`; `None` once the process is gone.
fn process_start(pid: u32) -> Option<u64> {
    stat_tail(pid).map(|(_, start)| start)
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_owned()
}

/// Whether a freshly opened descriptor can take an exclusive flock.
fn lease_locked(root: &Path, instance: &str) -> bool {
    let path = root.join("owners").join(format!("{instance}.lock"));
    let file = std::fs::File::open(path).unwrap();
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        false
    } else {
        true
    }
}

/// Poll a pinned process descriptor. `Ok(true)` means the process exited,
/// `Ok(false)` that it is still running, and `Err` that the descriptor could
/// not be observed, which proves nothing.
fn poll_exit(fd: &OwnedFd, timeout: Duration) -> std::io::Result<bool> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let timeout = Timespec {
        tv_sec: timeout.as_secs() as i64,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    loop {
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                return Ok(fds[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP))
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + STARTUP;
    while !path.exists() {
        if Instant::now() >= deadline {
            panic!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A FIFO held `O_RDWR` by this test is the gate the forked child blocks on.
/// Closing it later removes the last writer, so the child's `read` returns EOF.
fn make_gate(fixture: &Path) -> std::fs::File {
    use std::os::unix::ffi::OsStrExt;
    let gate = fixture.join("gate");
    let name = std::ffi::CString::new(gate.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed");
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate)
        .unwrap()
}

/// The gate keeper plus the verified descriptor of the forked child.
///
/// The keeper is released first in cleanup, so the child's blocked `read` sees
/// EOF; the pinned descriptor then observes the exit. A forked child is never
/// signalled by numeric PID: only the descriptor obtained after its published
/// start ticks and the owning helper's PID both matched may receive a signal.
struct ForkGuard {
    keeper: Option<std::fs::File>,
    pinned: Option<OwnedFd>,
}

impl ForkGuard {
    fn new(keeper: std::fs::File) -> Self {
        Self {
            keeper: Some(keeper),
            pinned: None,
        }
    }

    /// Verify the published identity before and after opening the descriptor.
    fn pin(
        &mut self,
        helper_pid: u32,
        forked: u32,
        published_start: u64,
    ) -> std::io::Result<()> {
        verify_fork(forked, helper_pid, published_start)?;
        let pid = rustix::process::Pid::from_raw(forked as i32)
            .ok_or_else(|| std::io::Error::other("the forked PID is not usable"))?;
        let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .map_err(std::io::Error::from)?;
        verify_fork(forked, helper_pid, published_start)?;
        self.pinned = Some(fd);
        Ok(())
    }

    fn is_live(&self) -> bool {
        self.pinned
            .as_ref()
            .is_some_and(|fd| matches!(poll_exit(fd, Duration::ZERO), Ok(false)))
    }

    fn release_gate(&mut self) {
        self.keeper = None;
    }

    /// Wait for the pinned child to exit, with a descriptor-only kill as the
    /// bounded fallback. ESRCH means the pinned process is already gone.
    fn wait_exit(&self, deadline: Instant) -> bool {
        let Some(fd) = self.pinned.as_ref() else {
            return false;
        };
        loop {
            match poll_exit(fd, Duration::ZERO) {
                Ok(true) => return true,
                Err(_) => return false,
                Ok(false) => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        match rustix::process::pidfd_send_signal(fd, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(_) => return false,
        }
        let deadline = Instant::now() + CLEANUP;
        while Instant::now() < deadline {
            match poll_exit(fd, Duration::ZERO) {
                Ok(true) => return true,
                Ok(false) | Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn cleanup(&mut self) {
        self.release_gate();
        if self.pinned.is_some() {
            let _ = self.wait_exit(Instant::now() + CLEANUP);
        }
    }
}

impl Drop for ForkGuard {
    fn drop(&mut self) {
        if self.keeper.is_none() && self.pinned.is_none() {
            return;
        }
        self.cleanup();
    }
}

/// The published forked child must still be the same process, still owned by the
/// helper, when it is pinned.
fn verify_fork(forked: u32, helper_pid: u32, published_start: u64) -> std::io::Result<()> {
    let (ppid, start) = stat_tail(forked)
        .ok_or_else(|| std::io::Error::other("the forked child is not observable"))?;
    if start != published_start {
        return Err(std::io::Error::other(format!(
            "the forked child was replaced: start {start} != {published_start}"
        )));
    }
    if ppid != helper_pid {
        return Err(std::io::Error::other(format!(
            "the forked child is not owned by the helper: ppid {ppid} != {helper_pid}"
        )));
    }
    Ok(())
}

/// The owner releases its logical lease while a forked child still holds a
/// duplicate of the same open file description. The explicit unlock releases
/// the lock anyway, and a failed probe never releases it.
#[test]
fn local_coordination_duplicate_descriptor_release() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let instance = uuid::Uuid::new_v4().to_string();
    let keeper = make_gate(root);
    let mut owner = spawn_helper("hold-with-dup", root, &instance);
    wait_ready(&mut owner, root);
    let record = read_record(root);
    let forked = read_pid(root, "forked-pid");
    let forked_start = read_ticks(root, "forked-start");
    let helper_pid = owner.id().expect("the helper is still owned");
    let mut forks = ForkGuard::new(keeper);
    forks
        .pin(helper_pid, forked, forked_start)
        .expect("the forked child is the verified fixture child");
    let paths = paths_in(root);

    // A failed probe (mismatched generation) must not unlock the owner.
    let mut wrong = record.clone();
    wrong.generation = uuid::Uuid::new_v4().to_string();
    assert_eq!(probe_owner(&paths, &instance, &wrong), OwnerProof::Unknown);
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Alive);
    assert!(lease_locked(&paths.root, &instance), "the owner still holds its lease");
    assert!(forks.is_live(), "the gated forked child is still alive");

    std::fs::write(root.join("release"), b"").unwrap();
    wait_for(&root.join("released"));
    assert!(
        !lease_locked(&paths.root, &instance),
        "an explicit release must unlock despite the duplicate descriptor"
    );
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);
    assert!(
        owner.try_wait().unwrap().is_none(),
        "the helper keeps running after releasing its logical lease"
    );
    assert!(
        forks.is_live(),
        "the forked child still holds the descriptor while the owner is free"
    );

    // Releasing the gate ends the forked read; the helper reaps its own child.
    forks.release_gate();
    assert!(
        forks.wait_exit(Instant::now() + CLEANUP),
        "the forked child must exit after EOF"
    );
    std::fs::write(root.join("final"), b"").unwrap();
    owner.wait_success(Instant::now() + CLEANUP).unwrap();
    assert!(
        root.join("reaped").exists(),
        "the helper reaped its own fork child"
    );
}

/// The original owner is killed while a pre-exec child keeps an inherited
/// descriptor open behind the gate. The lock can therefore still be held, but
/// the recorded owner is proved Gone; a different live owner stays Alive.
#[test]
fn local_coordination_preexec_descriptor_owner_death() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let instance = uuid::Uuid::new_v4().to_string();
    let keeper = make_gate(root);
    let mut owner = spawn_helper("fork-retain", root, &instance);
    wait_ready(&mut owner, root);
    let record = read_record(root);
    let forked = read_pid(root, "forked-pid");
    let forked_start = read_ticks(root, "forked-start");
    let helper_pid = owner.id().expect("the helper is still owned");
    let mut forks = ForkGuard::new(keeper);
    forks
        .pin(helper_pid, forked, forked_start)
        .expect("the retained child is the verified fixture child");
    let paths = paths_in(root);

    let control_instance = uuid::Uuid::new_v4().to_string();
    let control = OwnerLease::acquire(&paths, &control_instance).unwrap();
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Alive);
    assert!(lease_locked(&paths.root, &instance));

    // Only the owned handle is used to end the original owner.
    owner.terminate_and_reap().unwrap();
    // The forked child never ran the owner's unlock destructor.
    assert!(
        lease_locked(&paths.root, &instance),
        "the inherited descriptor still holds the lock after the owner dies"
    );
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Gone);
    assert!(forks.is_live(), "the retained forked child is still alive");
    assert_eq!(
        probe_owner(&paths, &control_instance, control.record()),
        OwnerProof::Alive
    );

    forks.release_gate();
    assert!(
        forks.wait_exit(Instant::now() + CLEANUP),
        "the retained child must exit after EOF"
    );
    drop(control);
}

/// A held lease whose record is malformed is Unknown: validation runs before
/// the record can be interpreted, so a matching bad record cannot become Gone
/// (boot text mismatch) or Alive (matching ticks).
#[test]
fn local_coordination_malformed_matching_identity_is_unknown() {
    let fixture = tempfile::tempdir().unwrap();
    let root = prepare_root(fixture.path());
    let paths = paths_in(fixture.path());
    let pid = std::process::id();
    let ticks = process_start(pid).expect("the test process is observable");
    let live = OwnerRecord {
        platform: "linux".into(),
        pid,
        boot_id: boot_id(),
        creation_id: ticks.to_string(),
        generation: uuid::Uuid::new_v4().to_string(),
    };

    // Control: a valid record held by the live test process proves Alive, so a
    // later Unknown is not just a free lease.
    let control_instance = uuid::Uuid::new_v4().to_string();
    let held = write_held_lease(&root, &control_instance, &live);
    assert_eq!(
        probe_owner(&paths, &control_instance, &live),
        OwnerProof::Alive
    );

    let cases = [
        OwnerRecord {
            boot_id: "invalid-boot-id".into(),
            ..live.clone()
        },
        OwnerRecord {
            generation: "not-a-uuid".into(),
            ..live.clone()
        },
        OwnerRecord {
            creation_id: format!("0{ticks}"),
            ..live.clone()
        },
    ];
    for (index, record) in cases.iter().enumerate() {
        let instance = uuid::Uuid::new_v4().to_string();
        let _held = write_held_lease(&root, &instance, record);
        assert_eq!(
            probe_owner(&paths, &instance, record),
            OwnerProof::Unknown,
            "malformed case {index} must stay Unknown"
        );
    }

    // A different but valid boot UUID is a real mismatch, not malformed
    // evidence: the old incarnation is Gone while the live process continues.
    let mut other_boot = live.clone();
    other_boot.boot_id = uuid::Uuid::new_v4().to_string();
    let instance = uuid::Uuid::new_v4().to_string();
    let _held = write_held_lease(&root, &instance, &other_boot);
    assert_eq!(
        probe_owner(&paths, &instance, &other_boot),
        OwnerProof::Gone
    );
    assert!(process_start(pid).is_some(), "the live test process continues");
    drop(held);
}

/// Missing, malformed or unsafe evidence is Unknown, never permission to
/// reclaim a lease.
#[test]
fn local_coordination_owner_unknown_is_not_gone() {
    let fixture = tempfile::tempdir().unwrap();
    let root = prepare_root(fixture.path());
    let paths = paths_in(fixture.path());
    let instance = uuid::Uuid::new_v4().to_string();
    let pid = std::process::id();
    let record = OwnerRecord {
        platform: "linux".into(),
        pid,
        boot_id: boot_id(),
        creation_id: process_start(pid).unwrap().to_string(),
        generation: uuid::Uuid::new_v4().to_string(),
    };

    // Absence alone proves nothing.
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);

    let lease_path = root.join("owners").join(format!("{instance}.lock"));
    std::fs::write(&lease_path, b"{not json").unwrap();
    set_mode(&lease_path, 0o600);
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);

    // Valid lease, wrong generation.
    let mut other = record.clone();
    other.generation = uuid::Uuid::new_v4().to_string();
    write_lease(&root, &instance, &other);
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);

    // Unsafe evidence: a group-readable lease is not private evidence.
    write_lease(&root, &instance, &record);
    set_mode(&lease_path, 0o644);
    assert_eq!(probe_owner(&paths, &instance, &record), OwnerProof::Unknown);

    // Malformed identity: unparseable creation ticks never prove a live owner.
    let mut malformed = record.clone();
    malformed.creation_id = "not-ticks".into();
    write_lease(&root, &instance, &malformed);
    assert_eq!(
        probe_owner(&paths, &instance, &malformed),
        OwnerProof::Unknown
    );

    // A held private lease is Alive; the same held lease made group-readable is
    // Unknown even though its owner is alive, so the mode check really runs.
    let held_instance = uuid::Uuid::new_v4().to_string();
    let held = OwnerLease::acquire(&paths, &held_instance).unwrap();
    assert_eq!(
        probe_owner(&paths, &held_instance, held.record()),
        OwnerProof::Alive
    );
    let held_path = root.join("owners").join(format!("{held_instance}.lock"));
    set_mode(&held_path, 0o644);
    assert_eq!(
        probe_owner(&paths, &held_instance, held.record()),
        OwnerProof::Unknown
    );
}

/// The direct-child guard reaps the helper even when an assertion unwinds, and
/// the reaped PID is no longer waitable by this process.
#[test]
fn local_coordination_fixture_cleanup_reaps_child_on_unwind() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let instance = uuid::Uuid::new_v4().to_string();
    let mut owner = spawn_helper("owner-ready", root, &instance);
    wait_ready(&mut owner, root);
    let pid = owner.id().expect("the helper is owned");
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::PidfdFlags::empty(),
    )
    .unwrap();

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        // The guard owns the helper through the intentional unwind.
        let _owner = owner;
        panic!("intentional unwind while the guard owns the helper");
    }));
    assert!(outcome.is_err(), "the fixture must unwind");

    let deadline = Instant::now() + CLEANUP;
    let mut exited = false;
    while Instant::now() < deadline {
        if poll_exit(&pidfd, Duration::ZERO).unwrap_or(false) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(exited, "the guarded helper must be terminated and reaped");

    let mut status = 0;
    let rc = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    assert_eq!(rc, -1, "the guard must have already reaped the direct child");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

/// A reused UUID never replaces the published lease, even after its owner is
/// gone: the original inode, bytes, mode and entry set survive both attempts.
#[test]
fn local_coordination_reused_uuid_preserves_original_lease() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let paths = paths_in(root);
    let instance = uuid::Uuid::new_v4().to_string();
    let mut owner = spawn_helper("owner-ready", root, &instance);
    wait_ready(&mut owner, root);

    let owners = paths.root.join("owners");
    let lease_path = owners.join(format!("{instance}.lock"));
    let before_meta = std::fs::metadata(&lease_path).unwrap();
    let before_bytes = std::fs::read(&lease_path).unwrap();
    let before_entries = entries(&owners);

    let error = OwnerLease::acquire(&paths, &instance)
        .err()
        .expect("a held lease must refuse a second acquisition");
    assert!(
        matches!(error, CoordinationError::InvalidRecord(_)),
        "a held lease must be refused as an invalid record"
    );
    assert_lease_unchanged(
        &lease_path,
        &owners,
        &before_meta,
        &before_bytes,
        &before_entries,
    );

    owner.terminate_and_reap().unwrap();
    let error = OwnerLease::acquire(&paths, &instance)
        .err()
        .expect("a released lease record still must not be replaced");
    assert!(
        matches!(error, CoordinationError::InvalidRecord(_)),
        "a released lease record must still be refused"
    );
    assert_lease_unchanged(
        &lease_path,
        &owners,
        &before_meta,
        &before_bytes,
        &before_entries,
    );
}

/// Register through the public guard under a shared coordination root, hold the
/// incarnation until `release`, then close it and exit normally.
fn registry_helper(fixture: &Path, paths: &CoordinationPaths) {
    let data = PathBuf::from(std::env::var_os("DCL_LC_PROC_DATA").expect("helper data dir"));
    let session = std::env::var("DCL_LC_PROC_SESSION").expect("helper session");
    let role = std::env::var("DCL_LC_PROC_ROLE").expect("helper role");
    let worker = std::env::var("DCL_LC_PROC_WORKER").ok();
    let fixture = fixture.to_path_buf();
    let paths = paths.clone();
    let now = 1_000_i64;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let input = RegistrationInput {
            data_namespace_id: data_namespace(&data).expect("the fixture data dir is usable"),
            session_id: Some(session),
            kind: ParticipantKind::Cli,
            role: if role == "executor" {
                ParticipantRole::Executor
            } else {
                ParticipantRole::Frontend
            },
            parent_instance_id: None,
            worker_id: worker,
            job_id: None,
            workspace: observe_workspace(&fixture, now)
                .await
                .expect("the fixture workspace is observable"),
        };
        let mut guard =
            RegistryGuard::register(paths, input, now).expect("the fixture registers");
        std::fs::write(fixture.join("instance"), guard.instance_id()).unwrap();
        std::fs::write(fixture.join("ready"), b"").unwrap();
        while !fixture.join("release").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        guard.close().await.expect("the fixture closes");
        std::fs::write(fixture.join("closed"), b"").unwrap();
    });
}

/// Two real processes registering the same namespace and session form one
/// group with two distinct owner identities; the same session under another
/// namespace is a second group.
#[test]
fn local_coordination_grouped_frontend_worker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let coord = root.join("coord");
    let data1 = root.join("data1");
    let data2 = root.join("data2");
    let front_dir = root.join("front");
    let exec_dir = root.join("exec");
    let other_dir = root.join("other");
    for dir in [&data1, &data2, &front_dir, &exec_dir, &other_dir] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let session = uuid::Uuid::new_v4().to_string();
    let coord_value = coord.to_str().unwrap().to_string();
    let spawn = |fixture: &Path, data: &Path, role: &str, worker: Option<&str>| {
        let mut extra = vec![
            ("DCL_LC_PROC_COORD", coord_value.as_str()),
            ("DCL_LC_PROC_DATA", data.to_str().unwrap()),
            ("DCL_LC_PROC_SESSION", session.as_str()),
            ("DCL_LC_PROC_ROLE", role),
        ];
        if let Some(worker) = worker {
            extra.push(("DCL_LC_PROC_WORKER", worker));
        }
        spawn_helper_with(
            "registry-hold",
            fixture,
            &uuid::Uuid::new_v4().to_string(),
            &extra,
        )
    };

    // Start and confirm one helper at a time: the assertion is about
    // overlapping lifetimes, not about concurrent first-time bootstrapping.
    let mut front = spawn(&front_dir, &data1, "frontend", None);
    wait_ready(&mut front, &front_dir);
    let mut exec = spawn(&exec_dir, &data1, "executor", Some("worker-1"));
    wait_ready(&mut exec, &exec_dir);

    let paths = CoordinationPaths {
        root: coord.clone(),
    };
    let snapshot = registry_snapshot(&paths, 5_000).unwrap();
    assert_eq!(snapshot.groups.len(), 1, "one session is one group");
    let group = &snapshot.groups[0];
    assert_eq!(group.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(group.instances.len(), 2);
    assert_ne!(
        group.instances[0].registration.owner.pid,
        group.instances[1].registration.owner.pid,
        "each incarnation has its own owner"
    );
    let roles: Vec<ParticipantRole> = group
        .instances
        .iter()
        .map(|view| view.registration.role)
        .collect();
    assert!(roles.contains(&ParticipantRole::Frontend));
    assert!(roles.contains(&ParticipantRole::Executor));
    assert!(group
        .instances
        .iter()
        .any(|view| view.registration.worker_id.as_deref() == Some("worker-1")));
    assert!(group
        .instances
        .iter()
        .all(|view| view.owner_proof == OwnerProof::Alive));

    let mut other = spawn(&other_dir, &data2, "frontend", None);
    wait_ready(&mut other, &other_dir);
    let snapshot = registry_snapshot(&paths, 5_000).unwrap();
    assert_eq!(snapshot.groups.len(), 2, "another namespace is another group");
    let namespaces: Vec<&str> = snapshot
        .groups
        .iter()
        .map(|group| group.data_namespace_id.as_str())
        .collect();
    assert_ne!(namespaces[0], namespaces[1]);

    // Release and reap one helper at a time, keeping the others alive.
    for (child, dir) in [
        (&mut front, &front_dir),
        (&mut exec, &exec_dir),
        (&mut other, &other_dir),
    ] {
        std::fs::write(dir.join("release"), b"").unwrap();
        child.wait_success(Instant::now() + CLEANUP).unwrap();
        assert!(dir.join("closed").exists(), "the helper closed its row");
        assert_eq!(child.try_wait().unwrap(), None, "the helper is reaped");
    }

    // Every incarnation was closed explicitly, so nothing stays advertised.
    let snapshot = registry_snapshot(&paths, 5_000).unwrap();
    assert!(snapshot.groups.is_empty(), "closed rows are not advertised");
}

/// Fixture process started only by the tests above.
///
/// - `owner-ready`: publish a lease, record the identity, then wait to be killed.
/// - `fork-retain`: also fork a child that blocks on the gate while retaining
///   the inherited lease descriptor.
/// - `hold-with-dup`: fork such a child, wait for `release`, then drop the
///   logical lease explicitly (the explicit unlock must beat the duplicate) and
///   keep running.
/// - `registry-hold`: register through the public guard under a shared
///   coordination root, then close on `release`.
#[test]
#[ignore = "process helper: started only by the owner proof fixtures"]
fn lc_process_helper() {
    let mode = std::env::var("DCL_LC_PROC_MODE").expect("helper mode");
    let fixture = PathBuf::from(std::env::var_os("DCL_LC_PROC_DIR").expect("helper directory"));
    let nonce = std::env::var("DCL_LC_PROC_NONCE").expect("helper nonce");
    let recorded = std::fs::read_to_string(fixture.join("nonce")).expect("fixture nonce");
    assert_eq!(recorded, nonce, "unexpected fixture nonce");
    let paths = match std::env::var_os("DCL_LC_PROC_COORD") {
        Some(root) => CoordinationPaths {
            root: PathBuf::from(root),
        },
        None => paths_in(&fixture),
    };

    if mode == "registry-hold" {
        registry_helper(&fixture, &paths);
        return;
    }
    let instance = std::env::var("DCL_LC_PROC_INSTANCE").expect("helper instance");

    let lease = OwnerLease::acquire(&paths, &instance).expect("the fixture publishes its lease");
    let record = serde_json::to_vec(lease.record()).unwrap();
    std::fs::write(fixture.join("record.json"), record).unwrap();

    match mode.as_str() {
        "owner-ready" => {
            std::fs::write(fixture.join("ready"), b"").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "fork-retain" | "hold-with-dup" => {
            let gate = std::fs::File::open(fixture.join("gate")).expect("gate");
            let gate_fd = gate.as_raw_fd();
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                panic!("fork failed");
            }
            if pid == 0 {
                // The child never runs the owner's unlock destructor.
                let mut byte = [0u8; 1];
                unsafe {
                    libc::read(gate_fd, byte.as_mut_ptr().cast(), 1);
                    libc::_exit(0);
                }
            }
            drop(gate);
            let forked = pid as u32;
            let start = process_start(forked).expect("the forked child is observable");
            std::fs::write(fixture.join("forked-pid"), forked.to_string()).unwrap();
            std::fs::write(fixture.join("forked-start"), start.to_string()).unwrap();
            std::fs::write(fixture.join("ready"), b"").unwrap();
            if mode == "hold-with-dup" {
                wait_for(&fixture.join("release"));
                drop(lease);
                std::fs::write(fixture.join("released"), b"").unwrap();
                let mut status = 0;
                let reaped = loop {
                    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
                    if rc >= 0 {
                        break rc;
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    panic!("the helper cannot reap its own fork child: {error}");
                };
                assert_eq!(reaped, pid, "the helper must reap its own fork child");
                std::fs::write(fixture.join("reaped"), b"").unwrap();
                return;
            }
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        other => panic!("unknown helper mode {other}"),
    }
}




/// A killed owner is proved Gone from its process identity alone: no timeout,
/// heartbeat or lease-release heuristic is involved.
#[test]
fn local_coordination_owner_killed() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let instance = uuid::Uuid::new_v4().to_string();
    let mut child = spawn_helper("owner-ready", root, &instance);
    wait_ready(&mut child, root);
    let record = read_record(root);
    assert_eq!(
        probe_owner(&paths_in(root), &instance, &record),
        OwnerProof::Alive
    );
    child.terminate_and_reap().unwrap();
    assert_eq!(
        probe_owner(&paths_in(root), &instance, &record),
        OwnerProof::Gone
    );
}

/// A live PID with a different creation time is a reused PID, not the owner.
/// The probe reports Gone for the recorded identity and never signals the live
/// process that now holds that PID.
#[test]
fn local_coordination_pid_reuse() {
    let fixture = tempfile::tempdir().unwrap();
    let root = prepare_root(fixture.path());
    let mut sentinel = OwnedChild::new(
        Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = sentinel.id().expect("the sentinel is owned");
    let start = process_start(pid).expect("the sentinel is observable");
    let instance = uuid::Uuid::new_v4().to_string();
    let record = OwnerRecord {
        platform: "linux".into(),
        pid,
        boot_id: boot_id(),
        creation_id: (start + 1).to_string(),
        generation: uuid::Uuid::new_v4().to_string(),
    };
    write_lease(&root, &instance, &record);
    assert_eq!(
        probe_owner(&paths_in(fixture.path()), &instance, &record),
        OwnerProof::Gone
    );
    assert!(
        sentinel.try_wait().unwrap().is_none(),
        "the probe must not signal the live process that reused the PID"
    );
    sentinel.terminate_and_reap().unwrap();
}

/// A live locked owner stays Alive regardless of registry age: the proof reads
/// the immutable lease and process identity, not the heartbeat.
#[test]
fn local_coordination_live_stale_heartbeat_retained() {
    let fixture = tempfile::tempdir().unwrap();
    let paths = paths_in(fixture.path());
    let _store = deepseek_code_core::services::local_coordination::Store::open_rw(&paths).unwrap();
    let instance = uuid::Uuid::new_v4().to_string();
    let lease = OwnerLease::acquire(&paths, &instance).unwrap();
    assert_eq!(
        probe_owner(&paths, &instance, lease.record()),
        OwnerProof::Alive
    );
    std::thread::sleep(Duration::from_millis(1_100));
    assert_eq!(
        probe_owner(&paths, &instance, lease.record()),
        OwnerProof::Alive
    );
}

