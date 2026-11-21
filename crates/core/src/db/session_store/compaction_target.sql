ALTER TABLE compaction_request ADD COLUMN target_message TEXT;
UPDATE compaction_request SET target_message=(SELECT id FROM messages ORDER BY sort_order DESC,rowid DESC LIMIT 1);
CREATE TABLE compaction_cancellations(id TEXT PRIMARY KEY NOT NULL, reason TEXT NOT NULL);
CREATE TRIGGER cancel_rewound_compaction AFTER DELETE ON messages
WHEN EXISTS(SELECT 1 FROM compaction_request WHERE target_message=OLD.id)
BEGIN
    INSERT INTO compaction_cancellations SELECT id,'target removed by rewind' FROM compaction_request WHERE target_message=OLD.id;
    DELETE FROM compaction_request WHERE target_message=OLD.id;
END;
