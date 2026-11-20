use super::{Cli, RuntimePaths, Selection, Session};
use crate::binding_store;
use serde_json::json;
use std::ffi::OsString;

fn selection(limit: u32) -> Selection {
    Selection {
        provider_id: "test-provider".into(),
        model: "test-model".into(),
        providers: vec![serde_json::from_value(json!({
            "id":"test-provider","name":"Display name","enabled":true,
            "api_format":"openai","api_key":"fixture","base_url":"http://127.0.0.1:1",
            "models":[{"id":"test-model","name":"Test","enabled":true,
                "supports_web_search":false,"max_tokens":limit}]
        }))
        .unwrap()],
    }
}

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_with_mode_source(std::iter::once("dcl").chain(args.iter().copied())).unwrap()
}

#[tokio::test]
async fn restart_handoff_and_plain_observation_never_publish() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let startup = cli(&["--model", "test-model", "--max-tokens", "137"]);
    let owner = Session::open(&startup, &paths, selection(137), None)
        .await
        .unwrap();
    owner
        .store
        .db
        .with_conn(|db| {
            db.execute_batch(
                "CREATE TRIGGER reject_write BEFORE UPDATE OF provider,model ON conversations
         BEGIN SELECT RAISE(ABORT,'observer attempted binding write'); END;",
            )
        })
        .unwrap()
        .unwrap();
    let observer = cli(&["--resume", &owner.id]);
    let launch = crate::restart::resume(&startup, &owner).unwrap();
    let replacement = Cli::try_parse_with_mode_source(
        std::iter::once(OsString::from("dcl")).chain(launch.arguments),
    )
    .unwrap();
    // A previous-version originator may still send explicit flags alongside its generation.
    let old_handoff = cli(&[
        "--resume",
        &owner.id,
        "--binding-generation",
        "1",
        "--model",
        "test-model",
    ]);
    for args in [observer, replacement, old_handoff] {
        assert!(!args.changes_binding());
        let session = Session::open(&args, &paths, selection(4096), Some(&owner.id))
            .await
            .unwrap();
        assert_eq!(session.binding_state().generation, 1);
        assert_eq!(
            session.launch_provider().await.unwrap().model.max_tokens,
            Some(137)
        );
    }
    assert_eq!(
        binding_store::observe(&owner.store.db, &owner.id)
            .unwrap()
            .generation,
        1
    );
}

#[tokio::test]
async fn rejected_relaunch_keeps_only_the_winning_token_cap() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let startup = cli(&["--model", "test-model", "--max-tokens", "137"]);
    let mut owner = Session::open(&startup, &paths, selection(137), None)
        .await
        .unwrap();
    let stale = cli(&[
        "--resume",
        &owner.id,
        "--binding-generation",
        "1",
        "--max-tokens",
        "137",
    ]);
    owner.switch(&paths, selection(4096)).await.unwrap();
    let session = Session::open(&stale, &paths, selection(137), Some(&owner.id))
        .await
        .unwrap();
    let active = session.launch_provider().await.unwrap();
    assert_eq!(active.model.max_tokens, Some(4096));
    let turn = crate::background_commands::turn_arguments(
        &stale,
        &paths,
        &session,
        &session.id,
        "probe",
        &active,
    )
    .unwrap();
    assert!(!turn.iter().any(|arg| arg.contains("137")));
    let launch = crate::restart::resume(&stale, &session).unwrap();
    let next = Cli::try_parse_with_mode_source(
        std::iter::once(OsString::from("dcl")).chain(launch.arguments),
    )
    .unwrap();
    assert_eq!(next.max_tokens, None);
    assert_eq!(next.binding_generation, Some(2));
}

#[tokio::test]
async fn rejected_clear_rolls_back_the_conversation_and_binding() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let mut session = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let original = session.id.clone();
    session
        .store
        .db
        .with_conn(|db| {
            db.execute_batch(
                "CREATE TRIGGER reject_clear BEFORE INSERT ON cli_binding_revisions
         BEGIN SELECT RAISE(ABORT,'clear binding rejected'); END;",
            )
        })
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        assert!(
            format!("{:#}", session.clear().await.unwrap_err()).contains("clear binding rejected")
        );
        assert_eq!(session.id, original);
        for table in [
            "conversations",
            "cli_binding_generations",
            "cli_binding_revisions",
        ] {
            let count: u64 = session
                .store
                .db
                .with_conn(|db| {
                    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                })
                .unwrap()
                .unwrap();
            assert_eq!(count, 1, "{table}");
        }
    }
}

#[tokio::test]
async fn resume_race_returns_an_error_and_keeps_the_source_handle() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().into()).unwrap();
    let source = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let mut destination = Session::open(&cli(&[]), &paths, selection(137), None)
        .await
        .unwrap();
    let observed = (*destination.binding_state()).clone();
    destination.switch(&paths, selection(4096)).await.unwrap();
    let error = source
        .prepare_resumed_binding(&destination.id, observed)
        .unwrap_err();
    assert!(error.to_string().contains("binding changed during refresh"));
    assert_eq!(source.binding_state().generation, 1);
    assert_eq!(
        source.launch_provider().await.unwrap().model.max_tokens,
        Some(137)
    );
}
