CREATE TABLE domain_meta (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  domain_id TEXT NOT NULL UNIQUE, created_ms INTEGER NOT NULL
);
CREATE TABLE instances (
  instance_id TEXT PRIMARY KEY, data_namespace_id TEXT NOT NULL,
  session_id TEXT, kind TEXT NOT NULL, role TEXT NOT NULL,
  parent_instance_id TEXT, worker_id TEXT, job_id TEXT,
  owner_json TEXT NOT NULL, workspace_encoding TEXT NOT NULL,
  workspace_path BLOB NOT NULL, repo_id TEXT, worktree_id TEXT, branch_ref BLOB,
  activity TEXT NOT NULL, activity_since_ms INTEGER NOT NULL,
  started_ms INTEGER NOT NULL, heartbeat_ms INTEGER NOT NULL, last_git_observed_ms INTEGER,
  lifecycle TEXT NOT NULL CHECK(lifecycle IN ('open','closed','gone'))
);
CREATE INDEX instances_session ON instances(data_namespace_id,session_id);
CREATE INDEX instances_live ON instances(lifecycle,started_ms,instance_id);
PRAGMA user_version=1;
