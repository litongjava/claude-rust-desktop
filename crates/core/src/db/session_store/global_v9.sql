-- The execution parent is independent of the project's workspace binding.
CREATE TABLE IF NOT EXISTS session_execution_parents(
    child_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    parent_id TEXT NOT NULL, parent_incarnation TEXT NOT NULL, CHECK(child_id<>parent_id)
);
INSERT OR IGNORE INTO session_execution_parents
    SELECT child_id,parent_id,parent_incarnation FROM conversation_private_workspace_bindings;
