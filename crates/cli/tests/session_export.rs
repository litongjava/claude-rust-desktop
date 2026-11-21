//! Read the public wire format independently of the exporter.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use deepseek_code_core::db::{
    message_repo::ToolCallRow,
    session_store::{Branch, CreateOptions, ProjectSelection, Stores},
    turn_repo::TurnStatus,
};
use ring::digest::{digest, SHA256};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
};

fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|n| format!("{n:02x}"))
        .collect()
}

fn export(root: &Path, id: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dcl"));
    c.arg("--data-dir")
        .arg(root)
        .args(["sessions", "export", id])
        .stdin(Stdio::null());
    c
}

fn stdout(root: &Path, id: &str) -> Result<Vec<u8>> {
    let out = export(root, id).output()?;
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

fn decode(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut records = Vec::new();
    let mut parts = HashMap::<(String, String), Vec<Vec<u8>>>::new();
    let mut offset = 0;
    let mut ended = false;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        ensure!(line.ends_with(b"\n") && !ended, "invalid record boundary");
        let row: Value = serde_json::from_slice(line)?;
        let kind = row["type"].as_str().context("record type")?;
        if offset == 0 {
            ensure!(
                kind == "header" && row["format"] == "deepseek-code.session" && row["version"] == 1,
                "invalid header"
            );
        }
        if kind == "end" {
            ensure!(
                row["counts"] == json!(counts) && row["sha256"] == hash(&bytes[..offset]),
                "invalid footer"
            );
            ended = true;
        } else {
            *counts.entry(kind.into()).or_default() += 1;
            if kind == "chunk" {
                let key = (
                    row["owner"].as_str().unwrap().into(),
                    row["field"].as_str().unwrap().into(),
                );
                let list = parts.entry(key).or_default();
                ensure!(row["index"] == list.len(), "out of order chunk");
                list.push(STANDARD.decode(row["data"].as_str().unwrap())?);
                // The final descriptor must digest to the same value as every chunk tag.
                let descriptor = records
                    .iter()
                    .rev()
                    .find_map(|r: &Value| {
                        let field = &r[row["field"].as_str().unwrap()];
                        (field["owner"] == row["owner"]).then_some(field)
                    })
                    .context("unresolved chunk owner")?;
                ensure!(descriptor["sha256"] == row["sha256"], "wrong chunk digest");
            } else {
                records.push(row);
            }
        }
        offset += line.len();
    }
    ensure!(ended, "missing end record");
    for row in &mut records {
        for (name, field) in row.as_object_mut().unwrap() {
            if let Some(owner) = field["owner"].as_str() {
                let chunks = parts
                    .remove(&(owner.into(), name.clone()))
                    .context("missing chunks")?;
                ensure!(field["chunks"] == chunks.len(), "missing chunk");
                let data = chunks.concat();
                ensure!(
                    field["bytes"] == data.len() && field["sha256"] == hash(&data),
                    "bad field digest"
                );
                *field = match field["encoding"].as_str().unwrap() {
                    "utf8" => json!(String::from_utf8(data)?),
                    "binary" => json!({"encoding":"base64","data":STANDARD.encode(data)}),
                    other => anyhow::bail!("unknown encoding {other}"),
                };
            }
        }
    }
    ensure!(parts.is_empty(), "orphan chunks");
    Ok(records)
}

fn of<'a>(rows: &'a [Value], kind: &str) -> Vec<&'a Value> {
    rows.iter().filter(|r| r["type"] == kind).collect()
}

fn links(rows: &[Value]) {
    let id = &rows[0]["session_id"];
    for r in &rows[1..] {
        if let Some(session) = r.get("conversation_id") {
            assert_eq!(session, id);
        }
        for (field, kind, key) in [
            ("message_id", "message", "id"),
            ("turn_id", "turn", "id"),
            ("tool_call_id", "tool_call", "id"),
            ("parent_tool_call_id", "tool_call", "id"),
            ("tool_use_id", "tool_call", "id"),
            ("request_id", "model_request", "id"),
        ] {
            if !r[field].is_null() {
                assert!(
                    of(rows, kind).iter().any(|v| v[key] == r[field]),
                    "unresolved {field} on {r}"
                );
            }
        }
    }
}

#[test]
fn reconstruct_history_related_records_and_fork_after_rewind() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let session = db.create(
        "source",
        CreateOptions {
            title: Some("export title".into()),
            model: Some("model".into()),
            provider: Some("provider".into()),
            project: Some(ProjectSelection::Workspace(root.path().into())),
            ..Default::default()
        },
    )?;
    let typed = json!([{"type":"text","text":"one\ntwo"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]);
    let large = "界🙂\n".repeat(32000);
    let message = session.write(|w| {
        w.append_user("user", &typed)?;
        let turn = w.start_turn()?;
        let message = w.segment(&turn)?;
        w.message_content(&message, &large)?;
        w.snapshots(
            &message,
            &json!([{"id":"r1","systemPrompt":large},{"id":"r2","systemPrompt":large}]),
        )?;
        for (i, name) in ["parent", "child"].iter().enumerate() {
            let parent = format!("{message}:parent");
            w.tool(
                &ToolCallRow {
                    id: format!("{message}:{name}"),
                    message_id: message.clone(),
                    name: name.to_string(),
                    input: Some("input".into()),
                    output: Some("output".into()),
                    is_error: false,
                    sort_order: i as i64,
                },
                (i == 1).then_some((parent.as_str(), 1)),
            )?;
        }
        let tool = format!("{message}:child");
        w.tool_note(&tool, "note")?;
        w.tool_duration(&tool, 52)?;
        w.attachment(
            "file",
            &message,
            "external.txt",
            "file",
            "text/plain",
            17,
            "/external/file",
        )?;
        w.approval(&serde_json::from_value(json!({"id":"a","conversation_id":"source","message_id":message,
            "tool_name":"Write","action":"save","risk_level":"medium","status":"pending","created_at":"2026-01-01"}))?)?;
        w.decide("a","approved",Some("allow"),Some("fixture decision"),"2026-01-02")?;
        w.diff(&serde_json::from_value(json!({"id":"d","conversation_id":"source","message_id":message,
            "file_path":"f","original_content":"old","modified_content":"new","diff_text":"change","status":"pending","created_at":"2026-01-01"}))?)?;
        w.set_goal("finish export")?;
        w.approve_plan("approved plan")?;
        w.set_message_metadata(&message, &json!({"fixture":true}))?;
        w.set_request_metadata("r1", &json!({"fixture":1}))?;
        w.bind_agent("agent definition")?;
        w.set_git_branch(Some("topic"))?;
        w.finish_turn(&turn, TurnStatus::Failed, 15)?;
        let cancelled = w.start_turn()?;
        w.segment(&cancelled)?;
        w.finish_turn(&cancelled, TurnStatus::Cancelled, 5)?;
        let tail = w.append("assistant", "retired")?;
        w.snapshots(&tail, &json!([{"id":"gone","systemPrompt":"unreferenced"}]))?;
        w.rewind_from(&tail)?;
        Ok(message)
    })?;
    session.save_mcp(
        &message,
        "child",
        &json!({"content":[{"type":"text","text":"result"}],"_meta":{"trace":"ok"}}),
        false,
    )?;
    let history = db.executor_history("source")?;
    history.add_segment_input(
        &message,
        "child",
        &deepseek_code_core::services::input_queue::DeliveredInput {
            text: "queued".into(),
            content: json!("queued"),
            origin: "user".into(),
            message_id: None,
        },
    )?;
    history.save_segment(
        &message,
        deepseek_code_core::db::session_store::SegmentUpdate {
            thinking: Some("exact\nthought"),
            ..Default::default()
        },
    )?;
    let fork = session.branch("fork", Branch::Full)?;
    let (running, segment) = session.write(|w| {
        let turn = w.start_turn()?;
        let segment = w.segment(&turn)?;
        w.message_content(&segment, "committed prefix")?;
        Ok((turn, segment))
    })?;
    session.set_archived(true)?;
    let projection = session.read(|r| r.projection())?;
    let source = decode(&stdout(root.path(), "source")?)?;
    let copied = decode(&stdout(root.path(), "fork")?)?;
    for rows in [&source, &copied] {
        links(rows);
        let origins = of(rows, "tool_origin");
        assert_eq!(origins.len(), 1);
        let tools = of(rows, "tool_call");
        for (name, field) in [("parent", "parent_tool_call_id"), ("child", "tool_call_id")] {
            assert_eq!(
                origins[0][field],
                tools.iter().find(|t| t["name"] == name).unwrap()["id"]
            );
        }
        let messages = of(rows, "message");
        let user = messages.iter().find(|r| r["role"] == "user").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(user["user_content_json"].as_str().unwrap())?,
            session.read(|r| r.user_content("user"))?
        );
        let assistant = messages.iter().find(|r| r["content"] == large).unwrap();
        assert_eq!(assistant["thinking"], "exact\nthought");
        assert!(messages
            .windows(2)
            .all(|m| m[0]["sort_order"].as_i64() <= m[1]["sort_order"].as_i64()));
        assert!(messages.iter().all(|m| m["content"] != "retired"));
        let blobs = of(rows, "request_blob");
        assert_eq!(blobs.len(), 1);
        let body = blobs[0]["body"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<String>(body)?, large);
        assert_eq!(blobs[0]["hash"], hash(body.as_bytes()));
        let snapshots: Vec<Value> = serde_json::from_str(
            of(rows, "request_snapshot")[0]["requests_json"]
                .as_str()
                .unwrap(),
        )?;
        assert_eq!(snapshots.len(), 2);
        let requests = of(rows, "model_request");
        for r in snapshots {
            assert_eq!(r["systemPromptRef"], blobs[0]["hash"]);
            assert!(requests
                .iter()
                .any(|n| n["id"] == r["id"] && n["message_id"] == assistant["id"]));
        }
        assert_eq!(of(rows, "tool_note")[0]["note"], "note");
        assert_eq!(of(rows, "tool_duration")[0]["duration_ms"], 52);
        assert!(of(rows, "delivered_inputs")[0]["inputs_json"]
            .as_str()
            .unwrap()
            .contains("queued"));
        assert!(of(rows, "mcp_output")[0]["content_json"]
            .as_str()
            .unwrap()
            .contains("result"));
        assert_eq!(of(rows, "attachment")[0]["source"], "/external/file");
        let turns = of(rows, "turn");
        for status in ["failed", "cancelled"] {
            assert!(turns
                .iter()
                .any(|r| r["status"] == status && r["ended_at"].is_string()));
        }
    }
    assert_eq!(of(&source, "diff")[0]["original_content"], "old");
    assert_eq!(
        of(&source, "approval")[0]["decision_reason"],
        "fixture decision"
    );
    assert!(of(&source, "local_setting")
        .iter()
        .any(|r| r["value"].as_str().unwrap().contains("finish export")));
    assert!(of(&source, "permission_authority")
        .iter()
        .any(|r| r["source"] == "approved_plan"));
    let plan = &of(&source, "approved_plan_boundary")[0]["message_id"];
    assert!(of(&source, "message")
        .iter()
        .any(|r| r["id"] == *plan && r["is_compact_boundary"] == true));
    let h = &source[0];
    assert_eq!(h["status"], "running");
    assert_eq!(h["archived"], true);
    assert_eq!(h["incarnation"], session.incarnation());
    assert_eq!(h["content_revision"], projection.revision);
    assert_eq!(h["message_count"], of(&source, "message").len());
    assert_eq!(h["title"], "export title");
    assert_eq!(h["provider"], "provider");
    assert_eq!(h["model"], "model");
    assert!(h["metadata_revision"].is_i64() && h["project_id"].is_string());
    let turn = of(&source, "turn")
        .into_iter()
        .find(|t| t["id"] == running)
        .unwrap();
    assert_eq!(turn["status"], "running");
    assert!(turn["ended_at"].is_null() && turn["elapsed_ms"].is_null());
    assert!(of(&source, "message").iter().any(|m| m["id"] == segment
        && m["turn_id"] == running
        && m["content"] == "committed prefix"));
    assert_eq!(copied[0]["incarnation"], fork.incarnation());
    for t in of(&copied, "turn") {
        assert!(of(&source, "turn")
            .iter()
            .any(|p| p["id"] == t["source_turn_id"] && p["id"] != t["id"]));
    }
    assert_eq!(of(&source, "agent")[0]["definition"], "agent definition");
    Ok(())
}

#[test]
fn pipe_pins_committed_prefix_and_interruption_has_no_footer() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let session = stores.create("pipe", Default::default())?;
    let payload = "bounded output 界\n".repeat(100000);
    session.write(|w| w.append("assistant", &payload))?;
    let expected = stdout(root.path(), "pipe")?;
    let mut child = export(root.path(), "pipe").stdout(Stdio::piped()).spawn()?;
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut copy = Vec::new();
    reader.read_until(b'\n', &mut copy)?;
    let header: Value = serde_json::from_slice(&copy)?;
    assert_eq!(header["type"], "header");
    // Output exceeds pipe capacity. This separate writer commits while the
    // exporter is blocked writing the old snapshot to our pipe.
    session.write(|w| w.append("user", "later append"))?;
    reader.read_to_end(&mut copy)?;
    assert!(child.wait()?.success());
    assert_eq!(copy, expected);
    let prior = decode(&copy)?;
    assert_eq!(of(&prior, "message").len(), 1);
    assert_eq!(of(&prior, "message")[0]["content"], payload);
    assert_eq!(
        of(&decode(&stdout(root.path(), "pipe")?)?, "message").len(),
        2
    );

    let mut child = export(root.path(), "pipe")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut truncated = Vec::new();
    reader.read_until(b'\n', &mut truncated)?;
    drop(reader);
    let output = child.wait_with_output()?;
    assert!(!output.status.success());
    assert_eq!(
        decode(&truncated).unwrap_err().to_string(),
        "missing end record"
    );
    let mut damaged = expected;
    let pos = damaged.windows(9).position(|s| s == b"assistant").unwrap();
    damaged[pos] = b'A';
    assert_eq!(decode(&damaged).unwrap_err().to_string(), "invalid footer");
    session.delete()?;
    Ok(())
}

#[test]
fn publish_named_file_once_and_clean_up_after_failure() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    db.create("file", Default::default())?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("backup.jsonl");
    let named = |id| {
        export(root.path(), id)
            .current_dir(dir.path())
            .args(["--output", "backup.jsonl"])
            .output()
    };
    let out = named("file")?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert_eq!(
        decode(&fs::read(&path)?)?,
        decode(&stdout(root.path(), "file")?)?
    );
    fs::write(&path, b"already here")?;
    let collision = named("file")?;
    assert!(
        !collision.status.success() && collision.stdout.is_empty() && !collision.stderr.is_empty()
    );
    assert_eq!(fs::read(&path)?, b"already here");
    fs::remove_file(&path)?;
    let missing = named("absent")?;
    assert!(!missing.status.success() && missing.stdout.is_empty() && !missing.stderr.is_empty());
    assert_eq!(fs::read_dir(dir.path())?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn export_sessions_generated_by_both_streaming_protocols() -> Result<()> {
    use axum::{routing::post, Router};
    for protocol in ["openai", "anthropic"] {
        let root = tempfile::tempdir()?;
        let data = root.path().join("data");
        fs::create_dir(&data)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let (route, frames) = if protocol == "anthropic" {
            (
                "/v1/messages",
                vec![
                    json!({"type":"message_start","message":{"type":"message","id":"resp","role":"assistant","model":"model","content":[]}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello from fixture"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                    json!({"type":"message_stop"}),
                ],
            )
        } else {
            (
                "/v1/chat/completions",
                vec![
                    json!({"id":"resp","model":"model","choices":[{"index":0,"delta":{"content":"hello from fixture"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":3}}),
                ],
            )
        };
        let mut body = frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<String>();
        if protocol == "openai" {
            body.push_str("data: [DONE]\n\n");
        }
        let app = Router::new().route(
            route,
            post(move || async move { ([("content-type", "text/event-stream")], body) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        fs::write(data.join("providers.json"),json!([{"id":"fixture","name":"fixture","api_key":"excluded-provider-secret","base_url":url,
            "api_format":protocol,"enabled":true,"models":[{"id":"model","name":"model","max_tokens":4096,"supports_web_search":false,"supports_vision":false,"enabled":true}]}]).to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        for (key, _) in std::env::vars().filter(|(k, _)| {
            k.starts_with("DEEPSEEK_CODE")
                || k.starts_with("ANTHROPIC_")
                || k.starts_with("OPENAI_")
        }) {
            cmd.env_remove(key);
        }
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            cmd.arg("--data-dir")
                .arg(&data)
                .arg("--path")
                .arg(root.path())
                .args([
                    "--print",
                    "--provider",
                    "fixture",
                    "--model",
                    "model",
                    "--session-id",
                    &id,
                    "export this",
                ])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        server.abort();
        let _ = server.await;
        let out = out??;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bytes = stdout(&data, &id)?;
        assert!(!std::str::from_utf8(&bytes)?.contains("excluded-provider-secret"));
        let rows = decode(&bytes)?;
        links(&rows);
        assert!(of(&rows, "message")
            .iter()
            .any(|m| m["role"] == "assistant" && m["content"] == "hello from fixture"));
        assert!(of(&rows, "turn").iter().any(|t| t["status"] == "completed"));
        let req = of(&rows, "model_request");
        assert!(
            req.iter().any(|r| r["api_format"] == protocol),
            "missing normalized {protocol} request"
        );
        assert!(!of(&rows, "request_snapshot").is_empty());
    }
    Ok(())
}
