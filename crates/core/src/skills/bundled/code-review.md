Review the requested code or change: $ARGUMENTS

Use Bash to inspect git status and the requested diff, then Read/Grep/LSP to trace affected callers and invariants. If no target is provided, inspect the working-tree and staged changes; if those are empty, explain that and inspect the most recent commit. Preserve existing edits. Do not modify code unless fixes were requested.

Prioritize reproducible correctness regressions, security defects, data loss and missing cancellation or error handling. Check surrounding implementation before reporting. Run narrowly relevant existing tests if useful; do not invoke unknown scripts without inspecting them. Delegate independent review tasks only if the Agent tool is actually available and its execution is useful; do not claim parallel review otherwise.

Return findings ordered by severity, each with a precise file and line, concrete triggering scenario and consequence. Separate verified findings from uncertainties. If none are found say so, with remaining test coverage limits. Do not invent findings to fill a quota. Do not publish PR comments unless the user requests it.
