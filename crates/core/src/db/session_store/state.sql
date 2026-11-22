CREATE TABLE peripheral_settings(key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
CREATE TABLE file_read_stamps(path BLOB PRIMARY KEY NOT NULL, modified_ns TEXT NOT NULL, size TEXT NOT NULL);
CREATE TABLE compaction_request(singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL, keep TEXT NOT NULL);
CREATE TABLE compaction_checkpoint(fingerprint TEXT NOT NULL, request_key TEXT NOT NULL, summary TEXT NOT NULL, PRIMARY KEY(fingerprint,request_key));
CREATE TABLE approved_plan_boundaries(message_id TEXT PRIMARY KEY NOT NULL REFERENCES messages(id) ON DELETE CASCADE);
CREATE TABLE message_metadata(message_id TEXT PRIMARY KEY NOT NULL REFERENCES messages(id) ON DELETE CASCADE, value TEXT NOT NULL);
CREATE TABLE request_metadata(request_id TEXT PRIMARY KEY NOT NULL REFERENCES model_requests(id) ON DELETE CASCADE, value TEXT NOT NULL);
