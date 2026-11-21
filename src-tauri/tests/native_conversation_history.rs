use deepseek_desktop_tauri_lib::{bridge::BridgeServer, commands, runtime::RuntimePaths};
use serde_json::{json, Value};
use std::time::Duration;
use tauri::Manager;

#[tokio::test]
async fn native_and_bridge_share_conversation_history() {
    let root = tempfile::tempdir().unwrap();
    let db = RuntimePaths::new(root.path().into())
        .unwrap()
        .open_database()
        .unwrap();
    let app = tauri::test::mock_app();
    app.manage(db.clone());
    let native = || app.handle().clone();
    let server = BridgeServer::new(root.path().into(), db.clone());
    let key = server.get_api_key().to_string();
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let serving = tokio::spawn(async move { server.start(port).await });
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}/api/conversations");
    let list = || client.get(&base).header("x-api-key", &key);
    tokio::time::timeout(Duration::from_secs(10), async {
        while list().send().await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let a = commands::native_create_conversation(native(), "model".into(), Some("Native".into()))
        .await
        .unwrap();
    let rows: Value = list()
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(rows["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == a.id));
    let b: Value = client
        .post(&base)
        .header("x-api-key", &key)
        .json(&json!({"model":"model","title":"Bridge"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = b["id"].as_str().unwrap();
    assert!(commands::native_list_conversations(native())
        .await
        .unwrap()
        .iter()
        .any(|row| row.id == id));
    db.open_session(id)
        .unwrap()
        .write(|w| {
            let id = w.append("assistant", "saved answer")?;
            w.attachment(
                "file-id",
                &id,
                "notes.txt",
                "text",
                "text/plain",
                17,
                "upload",
            )
        })
        .unwrap();
    let messages = commands::native_get_messages(native(), id.into())
        .await
        .unwrap();
    assert_eq!(messages[0].content, "saved answer");
    let attachments = messages[0].attachments.as_ref().expect("stored attachment");
    assert_eq!(attachments.len(), 1);
    let file = &attachments[0];
    assert_eq!(
        (
            &*file.file_id,
            &*file.file_name,
            &*file.file_type,
            &*file.mime_type,
            file.size,
            file.source.as_deref()
        ),
        (
            "file-id",
            "notes.txt",
            "text",
            "text/plain",
            17,
            Some("upload")
        )
    );
    commands::native_delete_conversation(native(), id.into())
        .await
        .unwrap();
    assert!(db.conversation_metadata(id).unwrap().is_none());
    serving.abort();
    let _ = serving.await;
}

#[tokio::test]
async fn native_list_uses_private_child_workspace_binding() {
    let root = tempfile::tempdir().unwrap();
    let db = RuntimePaths::new(root.path().into())
        .unwrap()
        .open_database()
        .unwrap();
    let owner = db.create("owner", Default::default()).unwrap();
    let workspace = owner.workspace().unwrap();
    owner.derive_owned_child("child", &workspace).unwrap();
    let app = tauri::test::mock_app();
    app.manage(db);
    let rows = commands::native_list_conversations(app.handle().clone())
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "child")
            .unwrap()
            .workspace_path,
        workspace
    );
}

#[tokio::test]
async fn private_child_keeps_owner_workspace_after_newer_sync() {
    let root = tempfile::tempdir().unwrap();
    let db = RuntimePaths::new(root.path().into())
        .unwrap()
        .open_database()
        .unwrap();
    let owner = db.create("owner", Default::default()).unwrap();
    let workspace = owner.workspace().unwrap();
    let child = owner.derive_owned_child("child", &workspace).unwrap();
    let header = serde_json::from_value(json!({
        "id":"child", "title":"synced", "workspacePath":"/other-device/project",
        "createdAt":"2026-01-10T00:00:00Z", "updatedAt":"2099-01-10T00:00:01Z"
    }))
    .unwrap();
    db.sync_conversation("child", Some(&header), &[]).unwrap();
    assert_eq!(
        db.conversation_metadata("child")
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("synced")
    );
    assert!(db
        .conversation_metadata("child")
        .unwrap()
        .unwrap()
        .workspace_path
        .is_none());
    assert_eq!(child.workspace().unwrap(), workspace);
    let app = tauri::test::mock_app();
    app.manage(db);
    let rows = commands::native_list_conversations(app.handle().clone())
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "child")
            .unwrap()
            .workspace_path,
        workspace
    );
}
