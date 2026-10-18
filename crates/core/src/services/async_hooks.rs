//! Process-local background command ownership. Results are never restored or replayed.
use anyhow::Result;
use futures::{future::BoxFuture, FutureExt};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tokio::task::JoinHandle;

struct Delivery {
    directory: std::path::PathBuf,
    lease: Option<std::fs::File>,
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.lease.take();
        // Only this randomly named, runtime-created task directory is removed.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
struct Task {
    handle: JoinHandle<Result<(i32, String, String)>>,
    delivery: std::sync::Arc<Delivery>,
}
impl Task {
    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
    fn abort(&self) {
        self.handle.abort();
    }
}
fn registry() -> &'static Mutex<HashMap<String, Vec<Task>>> {
    static TASKS: OnceLock<Mutex<HashMap<String, Vec<Task>>>> = OnceLock::new();
    TASKS.get_or_init(Mutex::default)
}
fn directory(root: &std::path::Path, session: &str) -> std::path::PathBuf {
    let digest = ring::digest::digest(&ring::digest::SHA256, session.as_bytes());
    let key: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    root.join("hook-async").join(key)
}

pub(crate) fn register(
    root: &std::path::Path,
    session: &str,
    task: BoxFuture<'static, Result<(i32, String, String)>>,
) -> Result<()> {
    let directory = directory(root, session).join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&directory)?;
    let lease = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(directory.join("owner"))?;
    lease.lock()?;
    let delivery = std::sync::Arc::new(Delivery {
        directory,
        lease: Some(lease),
    });
    let writer = delivery.clone();
    let handle = tokio::spawn(async move {
        let output = tokio::select! {
            output = task => output?,
            _ = async {
                loop {
                    if writer.directory.join("cancel").exists() { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            } => anyhow::bail!("Async hook cancelled"),
        };
        let messages = output_messages(&output.1);
        std::fs::write(
            writer.directory.join("pending"),
            serde_json::to_vec(&messages)?,
        )?;
        std::fs::rename(
            writer.directory.join("pending"),
            writer.directory.join("result"),
        )?;
        Ok(output)
    });
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session.into())
        .or_default()
        .push(Task { handle, delivery });
    Ok(())
}

/// Collect a live host's output from a worker process. The owner holds an OS
/// lease until consumption/close; stale files from a dead process never replay.
/// Atomic rename gives competing consumers at most one delivery.
pub fn drain_shared(root: &std::path::Path, session: &str) -> Vec<String> {
    drain_directory(&directory(root, session))
}
fn drain_directory(directory: &std::path::Path) -> Vec<String> {
    let mut messages = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return messages;
    };
    for entry in entries.flatten() {
        let task = entry.path();
        if task.join("cancel").exists() {
            continue;
        }
        let Ok(owner) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(task.join("owner"))
        else {
            continue;
        };
        if !matches!(owner.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
            continue;
        }
        let claimed = task.join(format!("claimed-{}", uuid::Uuid::new_v4()));
        if std::fs::rename(task.join("result"), &claimed).is_err() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&claimed) {
            if let Ok(result) = serde_json::from_slice::<Vec<String>>(&bytes) {
                messages.extend(result);
            }
        }
        let _ = std::fs::remove_file(claimed);
    }
    messages
}

/// Consume process-local finished results and reclaim their ephemeral leases.
/// Only informational strings cross this boundary; decisions and exit code 2
/// never alter execution authority. Malformed JSON lines/fields are ignored.
pub fn drain(session: &str) -> Vec<String> {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    let Some(tasks) = registry.get_mut(session) else {
        return Vec::new();
    };
    let directories: std::collections::HashSet<_> = tasks
        .iter()
        .filter_map(|t| {
            t.delivery
                .directory
                .parent()
                .map(std::path::Path::to_path_buf)
        })
        .collect();
    let messages = directories
        .iter()
        .flat_map(|path| drain_directory(path))
        .collect();
    let mut pending = Vec::new();
    for task in tasks.drain(..) {
        if !task.is_finished() {
            pending.push(task);
            continue;
        }
        if let Some(Ok(Err(error))) = task.handle.now_or_never() {
            tracing::warn!("Async hook: {error:#}");
        }
    }
    *tasks = pending;
    if tasks.is_empty() {
        registry.remove(session);
    }
    messages
}

/// Notify other process owners as well as cancelling this process's tasks.
pub async fn cancel_shared(root: &std::path::Path, session: &str) {
    signal_cancel(root, session);
    cancel(session).await;
}

pub fn signal_cancel(root: &std::path::Path, session: &str) {
    if let Ok(entries) = std::fs::read_dir(directory(root, session)) {
        for entry in entries.flatten() {
            let _ = std::fs::write(entry.path().join("cancel"), "");
        }
    }
}

fn output_messages(stdout: &str) -> Vec<String> {
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
    {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !value.is_object() || value.get("async").is_some() {
            continue;
        }
        return [
            value.get("systemMessage"),
            value
                .get("hookSpecificOutput")
                .and_then(|v| v.get("additionalContext")),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect();
    }
    Vec::new()
}

/// Abort all children and wait until their process-tree and progress guards drop.
pub async fn cancel(session: &str) {
    let tasks = take_cancelled(session);
    for task in tasks {
        let _ = task.handle.await;
    }
}

fn take_cancelled(session: &str) -> Vec<Task> {
    let tasks = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session)
        .unwrap_or_default();
    for task in &tasks {
        task.abort();
    }
    tasks
}

/// A dropped model-loop future must also cancel its background hooks. A normal
/// completed turn keeps them alive for delivery on a later user/model turn.
pub(crate) struct TurnGuard(pub Option<String>);
impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Some(session) = &self.0 {
            let registry = registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(tasks) = registry.get(session) {
                for task in tasks {
                    task.abort();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tool_hooks::{HookEvent, HookRunner};
    use serde_json::json;
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };

    fn fixture(root: &Path, timeout: u64) -> HookRunner {
        std::fs::write(root.join("hook.cjs"), r#"
const fs = require('fs');
const {spawn} = require('child_process');
let input=''; process.stdin.on('data',d=>input+=d);
process.stdin.on('end',()=>{
 const session=JSON.parse(input).session_id;
 const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
 fs.writeFileSync(session+'.pid',JSON.stringify([process.pid,child.pid]));
 fs.appendFileSync(session+'.runs','x');
 const timer=setInterval(()=>{
  if(!fs.existsSync(session+'.release')) return;
  clearInterval(timer);
  console.log('{invalid'); console.log('{"async":true}');
  console.log(JSON.stringify({systemMessage:'notice',continue:false,decision:'block',hookSpecificOutput:{hookEventName:'PreToolUse',permissionDecision:'allow',additionalContext:'context'}}));
  process.exit(2);
 },20);
});
"#).unwrap();
        std::fs::write(root.join("settings.json"),json!({"hooks":{"PreToolUse":[{"matcher":"^Read$","hooks":[{"type":"command","command":"node hook.cjs","async":true,"once":true,"if":"Read","timeout":timeout,"statusMessage":"Background fixture"}]}]}}).to_string()).unwrap();
        HookRunner::load(root, root).unwrap()
    }

    async fn wait_for(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    fn alive(pid: u32) -> bool {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::{Foundation::CloseHandle, System::Threading::*};
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut code = 0;
            let ok = GetExitCodeProcess(handle, &mut code).is_ok();
            let _ = CloseHandle(handle);
            ok && code == 259
        }
        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        }
    }

    async fn run(runner: &HookRunner, session: &str) {
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            runner.run(
                HookEvent::PreToolUse,
                session,
                json!({"tool_name":"Read","tool_input":{}}),
            ),
        )
        .await
        .expect("async hook blocked main flow")
        .unwrap();
        assert!(!outcome.blocked && !outcome.allow_permission && !outcome.prevent_continuation);
        assert!(outcome.additional_context.is_empty());
    }

    #[tokio::test]
    async fn nonblocking_once_delivery_isolation_and_restore() {
        let root = tempfile::tempdir().unwrap();
        let runner = fixture(root.path(), 30);
        let session = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        runner
            .run(
                HookEvent::PreToolUse,
                &session,
                json!({"tool_name":"Write"}),
            )
            .await
            .unwrap();
        assert!(registry().lock().unwrap().get(&session).is_none());
        run(&runner, &session).await;
        wait_for(|| root.path().join(format!("{session}.pid")).exists()).await;
        let pids: Vec<u32> = serde_json::from_str(
            &std::fs::read_to_string(root.path().join(format!("{session}.pid"))).unwrap(),
        )
        .unwrap();
        assert!(pids.iter().all(|pid| alive(*pid)));
        assert!(drain(&session).is_empty());
        run(&runner, &session).await;
        assert_eq!(
            std::fs::read_to_string(root.path().join(format!("{session}.runs"))).unwrap(),
            "x"
        );
        assert!(!crate::services::hook_progress::subscribe(&session)
            .snapshot()
            .is_empty());
        std::fs::write(root.path().join(format!("{session}.release")), "").unwrap();
        wait_for(|| {
            registry()
                .lock()
                .unwrap()
                .get(&session)
                .unwrap()
                .iter()
                .all(Task::is_finished)
        })
        .await;
        assert!(drain(&other).is_empty());
        assert_eq!(drain(&session), vec!["notice", "context"]);
        assert!(drain(&session).is_empty());
        wait_for(|| pids.iter().all(|pid| !alive(*pid))).await;
        assert!(crate::services::hook_progress::subscribe(&session)
            .snapshot()
            .is_empty());
        run(
            &HookRunner::load(root.path(), root.path()).unwrap(),
            &session,
        )
        .await;
        assert!(drain(&session).is_empty());
        assert_eq!(
            std::fs::read_to_string(root.path().join(format!("{session}.runs"))).unwrap(),
            "x"
        );
        run(&runner, &other).await;
        wait_for(|| root.path().join(format!("{other}.pid")).exists()).await;
        cancel(&other).await;
    }

    #[tokio::test]
    async fn timeout_cancel_close_and_dropped_turn_kill_process_tree() {
        for mode in ["timeout", "cancel", "close", "drop", "signal"] {
            let root = tempfile::tempdir().unwrap();
            let runner = fixture(root.path(), if mode == "timeout" { 2 } else { 30 });
            let session = uuid::Uuid::new_v4().to_string();
            run(&runner, &session).await;
            let path = root.path().join(format!("{session}.pid"));
            wait_for(|| path.exists()).await;
            let pids: Vec<u32> =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(pids.iter().all(|pid| alive(*pid)));
            match mode {
                "timeout" => {
                    wait_for(|| {
                        registry()
                            .lock()
                            .unwrap()
                            .get(&session)
                            .unwrap()
                            .iter()
                            .all(Task::is_finished)
                    })
                    .await
                }
                "drop" => drop(TurnGuard(Some(session.clone()))),
                "signal" => signal_cancel(root.path(), &session),
                "close" => {
                    let context = crate::services::ServiceContext::open(
                        crate::runtime::RuntimePaths::new(root.path().join("data")).unwrap(),
                        std::sync::Arc::new(crate::services::EnvironmentCredentials),
                    )
                    .unwrap();
                    crate::services::session_hooks::end(
                        &context,
                        root.path(),
                        &session,
                        "test",
                        Duration::from_secs(2),
                    )
                    .await;
                }
                _ => cancel(&session).await,
            }
            wait_for(|| pids.iter().all(|pid| !alive(*pid))).await;
            cancel(&session).await;
            assert!(drain(&session).is_empty());
            assert!(crate::services::hook_progress::subscribe(&session)
                .snapshot()
                .is_empty());
            run(
                &HookRunner::load(root.path(), root.path()).unwrap(),
                &session,
            )
            .await;
            assert_eq!(
                std::fs::read_to_string(root.path().join(format!("{session}.runs"))).unwrap(),
                "x"
            );
        }
    }

    #[test]
    fn invalid_output_and_decisions_are_not_delivered() {
        for text in [
            "plain text",
            "{bad",
            r#"{"systemMessage":123,"hookSpecificOutput":{"additionalContext":[]}}"#,
            r#"{"decision":"approve","continue":false,"hookSpecificOutput":{"permissionDecision":"deny","updatedInput":{}}}"#,
        ] {
            assert!(output_messages(text).is_empty());
        }
        assert_eq!(output_messages("{bad\n{\"async\":true}\n{\"systemMessage\":\"first\"}\n{\"systemMessage\":\"second\"}"),vec!["first"]);
    }

    #[tokio::test]
    #[ignore = "subprocess helper for process_exit_kills_background_tree"]
    async fn process_exit_helper() {
        let root = PathBuf::from(std::env::var_os("CCL_ASYNC_EXIT_ROOT").unwrap());
        run(&HookRunner::load(&root, &root).unwrap(), "exit-test").await;
        std::future::pending::<()>().await;
    }

    #[test]
    #[ignore = "subprocess consumer for shared_delivery_is_once_and_requires_live_owner"]
    fn shared_delivery_helper() {
        let root = PathBuf::from(std::env::var_os("CCL_ASYNC_SHARED_ROOT").unwrap());
        let session = std::env::var("CCL_ASYNC_SHARED_SESSION").unwrap();
        std::fs::write(
            root.join("consumed.json"),
            serde_json::to_vec(&drain_shared(&root, &session)).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn shared_delivery_is_once_and_requires_live_owner() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let runner = fixture(root.path(), 30);
        run(&runner, &session).await;
        std::fs::write(root.path().join(format!("{session}.release")), "").unwrap();
        wait_for(|| {
            registry()
                .lock()
                .unwrap()
                .get(&session)
                .unwrap()
                .iter()
                .all(Task::is_finished)
        })
        .await;
        let consume = || {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "services::async_hooks::tests::shared_delivery_helper"
                ])
                .env("CCL_ASYNC_SHARED_ROOT", root.path())
                .env("CCL_ASYNC_SHARED_SESSION", &session)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success());
            serde_json::from_slice::<Vec<String>>(
                &std::fs::read(root.path().join("consumed.json")).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(consume(), vec!["notice", "context"]);
        assert!(consume().is_empty());
        assert!(drain(&session).is_empty());
        let stale = directory(root.path(), &session).join("dead-owner");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("owner"), "").unwrap();
        std::fs::write(stale.join("result"), r#"["must not replay"]"#).unwrap();
        assert!(consume().is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn process_exit_kills_background_tree() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), 30);
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "services::async_hooks::tests::process_exit_helper",
            ])
            .env("CCL_ASYNC_EXIT_ROOT", root.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let path = root.path().join("exit-test.pid");
        wait_for(|| path.exists()).await;
        let pids: Vec<u32> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(pids.iter().all(|pid| alive(*pid)));
        process.kill().unwrap();
        process.wait().unwrap();
        wait_for(|| pids.iter().all(|pid| !alive(*pid))).await;
        assert!(drain("exit-test").is_empty());
    }
}
