CREATE TABLE manifest (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    store_uuid TEXT NOT NULL, id TEXT NOT NULL UNIQUE, incarnation TEXT NOT NULL,
    sealed_operation TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    message_count INTEGER NOT NULL, content_revision INTEGER NOT NULL, last_turn_status TEXT
);
CREATE TABLE messages (
    id TEXT PRIMARY KEY NOT NULL, conversation_id TEXT NOT NULL REFERENCES manifest(id),
    role TEXT NOT NULL, content TEXT NOT NULL, thinking TEXT,
    created_at TEXT NOT NULL, is_compact_boundary INTEGER NOT NULL DEFAULT 0, sort_order INTEGER NOT NULL, model_content TEXT, model_recovery TEXT
);
CREATE TABLE message_user_content (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE, content_json TEXT NOT NULL
);
