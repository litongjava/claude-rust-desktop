-- The resume picker counts UTF-8 bytes in message text, thinking, tool input and output.
CREATE INDEX IF NOT EXISTS idx_execution_turns_conversation ON execution_turns(conversation_id);
UPDATE projection_state SET transcript_bytes=
    (SELECT coalesce(sum(length(CAST(content AS BLOB))+coalesce(length(CAST(thinking AS BLOB)),0)),0) FROM messages)
    +(SELECT coalesce(sum(coalesce(length(CAST(input AS BLOB)),0)+coalesce(length(CAST(output AS BLOB)),0)),0) FROM tool_calls)
WHERE singleton=1;
CREATE TRIGGER IF NOT EXISTS transcript_thinking_insert AFTER INSERT ON messages WHEN NEW.thinking IS NOT NULL BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+length(CAST(NEW.thinking AS BLOB));
END;
CREATE TRIGGER IF NOT EXISTS transcript_thinking_delete AFTER DELETE ON messages WHEN OLD.thinking IS NOT NULL BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes-length(CAST(OLD.thinking AS BLOB));
END;
CREATE TRIGGER IF NOT EXISTS transcript_thinking_update AFTER UPDATE OF thinking ON messages WHEN NEW.thinking IS NOT OLD.thinking BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+coalesce(length(CAST(NEW.thinking AS BLOB)),0)-coalesce(length(CAST(OLD.thinking AS BLOB)),0);
END;
CREATE TRIGGER IF NOT EXISTS transcript_tool_insert AFTER INSERT ON tool_calls BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+coalesce(length(CAST(NEW.input AS BLOB)),0)+coalesce(length(CAST(NEW.output AS BLOB)),0);
END;
CREATE TRIGGER IF NOT EXISTS transcript_tool_delete AFTER DELETE ON tool_calls BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes-coalesce(length(CAST(OLD.input AS BLOB)),0)-coalesce(length(CAST(OLD.output AS BLOB)),0);
END;
CREATE TRIGGER IF NOT EXISTS transcript_tool_update AFTER UPDATE OF input,output ON tool_calls WHEN NEW.input IS NOT OLD.input OR NEW.output IS NOT OLD.output BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+coalesce(length(CAST(NEW.input AS BLOB)),0)+coalesce(length(CAST(NEW.output AS BLOB)),0)
        -coalesce(length(CAST(OLD.input AS BLOB)),0)-coalesce(length(CAST(OLD.output AS BLOB)),0);
END;
