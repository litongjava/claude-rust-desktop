-- A path edited after an execution's starting revision shares the lease's
-- file lock until that execution ends. Current bindings also take direct locks.
CREATE TABLE IF NOT EXISTS execution_workspace_paths (
    incarnation TEXT NOT NULL REFERENCES conversations(incarnation) ON DELETE CASCADE,
    path_key TEXT NOT NULL,
    revision INTEGER NOT NULL,
    PRIMARY KEY (incarnation, path_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS execution_workspace_by_path ON execution_workspace_paths(path_key);
