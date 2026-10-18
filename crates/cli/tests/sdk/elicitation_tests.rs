use super::*;

fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("data")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elicitation.cjs");
    std::fs::write(root.join("data/mcp-servers.json"),json!({"mcpServers":{"fixture":{"command":"node","args":[script],"env":{"ELICITATION_RESULT":root.join("reply.json")},"enabled":true}}}).to_string()).unwrap();
}

#[tokio::test]
async fn elicitation_stdio_sdk_hooks_and_control_complete_roundtrip() {
    for scenario in [
        "accept", "decline", "cancel", "user", "modify", "block", "invalid",
    ] {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let server = start_server(false, vec![("mcp__fixture__elicit", json!({}))]).await;
        let mut client = Client::start(root.path(), &server, &[]);
        assert_eq!(
            initialize(
                &mut client,
                json!([]),
                json!({
                    "Elicitation":[{"matcher":"^fixture$","hookCallbackIds":["pre"]}],
                    "ElicitationResult":[{"matcher":"^fixture$","hookCallbackIds":["post"]}]
                })
            )
            .await["response"]["subtype"],
            "success"
        );
        client.send(user("Run the elicitation tool")).await;
        let mut asked = 0;
        let mut events = Vec::new();
        loop {
            let frame = next(&mut client).await;
            if frame["type"] == "result" {
                break;
            }
            if frame["type"] != "control_request" {
                continue;
            }
            let response = match frame["request"]["subtype"].as_str().unwrap() {
                "can_use_tool" => json!({"behavior":"allow"}),
                "elicitation" => {
                    asked += 1;
                    assert_eq!(frame["request"]["mcp_server_name"], "fixture");
                    assert_eq!(
                        frame["request"]["requested_schema"]["required"],
                        json!(["number"])
                    );
                    json!({"action":"accept","content":{"number":4}})
                }
                "hook_callback" => {
                    let input = &frame["request"]["input"];
                    events.push(input["hook_event_name"].as_str().unwrap().to_owned());
                    assert_eq!(input["mcp_server_name"], "fixture");
                    if input["hook_event_name"] == "Elicitation" {
                        assert_eq!(input["message"], "Choose a number");
                        match scenario {
                            "user" => json!({}),
                            "decline" | "cancel" => {
                                json!({"hookSpecificOutput":{"hookEventName":"Elicitation","action":scenario}})
                            }
                            _ => {
                                json!({"hookSpecificOutput":{"hookEventName":"Elicitation","action":"accept","content":{"number":2}}})
                            }
                        }
                    } else {
                        assert!(input.get("requested_schema").is_none());
                        match scenario {
                            "modify" => {
                                json!({"hookSpecificOutput":{"hookEventName":"ElicitationResult","action":"accept","content":{"number":8}}})
                            }
                            "invalid" => {
                                json!({"hookSpecificOutput":{"hookEventName":"ElicitationResult","action":"accept","content":{"number":"bad"}}})
                            }
                            "block" => json!({"decision":"block","reason":"fixture blocks"}),
                            _ => json!({}),
                        }
                    }
                }
                other => panic!("Unexpected request: {other}"),
            };
            answer(&mut client, &frame, response).await;
        }
        assert_eq!(asked, usize::from(scenario == "user"));
        assert_eq!(events, vec!["Elicitation", "ElicitationResult"]);
        let reply: Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join("reply.json")).unwrap())
                .unwrap();
        assert_eq!(reply["id"], "server-elicitation");
        if scenario == "invalid" {
            assert_eq!(reply["error"]["code"], -32602);
        } else {
            assert_eq!(
                reply["result"]["action"],
                match scenario {
                    "decline" | "block" => "decline",
                    "cancel" => "cancel",
                    _ => "accept",
                }
            );
            if matches!(scenario, "accept" | "modify" | "user") {
                assert_eq!(
                    reply["result"]["content"]["number"],
                    match scenario {
                        "modify" => 8,
                        "user" => 4,
                        _ => 2,
                    }
                );
            }
        }
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn elicitation_noninteractive_without_hooks_cancels() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let server = start_server(false, vec![("mcp__fixture__elicit", json!({}))]).await;
    let output = Command::new(env!("CARGO_BIN_EXE_ccl"))
        .args([
            "--data-dir",
            root.path().join("data").to_str().unwrap(),
            "--path",
            root.path().to_str().unwrap(),
            "--base-url",
            &server.url,
            "--api-format",
            "openai",
            "--api-key-env",
            "ELICITATION_FIXTURE_KEY",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
            "--print",
            "Run elicitation",
        ])
        .env("ELICITATION_FIXTURE_KEY", "fixture-only")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join("reply.json")).unwrap())
            .unwrap();
    assert_eq!(reply["result"], json!({"action":"cancel"}));
}

#[tokio::test]
async fn elicitation_sdk_hosted_server_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    let server = start_server(false, vec![("mcp__first__echo", json!({"value":"elicit"}))]).await;
    let mut client = Client::start(root.path(), &server, &[]);
    assert_eq!(
        initialize(&mut client, json!(["first"]), json!({})).await["response"]["subtype"],
        "success"
    );
    client.send(user("Run SDK elicitation")).await;
    let mut call_id = Value::Null;
    let mut replied = false;
    loop {
        let frame = next(&mut client).await;
        if frame["type"] == "result" {
            break;
        }
        if frame["type"] != "control_request" {
            continue;
        }
        let response = match frame["request"]["subtype"].as_str().unwrap() {
            "can_use_tool" => json!({"behavior":"allow"}),
            "elicitation" => json!({"action":"accept","content":{"number":7}}),
            "mcp_message" => {
                let message = &frame["request"]["message"];
                if message["method"] == "tools/call" {
                    call_id = message["id"].clone();
                    json!({"mcp_response":{"jsonrpc":"2.0","id":77,"method":"elicitation/create","params":{"message":"SDK number","requestedSchema":{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}}}})
                } else if message.get("method").is_none() {
                    assert_eq!(message["id"], 77);
                    assert_eq!(
                        message["result"],
                        json!({"action":"accept","content":{"number":7}})
                    );
                    replied = true;
                    json!({"mcp_response":{"jsonrpc":"2.0","id":call_id,"result":{"content":[{"type":"text","text":"SDK_ELICITATION_COMPLETE"}]}}})
                } else {
                    mcp_response(&frame, "")
                }
            }
            other => panic!("Unexpected request {other}"),
        };
        answer(&mut client, &frame, response).await;
    }
    assert!(replied);
    assert!(client.finish().await.0.success());
}
