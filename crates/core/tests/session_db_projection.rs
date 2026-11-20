use anyhow::{bail, Result};
use deepseek_code_core::db::{session_store::*, turn_repo::TurnStatus};
use deepseek_code_core::permissions::PermissionMode;
use deepseek_code_core::services::{compaction::GeneratedSummary, goal_proposals::ProposalState};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs,
    time::{Duration, Instant},
};
use tempfile::tempdir;

const A: &str = "10000000-1234-4234-9234-123456789abc";

#[test]
fn search_indexes_rollback_with_content_and_cursors_bind_to_the_incarnation() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open_for_test(root.path())?;
    let a = stores.create(A, Default::default())?;
    let b = stores.create("b", Default::default())?;
    let big = format!("{}needle old", "x".repeat(130_000));
    let (before, boundary, after) = a.write(|w| {
        let first = w.append("user", &big)?;
        let boundary = w.approve_plan("needle hidden boundary")?;
        let after = w.append("assistant", "needle new")?;
        Ok((first, boundary, after))
    })?;
    let search = |s: &SessionDb, q: Value| -> Result<Value> {
        Ok(serde_json::from_str(&s.context_search(q)?)?)
    };
    let q = json!({"query":"needle","limit":1});
    let page = search(&a, q.clone())?;
    assert_eq!(page["matches"][0]["message_id"], after);
    let cursor = page["next_cursor"].as_str().unwrap();
    let next = json!({"query":"needle","limit":1,"cursor":cursor});
    assert_eq!(
        search(&a, next.clone())?["matches"][0]["message_id"],
        before
    );
    assert!(b.context_search(next.clone()).is_err());
    assert!(a
        .context_search(json!({"query":"other","cursor":cursor}))
        .is_err());
    let page = search(
        &a,
        json!({"query":"nee.le","regex":true,"range":"before","boundary_id":boundary}),
    )?;
    assert_eq!(page["matches"].as_array().unwrap().len(), 1);
    assert_eq!(page["matches"][0]["message_id"], before);
    assert_eq!(
        search(&a, json!({"query":"needle","range":"after"}))?["matches"][0]["message_id"],
        after
    );
    let db = Connection::open(root.path().join("sessions").join(A).join("session.db"))?;
    let chunks = || {
        db.query_row(
            "SELECT count(*) FROM context_search_chunks WHERE id=?1",
            [&before],
            |r| r.get::<_, usize>(0),
        )
    };
    let n = chunks()?;
    assert!(n > 1);
    let aborted: Result<()> = a.write(|w| {
        w.message_content(&before, "replacement")?;
        bail!("abort after mutation")
    });
    assert!(aborted.is_err());
    assert_eq!(chunks()?, n);
    assert_eq!(
        search(&a, json!({"query":"needle old"}))?["matches"][0]["message_id"],
        before
    );
    a.write(|w| w.message_content(&before, "replacement"))?;
    assert_eq!(chunks()?, 0);
    assert!(search(&a, json!({"query":"needle old"}))?["matches"]
        .as_array()
        .unwrap()
        .is_empty());
    a.delete()?;
    assert!(stores
        .create(A, Default::default())?
        .context_search(next)
        .is_err());
    Ok(())
}

#[test]
fn projection_replaces_usage_and_recomputes_after_fork_rewind_and_restart() -> Result<()> {
    let root = tempdir()?;
    {
        let s = Stores::open_for_test(root.path())?;
        let a = s.create(A, Default::default())?;
        let (first, assistant) = a.write(|w| {
            let first = w.append("user", "first")?;
            let turn = w.start_turn()?;
            let m = w.segment(&turn)?;
            w.snapshots(
                &m,
                &json!([{"id":"old","totalTokens":100},{"id":"kept","inputTokens":10}]),
            )?;
            w.set_request_metadata("kept", &json!({"traced":true}))?;
            w.set_message_metadata(&m, &json!({"type":"trace"}))?;
            w.finish_turn(&turn, TurnStatus::Completed, 1)?;
            w.append("user", "latest")?;
            w.set_git_branch(Some("feature"))?;
            Ok((first, m))
        })?;
        a.write(|w| {
            w.snapshots(
                &assistant,
                &json!([{"id":"kept","inputTokens":4,"outputTokens":3}]),
            )
        })?;
        let p = s.global().summary(A)?.unwrap();
        assert_eq!(
            (
                p.request_count,
                p.total_tokens,
                p.input_tokens,
                p.output_tokens
            ),
            (1, 7, 4, 3)
        );
        assert_eq!(p.first_prompt.as_deref(), Some("first"));
        assert_eq!(p.latest_prompt.as_deref(), Some("latest"));
        assert_eq!(p.git_branch.as_deref(), Some("feature"));
        let fork = a.branch("fork", Branch::Full)?;
        let child = s.global().summary("fork")?.unwrap();
        assert_eq!(
            (child.count, child.total_tokens, child.request_count),
            (3, 7, 1)
        );
        fork.read(|r| {
            let m = &r.messages()?[1].id;
            let requests = r.requests(m)?;
            assert_ne!(requests[0]["id"], "kept");
            assert_eq!(
                r.request_metadata(requests[0]["id"].as_str().unwrap())?,
                Some(json!({"traced":true}))
            );
            assert_eq!(r.message_metadata(m)?, Some(json!({"type":"trace"})));
            Ok(())
        })?;
        a.write(|w| w.rewind_from(&assistant))?;
        assert_eq!(a.read(|r| r.messages())?[0].id, first);
    }
    let s = Stores::open_for_test(root.path())?;
    let p = s.global().summary(A)?.unwrap();
    assert_eq!(
        (
            p.count,
            p.latest_prompt.as_deref(),
            p.status,
            p.total_tokens,
            p.request_count
        ),
        (1, Some("first"), None, 0, 0)
    );
    Ok(())
}

#[test]
fn lost_projection_repairs_on_open_startup_and_unattended_rotation() -> Result<()> {
    let root = tempdir()?;
    let gpath = root.path().join("global.db");
    {
        let s = Stores::open_for_test(root.path())?;
        let a = s.create(A, Default::default())?;
        let g = Connection::open(&gpath)?;
        g.execute_batch("BEGIN IMMEDIATE")?;
        a.write(|w| w.append("user", "open repair"))?;
        assert_eq!(s.global().summary(A)?.unwrap().count, 0);
        g.execute_batch("COMMIT")?;
        s.open_session(A)?;
        assert_eq!(s.global().summary(A)?.unwrap().count, 1);
        g.execute_batch("BEGIN IMMEDIATE")?;
        a.write(|w| w.append("user", "startup repair"))?;
        g.execute_batch("COMMIT")?;
    }
    let s = Stores::open_for_test(root.path())?;
    assert_eq!(s.global().summary(A)?.unwrap().count, 2);
    let handles = (0..19)
        .map(|i| s.create(&format!("idle-{i}"), Default::default()))
        .collect::<Result<Vec<_>>>()?;
    let g = Connection::open(&gpath)?;
    g.execute_batch("BEGIN IMMEDIATE")?;
    for h in &handles {
        h.write(|w| w.append("user", "unattended"))?;
    }
    drop(handles);
    assert_eq!(
        g.query_row(
            "SELECT count(*) FROM conversations WHERE message_count=0",
            [],
            |r| r.get::<_, usize>(0)
        )?,
        19
    );
    g.execute_batch("COMMIT")?;
    let deadline = Instant::now() + Duration::from_secs(50);
    loop {
        let stale: usize = g.query_row(
            "SELECT count(*) FROM conversations WHERE message_count=0",
            [],
            |r| r.get(0),
        )?;
        if stale == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{stale} idle projections still stale"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[test]
fn interrupted_compaction_resumes_checkpoint_and_commits_one_boundary() -> Result<()> {
    let root = tempdir()?;
    let provider = deepseek_code_core::native_engine::provider_manager::ProviderManager::in_memory(
        vec![serde_json::from_value(json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"checkpoint-fixture",
        "api_format":"openai","enabled":true,"models":[{"id":"m","name":"m","enabled":true,"max_tokens":8192,"supports_web_search":false}]}))?],
        root.path().join("providers.json")).resolve_selected("p","m").unwrap();
    let key = CheckpointKey::new(&provider, "configuration", "system", "fragment", 1024);
    let other = CheckpointKey::new(&provider, "configuration", "system", "changed", 1024);
    {
        let s = Stores::open_for_test(root.path())?;
        let a = s.create(A, Default::default())?;
        a.write(|w| {
            w.append("user", "task")?;
            w.append("assistant", "answer")?;
            w.request_compaction("retain", 50, 100)
        })?;
        let owner = a.delivery_owner()?;
        let claim = owner.claim_compaction()?.unwrap();
        assert_eq!(claim.keep, "retain");
        a.write(|w| w.save_compaction_checkpoint(&key, "partial result"))?;
        a.write(|w| w.append("user", "new turn"))?;
        assert!(owner.finish_compaction(&claim, &"stale".into()).is_err());
    }
    let s = Stores::open_for_test(root.path())?;
    let a = s.open_session(A)?;
    let owner = a.delivery_owner()?;
    let claim = owner.claim_compaction()?.unwrap();
    assert!(claim.transcript.contains("new turn"));
    assert_eq!(
        a.read(|r| r.compaction_checkpoint(&key))?.as_deref(),
        Some("partial result")
    );
    assert!(a.read(|r| r.compaction_checkpoint(&other))?.is_none());
    let native = json!({"compaction":true,"opaque":"retained"});
    let id = owner.finish_compaction(
        &claim,
        &GeneratedSummary {
            text: "done".into(),
            native: Some(native.clone()),
        },
    )?;
    assert!(owner.claim_compaction()?.is_none());
    assert!(owner
        .finish_compaction(&claim, &"duplicate".into())
        .is_err());
    drop(owner);
    assert_eq!(a.read(|r| r.compaction_count())?, 1);
    assert!(a.read(|r| r.compaction_checkpoint(&key))?.is_none());
    assert_eq!(a.read(|r| r.native_compaction_boundary(&id))?, Some(native));
    assert_eq!(
        a.branch("branch", Branch::Full)?
            .read(|r| r.compaction_count())?,
        0
    );
    Ok(())
}

#[test]
fn typed_state_and_file_read_evidence_survive_resume_with_session_isolation() -> Result<()> {
    let root = tempdir()?;
    let file = root.path().join("input.txt");
    fs::write(&file, "before")?;
    {
        let s = Stores::open_for_test(root.path())?;
        let a = s.create(A, Default::default())?;
        assert!(a.check_file_write(&file).is_err());
        assert_eq!(
            a.read_tracked_file(&root.path().join("./input.txt"))?,
            b"before"
        );
        s.global()
            .set_setting(GlobalSetting::MachinePreference("theme"), &"dark")?;
        let proposals = serde_json::from_value::<ProposalState>(
            json!({"frontend":null,"pending":null,"declined":["old"]}),
        )?;
        a.write(|w| {
            w.set_goal("finish work")?;
            w.save_goal_proposals(&ProposalState::default(), &proposals)?;
            w.set_agent_mode(PermissionMode::PlanMode)?;
            w.approve_plan("approved work")
        })?;
        let b = s.create("other", Default::default())?;
        assert!(b.check_file_write(&file).is_err());
        assert!(b.read(|r| r.goal())?.is_none());
    }
    let s = Stores::open_for_test(root.path())?;
    let a = s.open_session(A)?;
    assert_eq!(
        s.global()
            .setting::<String>(GlobalSetting::MachinePreference("theme"))?
            .as_deref(),
        Some("dark")
    );
    assert_eq!(a.read(|r| r.goal())?.unwrap().condition, "finish work");
    assert_eq!(
        serde_json::to_value(a.read(|r| r.goal_proposals())?)?["declined"],
        json!(["old"])
    );
    assert_eq!(a.read(|r| r.agent_mode())?, Some(PermissionMode::PlanMode));
    assert!(a.read(|r| r.approved_plan_boundary())?.is_some());
    let boundary = a.write(|w| w.approve_plan("revised approval"))?;
    assert!(a
        .write(|w| w.save_goal_proposals(&ProposalState::default(), &ProposalState::default()))
        .is_err());
    assert!(a.write(|w| w.clear_goal())?);
    assert!(a.read(|r| r.goal())?.is_none());
    a.check_file_write(&file)?;
    fs::write(&file, "changed since read")?;
    assert!(a
        .check_file_write(&file)
        .unwrap_err()
        .to_string()
        .contains("modified since read"));
    a.read_tracked_file(&file)?;
    a.check_file_write(&file)?;
    assert!(a.check_file_write(&root.path().join("new.txt")).is_ok());
    let branched = a.branch("clear", Branch::Clear)?;
    assert!(branched.check_file_write(&file).is_err());
    a.write(|w| w.rewind_from(&boundary))?;
    assert!(a.read(|r| r.approved_plan_boundary())?.is_none());
    Ok(())
}

#[test]
fn upgrading_a_populated_p2_root_backfills_chunks_and_usage() -> Result<()> {
    let root = tempdir()?;
    let text = format!("{}archived needle", "x".repeat(70_000));
    let seal = {
        let s = Stores::open_for_test(root.path())?;
        let a = s.create(A, Default::default())?;
        a.write(|w| {
            let m = w.append("assistant", &text)?;
            w.snapshots(&m, &json!([{"id":"old","totalTokens":19}]))
        })?;
        a.incarnation().to_owned()
    };
    let path = root.path().join("sessions").join(A).join("session.db");
    {
        let c = Connection::open(&path)?;
        c.execute_batch("BEGIN IMMEDIATE")?;
        for owner in ["messages", "tool_calls"] {
            for operation in ["insert", "update", "delete"] {
                c.execute_batch(&format!("DROP TRIGGER context_search_{owner}_{operation}"))?;
            }
        }
        for table in [
            "context_search_chunks",
            "context_search_sources",
            "message_metadata",
            "request_metadata",
            "approved_plan_boundaries",
            "file_read_stamps",
            "peripheral_settings",
            "compaction_checkpoint",
            "compaction_request",
        ] {
            c.execute_batch(&format!("DROP TABLE {table}"))?;
        }
        c.execute_batch("PRAGMA user_version=2; COMMIT")?;
        let g = Connection::open(root.path().join("global.db"))?;
        g.execute_batch(
            "BEGIN IMMEDIATE; DROP TABLE global_settings;
            ALTER TABLE conversations DROP COLUMN request_count;
            ALTER TABLE conversations DROP COLUMN input_tokens;
            ALTER TABLE conversations DROP COLUMN output_tokens;
            ALTER TABLE conversations DROP COLUMN total_tokens;
            PRAGMA user_version=3; COMMIT",
        )?;
    }
    let s = Stores::open_for_test(root.path())?;
    let p = s.global().summary(A)?.unwrap();
    assert_eq!((p.count, p.request_count, p.total_tokens), (1, 1, 19));
    let a = s.open_session(A)?;
    assert_eq!(a.incarnation(), seal);
    let result: Value =
        serde_json::from_str(&a.context_search(json!({"query":"archived needle"}))?)?;
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn tool_search_fragments_follow_result_replacement_and_rollback() -> Result<()> {
    use deepseek_code_core::db::message_repo::ToolCallRow;
    let root = tempdir()?;
    let a = Stores::open_for_test(root.path())?.create(A, Default::default())?;
    let m = a.write(|w| w.append("assistant", "reply"))?;
    let write = |w: &SessionWrite<'_>, name: &str, id: &str, text: &str| {
        w.tool(
            &ToolCallRow {
                id: id.into(),
                message_id: m.clone(),
                name: name.into(),
                input: None,
                output: Some(text.into()),
                is_error: false,
                sort_order: 0,
            },
            None,
        )
    };
    let large = format!("{} unique-result", "z".repeat(100_000));
    a.write(|w| {
        write(w, "custom", "original", &large)?;
        write(w, "ContextSearch", "excluded", &large)
    })?;
    let search = || -> Result<Value> {
        Ok(serde_json::from_str(&a.context_search(
            json!({"query":"unique-result","role":"tool"}),
        )?)?)
    };
    let failed: Result<()> = a.write(|w| {
        write(w, "custom", "original", "changed")?;
        bail!("rollback tool replacement")
    });
    assert!(failed.is_err());
    let hits = search()?;
    assert_eq!(hits["matches"].as_array().unwrap().len(), 1);
    assert_eq!(hits["matches"][0]["tool_call_id"], "original");
    a.write(|w| write(w, "custom", "original", "changed"))?;
    assert!(search()?["matches"].as_array().unwrap().is_empty());
    Ok(())
}

#[test]
fn a_summary_error_leaves_committed_history_available_for_repair() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open_for_test(root.path())?;
    let a = s.create(A, Default::default())?;
    let m = a.write(|w| {
        let m = w.append("assistant", "saved")?;
        w.snapshots(
            &m,
            &json!([{"id":"one","inputTokens":i64::MAX},{"id":"two","inputTokens":1}]),
        )?;
        Ok(m)
    })?;
    assert_eq!(a.read(|r| r.messages())?[0].content, "saved");
    assert_eq!(s.global().summary(A)?.unwrap().count, 0);
    a.write(|w| w.snapshots(&m, &json!([{"id":"two","inputTokens":1}])))?;
    let p = s.global().summary(A)?.unwrap();
    assert_eq!((p.count, p.input_tokens), (1, 1));
    Ok(())
}
