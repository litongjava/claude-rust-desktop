//! Bounded shell output and descendant lifetime management.
use crate::performance::command_lines::{LineCapture, Lines};
use crate::performance::command_output::{qualifies, shown_extent, ByteLines, ReplacementWidths};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
const CAPTURE_MARKER: &[u8] = b"\n... (capture truncated; middle omitted) ...\n";
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// bash reports a stage killed by SIGPIPE as 128 + 13. With `-o pipefail` that
/// only reaches us when a reader such as `head` closed the pipe early, and it is
/// the same number for Git Bash on Windows; other shells do not use it.
fn cut_short_by_reader(shell: &str, code: Option<i32>) -> bool {
    code == Some(128 + 13) && matches!(shell, "bash" | "zsh" | "sh" | "wsl")
}

struct Captured {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    extent: ByteLines,
    excerpts: Option<LineCapture>,
    /// Bytes kept before the middle is dropped. A caller that must parse a
    /// complete document can raise this; Bash keeps the historical value.
    limit: usize,
}
impl Default for Captured {
    fn default() -> Self {
        Self {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            extent: ByteLines::default(),
            excerpts: Some(LineCapture::default()),
            limit: OUTPUT_LIMIT,
        }
    }
}
impl Captured {
    fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.extent.append(bytes);
        if let Some(excerpts) = &mut self.excerpts {
            excerpts.push(bytes);
        }
        let prefix = bytes.len().min(self.limit.saturating_sub(self.head.len()));
        let head = bytes
            .len()
            .min((self.limit / 2).saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..prefix]);
        let tail_cap = self.limit - self.limit / 2;
        let rest = &bytes[head.max(bytes.len().saturating_sub(tail_cap))..];
        let remove = (self.tail.len() + rest.len()).saturating_sub(tail_cap);
        self.tail.drain(..remove.min(self.tail.len()));
        self.tail.extend(rest.iter().copied());
    }

    /// Whether more bytes arrived than this capture keeps.
    fn overflowed(&self) -> bool {
        self.extent.bytes > self.limit
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes_and_omission().0).into_owned()
    }

    fn bytes_and_omission(&self) -> (Vec<u8>, usize, Option<std::ops::Range<usize>>) {
        let mut bytes = self.head[..self.head.len().min(self.limit / 2)].to_vec();
        let tail: Vec<_> = self.tail.iter().copied().collect();
        let mut tail_start = 0;
        let mut retained = self.extent.bytes;
        let mut marker = None;
        if self.overflowed() {
            if let Err(error) = std::str::from_utf8(&bytes) {
                if error.error_len().is_none() {
                    bytes.truncate(error.valid_up_to());
                }
            }
            while tail_start < tail.len() && tail[tail_start] & 0xc0 == 0x80 {
                tail_start += 1;
            }
            retained = bytes.len() + tail.len() - tail_start;
            let start = String::from_utf8_lossy(&bytes).len();
            marker = Some(start..start + CAPTURE_MARKER.len());
            bytes.extend_from_slice(CAPTURE_MARKER);
        }
        bytes.extend_from_slice(&tail[tail_start..]);
        (bytes, self.extent.bytes - retained, marker)
    }

    fn present(&self) -> (String, bool, StreamCapture) {
        self.present_with_limit(super::max_output_bytes())
    }

    fn present_with_limit(&self, limit: usize) -> (String, bool, StreamCapture) {
        let (bytes, capture_elided_bytes, marker) = self.bytes_and_omission();
        self.present_bytes(limit, &bytes, capture_elided_bytes, marker, false)
    }

    fn present_bytes(
        &self,
        limit: usize,
        bytes: &[u8],
        capture_elided_bytes: usize,
        marker: Option<std::ops::Range<usize>>,
        capture_prefix: bool,
    ) -> (String, bool, StreamCapture) {
        let text = String::from_utf8_lossy(bytes);
        let sources = match marker {
            Some(marker) => {
                let tail = ByteLines::measure(&text.as_bytes()[marker.end..]);
                vec![
                    (0..marker.start, 0),
                    (marker.end..text.len(), self.extent.lines() - tail.lines()),
                ]
            }
            None => vec![(0..text.len(), 0)],
        };
        let (preview, kept) = crate::performance::preview_with_ranges(&text, limit);
        let mut visible = Vec::new();
        let mut ranges = Vec::new();
        let mut first_lines = Vec::new();
        for (part, offset) in kept.parts() {
            for (source, origin) in &sources {
                let start = part.start.max(source.start);
                let end = part.end.min(source.end);
                if start < end {
                    visible.push(start..end);
                    ranges.push(offset + (start - part.start)..offset + (end - part.start));
                    first_lines.push(
                        origin + ByteLines::measure(&text.as_bytes()[source.start..start]).newlines,
                    );
                }
            }
        }
        let truncated = self.overflowed() || text.len() > kept.kept();
        let (source_ranges, origins): (Vec<_>, Vec<_>) = sources.into_iter().unzip();
        let shown = shown_extent(
            &text,
            &source_ranges,
            &origins,
            &visible,
            &ReplacementWidths::of(&bytes),
        );
        let stats = StreamCapture {
            bytes: self.extent.bytes,
            lines: self.extent.lines(),
            excerpts: self.excerpts.as_ref().map(LineCapture::finish),
            first_lines,
            widths: shown.widths,
            capture_prefix,
            source: None,
            preview_digest: None,
            capture_elided_bytes,
            tool_elided_text_bytes: source_ranges.iter().map(|r| r.len()).sum::<usize>()
                - shown.text_bytes,
            ranges,
        };
        (preview, truncated, stats)
    }
}

/// If a tool-level cut would lose source, classify the original streams first.
fn present_streams(streams: [&Captured; 2], limit: usize) -> [(String, bool, StreamCapture); 2] {
    let needs_cut = streams
        .iter()
        .any(|s| s.overflowed() || s.head.len() > limit || std::str::from_utf8(&s.head).is_err());
    let preserve = needs_cut
        && streams.iter().any(|s| {
            !super::base64_collapse::captured_runs(
                &String::from_utf8_lossy(&s.head),
                s.overflowed(),
            )
            .is_empty()
        });
    streams.map(|s| {
        let (preview, truncated, mut stats) = s.present_with_limit(limit);
        if preserve {
            // Keep the historical tool/UI result. Only the model uses the source
            // metrics and bytes captured before that result was clipped.
            stats = s
                .present_bytes(
                    usize::MAX,
                    &s.head,
                    s.extent.bytes - s.head.len(),
                    None,
                    s.overflowed(),
                )
                .2;
            stats.source = Some(s.head.clone());
            stats.preview_digest = Some(StreamCapture::digest(&preview));
        }
        (preview, truncated, stats)
    })
}

async fn capture(
    mut pipe: impl AsyncRead + Unpin,
    output: &mut Captured,
    log: Option<std::path::PathBuf>,
) -> std::io::Result<()> {
    let mut log = log.map(std::fs::File::create).transpose()?;
    let mut buffer = [0u8; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.extent.bytes));
        output.push(&buffer[..count]);
        if let Some(log) = log.as_mut() {
            std::io::Write::write_all(log, &buffer[..keep])?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn capture_counts_original_bytes_and_lines_through_lossy_truncation() {
    let mut capture = Captured::with_limit(8);
    capture.push(b"a\n\xffb\n");
    capture.push(b"c\nd\n");
    capture.push(b"e\xff");
    let (shown, truncated, stats) = capture.present();
    assert!(truncated);
    assert_eq!(stats.bytes, 11);
    assert_eq!(stats.lines, 5);
    assert_eq!(stats.capture_elided_bytes, 3);
    assert_eq!(stats.tool_elided_text_bytes, 0);
    assert!(shown.contains('�'));
}

#[cfg(test)]
pub(crate) fn review_capture(large: bool) -> (String, ShellCapture) {
    let mut captured = Captured::default();
    let limit = if large {
        // Same bytes and chunk boundaries as review-01's Python reproduction.
        let mut row = vec![b'z'; 63];
        row.push(b'\n');
        let block = row.repeat(1024);
        captured.push(b"BEGIN\n");
        for _ in 0..400 {
            captured.push(&block);
        }
        captured.push(b"error: midpoint sentinel\n");
        for _ in 0..400 {
            captured.push(&block);
        }
        captured.push(b"\x1b[31mwarning: final fixture\x1b[0m\n");
        captured.push(b"invalid UTF-8: \xff\x00 suffix\n");
        102_400
    } else {
        captured.push(b"small payload that the configured limit truncates\n");
        24
    };
    let (text, truncated, stdout) = captured.present_with_limit(limit);
    assert!(truncated);
    (
        text,
        ShellCapture {
            source_file: None,
            duration_ms: 50,
            stdout,
            stderr: Captured::default().present_with_limit(limit).2,
        },
    )
}

#[cfg(test)]
pub(crate) fn fixture_capture(
    streams: [&[u8]; 2],
    capture_limit: usize,
    tool_limit: usize,
    exit: i64,
) -> (String, Option<CommandMetadata>) {
    let streams = streams.map(|bytes| {
        let mut captured = Captured::with_limit(capture_limit);
        for chunk in bytes.chunks(317) {
            captured.push(chunk);
        }
        captured
    });
    let [(out, out_cut, stdout), (err, err_cut, stderr)] =
        present_streams(streams.each_ref(), tool_limit);
    let capture = shell_metadata(37, stdout, stderr, [out.len(), err.len()]);
    let result = json!({"stdout":out, "stderr":err, "exit_code":exit, "stdout_truncated":out_cut,"stderr_truncated":err_cut});
    (result.to_string(), capture.map(CommandMetadata::Shell))
}

pub(super) async fn run(command: &mut Command, timeout: Duration, shell: &str) -> Result<Value> {
    let (value, capture) =
        run_logged_with_capture(command, Some(timeout), shell, None, None).await?;
    publish_capture(capture);
    Ok(value)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct StreamCapture {
    pub(crate) bytes: usize,
    pub(crate) lines: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) capture_prefix: bool,
    #[serde(skip)]
    pub(crate) source: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preview_digest: Option<[u8; 32]>,
    pub(crate) capture_elided_bytes: usize,
    pub(crate) tool_elided_text_bytes: usize,
    pub(crate) ranges: Vec<std::ops::Range<usize>>,
    #[serde(default)]
    pub(crate) first_lines: Vec<usize>,
    #[serde(default)]
    pub(crate) excerpts: Option<Lines>,
    #[serde(default)]
    pub(crate) widths: ReplacementWidths,
}

impl StreamCapture {
    pub(crate) fn digest(text: &str) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(text.as_bytes()).into()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellCapture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_file: Option<std::path::PathBuf>,
    pub(crate) duration_ms: u64,
    pub(crate) stdout: StreamCapture,
    pub(crate) stderr: StreamCapture,
}

impl ShellCapture {
    /// A single file contains stdout and stderr in order. The separator is
    /// outside both stream ranges. A stdout-only capture stays byte-identical.
    pub(crate) fn recovery(
        &self,
        root: &Path,
    ) -> std::io::Result<Option<(std::path::PathBuf, Vec<u8>)>> {
        if let Some(path) = &self.source_file {
            return Ok(Some((path.clone(), std::fs::read(path)?)));
        }
        let Some(mut bytes) = self.stdout.source.clone() else {
            return Ok(None);
        };
        let stderr = self.stderr.source.as_deref().unwrap_or_default();
        if !bytes.is_empty() && !stderr.is_empty() {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(stderr);
        let path = crate::performance::save_tool_output_bytes(root, &bytes)?;
        Ok(Some((path, bytes)))
    }

    pub(crate) fn source_ranges(&self) -> [std::ops::Range<usize>; 2] {
        let out = self.stdout.bytes - self.stdout.capture_elided_bytes;
        let err = self.stderr.bytes - self.stderr.capture_elided_bytes;
        let start = out + usize::from(out > 0 && err > 0);
        [0..out, start..start + err]
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum CommandMetadata {
    Shell(ShellCapture),
    Snapshot([crate::performance::command_output::SnapshotSize; 2]),
    ShellSnapshot {
        streams: [crate::performance::command_output::SnapshotSize; 2],
    },
    PageExit(i64), // Existing private sidecars.
    Page {
        bytes: [usize; 2],
        widths: [ReplacementWidths; 2],
    },
}

impl CommandMetadata {
    pub(crate) fn needed_for_base64(&self) -> bool {
        matches!(self, Self::Shell(shell) if shell.stdout.source.is_some())
    }
}

tokio::task_local! {
    static MODEL_CAPTURE: std::cell::RefCell<Option<CommandMetadata>>;
}

pub(crate) async fn with_capture<F: std::future::Future>(
    future: F,
) -> (F::Output, Option<CommandMetadata>) {
    MODEL_CAPTURE
        .scope(Default::default(), async {
            (future.await, MODEL_CAPTURE.with(|slot| slot.take()))
        })
        .await
}

pub(super) fn capture_sync<R>(run: impl FnOnce() -> R) -> (R, Option<CommandMetadata>) {
    MODEL_CAPTURE.sync_scope(Default::default(), || {
        (run(), MODEL_CAPTURE.with(|slot| slot.take()))
    })
}

pub(super) fn publish_capture(capture: Option<ShellCapture>) {
    if let Some(capture) = capture {
        publish_metadata(CommandMetadata::Shell(capture));
    }
}

pub(super) fn publish_metadata(metadata: CommandMetadata) {
    replace_metadata(Some(metadata));
}

pub(super) fn replace_metadata(metadata: Option<CommandMetadata>) {
    let _ = MODEL_CAPTURE.try_with(|slot| slot.replace(metadata));
}

pub(crate) fn publish_snapshot(
    streams: [crate::performance::command_output::SnapshotSize; 2],
    text_sizes: [usize; 2],
) {
    replace_metadata(qualifies(text_sizes).then_some(CommandMetadata::Snapshot(streams)));
}

pub(crate) fn publish_page(
    stdout: &super::output_page::StreamPage,
    stderr: &super::output_page::StreamPage,
) {
    let streams = [stdout, stderr];
    // Decoded length is an upper bound. Save exact source widths for any
    // candidate, including a page whose replacements crossed the threshold.
    replace_metadata(
        qualifies(streams.map(|s| s.text.len())).then(|| CommandMetadata::Page {
            bytes: streams.map(|s| s.widths.raw_bytes(s.text.len())),
            widths: streams.map(|s| s.widths.clone()),
        }),
    );
}

pub(super) async fn run_logged(
    command: &mut Command,
    timeout: Option<Duration>,
    shell: &str,
    logs: Option<(std::path::PathBuf, std::path::PathBuf)>,
    stdin: Option<std::fs::File>,
) -> Result<Value> {
    Ok(
        run_logged_with_capture(command, timeout, shell, logs, stdin)
            .await?
            .0,
    )
}

pub(super) async fn run_logged_with_capture(
    command: &mut Command,
    timeout: Option<Duration>,
    shell: &str,
    logs: Option<(std::path::PathBuf, std::path::PathBuf)>,
    stdin: Option<std::fs::File>,
) -> Result<(Value, Option<ShellCapture>)> {
    let started = Instant::now();
    let run = run_captured(command, timeout, logs, stdin, OUTPUT_LIMIT, true).await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    let [(stdout, stdout_truncated, stdout_stats), (stderr, stderr_truncated, stderr_stats)] =
        present_streams([&run.stdout, &run.stderr], super::max_output_bytes());
    let mut result = if run.timed_out {
        let timeout = timeout.unwrap_or_default();
        json!({"error":format!("Command timed out after {} ms; descendants were terminated. For long installations or builds, set timeout explicitly (milliseconds, at most 600000), or use run_in_background.",timeout.as_millis()),"is_error":true,"timed_out":true})
    } else {
        let status = run.status.expect("a finished capture has a status");
        let code = status.code();
        let cut_short = cut_short_by_reader(shell, code);
        let mut result =
            json!({"success":status.success() || cut_short,"pipeline_sigpipe":cut_short});
        record_status(&mut result, Some(status));
        result
    };
    let capture = shell_metadata(
        duration_ms,
        stdout_stats,
        stderr_stats,
        [stdout.len(), stderr.len()],
    );
    result["stdout"] = json!(stdout);
    result["stderr"] = json!(stderr);
    result["stdout_truncated"] = json!(stdout_truncated);
    result["stderr_truncated"] = json!(stderr_truncated);
    result["shell"] = json!(shell);
    Ok((result, capture))
}

// Keep source metrics when loss could make a short command look long on replay.
fn shell_metadata(
    duration_ms: u64,
    stdout: StreamCapture,
    stderr: StreamCapture,
    text_bytes: [usize; 2],
) -> Option<ShellCapture> {
    (stdout.source.is_some() || qualifies([stdout.bytes, stderr.bytes]) || qualifies(text_bytes))
        .then_some(ShellCapture {
            source_file: None,
            duration_ms,
            stdout,
            stderr,
        })
}

/// Runs a command with a bounded **raw** capture: the whole document within
/// `limit` is kept and returned as it was, with no head/tail presentation
/// preview, so a caller can parse complete JSON inside that explicit cap.
/// Anything larger is reported as overflow instead of being handed on as a
/// silently mangled document.
pub(super) async fn run_raw(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    stdin: Option<std::fs::File>,
) -> Result<Value> {
    let run = run_captured(command, Some(timeout), None, stdin, limit, false).await?;
    let mut value = json!({
        "stdout":run.stdout.text(),
        "stderr":run.stderr.text(),
        "stdout_truncated":run.stdout.overflowed(),
        "stderr_truncated":run.stderr.overflowed(),
        "success":run.status.is_some_and(|status| status.success()),
        "timed_out":run.timed_out
    });
    record_status(&mut value, run.status);
    Ok(value)
}

fn record_status(value: &mut Value, status: Option<std::process::ExitStatus>) {
    let code = status.and_then(|status| status.code());
    #[cfg(windows)]
    let code = code.map(|c| c as u32); // Windows process exit codes are DWORDs.
    value["exit_code"] = json!(code);
    #[cfg(unix)]
    if let Some(signal) = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s)) {
        value["signal"] = json!(signal);
    }
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn model_tool_output_records_exit_code_and_signal_distinctly() {
    for (script, code, signal) in [
        ("kill -TERM $$", None, Some(15)),
        ("exit 143", Some(143), None),
    ] {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        let result = run(&mut command, Duration::from_secs(5), "sh")
            .await
            .unwrap();
        assert_eq!(result["exit_code"].as_i64(), code);
        assert_eq!(result["signal"].as_i64(), signal);
        assert_eq!(result["success"], false);
    }
}

/// One bounded capture of a child process.
struct CapturedRun {
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    stdout: Captured,
    stderr: Captured,
}

async fn run_captured(
    command: &mut Command,
    timeout: Option<Duration>,
    logs: Option<(std::path::PathBuf, std::path::PathBuf)>,
    stdin: Option<std::fs::File>,
    limit: usize,
    collect_lines: bool,
) -> Result<CapturedRun> {
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin {
        Some(stdin) => command.stdin(Stdio::from(stdin)),
        None => command.stdin(Stdio::null()),
    };
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Starting shell command")?;
    let tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("Shell process ID missing")?,
    )?;
    let stdout = child.stdout.take().context("Shell stdout missing")?;
    let stderr = child.stderr.take().context("Shell stderr missing")?;
    let (mut stdout_bytes, mut stderr_bytes) =
        (Captured::with_limit(limit), Captured::with_limit(limit));
    if !collect_lines {
        stdout_bytes.excerpts = None;
        stderr_bytes.excerpts = None;
    }
    let (out_log, err_log) = logs
        .map(|(out, err)| (Some(out), Some(err)))
        .unwrap_or_default();
    let execution = async {
        // Reap the root before waiting for EOF: descendants may inherit the pipes.
        let wait = async {
            let status = child.wait().await;
            drop(tree);
            status
        };
        let (status, out, err) = tokio::join!(
            wait,
            capture(stdout, &mut stdout_bytes, out_log),
            capture(stderr, &mut stderr_bytes, err_log)
        );
        out?;
        err?;
        status
    };
    let result = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, execution).await,
        None => Ok(execution.await),
    };
    match result {
        Ok(Ok(status)) => Ok(CapturedRun {
            status: Some(status),
            timed_out: false,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        }),
        Ok(Err(error)) => Err(error).context("Reading shell command output"),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(CapturedRun {
                status: None,
                timed_out: true,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            })
        }
    }
}

/// Starts a long-lived service that must outlive this call. It is deliberately
/// outside the Job Object that `run_logged` relies on: the trade for surviving
/// our exit is that it also survives cancellation, so the caller owns cleanup.
pub(super) fn start_detached(mut command: Command, directory: &Path, shell: &str) -> Result<Value> {
    std::fs::create_dir_all(directory)?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");

    #[cfg(windows)]
    let (pid, breakaway) = {
        configure_detached(&mut command, &stdout, &stderr)?;
        command.creation_flags(detached_flags(true));
        match command.spawn() {
            Ok(child) => (child.id().unwrap_or(0), true),
            // A Job without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses the flag. Start
            // anyway and report the weaker guarantee rather than failing outright.
            Err(error) if error.raw_os_error() == Some(5) => {
                configure_detached(&mut command, &stdout, &stderr)?;
                command.creation_flags(detached_flags(false));
                let child = command.spawn().context("Starting the detached command")?;
                (child.id().unwrap_or(0), false)
            }
            Err(error) => return Err(error).context("Starting the detached command"),
        }
    };

    #[cfg(unix)]
    let (pid, breakaway) = {
        configure_detached(&mut command, &stdout, &stderr)?;
        unsafe {
            command.pre_exec(|| {
                // Its own session, so closing our terminal cannot signal it.
                rustix::process::setsid()
                    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
                Ok(())
            });
        }
        let child = command.spawn().context("Starting the detached command")?;
        (child.id().unwrap_or(0), true)
    };

    #[cfg(not(any(windows, unix)))]
    let (pid, breakaway) = {
        configure_detached(&mut command, &stdout, &stderr)?;
        let child = command.spawn().context("Starting the detached command")?;
        (child.id().unwrap_or(0), false)
    };

    // Only a real breakaway starts outside the caller's Job Object. Saying the
    // service survives when it does not would be worse than not starting it.
    let message = if breakaway {
        "Started as an unmanaged service: it outlives this call and is not tracked as a task. Stop it explicitly when it is no longer needed."
    } else {
        "Started, but the host refused to detach it from its process group (the enclosing Job Object does not allow breakaway). It may be terminated together with this call's process tree; run it from a terminal outside that Job Object if it must keep running."
    };
    Ok(json!({
        "detached": true,
        "pid": pid,
        "stdout_file": stdout,
        "stderr_file": stderr,
        "isolated": breakaway,
        "shell": shell,
        "message": message
    }))
}

fn configure_detached(command: &mut Command, stdout: &Path, stderr: &Path) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(stdout)?))
        .stderr(Stdio::from(std::fs::File::create(stderr)?))
        // Dropping the handle must not kill the service.
        .kill_on_drop(false);
    Ok(())
}

#[cfg(windows)]
fn detached_flags(breakaway: bool) -> u32 {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    DETACHED_PROCESS
        | CREATE_NEW_PROCESS_GROUP
        | if breakaway {
            CREATE_BREAKAWAY_FROM_JOB
        } else {
            0
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn root_exit_does_not_wait_for_inherited_pipes() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "require('child_process').spawn(process.execPath,['-e','setInterval(()=>{},10000)'],{stdio:'inherit'}).unref();console.log('root done');setTimeout(()=>process.exit(0),200)"]);
        let started = std::time::Instant::now();
        let result = run(&mut command, Duration::from_secs(8), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert!(result["stdout"].as_str().unwrap().contains("root done"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[tokio::test]
    async fn output_is_bounded_while_both_pipes_are_drained() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('START\\n'+'x'.repeat(2*1024*1024)+'\\nTEST SUMMARY');process.stderr.write('ERROR START\\n'+'y'.repeat(2*1024*1024)+'\\nFINAL ERROR')"]);
        let result = run(&mut command, Duration::from_secs(10), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        assert!(result["stdout"].as_str().unwrap().len() <= OUTPUT_LIMIT + 200);
        assert!(result["stdout"].as_str().unwrap().starts_with("START"));
        assert!(result["stdout"].as_str().unwrap().ends_with("TEST SUMMARY"));
        assert!(result["stderr"].as_str().unwrap().ends_with("FINAL ERROR"));
    }

    #[tokio::test]
    async fn tool_limit_sets_flags_before_capture_limit() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('x'.repeat(200*1024)+'END')"]);
        let result = run(&mut command, Duration::from_secs(10), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], false);
        assert!(result["stdout"].as_str().unwrap().ends_with("END"));
    }
    #[tokio::test]
    async fn timeout_retains_partial_diagnostics() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('install started');process.stderr.write('registry waiting');setInterval(()=>{},10000)"]);
        let result = run(&mut command, Duration::from_millis(700), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["is_error"], true);
        assert_eq!(result["stdout"], "install started");
        assert_eq!(result["stderr"], "registry waiting");
        assert_eq!(result["stdout_truncated"], false);
    }

    async fn descendants(cancel: bool) {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let leaked = temp.path().join("leaked");
        let code = r#"const cp=require('child_process'),fs=require('fs');cp.spawn(process.execPath,['-e',`setTimeout(()=>require('fs').writeFileSync(process.argv[1],'leaked'),1500)`,process.argv[2]],{stdio:'inherit'});fs.writeFileSync(process.argv[1],'ready');setInterval(()=>{},10000)"#;
        let mut command = Command::new("node");
        command.arg("-e").arg(code).arg(&ready).arg(&leaked);
        let task = tokio::spawn(async move {
            run(
                &mut command,
                Duration::from_millis(if cancel { 10000 } else { 700 }),
                "node-fixture",
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(task.await.unwrap().unwrap()["timed_out"], true);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !leaked.exists(),
            "Grandchild survived shell cancellation/timeout"
        );
    }
    #[tokio::test]
    async fn timeout_terminates_grandchildren() {
        descendants(false).await;
    }
    #[tokio::test]
    async fn cancellation_terminates_grandchildren() {
        descendants(true).await;
    }

    #[test]
    fn sigpipe_is_recognized_only_for_bash_like_shells() {
        assert!(cut_short_by_reader("bash", Some(141)));
        assert!(cut_short_by_reader("zsh", Some(141)));
        assert!(cut_short_by_reader("sh", Some(141)));
        assert!(!cut_short_by_reader("powershell", Some(141)));
        assert!(!cut_short_by_reader("cmd", Some(141)));
        assert!(!cut_short_by_reader("bash", Some(1)));
        assert!(!cut_short_by_reader("bash", None));
    }

    /// A detached start must hand back a running process whose output is in the
    /// log files, and must not claim a stronger lifetime guarantee than it got.
    #[tokio::test]
    async fn detached_start_reports_a_live_service_and_its_real_isolation() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("alive");
        let mut command = Command::new("node");
        command.arg("-e").arg(format!(
            "console.log('started');setTimeout(()=>require('fs').writeFileSync({:?},'alive'),600);setInterval(()=>{{}},10000)",
            marker.to_string_lossy()
        ));
        let result = start_detached(command, temp.path(), "node-fixture").unwrap();
        let pid = result["pid"].as_u64().unwrap();
        assert_eq!(result["detached"], true);
        assert!(pid > 0, "{result}");

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            marker.exists(),
            "Detached service did not keep running: {result}"
        );
        let log = std::fs::read_to_string(temp.path().join("stdout.log")).unwrap();
        assert!(
            log.contains("started"),
            "Detached stdout was not captured: {log:?}"
        );
        let isolated = result["isolated"].as_bool().unwrap();
        let message = result["message"].as_str().unwrap();
        assert_eq!(message.contains("outlives this call"), isolated, "{result}");
        stop_test_service(pid as u32);
    }

    /// procps `kill -9 -123456` without `--` signals pid -1 (every process of the
    /// user), so an external `kill` given a negative pid must separate it with `--`.
    #[test]
    fn external_kill_never_takes_a_negative_pid_without_separator() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut offenders = Vec::new();
        for dir in ["crates", "src-tauri/src"] {
            let mut stack = vec![root.join(dir)];
            while let Some(path) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|ext| ext == "rs") {
                        let text = std::fs::read_to_string(&path).unwrap();
                        let needle = concat!("Command::new(\"", "kill\")");
                        for (index, _) in text.match_indices(needle) {
                            let call: String = text[index..].lines().take(4).collect();
                            if call.contains("format!(\"-") && !call.contains("\"--\"") {
                                offenders.push(path.display().to_string());
                            }
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "negative pid without `--`: {offenders:?}"
        );
    }

    fn stop_test_service(pid: u32) {
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
        // Signal the group directly: procps `kill -9 -123456` (no `--`) parses as
        // `kill(-1, SIGKILL)` and kills every process of the user.
        #[cfg(unix)]
        if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}
