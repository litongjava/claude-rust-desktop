//! Linux process identity shared by owner leases and managed shell tasks.
//!
//! A bare PID is not an identity: the PID space is reused, so "a process with
//! this PID exists" can become true again after the owner dies. The boot ID
//! isolates reboots, where `/proc` start times restart from a different origin,
//! and the start time distinguishes two processes that held the same PID in one
//! boot.

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};

/// The identity of the engine process that owns a lease or a managed shell.
///
/// This is written into the lease file itself before it is published, so a
/// reader never sees a held lease without an identity that belongs to it.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinuxIdentity {
    pub(crate) pid: u32,
    pub(crate) boot: String,
    pub(crate) start: u64,
}

#[cfg(target_os = "linux")]
impl LinuxIdentity {
    /// The identity of this process: the engine that is about to own the lease.
    pub(crate) fn current() -> Result<Self> {
        let pid = std::process::id();
        Ok(Self {
            pid,
            boot: read_boot_id().context("Reading the machine boot identity")?,
            start: process_start_time(pid)
                .with_context(|| format!("Reading the creation identity of PID {pid}"))?,
        })
    }

    pub(crate) fn encode(&self) -> String {
        format!("dcl-owner-v1 {} {} {}\n", self.pid, self.boot, self.start)
    }

    /// Parses a recorded identity. Anything that is not exactly one complete,
    /// well-formed record yields `None`: a partial or foreign line must never be
    /// read as "some process owns this".
    pub(crate) fn parse(text: &str) -> Option<Self> {
        // Exactly one record: trailing non-empty content means this file is not
        // the record this build wrote, so it must not be read as an identity.
        let mut lines = text.lines();
        let first = lines.next()?;
        if lines.any(|line| !line.trim().is_empty()) {
            return None;
        }
        let mut fields = first.split_whitespace();
        if fields.next()? != "dcl-owner-v1" {
            return None;
        }
        let pid = fields.next()?.parse::<u32>().ok()?;
        let boot = fields.next()?.to_owned();
        let start = fields.next()?.parse::<u64>().ok()?;
        if fields.next().is_some() || boot.is_empty() {
            return None;
        }
        // PID 0 is the kernel's idle task and can never own a lease; anything
        // above the kernel's `PID_MAX_LIMIT` is not a PID this system can issue.
        if !valid_pid(pid) {
            return None;
        }
        Some(Self { pid, boot, start })
    }

    /// Whether the process with this PID is, right now, the very process this
    /// identity was recorded for.
    ///
    /// `Ok(false)` is a definite mismatch (the process is gone, was reaped, or
    /// the PID now names a different process). `Err` means the identity could
    /// not be checked, which never authorizes a write.
    pub(crate) fn matches_current_process(&self) -> std::io::Result<bool> {
        if read_boot_id()? != self.boot {
            return Ok(false);
        }
        match process_start_time(self.pid) {
            Ok(start) => Ok(start == self.start),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// Linux `PID_MAX_LIMIT`: the largest PID the kernel can ever allocate.
#[cfg(target_os = "linux")]
pub(crate) const PID_MAX_LIMIT: u32 = 4 * 1024 * 1024;

/// A PID that can name a real process: never 0 (the idle task), never above the
/// kernel's allocatable range.
#[cfg(target_os = "linux")]
pub(crate) fn valid_pid(pid: u32) -> bool {
    pid > 0 && pid <= PID_MAX_LIMIT
}

#[cfg(target_os = "linux")]
pub(crate) fn read_boot_id() -> std::io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

/// Field 22 of `/proc/<pid>/stat`: the process creation time in clock ticks.
///
/// The command name is parenthesized and may itself contain spaces or
/// parentheses, so the fixed fields are taken after the last `)`.
#[cfg(target_os = "linux")]
pub(crate) fn process_start_time(pid: u32) -> std::io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the creation field of PID {pid} is unreadable"),
            )
        })
}

/// Whether a pinned process descriptor reports that its process has exited. A
/// zombie counts as exited: nothing can consume stdin any more.
#[cfg(target_os = "linux")]
pub(crate) fn pidfd_exited(pidfd: &rustix::fd::OwnedFd) -> std::io::Result<bool> {
    let mut fds = [rustix::event::PollFd::new(
        pidfd,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    loop {
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}
