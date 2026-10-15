//! Managed shell tasks share durable session task records and explicit cancellation.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;
tokio::task_local! { static CONTEXT: Option<(PathBuf, String)>; }

pub(crate) async fn scope<F: Future>(context: Option<(PathBuf, String)>, future: F) -> F::Output {
    CONTEXT.scope(context, future).await
}

struct Completion {
    root: PathBuf,
    session: String,
    id: String,
    done: bool,
}
impl Completion {
    fn finish(&mut self, status: &str, output: Value) {
        let result = super::tasks::execute_for_session(
            "TaskUpdate",
            json!({"id":self.id,"status":status,"output":output.to_string()}),
            &self.root,
            &self.session,
        );
        self.done = result.is_ok();
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.done {
            self.finish(
                "cancelled",
                json!({"is_error":true,"error":"Shell owner exited or task was cancelled"}),
            );
        }
    }
}

pub(super) fn start(
    mut command: Command,
    timeout: Option<Duration>,
    shell: &str,
    description: &str,
) -> Result<Value> {
    let (root, session) = CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .context("Background Bash requires an owning engine session")?;
    let task = super::tasks::execute_for_session(
        "TaskCreate",
        json!({"subject":description,"metadata":{"managed_shell":true}}),
        &root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    let directory = root.join("shell-tasks").join(&session).join(&id);
    std::fs::create_dir_all(&directory)?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");
    super::tasks::execute_for_session(
        "TaskUpdate",
        json!({"id":id,"status":"running"}),
        &root,
        &session,
    )?;
    std::fs::write(directory.join("heartbeat"), b"running")?;
    let shell = shell.to_string();
    let logs = (stdout.clone(), stderr.clone());
    let mut completion = Completion {
        root,
        session,
        id: id.clone(),
        done: false,
    };
    tokio::spawn(async move {
        let result = tokio::select! {
            result = super::bash_process::run_logged(&mut command, timeout, &shell, Some(logs)) => result,
            _ = async { let mut heartbeat = std::time::Instant::now(); loop { if directory.join("cancel").exists() { break; } if heartbeat.elapsed() >= Duration::from_secs(1) { let _ = std::fs::write(directory.join("heartbeat"), b"running"); heartbeat = std::time::Instant::now(); } tokio::time::sleep(Duration::from_millis(50)).await; } } => {
                completion.finish("cancelled", json!({"is_error":true,"cancelled":true}));
                return;
            }
        };
        let output =
            result.unwrap_or_else(|error| json!({"is_error":true,"error":format!("{error:#}")}));
        let status = if output["success"] == true {
            "completed"
        } else {
            "failed"
        };
        completion.finish(status, output);
    });
    Ok(
        json!({"task_id":id,"status":"running","stdout_file":stdout,"stderr_file":stderr,"message":"Managed background command started. Use TaskOutput with task_id for current logs, TaskStop to terminate it. It runs until completion, timeout, cancellation, or worker exit."}),
    )
}

pub(super) fn control(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
) -> Result<Option<Value>> {
    if !matches!(name, "TaskOutput" | "TaskStop") {
        return Ok(None);
    }
    anyhow::ensure!(input.get("block").is_none() && input.get("timeout").is_none(), "TaskOutput/TaskStop do not support block or timeout; use nonblocking TaskOutput to inspect current state");
    let id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .context("task_id is required")?;
    let task = super::tasks::execute_for_session("TaskGet", json!({"id":id}), root, session)?;
    if task["metadata"]["managed_shell"] != true {
        return Ok(None);
    }
    let id = uuid::Uuid::parse_str(id)?.to_string();
    let session = uuid::Uuid::parse_str(session)?;
    let directory = root
        .join("shell-tasks")
        .join(session.to_string())
        .join(id.to_string());
    let fresh = std::fs::metadata(directory.join("heartbeat"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(10));
    let status = if task["status"] == "running" && !fresh {
        json!("unresponsive")
    } else {
        task["status"].clone()
    };
    if name == "TaskStop" {
        let active = task["status"] == "running";
        if active {
            std::fs::write(directory.join("cancel"), b"cancel")?;
        }
        return Ok(Some(
            json!({"task_id":id,"status":status,"cancellation_requested":active,"message":"Cancellation is requested, not confirmed. TaskOutput reports terminal status after the owner terminates the shell process tree; an unresponsive owner may not have received it yet."}),
        ));
    }
    let read = |name| -> Result<String> {
        use std::io::Read;
        match std::fs::File::open(directory.join(name)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(1024 * 1024).read_to_end(&mut bytes)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    };
    Ok(Some(
        json!({"task_id":id,"status":status,"stdout":read("stdout.log")?,"stderr":read("stderr.log")?,"result":task["output"],"block":false}),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unresponsive_owner_still_receives_cancel_and_unsupported_wait_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_session(
            "TaskCreate",
            json!({"subject":"stale shell","metadata":{"managed_shell":true}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        super::super::tasks::execute_for_session(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root.path(),
            &session,
        )
        .unwrap();
        let directory = root.path().join("shell-tasks").join(&session).join(id);
        std::fs::create_dir_all(&directory).unwrap();
        let stopped = control("TaskStop", &json!({"task_id":id}), root.path(), &session)
            .unwrap()
            .unwrap();
        assert_eq!(stopped["status"], "unresponsive");
        assert_eq!(stopped["cancellation_requested"], true);
        assert!(directory.join("cancel").exists());
        assert!(control(
            "TaskOutput",
            &json!({"task_id":id,"block":true}),
            root.path(),
            &session
        )
        .is_err());
    }
    async fn output(root: &Path, session: &str, id: &str, terminal: bool) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let value = super::super::tasks::execute_for_session(
                    "TaskOutput",
                    json!({"task_id":id}),
                    root,
                    session,
                )
                .unwrap();
                if (!terminal && value["stdout"].as_str().unwrap().contains("ready"))
                    || (terminal && value["status"] != "running")
                {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn managed_shell_survives_turn_returns_live_logs_and_cancels_descendants() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let leaked = root.path().join("leaked");
        let mut command = Command::new("node");
        command.arg("-e").arg("require('child_process').spawn(process.execPath,['-e',`setTimeout(()=>require('fs').writeFileSync(process.argv[1],'leaked'),1800)`,process.argv[1]],{stdio:'inherit'});console.log('ready');console.error('diagnostic');setInterval(()=>{},10000)").arg(&leaked);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(command, None, "node", "test managed shell").unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        let running = output(root.path(), &session, id, false).await;
        assert_eq!(running["status"], "running");
        assert!(running["stderr"].as_str().unwrap().contains("diagnostic"));
        assert!(super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":id}),
            root.path(),
            &uuid::Uuid::new_v4().to_string()
        )
        .is_err());
        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true);
        assert_eq!(
            output(root.path(), &session, id, true).await["status"],
            "cancelled"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !leaked.exists(),
            "background descendant escaped cancellation"
        );
    }
    #[tokio::test]
    async fn managed_shell_completion_and_timeout_preserve_output() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        for (code, expected) in [
            ("console.log('finished')", "completed"),
            ("console.log('partial');setInterval(()=>{},10000)", "failed"),
        ] {
            let mut command = Command::new("node");
            command.args(["-e", code]);
            let task = scope(Some((root.path().into(), session.clone())), async {
                start(command, Some(Duration::from_millis(600)), "node", "fixture").unwrap()
            })
            .await;
            let observed = output(
                root.path(),
                &session,
                task["task_id"].as_str().unwrap(),
                true,
            )
            .await;
            assert_eq!(observed["status"], expected);
            assert!(!observed["stdout"].as_str().unwrap().is_empty());
            if expected == "failed" {
                assert!(observed["result"].as_str().unwrap().contains("timed_out"));
            }
        }
    }
}
