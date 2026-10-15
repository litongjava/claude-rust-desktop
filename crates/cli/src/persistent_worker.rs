//! CLI engine adapter for the persistent, local-IPC session worker.
use crate::{
    args::{Cli, PermissionModeArg},
    session::Session,
};
use anyhow::{ensure, Context, Result};
use clap::Parser;
use claude_core::{
    runtime::RuntimePaths,
    services::{
        background_sessions as jobs,
        session_worker::{self as service, Reply, Request, Worker},
    },
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub async fn run_from_env() -> Result<bool> {
    let Some(value) = std::env::var_os(service::HOST_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(service::HOST_ENV);
    let dir = PathBuf::from(value);
    let worker = service::read_worker(&dir)?;
    ensure!(
        service::directory(&worker).canonicalize()? == dir.canonicalize()?,
        "Invalid session host directory"
    );
    let mut listener = service::Listener::bind(&worker)?;
    let session = Arc::new(tokio::sync::Mutex::new(None::<Session>));
    let active = Arc::new(AtomicBool::new(false));
    let fatal = Arc::new(AtomicBool::new(false));
    let mut idle = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _=tick.tick()=>{
                if service::read_worker(&dir).map_or(true, |owner| owner.status == "stopped" || chrono::Utc::now().timestamp_millis().max(0) as u64 > owner.heartbeat_ms.saturating_add(10_000)) {
                    fatal.store(true,Ordering::Release);
                    if let Ok(owner)=service::read_worker(&dir) { if let Some(id)=owner.active_job { let _=jobs::cancel(&worker.root,&id); } }
                }
                if active.load(Ordering::Acquire) {idle=Instant::now();}
                else if fatal.load(Ordering::Acquire) || idle.elapsed()>Duration::from_secs(300) {break;}
            },
            stream=listener.accept()=>{
                let stream=stream?;
                let parsed=tokio::time::timeout(Duration::from_secs(5),service::read_request(stream)).await;
                let Ok(Ok((stream,request)))=parsed else{continue};
                if matches!(&request,Request::Submit{..}|Request::Shutdown) {
                        // The supervisor publishes completion just before the host
                        // acknowledges it. Briefly drain that handoff, never queue
                        // a request behind a genuinely running turn.
                        if active.load(Ordering::Acquire) {
                            let finished=std::fs::read(dir.join("active.json")).ok()
                                .and_then(|bytes|serde_json::from_slice::<String>(&bytes).ok())
                                .and_then(|id|jobs::get(&worker.root,&id).ok())
                                .is_some_and(|job|job.finished_ms.is_some());
                            if finished {for _ in 0..25 {if !active.load(Ordering::Acquire){break;}tokio::time::sleep(Duration::from_millis(10)).await;}}
                        }
                }
                match request {
                    Request::Ping=>{let _=service::reply(stream,&Reply::ok(None)).await;},
                    Request::Shutdown=>{
                        if active.load(Ordering::Acquire) {let _=service::reply(stream,&Reply::error("Session worker has an active turn; cancel or detach it explicitly")).await;}
                        else {let _=service::reply(stream,&Reply::ok(None)).await;break;}
                    },
                    Request::Submit{id,prompt,arguments,environment}=>{
                        if fatal.load(Ordering::Acquire) {let _=service::reply(stream,&Reply::error("Session worker is shutting down; prompt was not submitted")).await;continue;}
                        if active.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() {
                            let _=service::reply(stream,&Reply::error("Session worker is busy; prompt was not submitted")).await;continue;
                        }
                        if let Err(error)=service::begin_job(&worker,&id,&prompt) {
                            active.store(false,Ordering::Release);let _=service::reply(stream,&Reply::error(error)).await;continue;
                        }
                        let worker=worker.clone();let sessions=session.clone();let active=active.clone();let fatal=fatal.clone();let turn_id=id.clone();
                        tokio::spawn(async move {
                            let task_worker=worker.clone();let task_id=turn_id.clone();
                            let result=tokio::spawn(async move {execute(&task_worker,&task_id,arguments,environment,sessions).await}).await;
                            let error=match result {Ok(result)=>result.err().map(|error|format!("{error:#}")),Err(error)=>{fatal.store(true,Ordering::Release);Some(format!("Session turn panicked: {error}"))}};
                            if let Some(error)=&error {let _=service::emit_event(&serde_json::json!({"type":"error","error":error}));}
                            if service::finish_job(&worker,&turn_id,error).await.is_err() {fatal.store(true,Ordering::Release);}
                            service::set_event_sink(None);
                            std::env::remove_var(jobs::CONTROL_ENV);
                            active.store(false,Ordering::Release);
                        });
                        // A lost acknowledgement never causes replay: the job ID is durable.
                        let _=service::reply(stream,&Reply::ok(Some(id))).await;
                        idle=Instant::now();
                    },
                }
            }
        }
    }
    if let Some(session) = session.lock().await.take() {
        session.close().await?;
    }
    Ok(true)
}

async fn execute(
    worker: &Worker,
    id: &str,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    sessions: Arc<tokio::sync::Mutex<Option<Session>>>,
) -> Result<()> {
    let dir = worker.root.join("background-tasks").join(id);
    service::set_event_sink(Some(dir.join("output.jsonl")));
    std::env::set_var(jobs::CONTROL_ENV, &dir);
    // These are the only settings accepted from the same-user IPC frontend.
    for name in [
        "CLAUDE_RUST_CHILD_API_KEY",
        "CLAUDE_RUST_WORKER_AUTO_ROUTE",
        "CLAUDE_RUST_WORKER_MODEL",
    ] {
        std::env::remove_var(name);
    }
    for (name, value) in environment {
        ensure!(
            [
                "CLAUDE_RUST_CHILD_API_KEY",
                "CLAUDE_RUST_WORKER_AUTO_ROUTE",
                "CLAUDE_RUST_WORKER_MODEL"
            ]
            .contains(&name.as_str()),
            "Unsupported worker environment setting"
        );
        std::env::set_var(name, value);
    }
    let mut cli = Cli::try_parse_from(std::iter::once("claude-code".to_owned()).chain(arguments))?;
    cli.normalize();
    ensure!(
        cli.resume.as_deref() == Some(worker.session_id.as_str()) && cli.command.is_none(),
        "Worker request must resume its own session without a subcommand"
    );
    let paths = RuntimePaths::new(worker.root.clone())?;
    let mut selection =
        crate::config::select(&cli, &paths, cli.model.as_deref(), cli.provider.as_deref())?;
    if let Ok(value) = std::env::var("CLAUDE_RUST_WORKER_MODEL") {
        let model: claude_core::native_engine::provider_manager::ModelConfig =
            serde_json::from_str(&value)?;
        let provider = selection
            .providers
            .iter_mut()
            .find(|provider| provider.id == selection.provider_id)
            .context("Worker provider missing")?;
        let expected=claude_core::native_engine::provider_manager::resolve_model_selection(&provider.models,&selection.model).context("Worker selected model missing")?;
        ensure!(model.id==expected.id,"Worker model snapshot mismatch");
        let target = provider
            .models
            .iter_mut()
            .find(|item| item.id == model.id)
            .context("Worker model missing")?;
        *target = model;
    }
    let mut current = sessions.lock().await;
    if let Some(session) = current.as_mut() {
        session.switch(&paths, selection).await?;
        session.auto_route =
            std::env::var("CLAUDE_RUST_WORKER_AUTO_ROUTE").as_deref() == Ok("true");
        session
            .set_permission_mode(match cli.permission_mode {
                PermissionModeArg::Ask => "ask",
                PermissionModeArg::AcceptEdits => "accept-edits",
                PermissionModeArg::Plan => "plan",
                PermissionModeArg::Bypass => "bypass",
            })
            .await?;
    } else {
        *current = Some(Session::open(&cli, &paths, selection, Some(&worker.session_id)).await?);
    }
    let prompt = cli.prompt.as_deref().context("Worker prompt missing")?;
    current
        .as_ref()
        .unwrap()
        .turn(&cli, &prompt, &mut None)
        .await
}
