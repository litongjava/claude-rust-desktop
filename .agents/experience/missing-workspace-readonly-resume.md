---
name: Missing workspace read-only resume
description: Interactive dcl can inspect a saved session whose project or worktree directory was deleted, while every execution path stays strict.
---

## Symptom

`dcl --resume <id>` and `/resume` failed with `Folder must be an existing absolute directory` or `Conversation worktree is unavailable` even though the session history was intact.

## Cause

Normal resume resolved the workspace before opening history, so a deleted project folder or bound worktree aborted the transition. The same resolution also guarded trust, archived state and canonical folder identity.

## Fix

- `SessionDb::code_selection_for_inspection` reports `ResumedWorkspace::Missing` for deleted project folders or bound worktrees, but keeps every other strict check. Existing folders must still canonicalize to their recorded trusted paths.
- Interactive startup and `/resume` use the inspection API; print, SDK, fork, init-only and worker paths keep strict resume.
- A missing session gets a display-only observation binding and no workspace, hook, permission-mode, coordination or session-DB writes. REPL prompts, shell/memory shortcuts and mutating commands are rejected through one allowlist; `turn`/`clear`/`branch`/permission switches also guard independently.
- Recreating the directory, or switching to a valid session, restores writable mode and the remembered lifecycle ownership.

## Verification

- `inspection_tests` (core) cover missing project/worktree, trust, changed canonical paths, archived/non-Code and projectless sessions.
- `missing_workspace_tests` and `resume_tests` (CLI) cover valid→missing→valid, recreate, deleted provider, labels, command allowlist and the footer label.
- Real `dcl` + tmux with a local SSE mock: strict `--print --resume` still fails; `/resume` and direct `--resume` show read-only history, reject prompts without changing `session.db`, `/history` and `/status` work; `Ctrl+A` picker shows `[missing]`; switching back and recreating the directory restore writable mode. Evidence in `dev-support/deepseek-code/resume-missing-readonly-20261126/`.
