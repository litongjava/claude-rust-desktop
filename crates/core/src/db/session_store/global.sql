CREATE TABLE store_identity (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    store_uuid TEXT NOT NULL UNIQUE
);
CREATE TABLE projects (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT, instructions TEXT,
    workspace_path TEXT, is_archived INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE project_folders (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path TEXT NOT NULL, trusted INTEGER NOT NULL DEFAULT 0, sort_order INTEGER NOT NULL,
    PRIMARY KEY(project_id, path)
);
CREATE TABLE conversations (
    id TEXT PRIMARY KEY NOT NULL,
    storage_key TEXT NOT NULL UNIQUE, incarnation TEXT NOT NULL UNIQUE,
    title TEXT, model TEXT, provider TEXT, workspace_path TEXT,
    project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
    workspace_mode TEXT NOT NULL CHECK(workspace_mode IN ('chat','work','study','code')),
    research_mode INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0, metadata_revision INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    message_count INTEGER NOT NULL, content_revision INTEGER NOT NULL,
    last_turn_status TEXT, first_prompt TEXT, latest_prompt TEXT,
    git_branch TEXT, transcript_bytes INTEGER NOT NULL
);
CREATE INDEX directory_activity ON conversations(archived, updated_at DESC, id);
CREATE TABLE lifecycle (
    operation TEXT PRIMARY KEY NOT NULL, store_uuid TEXT NOT NULL REFERENCES store_identity(store_uuid),
    id TEXT NOT NULL, storage_key TEXT NOT NULL, incarnation TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('create','delete')),
    metadata TEXT, quarantine_reason TEXT
);
CREATE UNIQUE INDEX lifecycle_pending ON lifecycle(storage_key) WHERE quarantine_reason IS NULL;
