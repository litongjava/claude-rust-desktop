//! Keep the initialized pool alive so contention occurs at the resume/startup write.
use super::*;
use deepseek_code_core::runtime::DATABASE_FILE;
use std::{thread, time::Duration};

struct Fixture {
    _root: tempfile::TempDir,
    paths: RuntimePaths,
    store: SessionStore,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store
            .create(root.path(), "old-model", "old-provider")
            .unwrap();
        Self {
            _root: root,
            paths,
            store,
            id,
        }
    }

    fn lock_writer(&self) -> rusqlite::Connection {
        let holder = rusqlite::Connection::open(self.paths.data_dir.join(DATABASE_FILE)).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        holder
    }

    fn hold_past_busy_timeout(&self) -> thread::JoinHandle<()> {
        let holder = self.lock_writer();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(6));
            holder.execute_batch("ROLLBACK").unwrap();
        })
    }

    async fn resume(&self, cli: &Cli) -> Session {
        let provider = serde_json::from_value(json!({
            "id":"fixture", "name":"Fixture", "base_url":"http://127.0.0.1:1",
            "api_key":"unused", "api_format":"openai", "enabled":true,
            "models":[{"id":"new-model","name":"Fixture","enabled":true,
                "supports_vision":false,"supports_web_search":false}]
        }))
        .unwrap();
        let selection = Selection {
            providers: vec![provider],
            provider_id: "fixture".into(),
            model: "new-model".into(),
        };
        Session::open(cli, &self.paths, selection, Some(&self.id))
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn model_and_provider_wait_for_writer_after_init() {
    let fixture = Fixture::new();
    let cli = Cli::try_parse_with_mode_source(["dcl", "--print"]).unwrap();
    let holder = fixture.hold_past_busy_timeout();
    let session = fixture.resume(&cli).await;
    holder.join().unwrap();
    let saved = fixture.store.resume(&fixture.id).unwrap();
    assert_eq!(saved.model.as_deref(), Some("new-model"));
    assert_eq!(saved.provider.as_deref(), Some("fixture"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn title_waits_for_writer_after_init() {
    let fixture = Fixture::new();
    let cli = Cli::try_parse_with_mode_source(["dcl", "--name", "New title"]).unwrap();
    let mut session = fixture.resume(&cli).await;
    let holder = fixture.hold_past_busy_timeout();
    crate::startup_options::apply(&cli, &mut session)
        .await
        .unwrap();
    holder.join().unwrap();
    assert_eq!(
        fixture.store.resume(&fixture.id).unwrap().title.as_deref(),
        Some("New title")
    );
    session.close().await.unwrap();
}

#[test]
fn idle_resume_releases_execution_lease_while_writer_is_held() {
    use deepseek_code_core::services::session_messages::ExecutionLease;
    let fixture = Fixture::new();
    let holder = fixture.lock_writer();
    let (done, finished) = std::sync::mpsc::channel();
    // Keep the writer until both settlement and an independent lease probe finish.
    thread::scope(|scope| {
        scope.spawn(|| {
            let settled = settle_abandoned_turns(&fixture.paths, &fixture.store, &fixture.id);
            done.send(settled).unwrap();
        });
        let result = finished.recv_timeout(Duration::from_secs(2));
        let probe = ExecutionLease::acquire(&fixture.paths.data_dir, &fixture.id);
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(result
            .expect("idle resume waited for the unrelated writer")
            .is_empty());
        assert!(probe.unwrap().is_some());
    });
}
