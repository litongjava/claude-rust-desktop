CREATE TABLE global_settings(domain TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(domain,name));
ALTER TABLE conversations ADD COLUMN request_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN total_tokens INTEGER NOT NULL DEFAULT 0;
UPDATE conversations SET content_revision=-1;
