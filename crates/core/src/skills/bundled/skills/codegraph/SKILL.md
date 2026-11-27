---
name: codegraph
description: "Use CodeGraph code intelligence to inspect symbols, call paths, and impact (initializing or syncing the .codegraph index when needed)"
user-invocable: true
disable-model-invocation: false
---
Use CodeGraph for fast symbol lookup, call-chain tracing, and change-impact analysis before editing files.

1. **Query directly when READY**: if the workspace index status below is READY, skip `codegraph status` and query immediately with `codegraph explore "<symbols or question>"`, `codegraph node <name>`, or `codegraph callers|callees|impact <symbol>`.
2. **Start with CodeGraph on turn 1 for cross-file tasks**: for tasks touching shared symbols, call chains, or cross-file refactors, start with `codegraph explore`, `codegraph callers`, or `codegraph impact` on turn 1 instead of serial `Grep`/`Bash` hops.
3. **Initialize only when worthwhile**: if the index is not initialized yet (common in fresh git worktrees), run `codegraph init` once before querying when the task involves multi-file exploration; for small single-file or already-known-path edits, skip indexing and use parallel `Read`/`Grep`/`Glob` directly.
4. **Avoid redundant reads & check impact**: treat CodeGraph results as verbatim source with line numbers and verified call paths—open a file separately only for lines outside the returned view or to prepare an edit. Run `codegraph impact <symbol>` before changing a shared symbol; do not infer call paths the tool did not return.
5. **Sync or fall back cleanly**: after large edits, run `codegraph sync` before further CodeGraph queries. If `codegraph` is unavailable or fails, fall back cleanly to `Read`/`Grep`/`Glob`.
