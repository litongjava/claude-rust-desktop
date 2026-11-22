// Check each producer at the model boundary against its ordinary rendered text.

use super::super::ToolLoopExecutor;
use super::{command_preview, tests::executor};
use crate::tools::{background_shell, base64_collapse, monitor, tasks};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

fn check_view(label: &str, root: &Path, original: &str, view: &str, encoded: bool) {
    let runs = base64_collapse::runs(original);
    assert_eq!(!runs.is_empty(), encoded, "{label}: fixture coverage");
    if !encoded {
        assert_eq!(original, view, "{label}");
        return;
    }
    assert!(
        base64_collapse::runs(view).is_empty(),
        "{label}: raw payload"
    );
    let refs: Vec<_> = view.split("; full text: ").skip(1).collect();
    assert_eq!(refs.len(), runs.len(), "{label}");
    for (reference, run) in refs.into_iter().zip(runs) {
        let (file, rest) = reference.split_once(", line ").unwrap();
        let line: usize = rest.split_once(']').unwrap().0.parse().unwrap();
        let path = Path::new(file);
        assert!(path.starts_with(root.join("tool-outputs")));
        assert_eq!(
            path.file_stem().unwrap().to_str().unwrap(),
            format!("{:x}", Sha256::digest(original))
        );
        let saved = std::fs::read_to_string(path).unwrap();
        assert_eq!(saved, original, "{label}");
        assert_eq!(
            line,
            1 + original[..run.range.start]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
        );
        assert!(saved
            .lines()
            .nth(line - 1)
            .unwrap()
            .contains(&original[run.range]));
    }
}

fn check_tool(
    label: &str,
    engine: &mut ToolLoopExecutor,
    root: &Path,
    name: &str,
    raw: &str,
    encoded: bool,
) {
    let original = command_preview(
        name,
        raw,
        Some(root),
        None,
        engine.summary_enabled(Some(root)),
    );
    let live = engine.retained_tool_output(name, raw);
    check_view(label, root, &original, &live, encoded);
    engine.messages = vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","type":"function","function":{"name":name,"arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":raw}),
    ];
    let replay = serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap();
    assert_eq!(replay[1]["content"], live, "{label}: OpenAI replay");
    engine.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"old","name":name,"input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"old","content":raw}]}),
    ];
    let replay = serde_json::to_value(engine.build_anthropic_messages().unwrap()).unwrap();
    assert_eq!(
        replay[1]["content"][0]["content"], live,
        "{label}: Anthropic replay"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn base64_model_boundary_matrix() {
    use crate::tools::execute_tool_async;
    use std::io::Write;
    use std::time::Duration;

    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root<with brackets>");
    std::fs::create_dir(&root).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let mut engine = executor(&root).with_conv_id(session.clone());
    for encoded in [false, true] {
        let payload = if encoded {
            "QUJD".repeat(256)
        } else {
            "流水.".repeat(256)
        };
        let text = format!("first line\n!{payload}!\n");
        std::fs::write(root.join("fixture"), &text).unwrap();
        let large = if encoded {
            "QUJD".repeat(32768)
        } else {
            ".".repeat(131072)
        };
        let timeout_source = format!("{text}!{large}!\n{text}");
        std::fs::write(root.join("timeout-fixture"), &timeout_source).unwrap();
        for timeout in [false, true] {
            let input = if timeout {
                json!({"command":"cat timeout-fixture; sleep 2","timeout":650})
            } else {
                json!({"command":"cat fixture"})
            };
            let raw = background_shell::scope(
                Some((root.clone(), session.clone())),
                execute_tool_async("Bash", input, root.to_str().unwrap()),
            )
            .await
            .unwrap();
            if timeout {
                assert_eq!(raw["timed_out_to_background"], true);
                assert!(raw["stdout"].as_str().unwrap().len() <= 32 * 1024);
                assert!(raw["stdout"].as_str().unwrap().len() < timeout_source.len());
                let snapshot = tasks::execute_for_session_async(
                    "TaskGet",
                    json!({"task_id":raw["task_id"],"block":true,"timeout":15000}),
                    &root,
                    &session,
                )
                .await
                .unwrap();
                assert_eq!(snapshot["status"], "completed");
                check_tool(
                    "shell snapshot",
                    &mut engine,
                    &root,
                    "TaskGet",
                    &snapshot.to_string(),
                    encoded,
                );
            }
            assert!(raw["stdout"].as_str().unwrap().contains(&payload));
            check_tool(
                if timeout {
                    "timeout handoff"
                } else {
                    "foreground Bash"
                },
                &mut engine,
                &root,
                "Bash",
                &raw.to_string(),
                encoded,
            );
        }

        // A completed console task can have an arbitrarily long append log.
        let task = tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"console","metadata":{"managed_shell":true,"interaction":"pty"}}),
            &root,
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"completed"}),
            &root,
            &session,
        )
        .unwrap();
        let logs = root.join("shell-tasks").join(&session).join(id);
        std::fs::create_dir_all(&logs).unwrap();
        let mib = 1024 * 1024;
        let log = format!("{text}{}{text}", ".".repeat(mib - text.len()));
        std::fs::write(logs.join("stdout.log"), &log).unwrap();
        std::fs::write(logs.join("stderr.log"), "").unwrap();
        let get = |input| tasks::execute_for_session("TaskGet", input, &root, &session).unwrap();
        let snapshot = get(json!({"id":id,"block":false}));
        assert_eq!(snapshot["stdout"].as_str().unwrap().len(), mib);
        check_tool(
            "bounded PTY snapshot",
            &mut engine,
            &root,
            "TaskGet",
            &snapshot.to_string(),
            encoded,
        );
        let first = get(json!({"id":id,"max_output_bytes":mib}));
        let page = get(json!({"id":id,"max_output_bytes":8192,"cursor":first["next_cursor"]}));
        assert_eq!(page["stdout"], text);
        check_tool(
            "PTY page beyond 1 MiB, also replayed without a sidecar",
            &mut engine,
            &root,
            "TaskGet",
            &page.to_string(),
            encoded,
        );

        let monitor_line = payload.repeat(8);
        std::fs::write(root.join("monitor-fixture"), format!("{monitor_line}\n")).unwrap();
        let mut command = tokio::process::Command::new("cat");
        command.arg(root.join("monitor-fixture"));
        let started = background_shell::scope(Some((root.clone(), session.clone())), async {
            monitor::start(
                command,
                "watch",
                "cat fixture",
                Duration::from_secs(10),
                None,
            )
        })
        .await
        .unwrap();
        let id = started["task_id"].as_str().unwrap();
        let mut events = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                events.extend(monitor::collect_events(&root, &session).unwrap());
                if events.iter().any(|event| event.kind == "exited") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let records = root
            .join("shell-tasks")
            .join(&session)
            .join(id)
            .join("events.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&records)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"seq":999,"kind":"output","text":format!("legacy!{payload}!") })
        )
        .unwrap();
        let legacy = monitor::collect_events(&root, &session).unwrap();
        let live = events
            .iter()
            .find(|event| event.kind == "output" && event.text.contains(&payload))
            .unwrap();
        assert_eq!(
            live.text,
            monitor_line.chars().take(4096).collect::<String>()
        );
        assert!(live
            .summary()
            .ends_with(&monitor_line.chars().take(200).collect::<String>()));
        for (label, event) in [("live Monitor", live), ("legacy Monitor", &legacy[0])] {
            let notice = event.notification();
            assert!(notice.contains(&payload));
            let input = crate::services::input_queue::DeliveredInput::resolve(
                Some(&root),
                &session,
                notice.clone(),
            );
            let queued = engine.queued_model_content(vec![input]);
            check_view(label, &root, &notice, queued[0].as_str().unwrap(), encoded);
            engine.messages = vec![json!({"role":"user","content":notice})];
            let replay = serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap();
            assert_eq!(replay[0]["content"], queued[0]);
            assert_eq!(engine.messages[0]["content"], notice);
        }
    }
}

#[test]
fn base64_i6_cut_references_the_exact_preview_and_full_result() {
    let temp = tempfile::tempdir().unwrap();
    // Arrange a canonical quartet on each edge of the actual retained preview.
    let root = temp.path();
    let engine = executor(root);
    let data = "Q".repeat(40_000);
    let raw = json!({"stdout":format!("!{data}!"),"exit_code":0}).to_string();
    let original = command_preview("Bash", &raw, Some(root), None, true);
    let view = engine.retained_tool_output("Bash", &raw);
    check_view("I6", root, &original, &view, true);
    let path = original
        .lines()
        .find(|line| line.starts_with("Saved tool result ("))
        .unwrap()
        .split_once("): ")
        .unwrap()
        .1;
    assert!(view.contains(path));
    assert!(std::fs::read_to_string(path).unwrap().contains(&data));
    assert!(view.contains("(partial at output cut)"));
    assert!(view.contains("Omitted output:"));
}

#[test]
fn base64_model_boundary_preserves_the_other_stream_and_every_visible_reference() {
    let temp = tempfile::tempdir().unwrap();
    let engine = executor(temp.path());
    let stdout = format!("{}\nLAST-ORDINARY-DIAGNOSTIC!", ".".repeat(1_052_698));
    let stderr = "QUJD".repeat(256);
    let (raw, metadata) = crate::tools::bash_process::fixture_capture(
        [stdout.as_bytes(), stderr.as_bytes()],
        1 << 20,
        102400,
        0,
    );
    let id = "mixed-streams";
    super::save_metadata(temp.path(), id, metadata.as_ref().unwrap()).unwrap();
    let ordinary = command_preview("Bash", &raw, Some(temp.path()), metadata.as_ref(), true);
    assert!(ordinary.contains("LAST-ORDINARY-DIAGNOSTIC!"));
    let live = engine.retained_tool_output_for_id("Bash", &raw, Some(id));
    assert!(live.contains("LAST-ORDINARY-DIAGNOSTIC!"));
    check_view("mixed streams", temp.path(), &ordinary, &live, true);
    let many = format!("!{}!\n", stderr).repeat(140);
    let projected =
        super::super::project_for_model("TaskGet", &many, Some(temp.path()), "session").unwrap();
    assert_eq!(projected.matches("; full text: ").count(), 140);
    check_view("140 historical runs", temp.path(), &many, &projected, true);
}

#[test]
fn base64_model_boundary_without_a_settings_path_uses_the_runtime_directory() {
    // Isolate the runtime directory from parallel tests and the user's home.
    const CHILD: &str = "I41_PROJECTION_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = Path::new(&root);
        let mut engine = executor(root);
        engine.search_config_path = None;
        let original = format!("!{}!", "QUJD".repeat(256));
        let live = engine.retained_tool_output("Bash", &original);
        check_view("default runtime directory", root, &original, &live, true);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let name = concat!(
        module_path!(),
        "::base64_model_boundary_without_a_settings_path_uses_the_runtime_directory"
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1])
        .env(CHILD, temp.path())
        .env(crate::runtime::DATA_DIR_ENV, temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}
