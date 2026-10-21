//! Request-local elicitation context; never retain a session on a shared connector.
use crate::services::tool_hooks::{HookEvent, HookRunner};
use anyhow::{ensure, Context, Result};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::Arc;

pub type Ask = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;
pub struct RequestContext {
    pub hooks: Option<Arc<HookRunner>>,
    pub session: String,
    pub ask: Ask,
}
tokio::task_local! { pub static CONTEXT: RequestContext; }

pub fn input(server: &str, params: &Value) -> Result<Value> {
    ensure!(
        params["message"].is_string(),
        "Elicitation message must be text"
    );
    let mode = match params.get("mode") {
        None => "form",
        Some(value) => value.as_str().context("Elicitation mode must be text")?,
    };
    ensure!(
        mode == "form",
        "Unsupported elicitation mode: {mode}; URL elicitation is not implemented"
    );
    let schema = &params["requestedSchema"];
    crate::structured_output::validate_schema(schema)?;
    ensure!(
        schema["type"] == "object",
        "Elicitation schema must describe an object"
    );
    Ok(
        json!({"mcp_server_name":server,"message":params["message"],"mode":mode,"requested_schema":schema}),
    )
}

/// Terminal form input is content, never a permission or control envelope.
pub fn user_response(text: &str) -> Result<Value> {
    match text.trim() {
        "decline" => Ok(json!({"action":"decline"})),
        "cancel" | "" => Ok(json!({"action":"cancel"})),
        text => {
            let content: Value =
                serde_json::from_str(text).context("Enter a JSON object, decline, or cancel")?;
            ensure!(content.is_object(), "Elicitation content must be an object");
            Ok(json!({"action":"accept","content":content}))
        }
    }
}

pub fn validate_response(result: &Value, schema: &Value) -> Result<()> {
    ensure!(
        matches!(
            result["action"].as_str(),
            Some("accept" | "decline" | "cancel")
        ),
        "Invalid elicitation action"
    );
    if let Some(content) = result.get("content") {
        ensure!(content.is_object(), "Elicitation content must be an object");
    }
    if result["action"] == "accept" {
        let content = result
            .get("content")
            .context("Accepted elicitation requires content")?;
        crate::structured_output::validate_result(schema, &content.to_string())?;
    }
    Ok(())
}

async fn hooks(
    context: &RequestContext,
    event: HookEvent,
    payload: Value,
) -> Result<Option<Value>> {
    let Some(runner) = &context.hooks else {
        return Ok(None);
    };
    let outcome = runner.run(event, &context.session, payload).await?;
    for warning in &outcome.warnings {
        tracing::warn!("{warning}");
    }
    if outcome.blocked {
        return Ok(Some(json!({"action":"decline"})));
    }
    Ok(outcome.elicitation_response)
}

pub async fn handle(server: &str, params: Value) -> Result<Value> {
    let payload = input(server, &params)?;
    let context = CONTEXT
        .try_with(|context| RequestContext {
            hooks: context.hooks.clone(),
            session: context.session.clone(),
            ask: context.ask.clone(),
        })
        .ok();
    let Some(context) = context else {
        return Ok(json!({"action":"cancel"}));
    };
    // Failed hooks do not provide a decision; the normal user path remains authoritative.
    let decision = hooks(&context, HookEvent::Elicitation, payload.clone())
        .await
        .unwrap_or_else(|error| {
            tracing::warn!("Elicitation hook: {error:#}");
            None
        });
    let mut result = match decision {
        Some(value) => value,
        None => (context.ask)(payload)
            .await
            .unwrap_or_else(|_| json!({"action":"cancel"})),
    };
    validate_response(&result, &params["requestedSchema"])?;
    let mut result_input =
        json!({"mcp_server_name":server,"mode":"form","action":result["action"]});
    if let Some(content) = result.get("content") {
        result_input["content"] = content.clone();
    }
    match hooks(&context, HookEvent::ElicitationResult, result_input).await {
        Ok(Some(mut replacement)) => {
            if replacement["action"] == "accept" && replacement.get("content").is_none() {
                if let Some(content) = result.get("content") {
                    replacement["content"] = content.clone();
                }
            }
            result = replacement;
        }
        Err(error) => tracing::warn!("ElicitationResult hook: {error:#}"),
        _ => {}
    }
    validate_response(&result, &params["requestedSchema"])?;
    if result["action"] != "accept" {
        result.as_object_mut().unwrap().remove("content");
    }
    Ok(result)
}

/// Preserve the server's JSON-RPC ID, including string IDs.
pub async fn reply(server: &str, request: &Value) -> Value {
    let response = if request["method"] == "elicitation/create" {
        match handle(server, request["params"].clone()).await {
            Ok(result) => json!({"result":result}),
            Err(error) => json!({"error":{"code":-32602,"message":format!("{error:#}")}}),
        }
    } else {
        json!({"error":{"code":-32601,"message":"Unsupported MCP client request"}})
    };
    let mut response = response;
    response["jsonrpc"] = json!("2.0");
    response["id"] = request["id"].clone();
    response
}
