use anyhow::Result;
use deepseek_code_core::db::{
    message_repo::ToolCallRow,
    session_store::{CreateOptions, DirectoryFilter, ProjectSelection, Stores},
};

#[test]
fn directory_filters_titles_and_sizes_follow_the_selected_incarnation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open_for_test(root.path())?;
    let a = stores.create(
        "a",
        CreateOptions {
            project: Some(ProjectSelection::Workspace(root.path().into())),
            workspace_mode: Some("code".into()),
            title: Some("CLI session".into()),
            ..Default::default()
        },
    )?;
    a.write(|w| {
        w.append("user", "\n  first\nsecond")?;
        let m = w.append("assistant", "π")?;
        w.snapshots(&m, &serde_json::json!({"id":"r"}))?;
        w.tool(
            &ToolCallRow {
                id: "tool".into(),
                message_id: m,
                name: "Read".into(),
                input: Some("[]".into()),
                output: Some("résumé".into()),
                is_error: false,
                sort_order: 0,
            },
            None,
        )?;
        w.set_git_branch(Some("branch"))
    })?;
    let archived = stores.create("archived", Default::default())?;
    archived.set_archived(true)?;
    stores.create("empty", Default::default())?;
    let project = stores
        .global()
        .conversation("a")?
        .unwrap()
        .project_id
        .unwrap();
    let rows = stores.global().list_sessions(DirectoryFilter {
        archived: Some(false),
        workspace: root.path().to_str(),
        project: Some(&project),
        mode: Some("code"),
        nonempty: true,
        ..Default::default()
    })?;
    assert_eq!(rows.len(), 1);
    let item = &rows[0];
    assert_eq!(item.conversation.title.as_deref(), Some("first"));
    assert_eq!(item.git_branch.as_deref(), Some("branch"));
    assert_eq!(
        item.transcript_bytes,
        ("\n  first\nsecond".len()
            + "π".len()
            + "[]".len()
            + "résumé".len()
            + r#"{"id":"r"}"#.len()) as i64
    );
    assert_eq!(
        stores.global().list_sessions(DirectoryFilter {
            archived: Some(true),
            ..Default::default()
        })?[0]
            .conversation
            .id,
        "archived"
    );
    assert!(stores
        .global()
        .list_sessions(DirectoryFilter {
            exclude: Some("a"),
            nonempty: true,
            ..Default::default()
        })?
        .is_empty());
    assert_eq!(stores.open_listed(item)?.incarnation(), a.incarnation());
    a.delete()?;
    stores.create("a", Default::default())?;
    assert!(stores.open_listed(item).is_err());
    Ok(())
}

/// The test watcher sees actual kernel opens, including opens bypassing the resolver.
#[cfg(target_os = "linux")]
#[test]
fn directory_listing_opens_zero_shards() -> Result<()> {
    use std::{
        ffi::CString,
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let root = tempfile::tempdir()?;
    let stores = Stores::open_for_test(root.path())?;
    let count = if std::env::var_os("I46_DIRECTORY_SCALE").is_some() {
        2048
    } else {
        8
    };
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    anyhow::ensure!(fd >= 0, std::io::Error::last_os_error());
    let mut events = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut paths = Vec::new();
    for _ in 0..count {
        let id = uuid::Uuid::new_v4().to_string();
        stores.create(&id, Default::default())?;
        paths.push(root.path().join("sessions").join(id).join("session.db"));
    }
    // This isolates the measured query from the independent repair thread.
    let namespace = std::fs::File::options()
        .read(true)
        .write(true)
        .open(root.path().join("session-locks/.directory"))?;
    namespace.lock()?;
    for path in &paths {
        let c = CString::new(path.as_os_str().as_bytes())?;
        anyhow::ensure!(
            unsafe { libc::inotify_add_watch(events.as_raw_fd(), c.as_ptr(), libc::IN_OPEN) } >= 0,
            std::io::Error::last_os_error()
        );
    }
    let started = std::time::Instant::now();
    let rows = stores.global().list_sessions(DirectoryFilter::default())?;
    assert_eq!(rows.len(), count);
    let elapsed = started.elapsed();
    let mut buf = [0u8; 4096];
    assert_eq!(
        events.read(&mut buf).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    // Positive control for the installed watches.
    drop(std::fs::File::open(&paths[0])?);
    assert!(events.read(&mut buf)? >= std::mem::size_of::<libc::inotify_event>());
    eprintln!("listed {count} published sessions in {elapsed:?}, zero session.db open events");
    Ok(())
}

#[test]
fn usage_reports_exact_prices_and_retains_healthy_sessions_when_one_is_missing() -> Result<()> {
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let stores = Stores::open_for_test(root.path())?;
    let a = stores.create("a", Default::default())?;
    a.write(|w| {
        let message=w.append("assistant","answer")?;
        w.snapshots(&message,&json!([
            {"id":"r1","startedAt":1000,"endedAt":2000,"inputTokens":20,"model":"one","usage":{"cost_usd":0.25}},
            {"id":"r2","startedAt":3000,"endedAt":5000,"cacheReadTokens":7,"model":"one"},
            {"id":"r3","startedAt":86_401_000,"model":"two","outputTokens":19}
        ]))
    })?;
    let b = uuid::Uuid::new_v4().to_string();
    stores.create(&b, Default::default())?;
    std::fs::rename(
        root.path().join("sessions").join(&b).join("session.db"),
        root.path().join("damaged.db"),
    )?;
    let usage = stores.usage(|| false)?;
    assert_eq!(usage.days.len(), 2);
    let day = usage.days.iter().find(|d| d.model == "one").unwrap();
    assert_eq!(
        (
            day.started_ms,
            day.ended_ms,
            day.requests,
            day.tokens,
            day.heat
        ),
        (1000, 5000, 2, [20, 0, 7, 0], 27)
    );
    assert_eq!(usage.failures.len(), 1);
    assert_eq!(usage.failures[0].0, b);
    let cost = a.read(|r| r.cost(&Default::default()))?;
    assert_eq!(cost["known_subtotal_usd"], 0.25);
    assert_eq!(cost["reported_requests"], 1);
    assert_eq!(cost["unpriced_requests"], 2);
    assert!(cost["total_usd"].is_null());
    assert!(stores.usage(|| true).is_err());
    Ok(())
}

#[test]
fn conversation_snapshot_preserves_typed_content_and_turns_and_rolls_back_bad_ids() -> Result<()> {
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let shard = stores.create("detail", Default::default())?;
    let (turn, message) = shard.write(|s| {
        let turn = s.start_turn()?;
        let message = s.segment(&turn)?;
        s.finish_turn(
            &turn,
            deepseek_code_core::db::turn_repo::TurnStatus::Cancelled,
            42,
        )?;
        Ok((turn, message))
    })?;
    let prompt = json!([{"type":"text","text":"question"}]);
    shard.write(|s| {
        s.replace_messages(&[
            json!({"id":"prompt","role":"user","content":prompt}),
            json!({"id":message,"role":"assistant","content":"answer"}),
        ])
    })?;
    assert!(shard
        .write(|s| s.replace_messages(&[
            json!({"id":"prompt","content":"changed"}),
            json!({"id":"prompt"})
        ]))
        .is_err());
    let rows = shard.read(|s| s.enriched_messages())?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["content"], prompt);
    assert_eq!(rows[1]["turn"]["turnId"], turn);
    assert_eq!(rows[1]["turn"]["status"], "cancelled");
    assert_eq!(rows[1]["content"], "answer");
    assert_eq!(
        stores
            .global()
            .conversation("detail")?
            .unwrap()
            .message_count,
        2
    );
    Ok(())
}

#[test]
fn im_threads_share_the_directory_without_owning_a_code_transcript() -> Result<()> {
    use deepseek_code_core::im_integration::session_manager::{SessionContext, SessionMessage};
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let now = chrono::Utc::now();
    let thread = SessionContext {
        platform: "feishu".into(),
        user_id: "user".into(),
        chat_id: "chat".into(),
        thread_id: Some("thread".into()),
        created_at: now,
        updated_at: now,
        message_count: 1,
        last_message_content: Some("preview".into()),
        metadata: json!({"dispatch":"one"}),
    };
    let msg = SessionMessage {
        id: "event-id".into(),
        session_key: thread.thread_key(),
        role: "user".into(),
        content: "payload".repeat(20_000),
        message_type: "text".into(),
        timestamp: now,
        raw_data: Some(json!({"body":"original"})),
    };
    stores.save_im_thread(&thread, Some(&msg))?;
    stores.save_im_thread(&thread, Some(&msg))?;
    let other = SessionContext {
        thread_id: Some("another".into()),
        ..thread.clone()
    };
    stores.save_im_thread(&other, None)?;
    assert!(stores
        .global()
        .list_sessions(Default::default())?
        .is_empty());
    drop(stores);
    let stores = Stores::open(root.path())?;
    let threads = stores.im_threads()?;
    assert_eq!(threads.len(), 2);
    assert!(threads
        .iter()
        .all(|t| t.message_count == 1 && t.metadata == thread.metadata));
    assert!(stores.im_messages(&other.thread_key(), 10)?.is_empty());
    let found = stores.im_messages(&thread.thread_key(), 10)?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].content, msg.content);
    assert_eq!(found[0].raw_data, msg.raw_data);
    assert!(stores
        .save_im_thread(
            &thread,
            Some(&SessionMessage {
                content: "different".into(),
                ..msg.clone()
            })
        )
        .is_err());
    stores.delete_im_thread(&thread.thread_key())?;
    assert!(stores.im_messages(&thread.thread_key(), 10)?.is_empty());
    assert_eq!(stores.im_threads()?[0].thread_key(), other.thread_key());
    Ok(())
}

#[test]
fn approval_decisions_require_the_selected_shard_or_one_unique_match() -> Result<()> {
    use deepseek_code_core::db::permission_repo::PermissionApprovalRow;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    for session in ["one", "two"] {
        stores.create(session, Default::default())?.write(|w| {
            let message = w.append("assistant", "tool")?;
            w.approval(&PermissionApprovalRow {
                id: "request".into(),
                conversation_id: session.into(),
                message_id: message,
                tool_name: "Read".into(),
                action: "read".into(),
                risk_level: "low".into(),
                status: "pending".into(),
                created_at: "now".into(),
                user_decision: None,
                decision_reason: None,
                decided_at: None,
            })
        })?;
    }
    assert!(stores
        .decide_approval(None, "request", "approved", None, "now")
        .is_err());
    stores.decide_approval(Some("one"), "request", "rejected", Some("declined"), "now")?;
    let first = stores.open_session("one")?;
    assert_eq!(
        first
            .read(|r| r.approval("request"))?
            .unwrap()
            .decision_reason
            .as_deref(),
        Some("declined")
    );
    let second = stores.open_session("two")?;
    assert_eq!(
        second.read(|r| r.approval("request"))?.unwrap().status,
        "pending"
    );
    first.delete()?;
    stores.decide_approval(None, "request", "approved", None, "later")?;
    assert_eq!(
        second.read(|r| r.approval("request"))?.unwrap().status,
        "approved"
    );
    Ok(())
}

#[test]
fn runtime_resolves_new_resume_clear_and_child_under_one_root() -> Result<()> {
    use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let paths = RuntimePaths::new(root.path().join("data"))?;
    let sentinel = paths.data_dir.join("deepseek_code.db");
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::write(&sentinel, b"existing user history")?;
    let store = SessionStore::new(std::sync::Arc::new(paths.open_stores()?));
    let id = store.create(root.path(), "model", "provider")?;
    assert!(store
        .create_with_id(root.path(), "other", "provider", &id)
        .is_err());
    let message = store.append_hook_user(&id, "startup")?;
    let shard = store.db.open_session(&id)?;
    assert_eq!(
        shard.read(|s| s.message_metadata(&message))?.unwrap()["type"],
        "hook"
    );
    assert!(shard.read(|s| s.authority())?.is_empty());
    store.append_user_content(
        &id,
        "question",
        &json!([{"type":"text","text":"first input"}]),
    )?;
    assert_eq!(store.history(&id)?[1]["content"], "question");
    assert_eq!(store.resume(&id)?.model.as_deref(), Some("model"));
    assert_eq!(store.latest(root.path())?.unwrap().id, id);
    let child = store.derive_owned_child(&id, &paths.data_dir.join("workspaces"), root.path())?;
    assert_eq!(
        store.workspace(&child, &paths.data_dir.join("workspaces"))?,
        root.path()
    );
    let cleared = store.clear(&id)?;
    assert!(store.history(&cleared)?.is_empty());
    assert_eq!(store.history(&store.fork(&id)?)?.len(), 2);
    assert_eq!(std::fs::read(&sentinel)?, b"existing user history");
    Ok(())
}

fn provider_choice(
    id: &str,
) -> deepseek_code_core::native_engine::session_binding::BindingSnapshot {
    use deepseek_code_core::native_engine::session_binding::BindingSnapshot;
    use serde_json::json;
    BindingSnapshot {
        provider_id:id.into(),model:"m".into(),generation:0,observation_only:false,environment:Default::default(),
        providers:vec![serde_json::from_value(json!({"id":id,"name":id,"enabled":true,"api_format":"openai","api_key":"fixture","base_url":"http://localhost","models":[{"id":"m","name":"model","enabled":true,"supports_web_search":false}]})).unwrap()],
    }
}

#[test]
fn provider_publication_cannot_overtake_a_turn_inside_its_admission_transaction() -> Result<()> {
    use deepseek_code_core::native_engine::session_binding::store::Write;
    use std::{sync::mpsc, thread, time::Duration};
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let session = stores.create("admission", Default::default())?;
    let owner = session.delivery_owner()?;
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let turn_session = session.clone();
    let turn = thread::spawn(move || {
        turn_session.write(|w| {
            entered.send(())?;
            wait.recv_timeout(Duration::from_secs(5))?;
            w.start_turn()
        })
    });
    ready.recv_timeout(Duration::from_secs(5))?;
    let (published, done) = mpsc::channel();
    let picker = session.clone();
    let selection = thread::spawn(move || {
        let result = picker.publish_provider(
            0,
            &provider_choice("b"),
            Some(("worker", Some(123))),
            "picker",
            None,
        );
        published.send(()).unwrap();
        result
    });
    let early = done.recv_timeout(Duration::from_millis(150)).is_ok();
    release.send(())?;
    turn.join().unwrap()?;
    assert_eq!(selection.join().unwrap()?, Write::Published(1));
    assert!(!early);
    assert!(session.write(|w| w.start_turn()).is_err());
    drop(owner);
    Ok(())
}

#[test]
fn provider_reservation_rejects_stale_turns_and_applies_picker_once_on_resume() -> Result<()> {
    use deepseek_code_core::{
        db::session_store::settings::{Effort, PREFERENCES},
        native_engine::session_binding::{store::Write, SessionBinding},
    };
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let session = stores.create("bound", Default::default())?;
    let mut original = provider_choice("a");
    assert_eq!(
        session.publish_provider(0, &original, None, "", None)?,
        Write::Published(1)
    );
    original.generation = 1;
    let binding = SessionBinding::default();
    binding.track_session(session.clone());
    binding.install(original.clone())?;
    let active = binding.resolve(None).unwrap();
    active.ensure_current()?;
    let next = provider_choice("b");
    assert_eq!(
        session.publish_provider(0, &next, None, "", None)?,
        Write::Conflict
    );
    assert_eq!(
        session.publish_provider(
            1,
            &next,
            Some(("worker", None)),
            "picker",
            Some(Some("high"))
        )?,
        Write::Published(2)
    );
    assert!(active.ensure_current().is_err());
    assert!(session.write(|w| w.start_turn()).is_err());
    let row = stores.global().conversation(session.id())?.unwrap();
    assert_eq!(row.provider.as_deref(), Some("a"));
    assert!(session.read(|r| r.setting(PREFERENCES))?.is_none());
    session.activate_provider("wrong", 2)?;
    assert!(session.observe_provider()?.pending.is_some());
    session.activate_provider("worker", 2)?;
    let reopened = Stores::open_for_test(root.path())?.open_session(session.id())?;
    let mut prefs = reopened.read(|r| r.setting(PREFERENCES))?.unwrap();
    assert_eq!(prefs.effort, Some(Effort::High));
    prefs.effort = Some(Effort::Low);
    reopened.write(|w| w.set_setting(PREFERENCES, &prefs))?;
    assert_eq!(
        session.read(|r| r.setting(PREFERENCES))?.unwrap().effort,
        Some(Effort::Low)
    );
    assert!(session.write(|w| w.start_turn()).is_ok());
    session.delete()?;
    let fresh = stores.create("bound", Default::default())?;
    assert_eq!(fresh.observe_provider()?.generation, 0);
    assert!(active.ensure_current().is_err());
    Ok(())
}

#[test]
fn sync_buckets_replay_after_content_commit_and_fence_retired_sessions() -> Result<()> {
    use deepseek_code_core::db::{
        message_repo::MessageRow,
        session_store::{Phase, SyncHeader},
    };
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let header: SyncHeader = serde_json::from_value(serde_json::json!({
        "id":"remote","title":"From phone","pinned":true,"createdAt":"2026-01-01T00:00:00Z",
        "updatedAt":"2026-01-02T00:00:00Z"
    }))?;
    let row: MessageRow = serde_json::from_value(serde_json::json!({
        "id":"message","conversationId":"remote","role":"user","content":"first",
        "createdAt":"2026-01-01T00:00:00Z","sortOrder":0
    }))?;
    stores.sync_conversation("remote", Some(&header), std::slice::from_ref(&row))?;
    let g = stores.global().conversation("remote")?.unwrap();
    assert!(g.pinned);
    assert_eq!(g.message_count, 1);
    let mut edit = row.clone();
    edit.content = "remote edit".into();
    let fault = stores.with_fault_hook(|phase| {
        anyhow::ensure!(
            phase != Phase::SyncContent,
            "interrupted before acknowledgement"
        );
        Ok(())
    });
    assert!(fault
        .sync_conversation("remote", None, std::slice::from_ref(&edit))
        .is_err());
    assert_eq!(stores.sync_messages("remote")?[0].content, "remote edit");
    let mut local = row.clone();
    local.content = "later local edit".into();
    stores.sync_conversation("remote", None, std::slice::from_ref(&local))?;
    stores.sync_conversation("remote", None, std::slice::from_ref(&edit))?;
    assert_eq!(
        stores.sync_messages("remote")?[0].content,
        "later local edit"
    );
    let mut invalid = row.clone();
    invalid.role = "assistant".into();
    let mut insert = row.clone();
    insert.id = "never committed".into();
    assert!(stores
        .sync_conversation("remote", None, &[insert, invalid])
        .is_err());
    assert_eq!(stores.sync_messages("remote")?.len(), 1);
    assert!(stores
        .sync_conversation("someone-else", None, std::slice::from_ref(&row))
        .is_err());
    stores.sync_delete("remote")?;
    assert!(stores
        .sync_conversation("remote", Some(&header), &[row])
        .is_err());
    assert!(stores.global().conversation("remote")?.is_none());
    Ok(())
}

#[tokio::test]
async fn im_host_resumes_transport_threads_and_reads_only_their_messages() -> Result<()> {
    use deepseek_code_core::im_integration::{
        message_router::{MessageType, UnifiedMessage},
        session_manager::SessionManager,
    };
    use std::sync::Arc;
    let root = tempfile::tempdir()?;
    let stores = Arc::new(Stores::open(root.path())?);
    let host = SessionManager::new(stores.clone());
    host.initialize().await?;
    for thread in ["first", "second"] {
        let event = UnifiedMessage {
            platform: "feishu".into(),
            user_id: "user".into(),
            chat_id: "chat".into(),
            thread_id: Some(thread.into()),
            content: thread.into(),
            message_type: MessageType::Text,
            timestamp: chrono::Utc::now(),
            raw_data: serde_json::json!({"thread":thread}),
        };
        host.get_or_create_session(&event).await?;
        host.update_session(&event).await?;
    }
    drop(host);
    let resumed = SessionManager::new(stores.clone());
    resumed.initialize().await?;
    for thread in ["first", "second"] {
        let messages = resumed
            .get_session_messages("feishu", "user", "chat", Some(thread), 10)
            .await?;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].raw_data.as_ref().unwrap()["thread"], thread);
    }
    resumed
        .delete_session("feishu", "user", "chat", Some("first"))
        .await?;
    assert_eq!(resumed.list_sessions().await.len(), 1);
    assert!(stores.conversations()?.is_empty());
    Ok(())
}

#[test]
fn project_chat_rechecks_trust_and_recovers_the_prepared_workspace() -> Result<()> {
    use deepseek_code_core::db::{
        conversation_repo::WorkspaceMode,
        session_store::{Phase, ProjectEdit, Projects},
    };
    let root = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let folders = vec![workspace.path().to_string_lossy().into_owned()];
    stores.create_project(
        "project",
        &ProjectEdit {
            name: Some("Project".into()),
            folders: Some(folders.clone()),
            ..Default::default()
        },
    )?;
    assert!(stores
        .create_project_chat("project", None, None, WorkspaceMode::Code, None, false)
        .is_err());
    stores.trust_project("project", &folders)?;
    let fault = stores.with_fault_hook(|phase| {
        anyhow::ensure!(phase != Phase::Sealed, "interrupted after seal");
        Ok(())
    });
    assert!(fault
        .create_project_chat(
            "project",
            Some("Desktop chat"),
            None,
            WorkspaceMode::Code,
            None,
            false
        )
        .is_err());
    let reopened = Stores::open_for_test(root.path())?;
    let (project, _, chats) = reopened.project_detail("project")?;
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(chats[0].title.as_deref(), Some("Desktop chat"));
    assert_eq!(
        reopened.open_session(&chats[0].id)?.workspace()?,
        workspace.path().canonicalize()?
    );
    Ok(())
}

#[test]
fn conversation_routes_keep_rewind_and_full_branch_scoped_to_the_selected_chat() -> Result<()> {
    use deepseek_code_core::db::session_store::Rewind;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let session = stores.create("original", Default::default())?;
    let first = session.write(|s| s.append("user", "first"))?;
    session.write(|s| s.append("assistant", "second"))?;
    let full = stores.branch_conversation("original", None, None)?;
    assert_eq!(stores.conversation_detail(&full)?.1.len(), 2);
    let partial = stores.branch_conversation("original", Some(&first), None)?;
    assert_eq!(stores.conversation_detail(&partial)?.1.len(), 1);
    assert_eq!(
        stores.rewind_messages("original", Rewind::Tail(1))?.len(),
        1
    );
    assert!(stores
        .rewind_messages("original", Rewind::From(&first))?
        .is_empty());
    assert_eq!(stores.conversation_detail(&full)?.1.len(), 2);
    Ok(())
}

#[test]
fn retiring_worker_retains_its_own_and_its_project_childs_inbox() -> Result<()> {
    use deepseek_code_core::native_engine::session_binding::store::Write;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let create = || uuid::Uuid::new_v4().to_string();
    let parent = stores.create_code_session(root.path(), "m", "a", &create())?;
    let child = parent.derive_owned_child(&create(), root.path())?;
    let sender = stores.create_code_session(root.path(), "m", "a", &create())?;
    let to_parent = sender.send_peer(&parent, "parent-msg", "parent feedback")?;
    let to_child = sender.send_peer(&child, "child-msg", "child feedback")?;
    let mut parent_owner = parent.delivery_owner()?;
    let mut child_owner = child.delivery_owner()?;
    assert_eq!(
        parent.publish_provider(
            0,
            &provider_choice("b"),
            Some(("worker", Some(std::process::id()))),
            "picker",
            None
        )?,
        Write::Published(1)
    );
    assert!(parent_owner
        .next_peer()
        .unwrap_err()
        .to_string()
        .contains("deliver input"));
    assert!(child_owner
        .next_peer()
        .unwrap_err()
        .to_string()
        .contains("inbox retained"));
    assert_eq!(child.read(|r| r.receipt_count())?, 0);
    parent.activate_provider("worker", 1)?;
    assert_eq!(parent_owner.next_peer()?.unwrap().id, to_parent);
    assert_eq!(child_owner.next_peer()?.unwrap().id, to_child);
    assert!(child_owner.next_peer()?.is_none());
    Ok(())
}

#[test]
fn first_bridge_message_is_published_with_typed_history_and_chat_authority() -> Result<()> {
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let blocks = json!([{"type":"text","text":"typed hello"}]);
    stores.admit_chat_message("chat", "model", Some(&blocks))?;
    let shard = stores.open_session("chat")?;
    shard.read(|r| {
        let messages = r.messages()?;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "typed hello");
        assert_eq!(r.user_content(&messages[0].id)?, blocks);
        assert_eq!(r.authority()?.len(), 1);
        Ok(())
    })?;
    stores.admit_chat_message("chat", "model", Some(&json!("second")))?;
    assert_eq!(shard.read(|r| r.messages())?.len(), 2);
    assert!(stores
        .admit_chat_message("invalid", "model", Some(&json!({"type":"text"})))
        .is_err());
    assert!(stores.global().conversation("invalid")?.is_none());
    Ok(())
}

#[test]
fn synced_project_chat_uses_the_locally_trusted_workspace() -> Result<()> {
    use deepseek_code_core::db::session_store::SyncHeader;
    let root = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let existing =
        stores.create_code_session(project.path(), "m", "p", &uuid::Uuid::new_v4().to_string())?;
    let project_id = stores
        .global()
        .conversation(existing.id())?
        .unwrap()
        .project_id
        .unwrap();
    let header: SyncHeader = serde_json::from_value(serde_json::json!({
        "id":"synced","title":"Remote project","projectId":project_id,"workspacePath":"/another/device/path",
        "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z"
    }))?;
    stores.sync_conversation(&header.id, Some(&header), &[])?;
    let (catalog, _) = stores.sync_contents()?;
    let selected = catalog.iter().find(|row| row.id == header.id).unwrap();
    assert_eq!(
        selected.workspace_path.as_deref(),
        project.path().canonicalize()?.to_str()
    );
    Ok(())
}

#[test]
fn h5_tokens_resolve_only_the_issuing_incarnation_and_expire_with_it() -> Result<()> {
    use deepseek_code_core::db::session_store::HostAccess;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let shard=stores.create("phone",Default::default())?;
    let other=stores.create("other",Default::default())?;
    shard.write(|w|w.append("user","phone question").map(drop))?;
    other.write(|w|w.append("user","unrelated").map(drop))?;
    let expiry = (chrono::Utc::now()+chrono::Duration::days(1)).to_rfc3339();
    stores.insert_h5_token("grant","token","phone",&expiry,&chrono::Utc::now().to_rfc3339())?;
    let grant=stores.authorize_h5("token")?.unwrap();
    assert_eq!(grant.conversation.id,"phone");
    assert_eq!(grant.messages()?.iter().map(|m|m.content.as_str()).collect::<Vec<_>>(),["phone question"]);
    drop(grant);
    shard.delete()?;
    stores.create("phone",Default::default())?;
    assert!(stores.authorize_h5("token")?.is_none());
    Ok(())
}

#[test]
fn service_heartbeats_read_through_a_busy_directory_and_stop_on_request() -> anyhow::Result<()> {
    use deepseek_code_core::services::{lifecycle, EnvironmentCredentials, ServiceContext};
    use deepseek_code_core::runtime::RuntimePaths;
    let dir = tempfile::tempdir()?;
    let paths = RuntimePaths::new(dir.path().to_owned())?;
    let host = ServiceContext::from_stores(paths.clone(), std::sync::Arc::new(EnvironmentCredentials))?;
    let controller = ServiceContext::from_stores(paths, std::sync::Arc::new(EnvironmentCredentials))?;
    let lease = lifecycle::Lease::acquire(&host, "sync")?;
    assert!(lifecycle::Lease::acquire(&controller, "sync").is_err());
    let blocker = rusqlite::Connection::open_with_flags(dir.path().join("global.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    blocker.execute_batch("BEGIN IMMEDIATE")?;
    assert!(lease.renew()?);
    blocker.execute_batch("ROLLBACK")?;
    lease.record(None)?;
    lifecycle::stop(&controller, "sync")?;
    assert!(!lease.renew()?);
    drop(lease);
    let status = lifecycle::status(&controller)?;
    assert_eq!((status[0].state.as_str(), status[0].completed), ("stopped",1));
    Ok(())
}

#[test]
fn manual_compaction_binds_the_prepared_transcript_to_its_incarnation() -> Result<()> {
    use deepseek_code_core::services::compaction::{commit, count, prepare, CompactionCount};
    let dir = tempfile::tempdir()?;
    let stores = Stores::open(dir.path())?;
    let session = stores.create("compact", Default::default())?;
    session.write(|w| { w.append("user", "Task")?; w.append("assistant", "Progress")?; Ok(()) })?;
    let prepared = prepare(&stores, session.id())?;
    session.set_archived(true)?;
    assert!(commit(&stores, prepared, "Summary").is_err());
    session.set_archived(false)?;
    let stale = prepare(&stores, session.id())?;
    let saved = commit(&stores, prepare(&stores, session.id())?, "Summary")?;
    assert_eq!(saved.summarized_messages, 2);
    assert_eq!(count(&stores, session.id())?, CompactionCount::Exact(1));
    session.delete()?;
    let replacement = stores.create("compact", Default::default())?;
    assert!(commit(&stores, stale, "Stale summary").is_err());
    assert!(replacement.read(|r| r.messages())?.is_empty());
    Ok(())
}
