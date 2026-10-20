use super::*;
use std::sync::Mutex;

static ENV: Mutex<()> = Mutex::new(());

fn enabled_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
    std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
    guard
}

fn root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    (temp, data)
}

#[test]
fn create_persists_lead_and_allocates_unique_names() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let output = create(
        &root,
        "leader",
        "My Team!",
        Some("purpose"),
        None,
        Some("m"),
        &cwd,
    )
    .unwrap();
    assert_eq!(output.team.name, "my-team");
    assert_eq!(output.team.lead_agent_id, "team-lead@my-team");
    assert_eq!(output.team.lead_session_id, "leader");
    assert_eq!(output.team.members.len(), 1);
    assert_eq!(output.team.members[0].name, LEAD_NAME);
    assert!(!output.team.members[0].is_active);
    assert_eq!(output.team_file_path, team_path(&root, "my-team"));
    assert!(output.team_file_path.exists());

    let other = create(&root, "other", "My Team!", None, None, None, &cwd).unwrap();
    assert_eq!(other.team.name, "my-team-2");
    // A different leader may create another team with the same requested name.
    assert!(create(&root, "leader", "another", None, None, None, &cwd).is_err());
}

#[test]
fn members_bind_finish_and_recover_stale_sessions() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "alpha", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root,
        &team.name,
        "leader",
        "researcher",
        Some("general"),
        Some("m"),
        "investigate",
        &cwd,
    )
    .unwrap();
    assert_eq!(member.agent_id, "researcher@alpha");
    bind_member_session(&root, &team.name, &member.agent_id, "child-1").unwrap();
    register_live("child-1");
    let read = read_recovered(&root, &team.name).unwrap().unwrap();
    let entry = read
        .members
        .iter()
        .find(|m| m.name == "researcher")
        .unwrap();
    assert!(entry.is_active);
    assert_eq!(entry.session_id.as_deref(), Some("child-1"));

    // A restart loses the live registry: persisted active members become stale.
    unregister_live("child-1");
    let recovered = read_recovered(&root, &team.name).unwrap().unwrap();
    let entry = recovered
        .members
        .iter()
        .find(|m| m.name == "researcher")
        .unwrap();
    assert!(!entry.is_active);
    assert_eq!(entry.status.as_deref(), Some("stale"));

    // Other leaders cannot add members.
    assert!(register_member(&root, &team.name, "intruder", "x", None, None, "p", &cwd).is_err());
    assert!(
        register_member(&root, &team.name, "leader", LEAD_NAME, None, None, "p", &cwd).is_err()
    );
    assert!(register_member(
        &root,
        &team.name,
        "leader",
        "Researcher",
        None,
        None,
        "p",
        &cwd
    )
    .is_err());
}

#[test]
fn delete_refuses_active_members_then_removes_the_team() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "beta", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root, &team.name, "leader", "tester", None, None, "run", &cwd,
    )
    .unwrap();
    bind_member_session(&root, &team.name, &member.agent_id, "child-2").unwrap();
    register_live("child-2");

    let refused = delete(&root, "leader").unwrap();
    assert!(!refused.success);
    assert_eq!(refused.team_name.as_deref(), Some("beta"));
    assert!(refused.message.contains("tester"));
    assert!(team_dir(&root, "beta").exists());

    finish_member(&root, &team.name, &member.agent_id, "idle", Some("done")).unwrap();
    let removed = delete(&root, "leader").unwrap();
    assert!(removed.success);
    assert!(removed.message.contains("beta"));
    assert!(!team_dir(&root, "beta").exists());

    let again = delete(&root, "leader").unwrap();
    assert!(again.success);
    assert_eq!(again.team_name, None);
}

#[test]
fn gate_requires_opt_in() {
    assert!(!enabled_with(None));
    for value in ["0", "false", "off", ""] {
        assert!(!enabled_with(Some(value)));
    }
    for value in ["1", "true", "YES", " on "] {
        assert!(enabled_with(Some(value)));
    }
}

#[test]
fn finishes_are_idempotent_for_removed_teams_and_results_are_bounded() {
    let _env = enabled_guard();
    let (_temp, root) = root();
    let cwd = root.join("project");
    let team = create(&root, "leader", "delta", None, None, None, &cwd)
        .unwrap()
        .team;
    let member = register_member(
        &root, &team.name, "leader", "writer", None, None, "write", &cwd,
    )
    .unwrap();
    finish_member(
        &root,
        &team.name,
        &member.agent_id,
        "idle",
        Some(&"x".repeat(9000)),
    )
    .unwrap();
    let read = read_recovered(&root, &team.name).unwrap().unwrap();
    let entry = read.members.iter().find(|m| m.name == "writer").unwrap();
    assert_eq!(entry.result.as_deref().unwrap().chars().count(), 4097);
    delete(&root, "leader").unwrap();
    finish_member(&root, &team.name, &member.agent_id, "idle", None).unwrap();
}
