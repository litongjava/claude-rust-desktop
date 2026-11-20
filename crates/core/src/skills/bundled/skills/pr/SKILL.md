---
name: pr
description: "Open or update a GitHub pull request with the gh CLI, stating the problem, change, and verification"
user-invocable: true
disable-model-invocation: false
---
Open or update a pull request for the current work: $ARGUMENTS

1. Read the repository instructions and inspect the branch: `git status`,
   `git log`, `git diff <base>...HEAD` and `git remote -v`. Choose the base
   branch from the project's convention, usually `main`.
2. Check whether the branch is pushed:
   `git rev-parse --abbrev-ref --symbolic-full-name @{u}` and
   `git status -sb`. If it is not pushed or is behind, ask before pushing;
   never force-push.
3. Look for an existing pull request with `gh pr view` or
   `gh pr list --head <branch>`. Create one with
   `gh pr create --base <base> --title <title> --body-file <file>`, or update
   it with `gh pr edit <number> --title ... --body-file ...`. Write the body
   to a temporary file instead of fighting shell quoting.
4. In the title, say what changed. In the body, cover the problem and its
   evidence, the change and its scope, how it was verified with commands and
   results, unverified platforms, integrations or risks, and linked issues the
   user provided. Include screenshots for UI changes when you have them.
5. Run `gh pr view <number> --json url,title,state` and return the URL with
   the pull request number.

Report the URL, base and head branches, title, and anything that remains
unverified. Do not merge, close, push tags or request reviews unless the user
asked.
