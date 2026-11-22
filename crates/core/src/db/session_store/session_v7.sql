CREATE TABLE IF NOT EXISTS sync_receipts(
    digest TEXT PRIMARY KEY,content_revision INTEGER NOT NULL, metadata_revision INTEGER
);
