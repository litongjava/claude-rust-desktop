-- Shared project, scheduling, routing and orchestration records. Super Agent
-- evidence intentionally survives deletion of a member conversation.
CREATE TABLE IF NOT EXISTS conversation_binding_generations (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS project_files (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    file_name TEXT,
    file_path TEXT,
    file_size INTEGER,
    mime_type TEXT,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS always_allow_rules (
    id TEXT PRIMARY KEY,
    rule_pattern TEXT NOT NULL,
    rule_type TEXT NOT NULL,
    is_enabled INTEGER DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS scheduled_tasks (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    cron_expression TEXT NOT NULL,
    task_type TEXT NOT NULL,
    task_config TEXT NOT NULL,
    conversation_id TEXT,
    is_enabled INTEGER DEFAULT 1,
    last_run_at TEXT,
    last_run_status TEXT,
    last_run_output TEXT,
    next_run_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS h5_access_tokens (
    id TEXT PRIMARY KEY,
    token TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    is_revoked INTEGER DEFAULT 0,
    created_at TEXT NOT NULL,
    used_at TEXT,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS task_runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL DEFAULT 'running',
    output TEXT,
    error_message TEXT,
    duration_ms INTEGER,
    FOREIGN KEY (task_id) REFERENCES scheduled_tasks(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS im_configs (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL UNIQUE,
    config_json TEXT NOT NULL,
    connection_type TEXT NOT NULL DEFAULT 'webhook' CHECK(connection_type IN ('webhook', 'websocket', 'polling')),
    status TEXT NOT NULL DEFAULT 'disconnected',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS im_connections (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    connection_type TEXT NOT NULL DEFAULT 'websocket' CHECK(connection_type IN ('websocket', 'polling')),
    app_id TEXT,
    app_secret TEXT,
    bot_token TEXT,
    status TEXT NOT NULL DEFAULT 'disconnected',
    ws_url TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS im_message_stats (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    date TEXT NOT NULL,
    message_count INTEGER DEFAULT 0,
    user_count INTEGER DEFAULT 0,
    avg_response_time REAL DEFAULT 0.0,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS im_user_permissions (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    user_id TEXT NOT NULL,
    permission_mode TEXT NOT NULL DEFAULT 'allow' CHECK(permission_mode IN ('allow', 'deny')),
    is_allowed INTEGER DEFAULT 1,
    paired_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS im_error_logs (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    error_type TEXT NOT NULL,
    error_message TEXT,
    stack_trace TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS im_sessions (
    session_key TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    user_id TEXT NOT NULL,
    chat_id TEXT,
    thread_id TEXT,
    conversation_context TEXT,
    last_message_content TEXT,
    last_message_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT,
    message_count INTEGER DEFAULT 0,
    metadata TEXT
);

CREATE TABLE IF NOT EXISTS feishu_chat_mappings (
    chat_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    last_active_at TEXT NOT NULL,
    message_count INTEGER DEFAULT 0,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS sa_operations (
    operation_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    task_id TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sa_tasks (
    task_id TEXT PRIMARY KEY,
    router_session_id TEXT NOT NULL,
    pair_id TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    spec_version INTEGER NOT NULL DEFAULT 1,
    phase TEXT NOT NULL,
    status TEXT NOT NULL,
    review_round INTEGER NOT NULL DEFAULT 0,
    parent_task_id TEXT,
    depends_on_json TEXT NOT NULL DEFAULT '[]',
    coder_session_id TEXT,
    coder_previous_json TEXT NOT NULL DEFAULT '[]',
    coder_agent_status TEXT NOT NULL DEFAULT 'IDLE',
    reviewer_session_id TEXT,
    reviewer_agent_status TEXT NOT NULL DEFAULT 'IDLE',
    cleanup_status TEXT NOT NULL DEFAULT 'NOT_STARTED',
    permission_policy_version INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sa_task_repos (
    task_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    repo_root TEXT NOT NULL,
    git_common_dir TEXT NOT NULL,
    target_path TEXT NOT NULL,
    target_branch TEXT NOT NULL,
    task_branch TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    working_path TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    submitted_commit TEXT,
    reviewed_commit TEXT,
    integrated_commit TEXT,
    integration_status TEXT NOT NULL DEFAULT 'PENDING',
    verification_status TEXT NOT NULL DEFAULT 'PENDING',
    source_state_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY(task_id, repository_id)
);

CREATE TABLE IF NOT EXISTS sa_non_git_targets (
    task_id TEXT NOT NULL,
    path TEXT NOT NULL,
    snapshot_dir TEXT NOT NULL,
    snapshot_manifest_json TEXT NOT NULL,
    diff_ref TEXT,
    status TEXT NOT NULL DEFAULT 'PREPARED',
    lock_key TEXT NOT NULL,
    submission_json TEXT,
    PRIMARY KEY(task_id, path)
);

CREATE TABLE IF NOT EXISTS sa_events (
    event_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    pair_id TEXT NOT NULL,
    session_id TEXT,
    run_id TEXT,
    sequence INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    causation_id TEXT,
    operation_id TEXT NOT NULL UNIQUE,
    timestamp TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    processed INTEGER NOT NULL DEFAULT 0,
    spec_version INTEGER NOT NULL DEFAULT 1,
    review_round INTEGER NOT NULL DEFAULT 0,
    UNIQUE(task_id, sequence)
);

CREATE TABLE IF NOT EXISTS sa_approvals (
    approval_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL UNIQUE,
    task_id TEXT NOT NULL,
    pair_id TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    parent_session_id TEXT NOT NULL,
    request_type TEXT NOT NULL,
    request_json TEXT NOT NULL,
    task_spec_version INTEGER NOT NULL,
    permission_policy_version INTEGER NOT NULL,
    status TEXT NOT NULL,
    answer_json TEXT,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE TABLE IF NOT EXISTS sa_reports (
    report_id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL UNIQUE,
    task_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    spec_version INTEGER NOT NULL,
    round INTEGER NOT NULL DEFAULT 0,
    body_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    invalidated INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sa_locks (
    lock_key TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    owner_operation_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sa_delivery_plans (
    operation_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    spec_version INTEGER NOT NULL,
    review_round INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    plan_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sa_delivery_attempts (
    task_id TEXT NOT NULL,
    content_fingerprint TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(task_id, content_fingerprint)
);

CREATE TABLE IF NOT EXISTS sa_delivery_dispatches (
    task_id TEXT NOT NULL,
    plan_operation_id TEXT NOT NULL,
    spec_version INTEGER NOT NULL,
    review_round INTEGER NOT NULL,
    coder_session_id TEXT NOT NULL,
    purpose TEXT NOT NULL,
    job_id TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY(task_id, plan_operation_id)
);

CREATE TABLE IF NOT EXISTS sa_repository_dispatches (
    task_id TEXT NOT NULL,
    plan_operation_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    spec_version INTEGER NOT NULL,
    review_round INTEGER NOT NULL,
    coder_session_id TEXT NOT NULL,
    purpose TEXT NOT NULL,
    old_oid TEXT NOT NULL,
    new_oid TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    job_id TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY(task_id, plan_operation_id, repository_id)
);

CREATE TABLE IF NOT EXISTS sa_repository_delivery_observations (
    task_id TEXT NOT NULL,
    plan_operation_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    spec_version INTEGER NOT NULL,
    review_round INTEGER NOT NULL,
    coder_session_id TEXT NOT NULL,
    old_oid TEXT NOT NULL,
    new_oid TEXT NOT NULL,
    target_ref TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    observed_at TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'unknown',
    PRIMARY KEY(task_id, plan_operation_id, repository_id)
);

CREATE INDEX IF NOT EXISTS idx_project_files_project_id ON project_files(project_id);

CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_enabled ON scheduled_tasks(is_enabled);

CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_next_run ON scheduled_tasks(next_run_at);

CREATE INDEX IF NOT EXISTS idx_h5_tokens_conversation ON h5_access_tokens(conversation_id);

CREATE INDEX IF NOT EXISTS idx_h5_tokens_expires ON h5_access_tokens(expires_at);

CREATE INDEX IF NOT EXISTS idx_task_runs_task_id ON task_runs(task_id);

CREATE INDEX IF NOT EXISTS idx_task_runs_started_at ON task_runs(started_at);

CREATE INDEX IF NOT EXISTS idx_sa_tasks_router ON sa_tasks(router_session_id, updated_at);

CREATE INDEX IF NOT EXISTS idx_sa_tasks_status ON sa_tasks(status);

CREATE INDEX IF NOT EXISTS idx_sa_events_task_sequence ON sa_events(task_id, sequence);

CREATE INDEX IF NOT EXISTS idx_sa_approvals_task ON sa_approvals(task_id, status);

CREATE INDEX IF NOT EXISTS idx_sa_reports_task ON sa_reports(task_id, kind, spec_version, round);

CREATE INDEX IF NOT EXISTS idx_sa_delivery_plans_task ON sa_delivery_plans(task_id, spec_version, review_round);

CREATE TRIGGER IF NOT EXISTS conversation_binding_changed AFTER UPDATE OF model, provider ON conversations
WHEN NEW.model IS NOT OLD.model OR NEW.provider IS NOT OLD.provider
BEGIN
    INSERT INTO conversation_binding_generations VALUES (NEW.id, 1)
    ON CONFLICT(conversation_id) DO UPDATE SET generation = generation + 1;
END;
