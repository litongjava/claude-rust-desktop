CREATE TABLE IF NOT EXISTS sa_completion_evidence(
    event_id TEXT NOT NULL REFERENCES sa_events(event_id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,incarnation TEXT NOT NULL,content_revision INTEGER NOT NULL,
    PRIMARY KEY(event_id,session_id)
);
CREATE TABLE IF NOT EXISTS sa_member_sessions(
    task_id TEXT NOT NULL REFERENCES sa_tasks(task_id) ON DELETE CASCADE, role TEXT NOT NULL,
    session_id TEXT NOT NULL,incarnation TEXT NOT NULL,PRIMARY KEY(task_id,role)
);
CREATE TRIGGER IF NOT EXISTS sa_bind_coder AFTER UPDATE OF coder_session_id ON sa_tasks
WHEN NEW.coder_session_id IS NOT OLD.coder_session_id BEGIN
    DELETE FROM sa_member_sessions WHERE task_id=NEW.task_id AND role='coder';
    INSERT INTO sa_member_sessions SELECT NEW.task_id,'coder',id,incarnation FROM conversations WHERE id=NEW.coder_session_id;
END;
CREATE TRIGGER IF NOT EXISTS sa_bind_reviewer AFTER UPDATE OF reviewer_session_id ON sa_tasks
WHEN NEW.reviewer_session_id IS NOT OLD.reviewer_session_id BEGIN
    DELETE FROM sa_member_sessions WHERE task_id=NEW.task_id AND role='reviewer';
    INSERT INTO sa_member_sessions SELECT NEW.task_id,'reviewer',id,incarnation FROM conversations WHERE id=NEW.reviewer_session_id;
END;
