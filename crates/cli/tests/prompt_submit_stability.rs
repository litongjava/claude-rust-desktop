//! Real PTY/ConPTY frame-by-frame regression tests for prompt submission stability.
//!
//! Verifies that submitting a prompt in a 36x100 terminal:
//! 1. Keeps the bottom input box borders and footer on the exact same screen rows
//!    from the first idle prompt (with Tip) through submission, request waiting,
//!    streaming response, turn completion, and subsequent prompts.
//! 2. Never erases the bottom input box into an empty gap between Enter and the
//!    working live frame, even while the model request is delayed.
//! 3. Allows Esc to cancel a pending model request while the early live display
//!    owns the terminal.

use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const ROWS: u16 = 36;
const COLS: u16 = 100;
const SYNC_END: &[u8] = b"\x1b[?2026l";

#[derive(Clone)]
struct MockModel {
    requests: Arc<AtomicUsize>,
    arrived: Arc<Notify>,
    release: Arc<Notify>,
    replies: Arc<Mutex<Vec<String>>>,
}

impl MockModel {
    fn new(replies: Vec<&str>) -> Self {
        Self {
            requests: Arc::new(AtomicUsize::new(0)),
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            replies: Arc::new(Mutex::new(
                replies.into_iter().rev().map(str::to_owned).collect(),
            )),
        }
    }
}

async fn chat_completions(
    State(model): State<MockModel>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    // Auxiliary topic-naming requests should finish immediately without blocking the turn gate.
    let system = body["messages"]
        .as_array()
        .and_then(|messages| messages.first())
        .and_then(|message| message["content"].as_str())
        .unwrap_or("");
    if system.contains("short title") || system.contains("topic") {
        let stream = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"Test topic"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        );
        return ([("content-type", "text/event-stream")], stream).into_response();
    }

    model.requests.fetch_add(1, Ordering::SeqCst);
    model.arrived.notify_waiters();
    model.release.notified().await;

    let reply = model
        .replies
        .lock()
        .unwrap()
        .pop()
        .unwrap_or_else(|| "STABILITY_REPLY_DEFAULT".to_owned());
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":reply},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    );
    ([("content-type", "text/event-stream")], stream).into_response()
}

struct PtySession {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    raw_log: Vec<u8>,
    sync_tail: Vec<u8>,
}

impl PtySession {
    fn spawn(workspace: &Path, data: &Path, extra_env: &[(&str, &str)]) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_dcl"));
        cmd.cwd(workspace);
        cmd.args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "ask",
        ]);
        cmd.arg("--data-dir");
        cmd.arg(data);
        cmd.env("TERM", "xterm-256color");
        cmd.env("NO_COLOR", "1");
        // Strip inherited host session/worker environment variables so the test stays isolated.
        for key in [
            "DEEPSEEK_CODE_DATA_DIR",
            "DEEPSEEK_CODE_BACKGROUND_CONTROL",
            "DEEPSEEK_CODE_BACKGROUND_JOB",
            "DEEPSEEK_CODE_WORKER_MODEL",
            "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
            "DEEPSEEK_CODE_CHILD_API_KEY",
            "DEEPSEEK_CODE_COORDINATION_DIR",
            "DEEPSEEK_CODE_SESSION_HOST",
            "DEEPSEEK_CODE_SINGLE_TURN_WORKER",
        ] {
            cmd.env_remove(key);
        }
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        let child = pty.slave.spawn_command(cmd).expect("spawn dcl in PTY");
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().expect("clone PTY reader");
        let writer = pty.master.take_writer().expect("take PTY writer");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            master: pty.master,
            writer,
            rx,
            parser: vt100::Parser::new(ROWS, COLS, 200),
            raw_log: Vec::new(),
            sync_tail: Vec::new(),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write PTY");
        self.writer.flush().expect("flush PTY");
    }

    fn screen_rows(&self) -> Vec<String> {
        self.parser.screen().rows(0, COLS).collect()
    }

    /// Processes available PTY chunks, invoking `on_frame` after every completed
    /// synchronized frame (`\x1b[?2026l`) as well as after each chunk when no
    /// synchronized block is open.
    fn pump_with<F: FnMut(&[String])>(&mut self, mut on_frame: F) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.raw_log.extend_from_slice(&chunk);
            self.sync_tail.extend_from_slice(&chunk);
            // Answer DSR cursor queries (`\x1b[6n`) on Unix PTYs; ConPTY handles it internally.
            if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                let (row, col) = self.parser.screen().cursor_position();
                let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                let _ = self.writer.write_all(reply.as_bytes());
                let _ = self.writer.flush();
            }
            let mut pos = 0;
            while let Some(offset) = self.sync_tail[pos..]
                .windows(SYNC_END.len())
                .position(|w| w == SYNC_END)
            {
                let end = pos + offset + SYNC_END.len();
                self.parser.process(&self.sync_tail[pos..end]);
                let rows = self.screen_rows();
                on_frame(&rows);
                pos = end;
            }
            if pos > 0 {
                self.sync_tail.drain(..pos);
            } else if !self.sync_tail.windows(8).any(|w| w == b"\x1b[?2026h") {
                let pending = std::mem::take(&mut self.sync_tail);
                self.parser.process(&pending);
                let rows = self.screen_rows();
                on_frame(&rows);
            }
        }
    }

    async fn wait_for<P: Fn(&[String]) -> bool>(
        &mut self,
        timeout: Duration,
        label: &str,
        predicate: P,
        mut on_frame: impl FnMut(&[String]),
    ) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump_with(&mut on_frame);
            let rows = self.screen_rows();
            if predicate(&rows) {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {label}; last screen:\n{}",
                format_screen(&rows)
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = &self.master;
    }
}

fn format_screen(rows: &[String]) -> String {
    rows.iter()
        .enumerate()
        .map(|(idx, line)| format!("{idx:02}: {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 20 && trimmed.chars().all(|ch| ch == '─')
}

fn assert_bottom_box_intact(rows: &[String], context: &str) {
    let top_rule = ROWS as usize - 4;
    let bottom_rule = ROWS as usize - 2;
    let footer_row = ROWS as usize - 1;
    assert!(
        is_horizontal_rule(&rows[top_rule]),
        "{context}: top border missing/shifted from row {top_rule}:\n{}",
        format_screen(rows)
    );
    assert!(
        is_horizontal_rule(&rows[bottom_rule]),
        "{context}: bottom border missing/shifted from row {bottom_rule}:\n{}",
        format_screen(rows)
    );
    assert!(
        !rows[footer_row].trim().is_empty(),
        "{context}: footer missing from bottom row {footer_row}:\n{}",
        format_screen(rows)
    );
}

fn write_fixture_config(data: &Path, workspace: &Path, endpoint: &str) {
    let layout = deepseek_code_core::data_layout::DataLayout::from_root(data);
    deepseek_code_core::data_layout::write_config_file(
        layout.config_file("providers.json"),
        json!([{
            "id": "fixture",
            "name": "Fixture",
            "base_url": format!("{endpoint}/v1"),
            "api_key": "fixture-only",
            "api_format": "openai",
            "enabled": true,
            "models": [{
                "id": "fixture-model",
                "name": "Fixture",
                "enabled": true,
                "supports_vision": false,
                "supports_web_search": false
            }]
        }])
        .to_string(),
    )
    .unwrap();
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    deepseek_code_core::data_layout::write_config_file(
        layout.config_file("startup-prompts.json"),
        json!({"trusted_folders": [workspace, canonical]}).to_string(),
    )
    .unwrap();
    deepseek_code_core::data_layout::write_config_file(
        layout.config_file("settings.json"),
        json!({"promptSuggestions": false}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn pty_submit_keeps_bottom_box_fixed_across_first_turn_second_turn_and_early_esc() {
    let model = MockModel::new(vec!["FIRST_REPLY_OK", "SECOND_REPLY_NEVER"]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(model.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let data = root.path().join("data");
    std::fs::create_dir_all(&workspace).unwrap();
    write_fixture_config(&data, &workspace, &endpoint);

    let mut pty = PtySession::spawn(&workspace, &data, &[]);

    // 1. Wait for the first idle prompt (with Tip) and verify the bottom 4 rows.
    let idle_before = pty
        .wait_for(
            Duration::from_secs(20),
            "initial idle prompt",
            |rows| {
                is_horizontal_rule(&rows[ROWS as usize - 4])
                    && rows[ROWS as usize - 3].contains('❯')
                    && is_horizontal_rule(&rows[ROWS as usize - 2])
                    && rows.iter().any(|row| row.contains("Tip:"))
            },
            |_| {},
        )
        .await;
    assert_bottom_box_intact(&idle_before, "initial idle prompt");
    let tip_row = idle_before
        .iter()
        .position(|row| row.contains("Tip:"))
        .expect("initial Tip visible");
    assert!(
        tip_row < ROWS as usize - 4,
        "Tip must sit above the input box so the first submit cannot shift rows:\n{}",
        format_screen(&idle_before)
    );

    // 2. Type first question and submit while the model response is gated.
    pty.send(b"hello stability");
    pty.wait_for(
        Duration::from_secs(5),
        "typed draft",
        |rows| rows[ROWS as usize - 3].contains("hello stability"),
        |rows| assert_bottom_box_intact(rows, "while typing first prompt"),
    )
    .await;

    let mut observed_frames = 0usize;
    pty.send(b"\r");

    // 3. While the model response is STILL gated, the working frame must appear
    //    and every synchronized frame in between must keep the bottom box intact.
    let working_frame = pty
        .wait_for(
            Duration::from_secs(15),
            "working live frame before response release",
            |rows| {
                rows[..ROWS as usize - 4]
                    .iter()
                    .any(|row| row.contains("hello stability"))
                    && rows[ROWS as usize - 7..ROWS as usize - 4]
                        .iter()
                        .any(|row| row.contains("… ("))
            },
            |rows| {
                observed_frames += 1;
                assert_bottom_box_intact(rows, "during first prompt submit -> working transition");
            },
        )
        .await;
    assert_bottom_box_intact(&working_frame, "working live frame");
    assert!(
        observed_frames > 0,
        "expected to inspect at least one frame during submit transition"
    );

    // Ensure the mock endpoint actually received the request before releasing.
    tokio::time::timeout(Duration::from_secs(10), async {
        while model.requests.load(Ordering::SeqCst) < 1 {
            pty.pump_with(|rows| {
                assert_bottom_box_intact(rows, "waiting for first HTTP request");
            });
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("first model request did not arrive");

    // 4. Release the first response and verify the box stays fixed through completion
    //    and handoff back to the idle prompt.
    model.release.notify_waiters();
    let idle_after_first = pty
        .wait_for(
            Duration::from_secs(15),
            "first turn completion and idle handoff",
            |rows| {
                rows.iter().any(|row| row.contains("FIRST_REPLY_OK"))
                    && rows.iter().any(|row| row.contains("· done"))
                    && !rows[ROWS as usize - 7..ROWS as usize - 4]
                        .iter()
                        .any(|row| row.contains("… ("))
            },
            |rows| assert_bottom_box_intact(rows, "during first response streaming and handoff"),
        )
        .await;
    assert_bottom_box_intact(&idle_after_first, "idle prompt after first turn");

    // 5. Submit a second prompt (now without Tip) and press Esc while gated to
    //    verify both second-turn stability and early Esc cancellation.
    pty.send(b"second question");
    pty.wait_for(
        Duration::from_secs(5),
        "second typed draft",
        |rows| rows[ROWS as usize - 3].contains("second question"),
        |rows| assert_bottom_box_intact(rows, "while typing second prompt"),
    )
    .await;
    pty.send(b"\r");
    pty.wait_for(
        Duration::from_secs(15),
        "second working frame before Esc",
        |rows| {
            rows[..ROWS as usize - 4]
                .iter()
                .any(|row| row.contains("second question"))
                && rows[ROWS as usize - 7..ROWS as usize - 4]
                    .iter()
                    .any(|row| row.contains("… ("))
        },
        |rows| assert_bottom_box_intact(rows, "during second prompt submit -> working transition"),
    )
    .await;

    pty.send(b"\x1b");
    let after_esc = pty
        .wait_for(
            Duration::from_secs(10),
            "Esc cancellation back to idle prompt",
            |rows| {
                rows.iter().any(|row| row.contains("Interrupted"))
                    && !rows[ROWS as usize - 7..ROWS as usize - 4]
                        .iter()
                        .any(|row| row.contains("… ("))
            },
            |rows| assert_bottom_box_intact(rows, "during Esc cancellation"),
        )
        .await;
    assert_bottom_box_intact(&after_esc, "idle prompt after Esc cancellation");

    // Clean exit.
    pty.send(b"/exit\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while pty.child.try_wait().unwrap().is_none() {
        pty.pump_with(|_| {});
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();
}
