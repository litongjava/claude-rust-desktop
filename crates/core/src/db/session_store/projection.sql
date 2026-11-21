-- The directory can be repaired with one singleton read, including after an
-- active turn that has deliberately coalesced its publications to G.
CREATE TABLE projection_state(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), transcript_bytes INTEGER NOT NULL,
    first_prompt TEXT, latest_prompt TEXT, prompts_dirty INTEGER NOT NULL,
    history_dirty INTEGER NOT NULL, request_count INTEGER NOT NULL,
    input_tokens TEXT NOT NULL, output_tokens TEXT NOT NULL, total_tokens TEXT NOT NULL
);
INSERT INTO projection_state VALUES(1,
    (SELECT coalesce(sum(length(CAST(content AS BLOB))),0) FROM messages),
    NULL,NULL,1,0,0,'0','0','0');
CREATE INDEX projection_prompt_order ON messages(sort_order) WHERE role='user' AND content<>'';
CREATE TRIGGER projection_message_insert AFTER INSERT ON messages BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+length(CAST(NEW.content AS BLOB)),
        prompts_dirty=max(prompts_dirty,NEW.role='user'), history_dirty=1;
END;
CREATE TRIGGER projection_message_delete AFTER DELETE ON messages BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes-length(CAST(OLD.content AS BLOB)),
        prompts_dirty=max(prompts_dirty,OLD.role='user'), history_dirty=1;
END;
CREATE TRIGGER projection_message_update AFTER UPDATE ON messages BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes-length(CAST(OLD.content AS BLOB))+length(CAST(NEW.content AS BLOB)),
        prompts_dirty=max(prompts_dirty,OLD.role='user',NEW.role='user'), history_dirty=1;
END;
CREATE TRIGGER projection_typed_insert AFTER INSERT ON message_user_content BEGIN UPDATE projection_state SET history_dirty=1; END;
CREATE TRIGGER projection_typed_update AFTER UPDATE ON message_user_content BEGIN UPDATE projection_state SET history_dirty=1; END;
CREATE TRIGGER projection_typed_delete AFTER DELETE ON message_user_content BEGIN UPDATE projection_state SET history_dirty=1; END;

-- Fold the transaction's deltas with wide arithmetic before commit. Keep the
-- sign separate so even the minimum signed SQLite integer can be subtracted.
CREATE TABLE request_metric_deltas(direction INTEGER NOT NULL, input INTEGER, output INTEGER, total INTEGER);
INSERT INTO request_metric_deltas SELECT 1,input_tokens,output_tokens,total_tokens FROM model_requests;
CREATE TRIGGER projection_request_insert AFTER INSERT ON model_requests BEGIN
    INSERT INTO request_metric_deltas VALUES(1,NEW.input_tokens,NEW.output_tokens,NEW.total_tokens);
END;
CREATE TRIGGER projection_request_update AFTER UPDATE OF input_tokens,output_tokens,total_tokens ON model_requests BEGIN
    INSERT INTO request_metric_deltas VALUES(-1,OLD.input_tokens,OLD.output_tokens,OLD.total_tokens);
    INSERT INTO request_metric_deltas VALUES(1,NEW.input_tokens,NEW.output_tokens,NEW.total_tokens);
END;
CREATE TRIGGER projection_request_delete AFTER DELETE ON model_requests BEGIN
    INSERT INTO request_metric_deltas VALUES(-1,OLD.input_tokens,OLD.output_tokens,OLD.total_tokens);
END;
