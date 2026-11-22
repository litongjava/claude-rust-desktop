---
name: codegraph
description: "Use CodeGraph code intelligence to inspect symbols and call paths when the project has a .codegraph index"
user-invocable: true
disable-model-invocation: false
---
Use CodeGraph for this code question or change: $ARGUMENTS

1. Check for a `.codegraph/` directory at or above the project root and run
   `codegraph status` to see whether the index exists, how current it is and
   which project it describes.
2. When the `codegraph_explore` MCP tool is available, prefer it: load
   deferred tools with ToolLoad using the exact name, then query with symbol
   names, file names or a short question. Otherwise use the CLI:
   `codegraph explore "<symbols or question>"`, `codegraph node <name>`, or
   `codegraph callers|callees|impact <symbol>`.
3. Treat the result as verbatim source with line numbers plus the call path
   among the shown symbols. Consider those spans already read; open a file
   separately only for lines outside the returned view or to confirm an edit.
4. Use `impact` before editing a shared symbol and state which callers make the
   change risky. Do not infer call paths the tool did not return.
5. After a large edit, run `codegraph sync` so later queries are not stale. If
   no `.codegraph/` directory exists, do not create or download an index
   unless the user asks; fall back to Grep, Glob and Read.

Report which command or tool answered the question, the symbols and file:line
references that matter, and any part of the answer that comes from reasoning
rather than the index.
