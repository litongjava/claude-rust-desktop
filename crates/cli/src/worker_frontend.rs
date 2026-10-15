//! An attachable frontend for a turn owned by an independent supervisor/worker.
//! Only the worker submits the prompt, executes tools, and persists the transcript.
use crate::{
    args::Cli,
    session::Session,
    terminal::{
        input::TerminalInput,
        live::{Action, TurnDisplay},
    },
};
use anyhow::{ensure, Result};
use claude_core::services::background_sessions::{self as jobs, Launch};
use serde_json::Value;

pub async fn turn(
    cli: &Cli,
    session: &Session,
    prompt: &str,
    input: &mut Option<TerminalInput>,
) -> Result<()> {
    let root = &session.paths.data_dir;
    ensure!(!jobs::list(root, Some(&session.id))?.iter().any(|job| job.session_id == session.id && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")),
        "This session already has an active worker. Use /tasks attach ID, /tasks cancel ID, or /clear to work in a new session; the current prompt was not submitted.");
    let provider = session.selected_provider().await?;
    let executable = std::env::current_exe()?;
    let arguments = crate::background_commands::arguments(
        cli,
        &session.paths,
        session,
        &session.id,
        prompt,
        &provider,
    )?;
    let environment =
        crate::background_commands::worker_environment(session.auto_route, &provider)?;
    // Validate interactive configuration before launching a request that must survive us.
    let history = session
        .store
        .history(&session.id)?
        .iter()
        .filter(|row| row["role"] == "user")
        .filter_map(|row| row["content"].as_str().map(str::to_owned))
        .collect();
    let mut display = TurnDisplay::new(
        true,
        session.permission_mode(),
        &root.join("keybindings.json"),
        history,
    )?;
    let job = if std::env::var("CLAUDE_RUST_SINGLE_TURN_WORKER").as_deref() == Ok("1") {
        jobs::spawn_foreground(
            root,
            &executable,
            Launch {
                executable: executable.clone(),
                arguments,
                cwd: session.workspace.clone(),
            },
            &session.id,
            prompt,
            &environment,
        )?
    } else {
        use claude_core::services::session_worker::{self, Request};
        let worker =
            session_worker::ensure_worker(root, &executable, &session.id, &session.workspace)
                .await?;
        let id = uuid::Uuid::new_v4().to_string();
        let response = session_worker::call(
            &worker,
            &Request::Submit {
                id: id.clone(),
                prompt: prompt.into(),
                arguments,
                environment,
            },
        )
        .await;
        // Accepted jobs are durable even when the acknowledgement is lost. Never replay.
        match jobs::get(root, &id) {
            Ok(job) => job,
            Err(error) => {
                response?;
                return Err(error);
            }
        }
    };
    let result = observe(session, &job.id, input, &mut display).await;
    if result.is_err() {
        if jobs::get(root, &job.id)
            .is_ok_and(|job| matches!(job.status.as_str(), "starting" | "running"))
        {
            // Only genuinely detached/unobserved completion gets a later notice.
            let _ = jobs::detach(root, &job.id, &session.id);
            match jobs::cancel(root, &job.id) {
                Ok(_) => display.notice(&format!(
                    "Cancellation requested for worker {} after a frontend error.",
                    job.id
                )),
                Err(error) => display.notice(&format!(
                    "Worker {} may still be running: {error:#}. Inspect /tasks.",
                    job.id
                )),
            }
        }
    }
    // Raw engine events and terminal error remain in the durable job logs.
    // The footer owns the single user-facing error; do not rethrow it to REPL.
    let error_text = result.as_ref().err().map(|error| {
        let state = jobs::get(root, &job.id).ok();
        failure_message(
            &job.id,
            state
                .as_ref()
                .map(|job| job.status.as_str())
                .unwrap_or("failed"),
            &format!("{error:#}"),
        )
    });
    if let Err(error) = &result {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(
            root.join("background-tasks")
                .join(&job.id)
                .join("stderr.log"),
        ) {
            let _ = writeln!(file, "Frontend observed: {error:#}");
        }
    }
    let footer = if matches!(result, Ok(true)) {
        Some("detached - worker continues")
    } else {
        error_text.as_deref()
    };
    for action in display.finish(footer) {
        restore(input, action);
    }
    match result {
        Ok(_) => Ok(()),
        Err(_) => {
            Err(PresentedFailure(error_text.unwrap_or_else(|| "Request failed".into())).into())
        }
    }
}

fn restore(input: &mut Option<TerminalInput>, action: Action) {
    if let Some(input) = input {
        match action {
            Action::Draft(draft) => input.restore_draft(draft),
            Action::Submit(prompt) => input.queue_prompt(prompt),
            Action::SideQuestion(question) => input.queue_prompt(format!("/btw {question}")),
            _ => {}
        }
    }
}

async fn observe(
    session: &Session,
    id: &str,
    input: &mut Option<TerminalInput>,
    display: &mut TurnDisplay,
) -> Result<bool> {
    let root = &session.paths.data_dir;
    let mut cursor = 0;
    let mut streamed = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut answered = std::collections::HashSet::new();
    let mut pending: Option<(String, tokio::sync::oneshot::Receiver<String>)> = None;
    let mut side: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>,
    > = None;
    let mut cancelled = false;
    let mut notified = std::time::Instant::now();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (job, next, events) = jobs::events_since(root, id, &session.id, cursor)?;
                let drained = cursor == next;
                cursor = next;
                for event in events {
                    if event["type"] == "text" { streamed = true; }
                    if event["type"] == "result" {
                        if let Some(text)=result_text(&event,streamed) {display.before_text(text);}
                        streamed = false;
                    }
                    render(display, &event);
                }
                if pending.is_none() {
                    if let Some(question) = jobs::pending_question(root, id, &session.id)?.filter(|question| !answered.contains(&question.request_id)) {
                        if let Some(live) = &display.live {
                            pending = Some((question.request_id, live.question(question.question)));
                        } else if let Some(reader) = input.as_mut() {
                            if let Some(answer) = reader.read_answer(&question.question).await? {
                                jobs::answer_question(root, id, &session.id, &question.request_id, &answer)?;
                            }
                        }
                    }
                }
                if drained && !matches!(job.status.as_str(), "starting"|"running") {
                    if matches!(job.status.as_str(),"completed"|"failed"|"cancelled") {jobs::acknowledge_foreground(root,id,&session.id)?;}
                    ensure!(job.status == "completed", "Worker {}: {}{}", id, job.status, job.error.map(|error|format!(" - {error}")).unwrap_or_default());
                    if side.is_none() { return Ok(false); }
                    display.set_activity("Main task complete - finishing side question");
                }
                if notified.elapsed() >= std::time::Duration::from_secs(1) {
                    for notice in crate::background_commands::collect_completed(session)? { display.notice(&notice); }
                    notified = std::time::Instant::now();
                }
            },
            answer = async { match pending.as_mut() { Some((_, answer)) => answer.await, None => std::future::pending().await } } => {
                let (request, _) = pending.take().unwrap();
                jobs::answer_question(root, id, &session.id, &request, &answer?)?;
                answered.insert(request);
                display.set_activity("Working");
            },
            action = display.action() => {
                match action {
                    Some(Action::Detach) => {
                        jobs::detach(root, id, &session.id)?;
                        display.notice(&format!("Task {id} continues in the background without restarting its request. Use /tasks attach {id} to view it. This session is occupied until completion; /clear starts a new session."));
                        return Ok(true);
                    },
                    Some(Action::Cancel) | None => {
                        if !cancelled { jobs::cancel(root, id)?; cancelled = true; display.set_activity("Cancelling worker"); }
                    },
                    Some(Action::SideQuestion(question)) => {
                        if side.is_none() { side=Some(Box::pin(async move { crate::background_commands::side_question(session, &question).await })); }
                        else { display.notice("A side question is already running."); }
                    },
                    Some(action) => restore(input, action),
                }
            },
            answer = async { match side.as_mut() { Some(side) => side.await, None => std::future::pending().await } } => {
                side=None;
                match answer { Ok(answer)=>display.notice(&format!("Side answer:\n{answer}")), Err(error)=>display.notice(&format!("Side question failed: {error:#}")) }
            },
            _ = tokio::signal::ctrl_c() => {
                if !cancelled { jobs::cancel(root, id)?; cancelled = true; display.set_activity("Cancelling worker"); }
            },
        }
    }
}

fn render(display: &mut TurnDisplay, event: &Value) {
    let text = |name: &str| event[name].as_str().unwrap_or("");
    match text("type") {
        "text" => display.before_text(text("delta")),
        "thinking" => {
            display.record_thinking(text("delta"));
            display.thinking();
        }
        "usage" => display.reported_usage(&event["usage"]),
        "message_start" => display.set_activity("Composing"),
        "tool_start" => display.tool_started(text("id"), text("name"), &event["input"]),
        "tool_result" => display.tool_finished(
            text("id"),
            text("name"),
            text("output"),
            event["is_error"].as_bool().unwrap_or(false),
        ),
        "agent_progress" => display.agent_progress(
            text("session_id"),
            text("agent_type"),
            text("status"),
            event["tool_name"].as_str(),
        ),
        "error" => {} // Final job state produces one concise footer; logs retain all diagnostics.
        "result" => {
            display.reported_usage(&event["usage"]);
        }
        _ => {}
    }
}

#[derive(Debug)]
struct PresentedFailure(String);
impl std::fmt::Display for PresentedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PresentedFailure {}
pub fn already_presented(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PresentedFailure>().is_some()
}

fn failure_message(id: &str, status: &str, diagnostic: &str) -> String {
    if status == "cancelled" {
        return "Interrupted".into();
    }
    let lower = diagnostic.to_ascii_lowercase();
    let reason = if lower.contains("timed out") || lower.contains("timeout") {
        "Request timed out".to_owned()
    } else if lower.contains("error sending request") || lower.contains("connection") {
        "Connection failed".to_owned()
    } else {
        let http = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .collect::<Vec<_>>()
            .windows(2)
            .find_map(|pair| {
                ((pair[0] == "http" || pair[0] == "status") && pair[1].len() == 3)
                    .then(|| pair[1].parse::<u16>().ok())
                    .flatten()
                    .filter(|code| (400..600).contains(code))
            });
        http.map(|code| format!("HTTP {code}"))
            .unwrap_or_else(|| "Request failed".into())
    };
    // Only expose a parsed endpoint authority: credentials, path and query are discarded.
    let endpoint = diagnostic.split_whitespace().find_map(|word| {
        let value = word.trim_matches(|c: char| "()[]\"',".contains(c));
        let scheme = value.find("http://").or_else(|| value.find("https://"))?;
        let value = &value[scheme..];
        let authority = value
            .split_once("://")?
            .1
            .split(['/', '?', '#', ')'])
            .next()?;
        let authority = authority.rsplit('@').next()?;
        (!authority.is_empty()
            && authority.len() <= 80
            && authority
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".:-[]".contains(c)))
        .then_some(authority)
    });
    let detail = endpoint
        .map(|host| format!("{reason} ({host})"))
        .unwrap_or(reason);
    format!("{detail}. Details: /tasks show {id}")
        .chars()
        .take(160)
        .collect()
}

fn result_text(event: &Value, streamed: bool) -> Option<&str> {
    (!streamed && event["is_error"] != true)
        .then(|| event["result"].as_str())
        .flatten()
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn failed_results_do_not_repeat_engine_diagnostics_as_answer_text() {
        let event = serde_json::json!({"type":"result","is_error":true,"result":"Engine execution failed: API request failed: private endpoint","error":"diagnostic"});
        assert_eq!(result_text(&event, false), None);
        assert_eq!(
            failure_message("task-id", "failed", "unknown"),
            "Request failed. Details: /tasks show task-id"
        );
        assert_eq!(
            failure_message("task-id", "cancelled", "cancel"),
            "Interrupted"
        );
        let successful = serde_json::json!({"type":"result","is_error":false,"result":"answer"});
        assert_eq!(result_text(&successful, false), Some("answer"));
        assert_eq!(result_text(&successful, true), None);
        let safe=failure_message("id","failed","Engine execution failed: error sending request for url (https://user:secret@example.com:443/path?key=secret)");
        assert_eq!(
            safe,
            "Connection failed (example.com:443). Details: /tasks show id"
        );
        assert_eq!(
            failure_message("id", "failed", "API HTTP 400 secret response body"),
            "HTTP 400. Details: /tasks show id"
        );
        assert!(already_presented(&anyhow::Error::new(PresentedFailure(
            safe
        ))));
    }
}
