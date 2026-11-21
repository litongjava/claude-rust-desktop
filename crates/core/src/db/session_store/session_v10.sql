CREATE INDEX IF NOT EXISTS projection_prompt_created ON messages(created_at DESC) WHERE role='user';
