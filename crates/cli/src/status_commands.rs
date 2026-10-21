//! CLI rendering of read-only shared runtime observations.
use crate::session::Session;
use anyhow::{ensure, Result};
use claude_core::services::{
    anthropic_auth,
    runtime_status::{self, RuntimeStatus},
    session_worker,
};
use std::time::Duration;

pub async fn run(session: &Session, argument: &str) -> Result<()> {
    ensure!(
        matches!(argument.trim(), "" | "--check"),
        "Usage: /status [--check]"
    );
    let provider = session.selected_provider().await?;
    let (worker, probe) = tokio::join!(
        session_worker::runtime_status(&session.paths.data_dir, &session.id),
        async {
            if argument.trim() == "--check" {
                Some(
                    runtime_status::http_reachability(
                        &provider.provider.base_url,
                        Duration::from_secs(3),
                    )
                    .await,
                )
            } else {
                None
            }
        }
    );
    let (scope, tools) = match worker.tools {
        Some(tools) => ("persistent worker", Some(tools)),
        None => (
            "frontend process only; worker tools not observed",
            tokio::time::timeout(
                Duration::from_secs(1),
                session.runtime_status_source().snapshot(),
            )
            .await
            .ok(),
        ),
    };
    let account = if anthropic_auth::is_oauth_marker(&provider.provider.api_key) {
        match anthropic_auth::status(&session.paths.data_dir) {
            Ok(status) if status.authenticated => {
                if status
                    .expires_at
                    .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp())
                {
                    "OAuth credentials stored, expired locally; refresh not checked"
                } else {
                    "OAuth credentials stored; server authentication not checked"
                }
            }
            Ok(_) => "OAuth selected but no local credentials",
            Err(_) => "OAuth credential status unavailable",
        }
    } else if provider.provider.api_key.trim().is_empty() {
        "No API credential configured"
    } else {
        "API credential configured locally; authentication not checked"
    };
    let api = if let Some(check) = probe {
        format!(
            "{}{}; HTTP transport only, authentication and inference not verified",
            state_label(&check.state),
            check
                .http_status
                .map(|status| format!(" (HTTP {status})"))
                .unwrap_or_default()
        )
    } else {
        "Not checked; /status --check probes HTTP reachability without credentials or a model request".into()
    };
    let mut output = format!(
        "DeepSeek Code {version}\nSession: {id}\nProject: {workspace}\nData: {data}\nProvider: {provider} ({provider_id})\nModel: {model}\nRouting: {routing}\nPermissions (frontend): {permissions}\nAccount: {account}\nAPI origin: {origin}\nAPI: {api}\nWorker: {worker}{pid}\nTool observations: {scope}\n",
        version = env!("CARGO_PKG_VERSION"),
        id = runtime_status::label(&session.id),
        workspace = runtime_status::label(&session.workspace.to_string_lossy()),
        data = runtime_status::label(&session.paths.data_dir.to_string_lossy()),
        provider = runtime_status::label(&provider.provider.name),
        provider_id = runtime_status::label(&session.provider_id),
        model = runtime_status::label(&session.model),
        routing = if session.auto_route { "automatic" } else { "selected provider" },
        permissions = session.permission_mode(),
        origin = runtime_status::origin(&provider.provider.base_url),
        worker = state_label(&worker.state),
        pid = worker.pid.map(|pid| format!(" (PID {pid})")).unwrap_or_default(),
    );
    if let Some(tools) = tools {
        output.push_str(&render_tools(&tools));
    } else {
        output.push_str("Tool snapshot unavailable (inspection timed out)\n");
    }
    eprint!("{output}");
    Ok(())
}
fn state_label(state: &str) -> String {
    runtime_status::label(state).replace('_', " ")
}

fn render_tools(tools: &RuntimeStatus) -> String {
    let mut output = format!(
        "Native tools: {} registered (not health-probed)\nMCP: {}\n",
        tools.native_tools,
        state_label(&tools.mcp_configuration)
    );
    if tools.mcp.is_empty() {
        output.push_str("  No MCP entries observed\n");
    }
    for server in &tools.mcp {
        output.push_str(&format!(
            "  {}: {}; {} tools{}\n",
            runtime_status::label(&server.name),
            state_label(&server.state),
            server.tools,
            server
                .pid
                .map(|pid| format!("; PID {pid}"))
                .unwrap_or_default()
        ));
    }
    output.push_str(
        "  MCP running states are manager observations; no protocol health probe was sent.\n",
    );
    output.push_str(&format!(
        "LSP: {}; {} configured, {} idle pooled servers\n",
        state_label(&tools.lsp_state),
        tools.lsp_configured,
        tools.lsp_pooled.len()
    ));
    for server in &tools.lsp_pooled {
        output.push_str(&format!(
            "  {}: {}{}\n",
            server.name,
            state_label(&server.state),
            server
                .pid
                .map(|pid| format!("; PID {pid}"))
                .unwrap_or_default()
        ));
    }
    output.push_str("  LSP pool excludes in-flight requests; process presence does not verify protocol responsiveness.\n");
    output
}
