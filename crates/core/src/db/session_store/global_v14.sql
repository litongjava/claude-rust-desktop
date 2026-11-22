CREATE TABLE IF NOT EXISTS conversation_edit_decisions (
    incarnation TEXT PRIMARY KEY REFERENCES conversations(incarnation) ON DELETE CASCADE,
    operation TEXT NOT NULL
);
