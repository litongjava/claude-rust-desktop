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
