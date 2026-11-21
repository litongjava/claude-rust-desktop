use deepseek_code_core::runtime::RuntimePaths;
use serde_json::json;

#[test]
fn request_snapshots_survive_reopen_and_follow_message_deletion() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = RuntimePaths::new(temp.path().into())?;
    let snapshot = json!([{"id":"request-1", "number":1, "systemPrompt":"actual injected project instructions", "systemMessages":[{"role":"system","content":"retrieved memory"}], "tools":[{"name":"Read","input_schema":{"type":"object"}}]}]);
    let assistant = {
        let shard = paths.open_stores()?.create("conv", Default::default())?;
        let id = shard.write(|w| {
            let id = w.append("assistant", "")?;
            w.snapshots(&id, &snapshot)?;
            Ok(id)
        })?;
        assert_eq!(shard.read(|r| r.requests(&id))?, snapshot);
        shard.write(|w| w.message_content(&id, "answer"))?;
        id
    };
    let stores = paths.open_stores()?;
    let shard = stores.open_session("conv")?;
    assert_eq!(shard.read(|r| r.requests(&assistant))?, snapshot);
    assert_eq!(shard.read(|r| r.messages())?[0].content, "answer");
    shard.write(|w| w.rewind_from(&assistant))?;
    assert_eq!(shard.read(|r| r.requests(&assistant))?, json!([]));
    let next = shard.write(|w| {
        let id = w.append("assistant", "")?;
        w.snapshots(&id, &snapshot)?;
        Ok(id)
    })?;
    shard.delete()?;
    assert!(shard.read(|r| r.requests(&next)).is_err());
    let replacement = stores.create("conv", Default::default())?;
    assert_eq!(replacement.read(|r| r.requests(&next))?, json!([]));
    Ok(())
}

#[test]
fn model_response_metrics_are_independent_sql_columns_and_survive_reopen() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = RuntimePaths::new(temp.path().into())?;
    let mut snapshot = json!({"id":"request-27", "number":27, "model":"test-model", "providerId":"test-provider", "startedAt":1000, "status":"running", "responseBlocks":[]});
    let assistant = {
        let shard = paths.open_stores()?.create("conv", Default::default())?;
        let id = shard.write(|w| {
            let id = w.append("assistant", "")?;
            w.snapshots(&id, &json!([snapshot]))?;
            Ok(id)
        })?;
        assert!(shard
            .read(|r| r.request_metrics("request-27"))?
            .unwrap()
            .output_tokens
            .is_none());
        snapshot.as_object_mut().unwrap().extend(json!({"status":"completed", "endedAt":3070, "firstTokenAt":1976,
                "totalDurationMs":2070,"ttftMs":976,"generationMs":1094,"throughputTokensPerSecond":278.5,
                "inputTokens":100,"outputTokens":305,"reasoningTokens":44,"contentTokens":261,"totalTokens":405,
                "responseBlocks":[{"type":"thinking","text":"Think"},{"type":"text","text":"Answer"},{"type":"tool_call","id":"call","name":"Read","input":{"path":"file"}}],
                "usage":{"prompt_tokens":100,"completion_tokens":305}}).as_object().unwrap().clone());
        shard.write(|w| w.snapshots(&id, &json!([snapshot])))?;
        assert_eq!(shard.read(|r| r.projection())?.request_count, 1);
        id
    };
    let session = paths.open_stores()?.open_session("conv")?;
    let metrics = session.read(|r| r.request_metrics("request-27"))?.unwrap();
    assert_eq!(
        (
            metrics.output_tokens,
            metrics.reasoning_tokens,
            metrics.content_tokens,
            metrics.ttft_ms,
            metrics.generation_ms
        ),
        (Some(305), Some(44), Some(261), Some(976), Some(1094))
    );
    assert_eq!(metrics.status, "completed");
    assert_eq!(metrics.response_blocks[2]["name"], "Read");
    assert_eq!(session.read(|r| r.requests(&assistant))?, json!([snapshot]));
    session.write(|w| w.rewind_from(&assistant))?;
    assert!(session.read(|r| r.request_metrics("request-27"))?.is_none());
    Ok(())
}
