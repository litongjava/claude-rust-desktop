use super::*;

#[tokio::test(start_paused = true)]
async fn durable_permission_survives_detach_delay_and_cancels_without_execution() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    for outcome in ["approve", "disconnect", "shutdown", "ordinary-timeout"] {
        let root = tempfile::tempdir().unwrap();
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (tx, mut events) = mpsc::channel(16);
        let mut executor = ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        );
        executor.durable_user_input = outcome != "ordinary-timeout";
        let waiters = executor.get_answer_waiters();
        let task = tokio::spawn(async move {
            executor
                .execute_ask_user_confirmation(
                    "Write",
                    &json!({"file_path":"approval.txt","content":"exactly once"}),
                    "write-once",
                    "approve",
                )
                .await
        });
        let EngineEvent::ToolPermission { request_id, .. } = events.recv().await.unwrap() else {
            panic!("Missing permission request");
        };
        tokio::time::advance(Duration::from_secs(401)).await;
        tokio::task::yield_now().await;
        assert!(!root.path().join("approval.txt").exists());
        if outcome == "ordinary-timeout" {
            assert!(task.await.unwrap().2);
            assert!(waiters.lock().await.is_empty());
            continue;
        }
        assert!(
            !task.is_finished(),
            "Detached worker must still await this exact request"
        );
        let sender = waiters.lock().await.remove(&request_id).unwrap();
        match outcome {
            "approve" => {
                sender.send("yes".into()).unwrap();
                let (_, output, failed) = task.await.unwrap();
                assert!(!failed, "{output}");
                assert_eq!(
                    std::fs::read_to_string(root.path().join("approval.txt")).unwrap(),
                    "exactly once"
                );
                assert!(
                    waiters.lock().await.remove(&request_id).is_none(),
                    "Approval cannot be delivered twice"
                );
            }
            "disconnect" => {
                drop(sender);
                assert!(task.await.unwrap().2);
            }
            "shutdown" => {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                assert!(sender.send("yes".into()).is_err());
            }
            _ => unreachable!(),
        }
        if outcome != "approve" {
            assert!(!root.path().join("approval.txt").exists());
        }
    }
}
