CREATE INDEX IF NOT EXISTS directory_order ON conversations(updated_at DESC,id);
CREATE INDEX IF NOT EXISTS directory_project_order ON conversations(project_id,updated_at DESC,id);
CREATE TABLE IF NOT EXISTS service_leases(
    name TEXT PRIMARY KEY,owner TEXT NOT NULL,pid INTEGER NOT NULL,expires_at INTEGER NOT NULL,
    stop_requested INTEGER NOT NULL DEFAULT 0,state TEXT NOT NULL,error TEXT,completed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS team_memories(
    team TEXT NOT NULL,id TEXT NOT NULL,title TEXT NOT NULL,content TEXT NOT NULL,tags TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,revision INTEGER NOT NULL DEFAULT 0,dirty INTEGER NOT NULL DEFAULT 1,
    mutation TEXT NOT NULL,PRIMARY KEY(team,id));
CREATE TABLE IF NOT EXISTS team_cursors(team TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS team_conflicts(
    team TEXT NOT NULL,id TEXT NOT NULL,remote TEXT NOT NULL,PRIMARY KEY(team,id));
CREATE TABLE IF NOT EXISTS team_import_sources(team TEXT NOT NULL,id TEXT NOT NULL,source TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(team,id));
CREATE TABLE IF NOT EXISTS team_bindings(team TEXT PRIMARY KEY,endpoint TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS magic_documents(
id TEXT PRIMARY KEY, root TEXT NOT NULL, path TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
auto_update INTEGER NOT NULL DEFAULT 0, updated_at TEXT, update_count INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS magic_previews(
id TEXT PRIMARY KEY, document_id TEXT NOT NULL, original TEXT NOT NULL, proposed TEXT NOT NULL,
created_at TEXT NOT NULL, FOREIGN KEY(document_id) REFERENCES magic_documents(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS installed_plugins(id TEXT PRIMARY KEY, bundle TEXT NOT NULL, source TEXT NOT NULL, enabled INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS mcp_hosts(id TEXT PRIMARY KEY,pid INTEGER NOT NULL,expires_at INTEGER NOT NULL,servers TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS mcp_requests(id TEXT PRIMARY KEY,host TEXT NOT NULL,server TEXT NOT NULL,expires_at INTEGER NOT NULL,state TEXT NOT NULL,error TEXT);
CREATE TABLE IF NOT EXISTS magic_auto_inputs(document_id TEXT PRIMARY KEY,input TEXT NOT NULL,FOREIGN KEY(document_id) REFERENCES magic_documents(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS legacy_magic_imports(path TEXT PRIMARY KEY,payload TEXT NOT NULL,document_id TEXT NOT NULL,FOREIGN KEY(document_id) REFERENCES magic_documents(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS im_session_messages (
    id TEXT PRIMARY KEY,
    session_key TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    message_type TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    raw_data TEXT,
    FOREIGN KEY (session_key) REFERENCES im_sessions(session_key) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_im_configs_platform ON im_configs(platform);
CREATE INDEX IF NOT EXISTS idx_im_configs_status ON im_configs(status);
CREATE INDEX IF NOT EXISTS idx_im_configs_connection_type ON im_configs(connection_type);
CREATE INDEX IF NOT EXISTS idx_im_connections_platform ON im_connections(platform);
CREATE INDEX IF NOT EXISTS idx_im_connections_status ON im_connections(status);
CREATE INDEX IF NOT EXISTS idx_im_connections_type ON im_connections(connection_type);
CREATE INDEX IF NOT EXISTS idx_im_message_stats_platform_date ON im_message_stats(platform, date);
CREATE INDEX IF NOT EXISTS idx_im_message_stats_date ON im_message_stats(date);
CREATE INDEX IF NOT EXISTS idx_im_user_permissions_platform_user ON im_user_permissions(platform, user_id);
CREATE INDEX IF NOT EXISTS idx_im_user_permissions_user_id ON im_user_permissions(user_id);
CREATE INDEX IF NOT EXISTS idx_im_user_permissions_paired_code ON im_user_permissions(paired_code);
CREATE INDEX IF NOT EXISTS idx_im_error_logs_platform ON im_error_logs(platform);
CREATE INDEX IF NOT EXISTS idx_im_error_logs_error_type ON im_error_logs(error_type);
CREATE INDEX IF NOT EXISTS idx_im_error_logs_created_at ON im_error_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_im_sessions_platform ON im_sessions(platform);
CREATE INDEX IF NOT EXISTS idx_im_sessions_user ON im_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_im_sessions_chat ON im_sessions(chat_id);
CREATE INDEX IF NOT EXISTS idx_im_session_messages_session ON im_session_messages(session_key);
CREATE INDEX IF NOT EXISTS idx_im_session_messages_timestamp ON im_session_messages(timestamp);
CREATE TABLE IF NOT EXISTS desktop_queue_context(session_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,request_json TEXT NOT NULL);
