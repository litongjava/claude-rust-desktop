use crate::db::session_store::{FeishuChatMapping, Stores};
use std::sync::Arc;

pub async fn feishu_get_or_create_conversation_inner(
    db: &Stores,
    chat_id: String,
    title: Option<String>,
) -> Result<FeishuChatMapping, String> {
    db.feishu_conversation(&chat_id, title)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feishu_get_or_create_conversation(
    db: tauri::State<'_, Arc<Stores>>,
    chat_id: String,
    title: Option<String>,
) -> Result<FeishuChatMapping, String> {
    feishu_get_or_create_conversation_inner(&db, chat_id, title).await
}

#[tauri::command]
pub async fn feishu_list_conversations(
    db: tauri::State<'_, Arc<Stores>>,
) -> Result<Vec<FeishuChatMapping>, String> {
    db.feishu_mappings().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feishu_delete_conversation(
    db: tauri::State<'_, Arc<Stores>>,
    chat_id: String,
) -> Result<(), String> {
    db.delete_feishu_mapping(&chat_id)
        .map_err(|e| e.to_string())
}
