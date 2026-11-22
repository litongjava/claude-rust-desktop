-- Deduplication and delivery records retire with the recipient. The sender
-- address retains its incarnation even after the logical ID is reused.
ALTER TABLE session_inbox_operations RENAME TO inbox_operations_v2;
ALTER TABLE session_inbox RENAME TO inbox_v2;
DROP INDEX inbox_fifo;
CREATE TABLE session_inbox (
    id TEXT PRIMARY KEY, sender TEXT NOT NULL, sender_incarnation TEXT NOT NULL,
    recipient TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE, project_source TEXT NOT NULL,
    payload TEXT NOT NULL, size INTEGER NOT NULL, created_at TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued', generation INTEGER,
    read INTEGER NOT NULL DEFAULT 0, blocked_reason TEXT, committed_at TEXT
);
INSERT INTO session_inbox SELECT i.id,i.sender,
    (SELECT incarnation FROM conversations WHERE id=i.sender),i.recipient,i.project_source,
    i.payload,i.size,i.created_at,i.state,i.generation,i.read,i.blocked_reason,i.committed_at
    FROM inbox_v2 i ORDER BY i.rowid;
CREATE INDEX inbox_fifo ON session_inbox(recipient,state,created_at);
CREATE TABLE session_inbox_operations(sender TEXT NOT NULL, operation_id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES session_inbox(id) ON DELETE CASCADE);
INSERT INTO session_inbox_operations SELECT sender,operation_id,message_id FROM inbox_operations_v2;
DROP TABLE inbox_operations_v2;
DROP TABLE inbox_v2;
