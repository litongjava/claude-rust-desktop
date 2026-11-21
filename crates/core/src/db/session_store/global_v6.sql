CREATE TABLE IF NOT EXISTS cli_binding_generations(
    session_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,pending TEXT,legacy_pid INTEGER,target_provider TEXT,target_model TEXT
);
CREATE TABLE IF NOT EXISTS cli_binding_failures(
    session_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,requested TEXT NOT NULL,restored TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS cli_binding_reservations(
    session_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,generation INTEGER NOT NULL,
    requester TEXT NOT NULL,effort TEXT,requested TEXT NOT NULL,restored TEXT,PRIMARY KEY(session_id,generation)
);
CREATE TABLE IF NOT EXISTS cli_binding_revisions(
    session_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,generation INTEGER NOT NULL,
    previous INTEGER NOT NULL,digest TEXT NOT NULL,options TEXT NOT NULL,PRIMARY KEY(session_id,generation)
);
CREATE TABLE IF NOT EXISTS cli_binding_efforts(
    session_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,revision INTEGER NOT NULL,effort TEXT
);
CREATE TRIGGER IF NOT EXISTS directory_binding_revision AFTER UPDATE OF model,provider ON conversations
WHEN NEW.model IS NOT OLD.model OR NEW.provider IS NOT OLD.provider BEGIN
    UPDATE conversations SET metadata_revision=metadata_revision+1 WHERE id=NEW.id;
END;
