---
name: commit
description: "Create a Git commit from the user's intended changes"
user-invocable: true
disable-model-invocation: false
---
Create a Git commit from the changes the user intends: $ARGUMENTS

1. Read the repository instructions (AGENTS.md, CLAUDE.md and anything they
   point to). Run `git status`, `git diff`, `git diff --staged` and
   `git log -8 --format='%h %s'` to see what changed and which message style
   the project uses.
2. Decide the exact set of paths. Stage them by name with `git add <paths>`;
   leave unrelated edits, generated files and untracked work alone unless the
   user asked for them. Never stage credentials, `.env` files, tokens or
   private keys.
3. Write an imperative subject that states the problem and the resulting
   behavior, using the prefixes the log shows (for example `fix:` or `feat:`).
   Add a body only when the reason or scope is not obvious from the subject.
4. Create the commit from that staged set. Do not amend, force anything,
   reset, bypass hooks or change Git configuration unless the user explicitly
   asked for it.
5. Verify the result with `git show --stat --oneline HEAD` and `git status`;
   confirm the expected paths are in the commit and the working tree still
   holds the rest.

If the intended changes are ambiguous, ask with AskUserQuestion instead of
guessing.

Report the commit hash, subject, files included, the verification command and
its result, and anything deliberately left uncommitted. Do not push unless the
user asked.
