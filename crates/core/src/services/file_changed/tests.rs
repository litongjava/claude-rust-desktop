use super::*;
use crate::services::{hook_callbacks, tool_hooks::watch_paths};
use futures::future::BoxFuture;
use serde_json::Value;
use std::{path::Path, sync::Arc};

#[derive(Default)]
struct Capture(Mutex<Vec<Value>>, Mutex<Vec<String>>);
impl hook_callbacks::Host for Capture {
    fn invoke<'a>(
        &'a self,
        _: &'a str,
        input: Value,
        _: Option<String>,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(input.clone());
            Ok(
                json!({"continue":false,"decision":"block","systemMessage":"observer notice","hookSpecificOutput":{"hookEventName":input["hook_event_name"],"watchPaths":self.1.lock().unwrap().clone()}}),
            )
        })
    }
}
fn install(session: &str, host: &Arc<Capture>, matcher: &str) -> hook_callbacks::Guard {
    hook_callbacks::install(
        session,
        hook_callbacks::validate(
            &json!({"FileChanged":[{"matcher":matcher,"hookCallbackIds":["capture"]}]}),
        )
        .unwrap(),
        host.clone(),
    )
}
async fn count(host: &Capture, expected: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while host.0.lock().unwrap().len() < expected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(host.0.lock().unwrap().len(), expected);
}
async fn quiet() {
    tokio::time::sleep(Duration::from_millis(900)).await;
}

#[tokio::test]
async fn ignore_initial_add_change_unlink_debounce_parent_transition_and_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("existing");
    std::fs::write(&existing, "initial").unwrap();
    let future = dir.path().join("missing").join("watched");
    let session = uuid::Uuid::new_v4().to_string();
    let host = Arc::new(Capture::default());
    let _guard = install(&session, &host, "");
    let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
    start(
        &session,
        runner,
        vec![
            existing.to_string_lossy().into(),
            future.to_string_lossy().into(),
        ],
    )
    .await
    .unwrap();
    quiet().await;
    assert!(host.0.lock().unwrap().is_empty());
    std::fs::create_dir(future.parent().unwrap()).unwrap();
    std::fs::write(&future, "a").unwrap();
    count(&host, 1).await;
    for i in 0..5 {
        std::fs::write(&future, "x".repeat(i + 2)).unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    // Atomic save: unlink/recreate within stability window is one change.
    std::fs::remove_file(&future).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::write(&future, "atomic replacement").unwrap();
    count(&host, 2).await;
    std::fs::remove_file(&future).unwrap();
    count(&host, 3).await;
    quiet().await;
    let rows = host.0.lock().unwrap().clone();
    assert_eq!(
        rows.iter()
            .map(|v| v["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["add", "change", "unlink"]
    );
    assert_eq!(
        rows[0],
        json!({"hook_event_name":"FileChanged","session_id":session,"cwd":dir.path(),"file_path":future,"event":"add","transcript_path":rows[0]["transcript_path"]})
    );
    assert!(Path::new(rows[0]["transcript_path"].as_str().unwrap()).is_file());
    stop(&session).await;
    assert!(!registry().lock().unwrap().contains_key(&session));
    std::fs::write(&future, "closed").unwrap();
    quiet().await;
    assert_eq!(host.0.lock().unwrap().len(), 3);
    // A fresh watcher can acquire the file after the old native watcher is dropped.
    let state = State::new(vec![future.clone()]).unwrap();
    assert!(state.roots.contains(&future));
}

#[tokio::test]
async fn cwd_reset_static_paths_dynamic_replacement_and_session_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let next = dir.path().join("next");
    std::fs::create_dir(&next).unwrap();
    let a = uuid::Uuid::new_v4().to_string();
    let b = uuid::Uuid::new_v4().to_string();
    let ha = Arc::new(Capture::default());
    let hb = Arc::new(Capture::default());
    let _ga = install(&a, &ha, "watched");
    let _gb = install(&b, &hb, "watched");
    let old_dynamic = dir.path().join("old-watched");
    let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
    start(
        &a,
        runner.clone(),
        vec![old_dynamic.to_string_lossy().into()],
    )
    .await
    .unwrap();
    start(&b, runner, vec![]).await.unwrap();
    let new_runner = HookRunner::load(dir.path(), &next).unwrap();
    new_runner
        .run(
            HookEvent::CwdChanged,
            &a,
            json!({"old_cwd":dir.path(),"new_cwd":next}),
        )
        .await
        .unwrap();
    std::fs::write(&old_dynamic, "ignored").unwrap();
    std::fs::write(dir.path().join("watched"), "only b").unwrap();
    count(&hb, 1).await;
    assert!(ha.0.lock().unwrap().is_empty());
    std::fs::write(next.join("watched"), "only a").unwrap();
    count(&ha, 1).await;
    stop(&a).await;
    std::fs::write(dir.path().join("watched"), "b remains active").unwrap();
    count(&hb, 2).await;
    stop(&b).await;
    assert_eq!(ha.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn filechanged_output_replaces_dynamic_paths_and_ignores_initial_new_targets() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    let new = dir.path().join("new");
    std::fs::write(&new, "already exists").unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host = Arc::new(Capture::default());
    *host.1.lock().unwrap() = vec![new.to_string_lossy().into()];
    let _guard = install(&session, &host, "");
    start(
        &session,
        HookRunner::load(dir.path(), dir.path()).unwrap(),
        vec![old.to_string_lossy().into()],
    )
    .await
    .unwrap();
    std::fs::write(&old, "trigger").unwrap();
    count(&host, 1).await;
    quiet().await;
    assert_eq!(host.0.lock().unwrap().len(), 1);
    std::fs::write(&old, "no longer watched").unwrap();
    std::fs::write(&new, "new content").unwrap();
    count(&host, 2).await;
    stop(&session).await;
    assert_eq!(host.0.lock().unwrap()[1]["file_path"], json!(new));
}

#[tokio::test]
async fn matcher_uses_basename_and_observer_output_cannot_block() {
    let dir = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host = Arc::new(Capture::default());
    let _guard = install(&session, &host, "^watched$");
    let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
    let result = runner
        .run_inner(
            HookEvent::FileChanged,
            &session,
            json!({"file_path":dir.path().join("watched"),"event":"change"}),
        )
        .await
        .unwrap();
    assert!(!result.blocked && !result.prevent_continuation && !result.allow_permission);
    assert_eq!(result.warnings, vec!["observer notice"]);
    runner
        .run_inner(
            HookEvent::FileChanged,
            &session,
            json!({"file_path":dir.path().join("other"),"event":"add"}),
        )
        .await
        .unwrap();
    assert_eq!(host.0.lock().unwrap().len(), 1);
    for event in [
        HookEvent::SessionStart,
        HookEvent::CwdChanged,
        HookEvent::FileChanged,
    ] {
        assert!(watch_paths(
            event,
            &json!({"hookSpecificOutput":{"hookEventName":event,"watchPaths":[dir.path()]}})
        )
        .is_ok());
        assert!(watch_paths(
            event,
            &json!({"hookSpecificOutput":{"hookEventName":event,"watchPaths":["relative"]}})
        )
        .is_err());
        assert!(watch_paths(
            event,
            &json!({"hookSpecificOutput":{"hookEventName":event,"watchPaths":[2]}})
        )
        .is_err());
    }
    assert!(watch_paths(
        HookEvent::PreToolUse,
        &json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","watchPaths":[]}})
    )
    .is_err());
}

#[tokio::test]
async fn stop_cancels_inflight_callback_and_releases_resources() {
    struct Slow(Arc<std::sync::atomic::AtomicUsize>);
    impl hook_callbacks::Host for Slow {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            _: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                struct Running(Arc<std::sync::atomic::AtomicUsize>);
                impl Drop for Running {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _running = Running(self.0.clone());
                std::future::pending().await
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host: Arc<dyn hook_callbacks::Host> = Arc::new(Slow(active.clone()));
    let session = uuid::Uuid::new_v4().to_string();
    let _guard = hook_callbacks::install(
        &session,
        hook_callbacks::validate(
            &json!({"FileChanged":[{"matcher":"watched","hookCallbackIds":["slow"]}]}),
        )
        .unwrap(),
        host.clone(),
    );
    start(
        &session,
        HookRunner::load(dir.path(), dir.path()).unwrap(),
        vec![],
    )
    .await
    .unwrap();
    std::fs::write(dir.path().join("watched"), "start").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while active.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    stop(&session).await;
    assert_eq!(active.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn idle_ticks_do_not_scan_and_missing_fallback_only_checks_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("future");
    let mut state = State::new(vec![file.clone()]).unwrap();
    let baseline = state.scans;
    for _ in 0..20 {
        state.last_missing_check = Instant::now() - MISSING_POLL;
        assert!(state.tick(false).is_empty());
    }
    assert_eq!(state.scans, baseline);
    // Simulate a lost native notification: metadata fallback still discovers it.
    std::fs::write(&file, "created").unwrap();
    state.last_missing_check = Instant::now() - MISSING_POLL;
    assert!(state.tick(false).is_empty());
    assert_eq!(state.scans, baseline + 1);
    assert!(state.roots.contains(&file));
    tokio::time::sleep(STABILITY + POLL).await;
    assert_eq!(state.tick(false), vec![(file, "add")]);
    let stable = state.scans;
    for _ in 0..20 {
        assert!(state.tick(false).is_empty());
    }
    assert_eq!(state.scans, stable);
}

#[test]
fn scan_budget_degrades_without_false_unlinks_and_keeps_known_files() {
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known");
    std::fs::write(&known, "initial").unwrap();
    let mut state = State::new(vec![dir.path().to_owned()]).unwrap();
    for i in 0..10 {
        std::fs::write(dir.path().join(format!("extra-{i}")), "x").unwrap();
    }
    let (partial, truncated) = snapshot_paths(&state.paths, true, 4);
    assert!(truncated);
    assert!(partial.len() <= 3);
    let original = state.committed.clone();
    assert_eq!(state.snapshot_with_limit(4), original);
    assert!(state.degraded);
    std::fs::write(&known, "changed content").unwrap();
    let snapshot = state.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_ne!(snapshot[&known], original[&known]);
    std::fs::remove_file(&known).unwrap();
    assert!(state.snapshot().is_empty());
    std::fs::write(&known, "recreated").unwrap();
    assert!(state.snapshot().contains_key(&known));
}

#[test]
fn close_acknowledgement_has_a_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = State::new(vec![dir.path().to_owned()]).unwrap();
    let (_keep_sender_alive, receiver) = std::sync::mpsc::channel();
    state.closed = receiver;
    let started = Instant::now();
    drop(state);
    assert!(started.elapsed() >= CLOSE_TIMEOUT);
    assert!(started.elapsed() < CLOSE_TIMEOUT + Duration::from_secs(3));
}

#[cfg(target_os = "macos")]
#[test]
fn native_system_alias_events_wake_lexical_paths_including_deleted_files() {
    use notify::EventHandler;
    let (wake, mut receiver) = tokio::sync::mpsc::channel(1);
    let (closed, _) = std::sync::mpsc::channel();
    let mut handler = Handler {
        wake,
        closed,
        paths: vec![PathBuf::from("/var/folders/ccl-test/missing/file")],
    };
    handler.handle_event(Ok(notify::Event::new(notify::EventKind::Remove(
        notify::event::RemoveKind::File,
    ))
    .add_path(PathBuf::from("/private/var/folders/ccl-test/missing/file"))));
    assert!(receiver.try_recv().is_ok());
    handler.handle_event(Ok(notify::Event::new(notify::EventKind::Modify(
        notify::event::ModifyKind::Any,
    ))
    .add_path(PathBuf::from("/private/var/folders/unrelated/file"))));
    assert!(receiver.try_recv().is_err());
}
