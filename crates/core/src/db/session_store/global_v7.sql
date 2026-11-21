-- Retain a completed operation's owner across deletion to fence delayed retries.
CREATE TABLE IF NOT EXISTS sync_publications(
    id TEXT NOT NULL,digest TEXT NOT NULL,incarnation TEXT NOT NULL,PRIMARY KEY(id,digest)
);
