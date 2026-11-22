CREATE TABLE IF NOT EXISTS first_chat_receipts (
    message_id TEXT PRIMARY KEY NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    digest TEXT NOT NULL
);

-- A content edit also invalidates the original submission if the text is later restored.
CREATE TRIGGER IF NOT EXISTS first_chat_text_edit AFTER UPDATE OF role,content ON messages
WHEN OLD.role IS NOT NEW.role OR OLD.content IS NOT NEW.content BEGIN
    DELETE FROM first_chat_receipts WHERE message_id=OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS first_chat_typed_insert AFTER INSERT ON message_user_content BEGIN
    DELETE FROM first_chat_receipts WHERE message_id=NEW.message_id;
END;
CREATE TRIGGER IF NOT EXISTS first_chat_typed_edit AFTER UPDATE ON message_user_content
WHEN OLD.content_json IS NOT NEW.content_json OR OLD.message_id IS NOT NEW.message_id BEGIN
    DELETE FROM first_chat_receipts WHERE message_id IN (OLD.message_id,NEW.message_id);
END;
CREATE TRIGGER IF NOT EXISTS first_chat_typed_delete AFTER DELETE ON message_user_content BEGIN
    DELETE FROM first_chat_receipts WHERE message_id=OLD.message_id;
END;
