//! Immutable owner incarceration leases and the Linux liveness proof.
//!
//! An owner lease is a private `owners/<instance>.lock` file published with a
//! hard link only after it is locked and after it records the owning process's
//! Linux identity and a fresh generation. The file is never removed: a reused
//! instance ID refuses to overwrite the record even after its owner dies. The
//! lock alone proves nothing, because `fork` before `exec` can leave a
//! descriptor holding the same open file description after the real owner is
//! gone; liveness is established from the recorded identity instead.

use crate::fs::OwnedFileLock;

use super::types::{CoordResult, OwnerRecord};
#[cfg(not(target_os = "linux"))]
use super::types::CoordinationError;
use super::CoordinationPaths;

/// What the recorded owner identity proves right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerProof {
    /// The recorded process is alive and its lease is still held.
    Alive,
    /// The recorded process is demonstrably gone. A forked descendant may still
    /// hold the file description, which is exactly why a held lease is not proof.
    Gone,
    /// The evidence is missing, malformed or unreadable; it never permits reclaim.
    Unknown,
}

/// An exclusive owner lease held by the acquiring process.
///
/// The lock is released explicitly by [`OwnedFileLock`] on drop, but only in the
/// process that took it: a fork shares the open file description, and the child
/// must never run the parent's unlock. `SIGKILL` skips `Drop`, so a held lease
/// is never interpreted as liveness on its own.
pub struct OwnerLease {
    _lock: OwnedFileLock,
    record: OwnerRecord,
}

impl OwnerLease {
    /// Publish an immutable lease for `instance_id` in the coordination root.
    ///
    /// The identity of this process and a fresh generation are fixed and written
    /// (with `fsync`) before the lease is locked and linked into place. The hard
    /// link cannot replace an existing lease, so a reused instance ID fails even
    /// when its previous owner is gone.
    pub fn acquire(paths: &CoordinationPaths, instance_id: &str) -> CoordResult<Self> {
        #[cfg(target_os = "linux")]
        {
            linux::acquire(paths, instance_id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (paths, instance_id);
            Err(CoordinationError::UnsupportedPlatform)
        }
    }

    /// The immutable owner record written into the lease.
    pub fn record(&self) -> &OwnerRecord {
        &self.record
    }
}

/// Decide whether the recorded owner of `instance_id` is alive, gone or unknown.
///
/// The lease is read through an existing-only, bounded, no-follow private open.
/// Its instance ID, generation and owner content must match `expected` exactly;
/// anything missing or malformed, and any error while observing the process, is
/// [`OwnerProof::Unknown`]. The probe never signals a process and never unlocks
/// a lease it did not take.
pub fn probe_owner(
    paths: &CoordinationPaths,
    instance_id: &str,
    expected: &OwnerRecord,
) -> OwnerProof {
    #[cfg(target_os = "linux")]
    {
        linux::probe(paths, instance_id, expected)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (paths, instance_id, expected);
        OwnerProof::Unknown
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::Path;

    use serde::{Deserialize, Serialize};

    use crate::fs::OwnedFileLock;

    use super::super::paths;
    use super::super::process_identity::{
        pidfd_exited, process_start_time, read_boot_id, valid_pid, LinuxIdentity,
    };
    use super::super::types::{
        is_uuid, CoordResult, CoordinationError, OwnerRecord,
    };
    use super::super::CoordinationPaths;
    use super::OwnerProof;

    const OWNERS_DIR: &str = "owners";
    const LEASE_VERSION: u32 = 1;
    const MAX_LEASE_BYTES: u64 = 4096;

    #[derive(Serialize, Deserialize)]
    struct LeaseFile {
        version: u32,
        instance_id: String,
        owner: OwnerRecord,
    }

    fn coord_io(error: anyhow::Error) -> CoordinationError {
        CoordinationError::Io(std::io::Error::other(error.to_string()))
    }

    fn canonical_instance(instance_id: &str) -> CoordResult<String> {
        let parsed = uuid::Uuid::parse_str(instance_id).map_err(|_| {
            CoordinationError::InvalidRecord("owner lease instance is not a UUID".into())
        })?;
        let canonical = parsed.to_string();
        if canonical != instance_id {
            return Err(CoordinationError::InvalidRecord(
                "owner lease instance is not a canonical UUID".into(),
            ));
        }
        Ok(canonical)
    }

    pub(super) fn acquire(
        paths: &CoordinationPaths,
        instance_id: &str,
    ) -> CoordResult<super::OwnerLease> {
        let instance = canonical_instance(instance_id)?;
        let identity = LinuxIdentity::current().map_err(coord_io)?;
        let record = OwnerRecord {
            platform: "linux".into(),
            pid: identity.pid,
            boot_id: identity.boot,
            creation_id: identity.start.to_string(),
            generation: uuid::Uuid::new_v4().to_string(),
        };
        // The record this process is about to publish must satisfy the same
        // rules a probe applies, before any staging file exists.
        record.validate()?;
        if identity_from_record(&record).is_none() {
            return Err(CoordinationError::InvalidRecord(
                "the current owner identity is not a usable record".into(),
            ));
        }
        let owners_root = paths.root.join(OWNERS_DIR);
        CoordinationPaths {
            root: owners_root.clone(),
        }
        .prepare_private()
        .map_err(coord_io)?;
        let directory = paths::open_private_dir(&owners_root).map_err(coord_io)?;
        let staging = format!(".{instance}.lock.new-{}", uuid::Uuid::new_v4());
        let file = paths::create_private_at(&directory, OsStr::new(&staging), true)
            .map_err(coord_io)?;
        let publication = publish(&directory, &file, &staging, &instance, &record);
        let _ = rustix::fs::unlinkat(
            &directory,
            Path::new(&staging),
            rustix::fs::AtFlags::empty(),
        );
        Ok(super::OwnerLease {
            _lock: publication?,
            record,
        })
    }

    fn publish(
        directory: &File,
        mut file: &File,
        staging: &str,
        instance: &str,
        record: &OwnerRecord,
    ) -> CoordResult<OwnedFileLock> {
        let lease = LeaseFile {
            version: LEASE_VERSION,
            instance_id: instance.to_owned(),
            owner: record.clone(),
        };
        let bytes = serde_json::to_vec(&lease).map_err(|_| {
            CoordinationError::InvalidRecord("owner lease cannot be encoded".into())
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(CoordinationError::Io)?;
        let lock = OwnedFileLock::try_acquire(file.try_clone().map_err(CoordinationError::Io)?)
            .map_err(CoordinationError::Io)?
            .ok_or(CoordinationError::Busy)?;
        let final_name = format!("{instance}.lock");
        match rustix::fs::linkat(
            directory,
            Path::new(staging),
            directory,
            Path::new(&final_name),
            rustix::fs::AtFlags::empty(),
        ) {
            Ok(()) => rustix::fs::fsync(directory).map_err(|error| {
                CoordinationError::Io(std::io::Error::from(error))
            })?,
            Err(rustix::io::Errno::EXIST) => {
                return Err(CoordinationError::InvalidRecord(
                    "an owner lease already exists for this instance".into(),
                ))
            }
            Err(error) => return Err(CoordinationError::Io(error.into())),
        }
        Ok(lock)
    }

    pub(super) fn probe(
        paths: &CoordinationPaths,
        instance_id: &str,
        expected: &OwnerRecord,
    ) -> OwnerProof {
        let Ok(instance) = canonical_instance(instance_id) else {
            return OwnerProof::Unknown;
        };
        let owners_root = paths.root.join(OWNERS_DIR);
        let Ok(directory) = paths::open_private_dir(&owners_root) else {
            return OwnerProof::Unknown;
        };
        let name = format!("{instance}.lock");
        let Ok(file) = paths::open_existing_private_at(&directory, OsStr::new(&name), false)
        else {
            return OwnerProof::Unknown;
        };
        let Ok(bytes) = read_bounded(&file) else {
            return OwnerProof::Unknown;
        };
        let Ok(lease) = serde_json::from_slice::<LeaseFile>(&bytes) else {
            return OwnerProof::Unknown;
        };
        if lease.version != LEASE_VERSION || lease.instance_id != instance {
            return OwnerProof::Unknown;
        }
        // Identity is interpreted only after it validates: a matching but
        // malformed record must never turn into Gone or Alive.
        let Some(identity) = identity_from_record(&lease.owner) else {
            return OwnerProof::Unknown;
        };
        if lease.owner != *expected {
            return OwnerProof::Unknown;
        }
        prove(&identity, &file)
    }

    /// The recorded process is the decisive evidence; the lease byte only says
    /// whether it still holds the channel.
    ///
    /// A *valid observed* boot or creation mismatch proves the recorded
    /// incarnation is Gone. A missing or unreadable observation proves nothing:
    /// procfs policy can hide a live process, so those paths require independent
    /// death evidence (ESRCH, or a pinned descriptor reporting exit).
    fn prove(identity: &LinuxIdentity, lease: &File) -> OwnerProof {
        let boot = match read_boot_id() {
            Ok(boot) => boot,
            Err(_) => return OwnerProof::Unknown,
        };
        if !is_uuid(&boot) {
            return OwnerProof::Unknown;
        }
        if boot != identity.boot {
            return OwnerProof::Gone;
        }
        prove_same_boot(
            identity,
            process_start_time,
            |pid| rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()),
            pidfd_exited,
            || lease_held(lease),
        )
    }

    /// The single-boot owner decision, with the observations injected so tests
    /// can exercise unreadable and erroring paths without host procfs changes.
    ///
    /// Returns `Gone` only for a valid mismatch or confirmed disappearance.
    /// Anything that could not be observed stays `Unknown`.
    fn prove_same_boot<H>(
        identity: &LinuxIdentity,
        mut read_start: impl FnMut(u32) -> std::io::Result<u64>,
        pin: impl FnOnce(rustix::process::Pid) -> rustix::io::Result<H>,
        exited: impl FnOnce(&H) -> std::io::Result<bool>,
        held: impl FnOnce() -> std::io::Result<bool>,
    ) -> OwnerProof {
        let before_matches = match read_start(identity.pid) {
            Ok(ticks) if ticks == identity.start => true,
            Ok(_) => return OwnerProof::Gone,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return OwnerProof::Unknown,
        };
        let Some(pid) = rustix::process::Pid::from_raw(identity.pid as i32) else {
            return OwnerProof::Unknown;
        };
        let handle = match pin(pid) {
            Ok(handle) => handle,
            Err(rustix::io::Errno::SRCH) => return OwnerProof::Gone,
            Err(_) => return OwnerProof::Unknown,
        };
        // A live descriptor without a readable creation identity cannot be
        // confirmed as the recorded incarnation, even though it names the PID.
        let verified = if before_matches {
            match read_start(identity.pid) {
                Ok(ticks) if ticks == identity.start => true,
                Ok(_) => return OwnerProof::Gone,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => return OwnerProof::Unknown,
            }
        } else {
            false
        };
        match exited(&handle) {
            Ok(true) => return OwnerProof::Gone,
            Ok(false) => {}
            Err(_) => return OwnerProof::Unknown,
        }
        if !verified {
            return OwnerProof::Unknown;
        }
        match held() {
            Ok(true) => OwnerProof::Alive,
            // A free lease with a live recorded process is the normal closed
            // state, so it proves neither life nor death of the channel.
            Ok(false) | Err(_) => OwnerProof::Unknown,
        }
    }

    fn identity_from_record(record: &OwnerRecord) -> Option<LinuxIdentity> {
        record.validate().ok()?;
        if !valid_pid(record.pid) {
            return None;
        }
        let start = record.creation_id.parse::<u64>().ok()?;
        // Only the writer's decimal form is accepted.
        if start.to_string() != record.creation_id {
            return None;
        }
        Some(LinuxIdentity {
            pid: record.pid,
            boot: record.boot_id.clone(),
            start,
        })
    }

    fn read_bounded(file: &File) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut limited = file.take(MAX_LEASE_BYTES + 1);
        limited.read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_LEASE_BYTES {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        Ok(bytes)
    }

    /// Whether some descriptor holds an exclusive lease. A shared probe lock is
    /// released at once and never keeps another process's lease unlocked.
    fn lease_held(file: &File) -> std::io::Result<bool> {
        use rustix::fs::{flock, FlockOperation};
        match flock(file, FlockOperation::NonBlockingLockShared) {
            Ok(()) => {
                let _ = flock(file, FlockOperation::Unlock);
                Ok(false)
            }
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    /// Injected-observation tests for the single-boot decision. They call the
    /// same body as production with closures standing in for `/proc` and pidfd,
    /// so unreadable and erroring evidence is reproducible without touching the
    /// host's procfs policy.
    #[cfg(test)]
    mod tests {
        use super::*;

        const TICKS: u64 = 7_000;

        fn identity() -> LinuxIdentity {
            LinuxIdentity {
                pid: 4_242,
                boot: "0f0f".into(),
                start: TICKS,
            }
        }

        fn err(kind: i32) -> std::io::Result<u64> {
            Err(std::io::Error::from_raw_os_error(kind))
        }

        /// One result per expected read; an extra read panics, which also pins
        /// the number of stat lookups each path performs.
        fn reads(
            results: Vec<std::io::Result<u64>>,
        ) -> impl FnMut(u32) -> std::io::Result<u64> {
            let mut results = results.into_iter();
            move |_| results.next().expect("unexpected extra start-tick read")
        }

        #[test]
        fn local_coordination_unreadable_owner_stat_is_unknown() {
            // Missing before the pin: a live descriptor names the PID but proves
            // nothing about the recorded incarnation, so the lease is not read.
            let pinned = std::cell::Cell::new(false);
            let proof = prove_same_boot(
                &identity(),
                reads(vec![err(libc::ENOENT)]),
                |_| {
                    pinned.set(true);
                    Ok(())
                },
                |_| Ok(false),
                || panic!("the lease must not be consulted without a verified identity"),
            );
            assert_eq!(proof, OwnerProof::Unknown);
            assert!(pinned.get(), "the missing stat must still be pinned");

            // Missing only after matching ticks: a live descriptor still cannot
            // prove its creation identity once the stat disappears.
            let proof = prove_same_boot(
                &identity(),
                reads(vec![Ok(TICKS), err(libc::ENOENT)]),
                |_| Ok(()),
                |_| Ok(false),
                || panic!("the lease must not be consulted without a verified identity"),
            );
            assert_eq!(proof, OwnerProof::Unknown);

            // Permission-denied evidence is unreadable in either position.
            for results in [
                vec![err(libc::EACCES)],
                vec![Ok(TICKS), err(libc::EACCES)],
            ] {
                let proof = prove_same_boot(
                    &identity(),
                    reads(results),
                    |_| Ok(()),
                    |_| Ok(false),
                    || panic!("the lease must not be consulted after an unreadable stat"),
                );
                assert_eq!(proof, OwnerProof::Unknown);
            }
        }

        #[test]
        fn local_coordination_missing_owner_stat_requires_pidfd_death() {
            // Missing stat plus ESRCH: the PID namespace independently confirms
            // that the recorded process no longer exists.
            let proof = prove_same_boot(
                &identity(),
                reads(vec![err(libc::ENOENT)]),
                |_| Err(rustix::io::Errno::SRCH),
                |_: &()| panic!("no descriptor exists to poll"),
                || panic!("the lease must not be consulted"),
            );
            assert_eq!(proof, OwnerProof::Gone);

            // Missing first read with a successful pin: only the descriptor's
            // own exit can establish death.
            let exited = std::cell::Cell::new(false);
            let proof = prove_same_boot(
                &identity(),
                reads(vec![err(libc::ENOENT)]),
                |_| Ok(()),
                |_| {
                    exited.set(true);
                    Ok(true)
                },
                || panic!("the lease must not be consulted"),
            );
            assert_eq!(proof, OwnerProof::Gone);
            assert!(exited.get(), "a successful pin must be polled");

            // Missing second read behaves the same way.
            let exited = std::cell::Cell::new(false);
            let proof = prove_same_boot(
                &identity(),
                reads(vec![Ok(TICKS), err(libc::ENOENT)]),
                |_| Ok(()),
                |_| {
                    exited.set(true);
                    Ok(true)
                },
                || panic!("the lease must not be consulted"),
            );
            assert_eq!(proof, OwnerProof::Gone);
            assert!(exited.get(), "a successful pin must be polled");
        }

        #[test]
        fn local_coordination_pidfd_errors_do_not_prove_owner_death() {
            // A pin error other than ESRCH is unreadable evidence, not a death.
            for first in [true, false] {
                for pin_error in [rustix::io::Errno::PERM, rustix::io::Errno::NOSYS] {
                    let results = if first {
                        vec![Ok(TICKS)]
                    } else {
                        vec![err(libc::ENOENT)]
                    };
                    let proof = prove_same_boot(
                        &identity(),
                        reads(results),
                        |_| Err(pin_error),
                        |_: &()| panic!("no descriptor exists to poll"),
                        || panic!("the lease must not be consulted"),
                    );
                    assert_eq!(proof, OwnerProof::Unknown);
                }
            }

            // A poll error after a successful pin is unreadable evidence too.
            for first in [true, false] {
                let results = if first {
                    vec![Ok(TICKS), Ok(TICKS)]
                } else {
                    vec![err(libc::ENOENT)]
                };
                let proof = prove_same_boot(
                    &identity(),
                    reads(results),
                    |_| Ok(()),
                    |_| Err(std::io::Error::from_raw_os_error(libc::EIO)),
                    || panic!("the lease must not be consulted"),
                );
                assert_eq!(proof, OwnerProof::Unknown);
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::services::local_coordination::{
        Activity, Lifecycle, NativePath, ParticipantKind, ParticipantRole, Registration, Store,
    };
    use std::path::Path;

    /// A live locked owner stays Alive no matter how old its registry heartbeat
    /// is: owner proof reads the immutable lease and the process identity, never
    /// the row's age. The stale row is written here because `Store::insert` is
    /// crate-private and fixtures must not widen it.
    #[test]
    fn local_coordination_live_stale_heartbeat_retained() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let paths = CoordinationPaths {
            root: temp.path().join("coord"),
        };
        let mut store = Store::open_rw(&paths)?;
        let instance = uuid::Uuid::new_v4().to_string();
        let lease = OwnerLease::acquire(&paths, &instance)?;
        store.insert(&Registration {
            instance_id: instance.clone(),
            data_namespace_id: "b".repeat(64),
            session_id: None,
            kind: ParticipantKind::Cli,
            role: ParticipantRole::Frontend,
            parent_instance_id: None,
            worker_id: None,
            job_id: None,
            owner: lease.record().clone(),
            workspace: NativePath::from_path(Path::new("/workspace/example")),
            repo_id: None,
            worktree_id: None,
            branch_ref: None,
            activity: Activity::Idle,
            activity_since_ms: 1,
            started_ms: 1,
            heartbeat_ms: 1,
            last_git_observed_ms: None,
            lifecycle: Lifecycle::Open,
        })?;
        let row = store.rows(10)?.remove(0);
        assert_eq!(row.heartbeat_ms, 1);
        assert_eq!(
            probe_owner(&paths, &instance, lease.record()),
            OwnerProof::Alive
        );
        Ok(())
    }
}