-- A surviving child keeps its original parent's identity even after that parent retires.
CREATE TABLE IF NOT EXISTS child_owners (
    child_incarnation TEXT PRIMARY KEY REFERENCES conversations(incarnation) ON DELETE CASCADE,
    parent_incarnation TEXT NOT NULL,
    record TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS parked_sessions (
    parent_incarnation TEXT NOT NULL REFERENCES conversations(incarnation) ON DELETE CASCADE,
    child_incarnation TEXT NOT NULL REFERENCES conversations(incarnation) ON DELETE CASCADE,
    initial_worker_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(parent_incarnation,child_incarnation)
);
CREATE INDEX IF NOT EXISTS parked_sessions_child ON parked_sessions(child_incarnation);
