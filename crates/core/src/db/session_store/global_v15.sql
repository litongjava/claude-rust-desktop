-- A copied worktree binding takes precedence over the project's root.
UPDATE conversations AS c SET workspace_path=w.path,metadata_revision=metadata_revision+1
FROM conversation_worktrees AS w WHERE c.id=w.conversation_id AND c.project_id=w.project_id
AND c.workspace_path IS NOT w.path;
