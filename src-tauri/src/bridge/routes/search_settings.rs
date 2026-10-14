use super::super::AppState;
use crate::config::DeepSeekSearchConfig;
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde_json::{json, Value};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn failure(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message})))
}

fn public_settings(config: &DeepSeekSearchConfig) -> Value {
    json!({"baseUrl":config.base_url, "model":config.model, "hasApiKey":!config.api_key.trim().is_empty()})
}

fn patched_settings(
    previous: &DeepSeekSearchConfig,
    value: &Value,
) -> anyhow::Result<DeepSeekSearchConfig> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("搜索设置必须是 JSON 对象"))?;
    let mut next = previous.clone();
    for (field, target) in [("baseUrl", &mut next.base_url), ("model", &mut next.model)] {
        if let Some(value) = object.get(field) {
            *target = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{field} 必须是字符串"))?
                .trim()
                .into();
        }
    }
    let clear = match object.get("clearApiKey") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("clearApiKey 必须是布尔值"))?,
    };
    if let Some(value) = object.get("apiKey") {
        let key = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("apiKey 必须是字符串"))?
            .trim();
        if clear && !key.is_empty() {
            anyhow::bail!("不能同时设置和清除搜索 API Key");
        }
        // Empty password fields preserve stored credentials. Deletion is explicit.
        if !key.is_empty() {
            next.api_key = key.into();
        }
    }
    if clear {
        next.api_key.clear();
    }
    next.validate()?;
    Ok(next)
}

async fn get_settings(State(state): State<AppState>) -> ApiResult {
    let guard = state.config_manager.lock().await;
    let manager = guard
        .as_ref()
        .ok_or_else(|| failure(StatusCode::SERVICE_UNAVAILABLE, "配置服务暂不可用"))?;
    Ok(Json(public_settings(&manager.get_config().deepseek_search)))
}

async fn update_settings(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult {
    let mut guard = state.config_manager.lock().await;
    let manager = guard
        .as_mut()
        .ok_or_else(|| failure(StatusCode::SERVICE_UNAVAILABLE, "配置服务暂不可用"))?;
    let next = patched_settings(&manager.get_config().deepseek_search, &value)
        .map_err(|error| failure(StatusCode::BAD_REQUEST, &error.to_string()))?;
    manager
        .update_config(|config| config.deepseek_search = next.clone())
        .map_err(|_| {
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "保存搜索设置失败，请检查配置目录是否可写",
            )
        })?;
    Ok(Json(public_settings(&next)))
}

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/search-settings",
        get(get_settings).patch(update_settings),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_settings_preserve_replace_and_clear_key_without_echoing_it() {
        let initial = patched_settings(
            &DeepSeekSearchConfig::default(),
            &json!({"apiKey":"search-only-secret"}),
        )
        .unwrap();
        let changed = patched_settings(
            &initial,
            &json!({"model":"another-search-model","baseUrl":"https://search.example/v1"}),
        )
        .unwrap();
        assert_eq!(changed.api_key, initial.api_key);
        assert_eq!(
            patched_settings(&changed, &json!({"apiKey":""}))
                .unwrap()
                .api_key,
            initial.api_key
        );
        let output = public_settings(&changed);
        assert_eq!(output["hasApiKey"], true);
        assert!(output.get("apiKey").is_none());
        assert!(!output.to_string().contains("search-only-secret"));
        let cleared = patched_settings(&changed, &json!({"clearApiKey":true})).unwrap();
        assert!(cleared.api_key.is_empty());
        for invalid in [
            json!({"apiKey":true}),
            json!({"clearApiKey":"yes"}),
            json!({"model":""}),
            json!({"baseUrl":"https://user:key@example.com"}),
            json!({"baseUrl":"file:///tmp"}),
            json!({"apiKey":"new","clearApiKey":true}),
        ] {
            assert!(patched_settings(&changed, &invalid).is_err());
        }
    }

    #[test]
    fn independent_search_settings_roundtrip_without_changing_chat_providers() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = crate::config::ConfigManager::new(dir.path().into());
        let next = patched_settings(&manager.get_config().deepseek_search, &json!({"apiKey":"search-key", "model":"configured-search", "baseUrl":"https://search.example/anthropic/v1"})).unwrap();
        manager
            .update_config(|config| config.deepseek_search = next)
            .unwrap();
        let reloaded = crate::config::ConfigManager::new(dir.path().into());
        assert_eq!(reloaded.get_config().deepseek_search.api_key, "search-key");
        assert_eq!(
            reloaded.get_config().deepseek_search.model,
            "configured-search"
        );
        assert!(reloaded.get_config().providers.is_empty());
    }
}
