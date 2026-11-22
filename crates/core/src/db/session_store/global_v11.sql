ALTER TABLE session_inbox ADD COLUMN recipient_source TEXT;
ALTER TABLE session_inbox ADD COLUMN notified INTEGER NOT NULL DEFAULT 0;
CREATE INDEX inbox_notices ON session_inbox(recipient) WHERE notified=0;
CREATE INDEX IF NOT EXISTS sa_member_directory ON sa_member_sessions(session_id,incarnation,task_id);
-- Only a live router present at task creation can authorize a cross-project send.
CREATE TRIGGER IF NOT EXISTS sa_bind_router AFTER INSERT ON sa_tasks BEGIN
    INSERT INTO sa_member_sessions SELECT NEW.task_id,'router',id,incarnation
        FROM conversations WHERE id=NEW.router_session_id;
END;
