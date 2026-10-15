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
    )?.with_workspace(&session.workspace);
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
    present(session, &job, input, &mut display, true).await
}

/// Attach to durable events only: never submit or recreate a worker request.
pub async fn attach(session: &Session, id: &str, input: &mut Option<TerminalInput>) -> Result<()> {
    let job = jobs::get(&session.paths.data_dir, id)?;
    authorize_attachment(&job, &session.id)?;
    let workspace = session.store.workspace(&job.session_id, &session.paths.data_dir.join("workspaces"))?;
    let mut display = TurnDisplay::new(
        true,
        session.permission_mode(),
        &session.paths.data_dir.join("keybindings.json"),
        Vec::new(),
    )?.with_workspace(&workspace);
    display.notice(&format!(
        "Attached to task {id}. Esc cancels; Ctrl+B or /background detaches without restarting."
    ));
    if job.session_id != session.id {
        display.notice("Follow-up prompts are queued to your current session after this view closes; they do not steer the attached child. /btw uses your current session context.");
    }
    present(session, &job, input, &mut display, false).await
}

fn authorize_attachment(job: &jobs::Job, session: &str) -> Result<()> {
    ensure!(
        job.parent_session_id == session || job.session_id == session,
        "Task belongs to another session"
    );
    Ok(())
}

async fn present(
    session: &Session,
    job: &jobs::Job,
    input: &mut Option<TerminalInput>,
    display: &mut TurnDisplay,
    sync_plan_mode: bool,
) -> Result<()> {
    authorize_attachment(job, &session.id)?;
    let root = &session.paths.data_dir;
    let result = observe(session, &job.id, &job.parent_session_id, input, display, sync_plan_mode).await;
    let still_running = result.is_err()
        && jobs::get(root, &job.id)
            .is_ok_and(|job| matches!(job.status.as_str(), "starting" | "running"));
    if still_running {
        // Observation is not execution: a failed view must never cancel work.
        let _ = jobs::detach(root, &job.id, &job.parent_session_id);
        display.notice(&format!("Task {} continues in the background after a view error. Use /tasks attach {} to return; /tasks show {} contains the original diagnostic.", job.id, job.id, job.id));
    }
    // Raw engine events and terminal error remain in the durable job logs.
    // The footer owns the single user-facing error; do not rethrow it to REPL.
    let error_text = result.as_ref().err().map(|error| {
        if still_running {
            return format!(
                "Task view failed; worker continues. Details: /tasks show {}",
                job.id
            );
        }
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
            Action::CyclePermission => input.queue_permission_cycle(),
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
    owner: &str,
    input: &mut Option<TerminalInput>,
    display: &mut TurnDisplay,
    sync_plan_mode: bool,
) -> Result<bool> {
    let root = &session.paths.data_dir;
    let initial_job = jobs::get(root, id)?;
    let sync_mode = may_sync_permission_mode(&session.id, &initial_job.session_id, &initial_job.status, sync_plan_mode);
    let mut cursor = 0;
    let mut streamed = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut answered = std::collections::HashSet::new();
    let mut pending: Option<(String, tokio::sync::oneshot::Receiver<String>)> = None;
    let mut plain_pending: Option<(String, String)> = None;
    let mut side: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>,
    > = None;
    let mut cancelled = false;
    let mut notified = std::time::Instant::now();
    let mut notification_error = None;
    let mut final_mode_synced = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (job, next, events) = jobs::events_since(root, id, owner, cursor)?;
                let drained = cursor == next;
                cursor = next;
                let finished_mode = job.finished_ms.is_some() && matches!(job.status.as_str(), "completed" | "failed" | "cancelled");
                let refresh_mode = sync_mode && job.session_id == session.id
                    && (events.iter().any(permission_event) || (finished_mode && !final_mode_synced));
                if refresh_mode {
                    match refresh_permission_mode(session, &job).await {
                        Ok(mode) => { display.set_permission_mode(mode); final_mode_synced = finished_mode; },
                        Err(error) => display.notice(&format!("Could not refresh permission mode; keeping last confirmed mode: {error:#}")),
                    }
                }
                for event in events {
                    if event["type"] == "text" { streamed = true; }
                    if event["type"] == "result" {
                        if let Some(text)=result_text(&event,streamed) {display.before_text(text);}
                        streamed = false;
                    }
                    render(display, &event);
                }
                let question = jobs::pending_question(root, id, owner)?;
                let current = question.as_ref().map(|question| question.request_id.as_str());
                if pending.as_ref().is_some_and(|(request, _)| Some(request.as_str()) != current) {
                    // Closing this receiver tells the live renderer to discard
                    // only its answer draft and restore the queued prompt draft.
                    pending = None;
                    display.set_activity("Working");
                }
                if plain_pending.as_ref().is_some_and(|(request, _)| Some(request.as_str()) != current) {
                    plain_pending = None;
                    if let Some(reader) = input.as_mut() { reader.dismiss_answer().await?; }
                    display.set_activity("Working");
                }
                if pending.is_none() && plain_pending.is_none() {
                    if let Some(question) = question.filter(|question| !answered.contains(&question.request_id)) {
                        if let Some(live) = &display.live {
                            pending = Some((question.request_id, live.question(question.question)));
                        } else if input.is_some() {
                            plain_pending = Some((question.request_id, question.question));
                        }
                    }
                }
                if drained && !matches!(job.status.as_str(), "starting"|"running") {
                    if job.session_id == owner && matches!(job.status.as_str(),"completed"|"failed"|"cancelled") {jobs::acknowledge_foreground(root,id,owner)?;}
                    ensure!(job.status == "completed", "Worker {}: {}{}", id, job.status, job.error.map(|error|format!(" - {error}")).unwrap_or_default());
                    if side.is_none() { return Ok(false); }
                    display.set_activity("Main task complete - finishing side question");
                }
                if notified.elapsed() >= std::time::Duration::from_secs(1) {
                    match crate::background_commands::collect_completed(session) {
                        Ok(notices) => {
                            notification_error = None;
                            for notice in notices { display.notice(&notice); }
                        },
                        Err(error) => {
                            // Other jobs' notification state is not this turn's failure.
                            let diagnostic = format!("Background notifications unavailable: {error:#}");
                            if notification_error.as_ref() != Some(&diagnostic) {
                                display.notice(&diagnostic);
                                notification_error = Some(diagnostic);
                            }
                        }
                    }
                    notified = std::time::Instant::now();
                }
            },
            answer = async { match pending.as_mut() { Some((_, answer)) => answer.await, None => std::future::pending().await } } => {
                let (request, _) = pending.take().unwrap();
                answer_if_current(root, id, owner, &request, &answer?)?;
                answered.insert(request);
                display.set_activity("Working");
            },
            answer = async {
                if let (Some((_, question)), Some(reader)) = (plain_pending.as_ref(), input.as_mut()) {
                    reader.read_answer(question).await
                } else { std::future::pending().await }
            } => {
                let (request, _) = plain_pending.take().unwrap();
                if let Some(answer) = answer? { answer_if_current(root, id, owner, &request, &answer)?; }
                answered.insert(request);
                display.set_activity("Working");
            },
            action = display.action() => {
                match action {
                    Some(Action::CyclePermission) => {
                        let change = async {
                            let job = jobs::get(root, id)?;
                            ensure!(job.session_id == session.id, "Permission cycling applies to your current session; attach that session directly to change its mode");
                            let mode = session.next_permission_mode();
                            claude_core::services::session_worker::set_permission_mode(root, &session.id, id, mode).await?;
                            session.permissions.set_mode(claude_core::services::session_permission_mode::parse(mode)?);
                            Ok::<_, anyhow::Error>(mode)
                        }.await;
                        match change {
                            Ok(mode) => display.set_permission_mode(mode),
                            Err(error) => display.notice(&format!("Permission mode unchanged: {error:#}")),
                        }
                    },
                    Some(Action::Detach) => {
                        jobs::detach(root, id, owner)?;
                        display.notice(&format!("Task {id} continues in the background without restarting its request. Use /tasks attach {id} to return."));
                        if jobs::get(root, id)?.session_id == session.id {
                            display.notice("This session is occupied until completion; /clear starts a new session.");
                        }
                        return Ok(true);
                    },
                    Some(Action::Cancel) | None => {
                        if !cancelled { jobs::cancel(root, id)?; cancelled = true; display.set_activity("Cancelling worker"); }
                    },
                    Some(Action::SideQuestion(question)) => {
                        if side.is_none() { side=Some(Box::pin(async move { crate::background_commands::side_question(session, &question).await })); }
                        else { display.notice("A side question is already running."); }
                    },
                    Some(Action::Submit(prompt)) => {
                        if jobs::get(root, id)?.session_id == session.id {
                            if let Some(message) = crate::session_commands::live_goal_command(session, &prompt)? {
                                display.notice(&message);
                            } else {
                                if let Some(goal) = claude_core::services::session_goals::active(&session.store.db, &session.id)?.filter(|g|g.state == "waiting") {
                                    claude_core::services::session_goals::pause(&session.store.db, &session.id, &goal.id, "New user input interrupted the retry delay")?;
                                    jobs::cancel(root, id)?;
                                    cancelled = true;
                                }
                                restore(input, Action::Submit(prompt));
                            }
                        } else { restore(input, Action::Submit(prompt)); }
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

fn answer_if_current(root: &std::path::Path, id: &str, owner: &str, request: &str, answer: &str) -> Result<()> {
    if jobs::pending_question(root, id, owner)?.is_some_and(|question| question.request_id == request) {
        if let Err(error) = jobs::answer_question(root, id, owner, request, answer) {
            // The hook can claim between the read and atomic answer reservation.
            if jobs::pending_question(root, id, owner)?.is_some_and(|question| question.request_id == request) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn may_sync_permission_mode(
    current: &str,
    job_session: &str,
    initial_status: &str,
    foreground: bool,
) -> bool {
    current == job_session
        && (foreground || matches!(initial_status, "starting" | "running" | "unresponsive"))
}

#[test]
fn historical_attachment_does_not_refresh_permissions_but_live_attachment_can() {
    assert!(!may_sync_permission_mode(
        "session",
        "session",
        "completed",
        false
    ));
    assert!(!may_sync_permission_mode(
        "session", "session", "failed", false
    ));
    assert!(!may_sync_permission_mode(
        "session", "child", "running", false
    ));
    assert!(may_sync_permission_mode(
        "session", "session", "running", false
    ));
    assert!(may_sync_permission_mode(
        "session",
        "session",
        "completed",
        true
    ));
    let event = serde_json::json!({"type":"tool_result","name":"EnterPlanMode","output":r#"{"mode":"plan"}"#,"is_error":false});
    assert!(permission_event(&event)); // Trigger a fresh worker query, never apply this old payload.
}

fn permission_event(event: &Value) -> bool {
    event["type"] == "permission_mode" || plan_event_mode(event).is_some()
}

async fn refresh_permission_mode(session: &Session, job: &jobs::Job) -> Result<&'static str> {
    refresh_permission_mode_with_query(
        session,
        job,
        claude_core::services::session_worker::permission_mode(
            &session.paths.data_dir,
            &session.id,
            &job.id,
        ),
    )
    .await
}

async fn refresh_permission_mode_with_query(
    session: &Session,
    job: &jobs::Job,
    query: impl std::future::Future<Output = Result<String>>,
) -> Result<&'static str> {
    use claude_core::services::session_permission_mode;
    ensure!(
        job.session_id == session.id,
        "Permission mode belongs to another session"
    );
    let terminal = |job: &jobs::Job| {
        job.finished_ms.is_some()
            && matches!(job.status.as_str(), "completed" | "failed" | "cancelled")
    };
    let persisted = || {
        session_permission_mode::load(&session.store.db, &session.id)?
            .ok_or_else(|| anyhow::anyhow!("Final permission mode unavailable"))
    };
    // Historical events only trigger a query; their old mode is never applied.
    let mode = if terminal(job) {
        persisted()?
    } else {
        match query.await {
            Ok(mode) => session_permission_mode::parse(&mode)?,
            Err(error) => {
                // The worker can finish after the poll reads a running snapshot.
                // Only a fresh terminal record for this session authorizes the
                // persisted fallback; active IPC failures remain visible.
                let finished = jobs::get(&session.paths.data_dir, &job.id)
                    .is_ok_and(|latest| latest.session_id == session.id && terminal(&latest));
                if !finished {
                    return Err(error);
                }
                persisted()?
            }
        }
    };
    session.permissions.set_mode(mode);
    Ok(session.permission_mode())
}


fn plan_event_mode(event: &Value) -> Option<claude_core::permissions::PermissionMode> {
    use claude_core::permissions::PermissionMode;
    if event["type"] != "tool_result" || event["is_error"] != false {
        return None;
    }
    let output: Value = serde_json::from_str(event["output"].as_str()?).ok()?;
    match event["name"].as_str()? {
        "EnterPlanMode" | "plan_mode" if output["mode"] == "plan" => Some(PermissionMode::PlanMode),
        "ExitPlanMode" | "plan_mode" if output["approved"] == true && output["mode"] == "ask" => {
            Some(PermissionMode::AskPermissions)
        }
        "ExitPlanMode" | "plan_mode" if output["approved"] == true && output["mode"] == "accept_edits" => Some(PermissionMode::AcceptEdits),
        _ => None,
    }
}

#[test]
fn plan_worker_events_only_apply_successful_transitions() {
    use claude_core::permissions::PermissionMode;
    use serde_json::json;
    let event = |name, output, failed| json!({"type":"tool_result","name":name,"output":output,"is_error":failed});
    assert_eq!(
        plan_event_mode(&event("EnterPlanMode", r#"{"mode":"plan"}"#, false)),
        Some(PermissionMode::PlanMode)
    );
    assert_eq!(
        plan_event_mode(&event(
            "ExitPlanMode",
            r#"{"approved":true,"mode":"ask"}"#,
            false
        )),
        Some(PermissionMode::AskPermissions)
    );
    for (name, output, failed) in [
        ("ExitPlanMode", r#"{"approved":true,"mode":"ask"}"#, true),
        ("ExitPlanMode", r#"{"mode":"ask"}"#, false),
        ("Read", r#"{"approved":true,"mode":"ask"}"#, false),
    ] {
        assert_eq!(plan_event_mode(&event(name, output, failed)), None);
    }
}

fn render(display: &mut TurnDisplay, event: &Value) {
    let text = |name: &str| event[name].as_str().unwrap_or("");
    match text("type") {
        "notice" => display.notice(text("message")),
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
            &event["input"],
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
        if http.is_none()
            && (lower.contains("output token limit")
                || (lower.contains("max_tokens")
                    && (lower.contains("stopped")
                        || lower.contains("stop_reason")
                        || lower.contains("finish_reason"))))
        {
            // Static text only: provider diagnostics can contain credentials.
            // Do not append an endpoint or truncate the task inspection command.
            return format!(
                "Output token limit reached; send a follow-up to continue. Details: /tasks show {id}"
            );
        }
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
    #[tokio::test]
    async fn attachment_checks_owner_and_reads_completed_job_without_resubmission() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = claude_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = claude_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let child = store.create(temp.path(), "fixture", "fixture").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let job = jobs::Job {
            id: id.clone(),
            foreground: false,
            parent_session_id: parent.clone(),
            session_id: child.clone(),
            title: "already completed".into(),
            status: "completed".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: 2,
            finished_ms: Some(2),
            exit_code: Some(0),
            error: None,
        };
        assert!(authorize_attachment(&job, &parent).is_ok());
        assert!(authorize_attachment(&job, &child).is_ok());
        assert!(authorize_attachment(&job, "unrelated").is_err());
        let directory = paths.data_dir.join("background-tasks").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let state = serde_json::to_vec(&job).unwrap();
        std::fs::write(directory.join("state.json"), &state).unwrap();
        let events = concat!(
            "{\"type\":\"thinking\",\"delta\":\"thought\"}\n",
            "{\"type\":\"text\",\"delta\":\"existing answer\"}\n",
            "{\"type\":\"result\",\"result\":\"existing answer\"}\n"
        );
        std::fs::write(directory.join("output.jsonl"), events).unwrap();
        let cli = Cli::try_parse_from(["claude-code"]).unwrap();
        for session_id in [&parent, &child] {
            let mut provider = claude_core::runtime::provider_from_config(
                &crate::config::provider_template("fixture"),
            );
            provider.id = "fixture".into();
            provider.base_url = "http://127.0.0.1:1".into();
            let session = Session::open(
                &cli,
                &paths,
                crate::config::Selection {
                    providers: vec![provider],
                    model: "fixture".into(),
                    provider_id: "fixture".into(),
                },
                Some(session_id),
            )
            .await
            .unwrap();
            let before = session.store.history(session_id).unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                attach(&session, &id, &mut None),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(session.store.history(session_id).unwrap(), before);
        }
        assert_eq!(std::fs::read(directory.join("state.json")).unwrap(), state);
        assert_eq!(
            std::fs::read_to_string(directory.join("output.jsonl")).unwrap(),
            events
        );
        assert!(!directory.join("cancel").exists());
        assert!(!paths.data_dir.join("session-workers").exists());
    }

    #[tokio::test]
    async fn failed_observation_preserves_running_job_and_original_diagnostic() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = claude_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = claude_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let mut provider = claude_core::runtime::provider_from_config(
            &crate::config::provider_template("fixture"),
        );
        provider.id = "fixture".into();
        let cli = Cli::try_parse_from(["claude-code"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                model: "fixture".into(),
                provider_id: "fixture".into(),
            },
            Some(&parent),
        )
        .await
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("background-tasks").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: true,
            parent_session_id: parent.clone(),
            session_id: parent,
            title: "still working".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: chrono::Utc::now().timestamp_millis() as u64,
            finished_ms: None,
            exit_code: None,
            error: None,
        };
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        std::fs::write(directory.join("output.jsonl"), "broken-json\n").unwrap();
        let error = attach(&session, &id, &mut None).await.unwrap_err();
        assert!(error.to_string().contains("worker continues"));
        assert_eq!(jobs::get(&paths.data_dir, &id).unwrap().status, "running");
        assert!(!directory.join("cancel").exists());
        assert!(directory.join("detached").exists());
        assert!(std::fs::read_to_string(directory.join("stderr.log"))
            .unwrap()
            .contains("Invalid background stream JSON"));
    }

    #[tokio::test]
    async fn permission_refresh_recovers_completion_race_but_preserves_active_errors() {
        use clap::Parser;
        let temp = tempfile::tempdir().unwrap();
        let paths = claude_core::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = claude_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "fixture", "fixture").unwrap();
        let mut provider = claude_core::runtime::provider_from_config(
            &crate::config::provider_template("fixture"),
        );
        provider.id = "fixture".into();
        let cli = Cli::try_parse_from(["claude-code"]).unwrap();
        let session = Session::open(
            &cli,
            &paths,
            crate::config::Selection {
                providers: vec![provider],
                model: "fixture".into(),
                provider_id: "fixture".into(),
            },
            Some(&parent),
        )
        .await
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = paths.data_dir.join("background-tasks").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: true,
            parent_session_id: parent.clone(),
            session_id: parent,
            title: "still working".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: 1,
            heartbeat_ms: chrono::Utc::now().timestamp_millis() as u64,
            finished_ms: None,
            exit_code: None,
            error: None,
        };
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        use claude_core::{permissions::PermissionMode, services::session_permission_mode};
        // The poll already captured this running job. Change its durable state
        // inside the query future to deterministically reproduce the IPC race.
        let old = job.clone();
        for status in ["completed", "failed", "cancelled"] {
            session.permissions.set_mode(PermissionMode::PlanMode);
            let refreshed = refresh_permission_mode_with_query(&session, &old, async {
                session_permission_mode::set(
                    &session.store.db,
                    &session.id,
                    &session.permissions,
                    PermissionMode::AskPermissions,
                )?;
                session.permissions.set_mode(PermissionMode::PlanMode);
                let mut completed = old.clone();
                completed.status = status.into();
                completed.finished_ms = Some(3);
                std::fs::write(
                    directory.join("state.json"),
                    serde_json::to_vec(&completed)?,
                )?;
                anyhow::bail!("Session worker has no active initialized turn")
            })
            .await
            .unwrap();
            assert_eq!(refreshed, "ask");
        }
        // A still-running worker must not fall back to even a valid saved mode.
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        session.permissions.set_mode(PermissionMode::PlanMode);
        let error = refresh_permission_mode_with_query(&session, &old, async {
            anyhow::bail!("real active IPC failure")
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "real active IPC failure");
        assert_eq!(session.permission_mode(), "plan");
        // Terminal state belonging to another session is never authoritative.
        let mut foreign = old.clone();
        foreign.session_id = uuid::Uuid::new_v4().to_string();
        foreign.status = "completed".into();
        foreign.finished_ms = Some(3);
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        assert!(refresh_permission_mode_with_query(&session, &old, async {
            anyhow::bail!("wrong session")
        })
        .await
        .is_err());
        assert_eq!(session.permission_mode(), "plan");
    }

    #[test]
    fn failure_message_explains_output_limits_without_leaking_diagnostics() {
        let id = "94727627-adc5-474f-9650-8143c8a6741f";
        for diagnostic in [
            "Incomplete response: provider stopped at the output token limit (max_tokens). Partial output was retained; send a follow-up to continue.",
            "Provider stopped: MAX_TOKENS https://user:secret@example.com/private?key=secret",
            "Incomplete response: stop_reason=max_tokens; private response body",
            "Incomplete response: finish_reason=max_tokens",
            "Incomplete response: provider stopped at the output token limit (length). Partial output was retained; send a follow-up to continue.",
        ] {
            assert_eq!(
                failure_message(id, "failed", diagnostic),
                format!("Output token limit reached; send a follow-up to continue. Details: /tasks show {id}")
            );
        }
        for (status, diagnostic, expected) in [
            ("cancelled", "output token limit", "Interrupted"),
            (
                "failed",
                "output token limit request timeout",
                "Request timed out. Details: /tasks show id",
            ),
            (
                "failed",
                "API HTTP 400 invalid max_tokens",
                "HTTP 400. Details: /tasks show id",
            ),
            (
                "failed",
                "HTTP 429 output token limit",
                "HTTP 429. Details: /tasks show id",
            ),
            (
                "failed",
                "unknown private credential max_tokens",
                "Request failed. Details: /tasks show id",
            ),
        ] {
            assert_eq!(failure_message("id", status, diagnostic), expected);
        }
    }

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
