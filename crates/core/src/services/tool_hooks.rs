//! Command lifecycle hooks. Hosts must load workspace configuration only after
//! selecting/trusting that workspace, and retain normal tool permission checks.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const OUTPUT_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    PermissionRequest,
    SubagentStart,
    SubagentStop,
    UserPromptSubmit,
    Stop,
    WorktreeCreate,
    WorktreeRemove,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
}
impl HookEvent {
    fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionDenied => "PermissionDenied",
            Self::PermissionRequest => "PermissionRequest",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::WorktreeCreate => "WorktreeCreate",
            Self::WorktreeRemove => "WorktreeRemove",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveHook {
    pub event: HookEvent,
    pub matcher: String,
    pub command: String,
    pub timeout: u64,
    pub source: PathBuf,
    pub shell: Option<String>,
}

#[derive(Debug, Default)]
pub struct HookOutcome {
    pub allow_permission: bool,
    pub permission_response: Option<HookPermissionResponse>,
    pub blocked: bool,
    pub reason: String,
    pub additional_context: Vec<String>,
    pub updated_input: Option<Value>,
    pub warnings: Vec<String>,
    /// An explicit hook request for approval must not become an automatic grant.
    pub ask_permission: bool,
    pub prevent_continuation: bool,
    /// PermissionDenied advice, never an execution grant.
    pub retry: bool,
}

#[derive(Debug)]
pub enum HookPermissionResponse {
    Allow { updated_input: Option<Value>, updated_permissions: Option<Value> },
    Deny { message: String, interrupt: bool },
}

#[derive(Debug, Deserialize)]
struct MatcherConfig {
    #[serde(default)]
    matcher: String,
    hooks: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct CommandConfig {
    command: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default)]
    shell: Option<String>,
}
fn default_timeout() -> u64 {
    60
}

pub struct HookRunner {
    workspace: PathBuf,
    hooks: Vec<ActiveHook>,
}

impl HookRunner {
    pub fn load(data_dir: &Path, workspace: &Path) -> Result<Self> {
        Self::load_with_configs(data_dir, workspace, Vec::new())
    }

    pub fn load_with_plugins(context: &super::ServiceContext, workspace: &Path) -> Result<Self> {
        Self::load_with_configs(
            &context.paths.data_dir,
            workspace,
            super::plugins::command_hooks(context)?,
        )
    }

    fn load_with_configs(
        data_dir: &Path,
        workspace: &Path,
        mut configs: Vec<(PathBuf, Value)>,
    ) -> Result<Self> {
        let sources = [
            data_dir.join("settings.json"),
            workspace.join(".claude/settings.json"),
            workspace.join(".claude/settings.local.json"),
        ];
        let mut hooks = Vec::new();
        let mut disabled = false;
        for source in sources {
            let content = match std::fs::read_to_string(&source) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Reading hooks from {}", source.display()))
                }
            };
            let settings: Value = serde_json::from_str(content.trim_start_matches('\u{feff}'))
                .with_context(|| format!("Invalid hook settings: {}", source.display()))?;
            configs.push((source, settings));
        }
        for (source, settings) in configs {
            if let Some(value) = settings.get("disableAllHooks") {
                disabled = value.as_bool().context("disableAllHooks must be boolean")?;
            }
            let Some(config) = settings.get("hooks") else {
                continue;
            };
            let events: BTreeMap<String, Vec<MatcherConfig>> =
                serde_json::from_value(config.clone())
                    .context("hooks must map event names to matcher arrays")?;
            for (name, groups) in events {
                let event = match name.as_str() {
                    "PreToolUse" => HookEvent::PreToolUse,
                    "PostToolUse" => HookEvent::PostToolUse,
                    "PostToolUseFailure" => HookEvent::PostToolUseFailure,
                    "PermissionDenied" => HookEvent::PermissionDenied,
                    "PermissionRequest" => HookEvent::PermissionRequest,
                    "SubagentStart" => HookEvent::SubagentStart,
                    "SubagentStop" => HookEvent::SubagentStop,
                    "UserPromptSubmit" => HookEvent::UserPromptSubmit,
                    "Stop" => HookEvent::Stop,
                    "WorktreeCreate" => HookEvent::WorktreeCreate,
                    "WorktreeRemove" => HookEvent::WorktreeRemove,
                    "SessionStart" => HookEvent::SessionStart,
                    "SessionEnd" => HookEvent::SessionEnd,
                    "PreCompact" => HookEvent::PreCompact,
                    "PostCompact" => HookEvent::PostCompact,
                    _ => continue,
                };
                for group in groups {
                    if !group.matcher.is_empty() && group.matcher != "*" {
                        regex::Regex::new(&group.matcher)
                            .context("Invalid hook matcher regular expression")?;
                    }
                    for hook in group.hooks {
                        ensure!(hook["type"] == "command", "Unsupported hook type for {name} in {}; only command hooks are implemented", source.display());
                        ensure!(
                            hook.get("async").and_then(Value::as_bool) != Some(true),
                            "Async command hooks are not implemented"
                        );
                        let hook: CommandConfig = serde_json::from_value(hook)?;
                        ensure!(
                            !hook.command.trim().is_empty(),
                            "Hook command cannot be empty"
                        );
                        ensure!(
                            (1..=3600).contains(&hook.timeout),
                            "Hook timeout must be 1..3600 seconds"
                        );
                        if let Some(shell) = &hook.shell {
                            ensure!(
                                ["bash", "sh", "cmd", "powershell", "pwsh"]
                                    .contains(&shell.as_str()),
                                "Unsupported hook shell: {shell}"
                            );
                        }
                        hooks.push(ActiveHook {
                            event,
                            matcher: group.matcher.clone(),
                            command: hook.command,
                            timeout: hook.timeout,
                            source: source.clone(),
                            shell: hook.shell,
                        });
                    }
                }
            }
        }
        if disabled {
            hooks.clear();
        }
        Ok(Self {
            workspace: workspace.to_path_buf(),
            hooks,
        })
    }

    pub fn list(&self) -> &[ActiveHook] {
        &self.hooks
    }

    /// Worktree hooks return a filesystem path, not tool-permission JSON.
    pub async fn run_worktree(&self, event: HookEvent, session: &str, mut payload: Value) -> Result<Option<String>> {
        ensure!(matches!(event, HookEvent::WorktreeCreate | HookEvent::WorktreeRemove), "Invalid worktree hook event");
        payload["session_id"] = json!(session);
        payload["cwd"] = json!(self.workspace);
        payload["hook_event_name"] = json!(event.name());
        let mut path = None;
        for hook in self.hooks.iter().filter(|hook|hook.event == event) {
            let (code, stdout, stderr) = execute(hook, &self.workspace, &payload).await?;
            ensure!(code == 0, "{} hook failed ({code}): {}", event.name(), stderr.trim());
            if event == HookEvent::WorktreeCreate && path.is_none() && !stdout.trim().is_empty() {
                path = Some(if stdout.trim().starts_with('{') {
                    let value: Value = serde_json::from_str(stdout.trim())?;
                    value["hookSpecificOutput"]["worktreePath"].as_str().context("WorktreeCreate JSON must provide hookSpecificOutput.worktreePath")?.to_owned()
                } else { stdout.trim().to_owned() });
            }
        }
        if event == HookEvent::WorktreeCreate { ensure!(path.is_some(), "WorktreeCreate returned no path"); }
        Ok(path)
    }

    pub async fn run(
        &self,
        event: HookEvent,
        session_id: &str,
        payload: Value,
    ) -> Result<HookOutcome> {
        let mut input = payload
            .as_object()
            .cloned()
            .context("Hook payload must be an object")?;
        input.insert("session_id".into(), json!(session_id));
        input.insert("cwd".into(), json!(self.workspace));
        input.insert("hook_event_name".into(), json!(event.name()));
        let lifecycle = matches!(
            event,
            HookEvent::SessionStart | HookEvent::SessionEnd | HookEvent::PreCompact | HookEvent::PostCompact | HookEvent::SubagentStart | HookEvent::PostToolUseFailure | HookEvent::PermissionDenied
        );
        let match_key = match event {
            HookEvent::SessionStart => "source",
            HookEvent::SessionEnd => "reason",
            HookEvent::PreCompact | HookEvent::PostCompact => "trigger",
            HookEvent::SubagentStart | HookEvent::SubagentStop => "agent_type",
            _ => "tool_name",
        };
        let name = input
            .get(match_key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if event == HookEvent::PermissionRequest {
            use futures::{stream::FuturesUnordered, StreamExt};
            let mut pending = FuturesUnordered::new();
            for hook in self.hooks.iter().filter(|hook| hook.event == event) {
                if !matches_tool(&hook.matcher, &name)? { continue; }
                let input = Value::Object(input.clone());
                pending.push(async move {
                    let (code, stdout, stderr) = execute(hook, &self.workspace, &input).await?;
                    parse_output(event, code, &stdout, &stderr)
                });
            }
            let mut result = HookOutcome::default();
            while let Some(outcome) = pending.next().await {
                match outcome {
                    Ok(outcome) => {
                        result.warnings.extend(outcome.warnings);
                        if outcome.blocked { result.warnings.push(outcome.reason); }
                        if outcome.permission_response.is_some() {
                            result.permission_response = outcome.permission_response;
                            // Dropping the losing futures kills their actual subprocesses.
                            return Ok(result);
                        }
                    }
                    Err(error) => result.warnings.push(format!("PermissionRequest hook failed: {error:#}")),
                }
            }
            return Ok(result);
        }
        let mut result = HookOutcome::default();
        for hook in self.hooks.iter().filter(|h| h.event == event) {
            if (lifecycle || matches!(event, HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::PostToolUse | HookEvent::SubagentStop))
                && !matches_tool(&hook.matcher, &name)?
            {
                continue;
            }
            let outcome = async {
                let output = execute(hook, &self.workspace, &Value::Object(input.clone())).await?;
                parse_output(event, output.0, &output.1, &output.2)
            }
            .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) if lifecycle => {
                    result
                        .warnings
                        .push(format!("{} hook: {error:#}", event.name()));
                    continue;
                }
                Err(error) => return Err(error),
            };
            result.additional_context.extend(outcome.additional_context);
            result.warnings.extend(outcome.warnings);
            result.ask_permission |= outcome.ask_permission;
            result.allow_permission |= outcome.allow_permission;
            if outcome.permission_response.is_some() {
                result.permission_response = outcome.permission_response;
                return Ok(result);
            }
            result.prevent_continuation |= outcome.prevent_continuation;
            result.retry |= outcome.retry;
            if let Some(updated) = outcome.updated_input {
                input.insert("tool_input".into(), updated.clone());
                result.updated_input = Some(updated);
            }
            if outcome.blocked && lifecycle {
                result.warnings.push(outcome.reason);
                continue;
            }
            if outcome.blocked {
                result.blocked = true;
                result.reason = outcome.reason;
                break;
            }
        }
        Ok(result)
    }
}

fn matches_tool(matcher: &str, tool: &str) -> Result<bool> {
    Ok(matcher.is_empty() || matcher == "*" || regex::Regex::new(matcher)?.is_match(tool))
}

fn parse_output(event: HookEvent, exit: i32, stdout: &str, stderr: &str) -> Result<HookOutcome> {
    let mut result = HookOutcome::default();
    if exit == 2 {
        result.blocked = true;
        result.reason = if stderr.trim().is_empty() {
            "Blocked by command hook (exit 2)".into()
        } else {
            stderr.trim().into()
        };
        return Ok(result);
    }
    if exit != 0 {
        result
            .warnings
            .push(format!("Hook exited with {exit}: {}", stderr.trim()));
        return Ok(result);
    }
    let output = stdout.trim();
    if output.is_empty() {
        return Ok(result);
    }
    // Compact hooks consume successful stdout as text, never as permission decisions.
    if event == HookEvent::PreCompact {
        result.additional_context.push(output.into());
        return Ok(result);
    }
    if event == HookEvent::PostCompact {
        result.warnings.push(output.into());
        return Ok(result);
    }
    if !output.starts_with('{') {
        if matches!(event, HookEvent::UserPromptSubmit | HookEvent::SessionStart | HookEvent::SubagentStart) {
            result.additional_context.push(output.into());
        }
        return Ok(result);
    }
    let output: Value = serde_json::from_str(output).context("Hook returned invalid JSON")?;
    if event == HookEvent::PermissionRequest {
        if let Some(message) = string_field(&output, "systemMessage")? { result.warnings.push(message.into()); }
        if let Some(specific) = output.get("hookSpecificOutput") {
            ensure!(specific.is_object(), "hookSpecificOutput must be an object");
            ensure!(string_field(specific, "hookEventName")? == Some(event.name()), "Hook returned an incorrect hookEventName");
            if let Some(decision) = specific.get("decision") {
                ensure!(decision.is_object(), "PermissionRequest decision must be an object");
                result.permission_response = Some(match string_field(decision, "behavior")? {
                    Some("allow") => {
                        let updated_input = decision.get("updatedInput").map(|value| {
                            ensure!(value.is_object(), "Hook updatedInput must be an object"); Ok(value.clone())
                        }).transpose()?;
                        HookPermissionResponse::Allow { updated_input, updated_permissions: decision.get("updatedPermissions").cloned() }
                    }
                    Some("deny") => HookPermissionResponse::Deny {
                        message: string_field(decision, "message")?.unwrap_or("Permission denied by hook").into(),
                        interrupt: decision.get("interrupt").map(|value| value.as_bool().context("Hook interrupt must be boolean")).transpose()?.unwrap_or(false),
                    },
                    _ => bail!("Invalid PermissionRequest behavior"),
                });
            }
        }
        return Ok(result);
    }
    if event == HookEvent::PermissionDenied {
        if let Some(message) = string_field(&output, "systemMessage")? { result.warnings.push(message.into()); }
        if let Some(specific) = output.get("hookSpecificOutput") {
            ensure!(specific.is_object(), "hookSpecificOutput must be an object");
            ensure!(string_field(specific, "hookEventName")? == Some(event.name()), "Hook returned an incorrect hookEventName");
            if let Some(retry) = specific.get("retry") { result.retry = retry.as_bool().context("PermissionDenied retry must be boolean")?; }
        }
        // This notification cannot mutate input, permit execution or stop a turn.
        return Ok(result);
    }
    if let Some(value) = output.get("continue") {
        ensure!(value.is_boolean(), "Hook continue must be boolean");
        if value == false {
            result.prevent_continuation = true;
            result.blocked = true;
            result.reason = string_field(&output, "stopReason")?
                .unwrap_or("Stopped by hook")
                .into();
        }
    }
    if let Some(decision) = string_field(&output, "decision")? {
        ensure!(
            ["approve", "block"].contains(&decision),
            "Invalid hook decision"
        );
        if decision == "block" {
            result.blocked = true;
            result.reason = string_field(&output, "reason")?
                .unwrap_or("Blocked by hook")
                .into();
        }
    }
    if let Some(message) = string_field(&output, "systemMessage")? {
        result.warnings.push(message.into());
    }
    if let Some(specific) = output.get("hookSpecificOutput") {
        ensure!(specific.is_object(), "hookSpecificOutput must be an object");
        ensure!(
            string_field(specific, "hookEventName")? == Some(event.name()),
            "Hook returned an incorrect hookEventName"
        );
        if let Some(context) = string_field(specific, "additionalContext")? {
            result.additional_context.push(context.into());
        }
        if event == HookEvent::PreToolUse {
            if let Some(decision) = string_field(specific, "permissionDecision")? {
                match decision {
                    "deny" => {
                        result.blocked = true;
                        result.reason = string_field(specific, "permissionDecisionReason")?
                            .unwrap_or("Tool denied by hook")
                            .into();
                    }
                    "ask" => result.ask_permission = true,
                    "allow" => result.allow_permission = true,
                    _ => bail!("Invalid hook permissionDecision"),
                }
            }
            if let Some(updated) = specific.get("updatedInput") {
                ensure!(updated.is_object(), "Hook updatedInput must be an object");
                result.updated_input = Some(updated.clone());
            }
        }
    }
    Ok(result)
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<Option<&'a str>> {
    value
        .get(name)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("Hook {name} must be a string"))
        })
        .transpose()
}

async fn execute(hook: &ActiveHook, cwd: &Path, input: &Value) -> Result<(i32, String, String)> {
    let shell = hook
        .shell
        .as_deref()
        .unwrap_or(if cfg!(windows) { "cmd" } else { "sh" });
    let mut command = Command::new(shell);
    match shell {
        "cmd" => {
            command.args(["/D", "/S", "/C"]);
            #[cfg(windows)]
            command.raw_arg(format!("\"chcp 65001 >nul & {}\"", hook.command));
            #[cfg(not(windows))]
            command.arg(&hook.command);
        }
        "powershell" | "pwsh" => {
            let utf8_command = format!("$OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::InputEncoding = $OutputEncoding; [Console]::OutputEncoding = $OutputEncoding; $PSDefaultParameterValues['*:Encoding'] = 'utf8';\n{}", hook.command);
            command.args(["-NoProfile", "-NonInteractive", "-Command", &utf8_command]);
        }
        _ => {
            command.args(["-c", &hook.command]);
        }
    }
    command
        .current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(metadata) = hook.source.parent().filter(|parent| {
        parent
            .file_name()
            .is_some_and(|name| name == ".claude-plugin")
    }) {
        if let Some(root) = metadata.parent() {
            command.env("CLAUDE_PLUGIN_ROOT", root);
        }
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Starting {} hook", hook.event.name()))?;
    let _tree = ProcessTree::new(child.id().context("Hook has no process ID")?)?;
    let mut stdin = child.stdin.take().context("Hook stdin unavailable")?;
    let stdout = child.stdout.take().context("Hook stdout unavailable")?;
    let stderr = child.stderr.take().context("Hook stderr unavailable")?;
    let mut data = serde_json::to_vec(input)?;
    data.push(b'\n');
    let operation = async {
        let write = async {
            stdin.write_all(&data).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let read_out = async {
            let mut bytes = Vec::new();
            stdout
                .take(OUTPUT_LIMIT + 1)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let read_err = async {
            let mut bytes = Vec::new();
            stderr
                .take(OUTPUT_LIMIT + 1)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let (written, status, stdout, stderr) =
            tokio::join!(write, child.wait(), read_out, read_err);
        // A hook may intentionally exit without reading its input.
        if let Err(error) = written {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(error.into());
            }
        }
        let stdout = stdout?;
        let stderr = stderr?;
        ensure!(
            stdout.len() <= OUTPUT_LIMIT as usize && stderr.len() <= OUTPUT_LIMIT as usize,
            "Hook output exceeded 1 MiB"
        );
        Ok((
            status?.code().unwrap_or(-1),
            String::from_utf8(stdout).context("Hook stdout is not UTF-8")?,
            String::from_utf8(stderr).context("Hook stderr is not UTF-8")?,
        ))
    };
    tokio::time::timeout(Duration::from_secs(hook.timeout), operation)
        .await
        .with_context(|| {
            format!(
                "{} hook timed out after {} seconds",
                hook.event.name(),
                hook.timeout
            )
        })?
}

#[cfg(windows)]
pub(crate) struct ProcessTree(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        use windows::Win32::{
            Foundation::CloseHandle,
            System::{JobObjects::*, Threading::*},
        };
        unsafe {
            let job = CreateJobObjectW(None, None)?;
            let tree = Self(job);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )?;
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)?;
            let assigned = AssignProcessToJobObject(job, process);
            let _ = CloseHandle(process);
            assigned?;
            Ok(tree)
        }
    }
}
#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(unix)]
pub(crate) struct ProcessTree(u32);
#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn new(pid: u32) -> Result<Self> {
        Ok(Self(pid))
    }
}
#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe {
            kill(-(self.0 as i32), 9);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_request_requires_typed_event_and_valid_decision() {
        for output in [
            json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","decision":{"behavior":"allow"}}}),
            json!({"hookSpecificOutput":{"decision":{"behavior":"allow"}}}),
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":[]}}}),
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","interrupt":"true"}}}),
        ] { assert!(parse_output(HookEvent::PermissionRequest,0,&output.to_string(),"").is_err()); }
        let allow = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{}}}});
        assert!(matches!(parse_output(HookEvent::PermissionRequest,0,&allow.to_string(),"").unwrap().permission_response,Some(HookPermissionResponse::Allow {..})));
        for exit in [1,2] {assert!(parse_output(HookEvent::PermissionRequest,exit,&allow.to_string(),"failure").unwrap().permission_response.is_none());}
    }

    #[tokio::test]
    async fn permission_request_hooks_race_each_other_and_ignore_invalid_peers() {
        let root = tempfile::tempdir().unwrap();
        let allow = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
        let commands = if cfg!(windows) {
            vec!["[IO.File]::WriteAllText('slow-started','yes'); Start-Sleep -Seconds 30".into(), "[Console]::Out.Write('{invalid')".into(), format!("while (-not [IO.File]::Exists('slow-started')) {{Start-Sleep -Milliseconds 20}}; [Console]::Out.Write('{allow}')")]
        } else {
            vec!["printf yes > slow-started; sleep 30".into(), "printf '%s' '{invalid'".into(), format!("while [ ! -f slow-started ]; do sleep 0.02; done; printf '%s' '{allow}'")]
        };
        let runner = HookRunner {workspace:root.path().into(),hooks:commands.into_iter().map(|command| ActiveHook {event:HookEvent::PermissionRequest,matcher:"Write".into(),command,timeout:35,source:root.path().into(),shell:Some(if cfg!(windows) {"powershell"} else {"sh"}.into())}).collect()};
        let result = tokio::time::timeout(Duration::from_secs(10), runner.run(HookEvent::PermissionRequest,"session",json!({"tool_name":"Write","tool_use_id":"exact-id"}))).await.unwrap().unwrap();
        assert!(matches!(result.permission_response,Some(HookPermissionResponse::Allow {..})));
        assert!(root.path().join("slow-started").exists());
    }

    #[test]
    fn matcher_and_output_decisions_are_validated() {
        assert!(matches_tool("Edit|Write", "Edit").unwrap());
        assert!(!matches_tool("^Read$", "ReadExtra").unwrap());
        assert!(matches_tool("*", "Anything").unwrap());
        assert!(
            parse_output(HookEvent::PreToolUse, 2, "", "policy reason")
                .unwrap()
                .blocked
        );
        let outcome = parse_output(HookEvent::PreToolUse, 0, r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"reason","updatedInput":{"path":"safe"}}}"#, "").unwrap();
        assert!(outcome.blocked);
        assert_eq!(outcome.reason, "reason");
        assert_eq!(outcome.updated_input.unwrap()["path"], "safe");
        assert!(parse_output(
            HookEvent::Stop,
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse"}}"#,
            ""
        )
        .is_err());
        assert!(parse_output(HookEvent::Stop, 0, r#"{"continue":"false"}"#, "").is_err());
    }

    #[test]
    fn permission_denied_output_is_only_retry_advice() {
        let outcome = parse_output(HookEvent::PermissionDenied, 0, r#"{"decision":"approve","continue":false,"hookSpecificOutput":{"hookEventName":"PermissionDenied","retry":true,"updatedInput":{"file_path":"unsafe"},"permissionDecision":"allow"}}"#, "").unwrap();
        assert!(outcome.retry); assert!(!outcome.blocked); assert!(!outcome.prevent_continuation); assert!(outcome.updated_input.is_none()); assert!(!outcome.ask_permission);
        for text in [r#"{"hookSpecificOutput":{"hookEventName":"PermissionDenied","retry":"true"}}"#, r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","retry":true}}"#, "{invalid"] { assert!(parse_output(HookEvent::PermissionDenied,0,text,"").is_err()); }
        assert!(!parse_output(HookEvent::PermissionDenied,1,r#"{"hookSpecificOutput":{"hookEventName":"PermissionDenied","retry":true}}"#,"failed").unwrap().retry);
    }

    #[test]
    fn configuration_merges_and_disables_effective_hooks() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        let config = r#"{"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"echo check"}]}]}}"#;
        std::fs::write(data.join("settings.json"), config).unwrap();
        std::fs::write(workspace.join(".claude/settings.json"), config).unwrap();
        assert_eq!(HookRunner::load(&data, &workspace).unwrap().list().len(), 2);
        std::fs::write(
            workspace.join(".claude/settings.local.json"),
            r#"{"disableAllHooks":true}"#,
        )
        .unwrap();
        assert!(HookRunner::load(&data, &workspace)
            .unwrap()
            .list()
            .is_empty());
    }

    #[tokio::test]
    async fn command_stdin_and_exit_two_work() {
        let root = tempfile::tempdir().unwrap();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "$v=[Console]::In.ReadToEnd() | ConvertFrom-Json; if ($v.session_id -ne 'test') {exit 7}; [Console]::Error.Write('denied'); exit 2")
        } else {
            (
                "sh",
                "read line; case \"$line\" in *test*) printf denied >&2; exit 2;; *) exit 7;; esac",
            )
        };
        let hook = ActiveHook {
            event: HookEvent::PreToolUse,
            matcher: "*".into(),
            command: command.into(),
            timeout: 10,
            source: root.path().into(),
            shell: Some(shell.into()),
        };
        let runner = HookRunner {
            workspace: root.path().into(),
            hooks: vec![hook],
        };
        let output = runner
            .run(HookEvent::PreToolUse, "test", json!({"tool_name":"Read"}))
            .await
            .unwrap();
        assert!(output.blocked);
        assert_eq!(output.reason, "denied");
    }

    #[tokio::test]
    async fn hook_timeout_is_an_error_not_permission_to_continue() {
        let root = tempfile::tempdir().unwrap();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "Start-Sleep -Seconds 30")
        } else {
            ("sh", "sleep 30")
        };
        let hook = ActiveHook {
            event: HookEvent::PreToolUse,
            matcher: "".into(),
            command: command.into(),
            timeout: 1,
            source: root.path().into(),
            shell: Some(shell.into()),
        };
        let error = execute(&hook, root.path(), &json!({})).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn cancelled_hook_cannot_keep_running() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_path_buf();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "[IO.File]::WriteAllText('started','yes'); Start-Sleep -Seconds 2; [IO.File]::WriteAllText('survived','bad')")
        } else {
            ("sh", "printf yes > started; sleep 2; printf bad > survived")
        };
        let hook = ActiveHook {
            event: HookEvent::PreToolUse,
            matcher: "".into(),
            command: command.into(),
            timeout: 10,
            source: cwd.clone(),
            shell: Some(shell.into()),
        };
        let task = tokio::spawn(async move { execute(&hook, &cwd, &json!({})).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!root.path().join("survived").exists());
    }
}
