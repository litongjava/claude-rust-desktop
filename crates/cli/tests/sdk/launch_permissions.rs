use super::*;

#[tokio::test]
async fn cli_launch_rules_apply_to_sdk_and_do_not_survive_a_new_launch() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(anthropic, Mode::Approval).await;
        let mut client = Client::start(
            root.path(),
            &server,
            &["--permission-mode", "ask", "--allowedTools", "Read,Write"],
        );
        client.send(user("Write the fixture file")).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        assert!(root.path().join("approval.txt").is_file());
        let session = result["session_id"].as_str().unwrap().to_owned();
        assert!(client.finish().await.0.success());
        std::fs::remove_file(root.path().join("approval.txt")).unwrap();
        let mut client = Client::start(
            root.path(),
            &server,
            &["--permission-mode", "ask", "--resume", &session],
        );
        client.send(user("Write after a new launch")).await;
        let request = client.frame("control_request").await;
        assert_eq!(request["request"]["tool_name"], "Write");
        client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":"deny"}}})).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(!root.path().join("approval.txt").exists());
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn cli_launch_denial_wins_over_bypass_allow_and_reaches_owned_children() {
    for anthropic in [false, true] {
        for child in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let server = Server::start(
                anthropic,
                if child {
                    Mode::RuntimeAgent
                } else {
                    Mode::Approval
                },
            )
            .await;
            let mut client = Client::start(
                root.path(),
                &server,
                &[
                    "--permission-mode",
                    "bypass",
                    "--allowed-tools",
                    "Write",
                    "--disallowed-tools",
                    "Write",
                ],
            );
            if child {
                client.send(json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize","agents":{"sdk-child":{"description":"fixture child","prompt":"Perform the requested task","tools":["Write"]}}}})).await;
                assert_eq!(
                    client.frame("control_response").await["response"]["subtype"],
                    "success"
                );
            }
            client.send(user("Write a fixture")).await;
            let result = client.result().await;
            assert_eq!(result["is_error"], false, "{result}");
            assert!(!root.path().join("approval.txt").exists());
            assert!(!root.path().join("child.txt").exists());
            assert!(server
                .fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.to_string().contains("Explicit tool deny rule")));
            assert!(client.finish().await.0.success());
        }
    }
}
