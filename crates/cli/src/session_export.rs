//! Publish a complete, interruptible backup without replacing an existing file.
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{db::session_store::Stores, services::session_worker::process_exited};
use std::{ffi::OsString, fs, io, path::Path};

pub(crate) fn run(root: &Path, id: &str, output: Option<&Path>) -> Result<()> {
    let Some(target) = output else {
        return Stores::export_session(root, id, io::BufWriter::new(io::stdout().lock()));
    };
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut prefix = OsString::from(".");
    prefix.push(target.file_name().context("Expected an export filename")?);
    prefix.push(".export-");
    let signal = ExportSignal::install()?;
    let check = || {
        ensure!(!signal.interrupted(), "Export interrupted");
        Ok(())
    };
    for entry in fs::read_dir(parent)? {
        check()?;
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name
            .as_encoded_bytes()
            .strip_prefix(prefix.as_encoded_bytes())
            .and_then(|s| s.strip_suffix(b".tmp"))
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.parse::<u32>().ok().filter(|pid| pid.to_string() == s))
        else {
            continue;
        };
        if process_exited(pid) && entry.file_type()?.is_file() {
            match fs::remove_file(entry.path()) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                result => result?,
            }
        }
    }
    let suffix = format!("{}.tmp", std::process::id());
    let mut file = tempfile::Builder::new()
        .prefix(&prefix)
        .rand_bytes(0)
        .suffix(&suffix)
        .tempfile_in(parent)?;
    Stores::export_session_interruptible(root, id, io::BufWriter::new(file.as_file_mut()), &check)?;
    check()?;
    file.as_file().sync_all()?;
    check()?;
    file.persist_noclobber(target)
        .with_context(|| format!("Cannot publish {}", target.display()))?;
    Ok(())
}

#[derive(Default)]
struct ExportSignal {
    #[cfg(unix)]
    received: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl ExportSignal {
    fn install() -> io::Result<Self> {
        #[allow(unused_mut)]
        let mut signal = Self::default();
        // Observe delivery synchronously even when the runtime has not yet
        // polled its process-wide signal stream before the final publish check.
        #[cfg(unix)]
        for sig in [libc::SIGINT, libc::SIGTERM] {
            signal
                .registrations
                .push(signal_hook::flag::register(sig, signal.received.clone())?);
        }
        Ok(signal)
    }

    fn interrupted(&self) -> bool {
        #[cfg(unix)]
        if self.received.load(std::sync::atomic::Ordering::SeqCst) {
            return true;
        }
        crate::signals::received_signal()
    }
}

#[cfg(unix)]
impl Drop for ExportSignal {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
