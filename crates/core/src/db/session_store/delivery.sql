CREATE TABLE session_input_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL, admission_digest TEXT NOT NULL, payload TEXT NOT NULL, mode TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1, state TEXT NOT NULL DEFAULT 'queued', delivery_id TEXT NOT NULL UNIQUE,
    target TEXT, UNIQUE(session_id,request_id)
);
CREATE INDEX queue_fifo ON session_input_queue(session_id,state,mode,id);
CREATE TABLE session_input_queue_control(session_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE, paused INTEGER NOT NULL);
CREATE TABLE session_inbox (
    id TEXT PRIMARY KEY, sender TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    recipient TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE, project_source TEXT NOT NULL,
    payload TEXT NOT NULL, size INTEGER NOT NULL, created_at TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'queued', generation INTEGER
);
CREATE INDEX inbox_fifo ON session_inbox(recipient,state,created_at);
CREATE TABLE session_inbox_consumers(session_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,generation INTEGER NOT NULL);
CREATE TABLE session_inbox_operations(sender TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE, operation_id TEXT NOT NULL,
    message_id TEXT NOT NULL REFERENCES session_inbox(id) ON DELETE CASCADE,PRIMARY KEY(operation_id));
CREATE TABLE legacy_session_imports(id TEXT PRIMARY KEY, digest TEXT NOT NULL, project_id TEXT NOT NULL);
CREATE TABLE conversation_private_workspace_bindings (
    child_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    -- Retain an invalid binding after parent deletion: never silently redirect a child.
    parent_id TEXT NOT NULL,
    CHECK(child_id <> parent_id)
);

CREATE TABLE conversation_worktrees (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL,
    source_path TEXT NOT NULL,
    repo_path TEXT NOT NULL,
    path TEXT NOT NULL,
    branch TEXT NOT NULL
);

CREATE TABLE conversation_worktree_backends (
    conversation_id TEXT PRIMARY KEY REFERENCES conversation_worktrees(conversation_id) ON DELETE CASCADE,
    backend TEXT NOT NULL CHECK (backend = 'hook'),
    metadata TEXT NOT NULL
);

