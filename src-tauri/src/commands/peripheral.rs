use anyhow::{ensure, Context, Result};
use claude_core::{
    runtime::RuntimePaths,
    services::{lifecycle, team, voice, Credentials, ServiceContext},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::Manager;
use tokio::sync::watch;

struct DesktopCredentials(tauri::AppHandle);
impl Credentials for DesktopCredentials {
    fn get(&self, name: &str) -> Result<String> {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
        crate::secure_storage::SecureKeyStore::new()
            .get_service_credential(&self.0, name)?
            .filter(|s| !s.trim().is_empty())
            .with_context(|| format!("Credential {name} is not configured"))
    }
}
fn context(app: &tauri::AppHandle) -> Result<ServiceContext> {
    Ok(ServiceContext {
        paths: RuntimePaths::new(app.path().app_data_dir()?)?,
        db: app
            .state::<Arc<claude_core::db::DbManager>>()
            .inner()
            .clone(),
        credentials: Arc::new(DesktopCredentials(app.clone())),
    })
}
#[derive(Default)]
pub struct PeripheralHost {
    task: Mutex<Option<tokio::task::JoinHandle<Result<()>>>>,
    voice_jobs: Mutex<HashMap<String, watch::Sender<bool>>>,
}
impl Drop for PeripheralHost {
    fn drop(&mut self) {
        if let Ok(task) = self.task.get_mut() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if let Ok(jobs) = self.voice_jobs.get_mut() {
            for (_, sender) in jobs.drain() {
                let _ = sender.send(true);
            }
        }
    }
}
pub async fn shutdown(app: &tauri::AppHandle) {
    let host = app.state::<PeripheralHost>();
    let task = host.task.lock().unwrap().take();
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
    for (_, sender) in host.voice_jobs.lock().unwrap().drain() {
        let _ = sender.send(true);
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Status,
    VoiceConfig {
        config: voice::VoiceConfig,
    },
    TeamAuth {
        config: team::TeamConfig,
    },
    TeamSync,
    TeamPut {
        id: Option<String>,
        title: String,
        content: String,
        tags: Vec<String>,
    },
    TeamDelete {
        id: String,
    },
    TeamResolve {
        id: String,
        keep_local: bool,
    },
    TeamImport {
        source: std::path::PathBuf,
        preview: bool,
    },
    Start {
        interval: u64,
    },
    Stop,
}
#[tauri::command]
pub async fn peripheral_command(
    app: tauri::AppHandle,
    host: tauri::State<'_, PeripheralHost>,
    request: Request,
) -> Result<Value, String> {
    async {
        let context=context(&app)?;
        match request {
            Request::Status=>Ok(json!({"voice":context.setting::<voice::VoiceConfig>("voice")?,
                "team":context.setting::<team::TeamConfig>("team-sync")?,"services":lifecycle::status(&context)?,
                "memories":if context.setting::<team::TeamConfig>("team-sync")?.is_some(){team::list(&context,false)?}else{vec![]},
                "conflicts":if context.setting::<team::TeamConfig>("team-sync")?.is_some(){team::conflicts(&context)?}else{vec![]}})),
            Request::VoiceConfig {config}=> {config.validate()?;context.save_setting("voice",&config)?;Ok(json!(config))}
            Request::TeamAuth {config}=> {team::auth(&context,config).await?;Ok(json!({"authenticated":true}))}
            Request::TeamSync=>Ok(json!(team::sync(&context).await?)),
            Request::TeamPut {id,title,content,tags}=>Ok(json!(team::put(&context,id,title,content,tags)?)),
            Request::TeamDelete {id}=> {team::delete(&context,&id)?;Ok(json!({"deleted":id}))}
            Request::TeamResolve {id,keep_local}=> {team::resolve(&context,&id,keep_local)?;Ok(json!({"resolved":id}))}
            Request::TeamImport {source,preview}=>Ok(json!(team::import_legacy(&context,&source,preview)?)),
            Request::Start {interval}=> {
                ensure!((1..=86400).contains(&interval),"Interval must be 1..86400 seconds");
                team::configured(&context)?;
                context.credentials.get(&team::configured(&context)?.credential)?;
                let mut task=host.task.lock().unwrap();
                ensure!(task.as_ref().is_none_or(|t|t.is_finished()),"Desktop already hosts team-sync");
                // Acquire before returning so duplicate hosts cannot both report success.
                let lease=lifecycle::Lease::acquire(&context,"team-sync")?;
                *task=Some(tokio::spawn(lifecycle::run_team_sync_with_lease(context,interval,lease)));
                Ok(json!({"started":true}))
            }
            Request::Stop=> {lifecycle::stop(&context,"team-sync")?;lifecycle::wait_stopped(&context,"team-sync").await?;
                let task=host.task.lock().unwrap().take();if let Some(task)=task {task.await??;}
                Ok(json!({"stopped":true}))}
        }
    }.await.map_err(|e:anyhow::Error|e.to_string())
}
#[tauri::command]
pub fn peripheral_credential(
    app: tauri::AppHandle,
    name: String,
    value: String,
) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("Invalid credential name".into());
    }
    crate::secure_storage::SecureKeyStore::new()
        .set_service_credential(&app, &name, &value)
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub fn peripheral_credential_delete(app: tauri::AppHandle, name: String) -> Result<(), String> {
    if !name.starts_with("STAGED_") || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("Only staged peripheral credentials can be removed".into());
    }
    crate::secure_storage::SecureKeyStore::new().delete_service_credential(&app, &name).map_err(|e| e.to_string())
}
#[tauri::command]
pub fn voice_begin(
    app: tauri::AppHandle,
    host: tauri::State<'_, PeripheralHost>,
) -> Result<String, String> {
    let prepare = || -> Result<_> {
        let context = context(&app)?;
        let config: voice::VoiceConfig = context
            .setting("voice")?
            .context("请先在设置 → 服务与团队中配置音频转写后端")?;
        config.validate()?;
        context.credentials.get(&config.credential)?;
        let id = uuid::Uuid::new_v4().to_string();
        host.voice_jobs
            .lock()
            .unwrap()
            .insert(id.clone(), watch::channel(false).0);
        Ok(id)
    };
    prepare().map_err(|e| e.to_string())
}
#[tauri::command]
pub fn voice_cancel(host: tauri::State<'_, PeripheralHost>, id: String) {
    if let Some(sender) = host.voice_jobs.lock().unwrap().remove(&id) {
        let _ = sender.send(true);
    }
}
#[tauri::command]
pub async fn voice_transcribe(
    app: tauri::AppHandle,
    host: tauri::State<'_, PeripheralHost>,
    id: String,
    audio: Vec<u8>,
    mime: String,
) -> Result<String, String> {
    let receiver = host
        .voice_jobs
        .lock()
        .unwrap()
        .get(&id)
        .map(|s| s.subscribe());
    let Some(mut cancellation) = receiver else {
        return Err("Recording was cancelled".into());
    };
    let result = async {
        let context = context(&app)?;
        tokio::select! {
            biased;
            _=cancellation.changed()=>Err(anyhow::anyhow!("Transcription was cancelled")),
            result=voice::transcribe(&context,audio,&mime)=>result,
        }
    }
    .await
    .map_err(|e: anyhow::Error| e.to_string());
    host.voice_jobs.lock().unwrap().remove(&id);
    result
}
