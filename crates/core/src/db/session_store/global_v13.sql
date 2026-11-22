CREATE TABLE IF NOT EXISTS background_agents (
    job TEXT PRIMARY KEY,
    parent_incarnation TEXT NOT NULL REFERENCES conversations(incarnation) ON DELETE CASCADE,
    child_incarnation TEXT NOT NULL REFERENCES conversations(incarnation) ON DELETE CASCADE,
    agent_type TEXT NOT NULL, description TEXT NOT NULL, started_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS background_agent_child ON background_agents(child_incarnation,started_at);
CREATE TABLE IF NOT EXISTS sa_coder_history (
    task_id TEXT NOT NULL REFERENCES sa_tasks(task_id) ON DELETE CASCADE,
    session_id TEXT NOT NULL, incarnation TEXT NOT NULL,
    PRIMARY KEY(task_id,session_id,incarnation)
);
INSERT OR IGNORE INTO sa_coder_history SELECT task_id,session_id,incarnation FROM sa_member_sessions WHERE role='coder';
CREATE TRIGGER IF NOT EXISTS sa_record_coder AFTER INSERT ON sa_member_sessions WHEN NEW.role='coder' BEGIN
    INSERT OR IGNORE INTO sa_coder_history VALUES(NEW.task_id,NEW.session_id,NEW.incarnation);
END;
