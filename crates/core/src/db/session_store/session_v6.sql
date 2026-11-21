-- Request snapshots are part of the resume picker's logical transcript size.
UPDATE projection_state SET transcript_bytes = transcript_bytes +
    (SELECT coalesce(sum(length(CAST(requests_json AS BLOB))),0) FROM message_trajectory);
CREATE TRIGGER transcript_trajectory_insert AFTER INSERT ON message_trajectory BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+length(CAST(NEW.requests_json AS BLOB));
END;
CREATE TRIGGER transcript_trajectory_delete AFTER DELETE ON message_trajectory BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes-length(CAST(OLD.requests_json AS BLOB));
END;
CREATE TRIGGER transcript_trajectory_update AFTER UPDATE OF requests_json ON message_trajectory
WHEN NEW.requests_json IS NOT OLD.requests_json BEGIN
    UPDATE projection_state SET transcript_bytes=transcript_bytes+length(CAST(NEW.requests_json AS BLOB))-length(CAST(OLD.requests_json AS BLOB));
END;
