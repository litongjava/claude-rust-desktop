---
name: debug
description: "Investigate a failure from reproduction to root cause before changing code"
user-invocable: true
disable-model-invocation: false
---
Investigate this failure: $ARGUMENTS

1. Reproduce it with the smallest command or scenario that fails. Record the
   exact invocation, working directory, environment and the full error or log
   lines; do not paraphrase away a stack trace.
2. Read the code on the path the failure reports, plus the configuration and
   recent commits that affect it. Use Grep, Glob and LSP to find callers and
   invariants.
3. State one hypothesis at a time and test it with the smallest experiment
   that can falsify it: an added log, a focused test or a narrowed input.
   Change one variable per experiment and keep the evidence.
4. Continue until the root cause is identified; do not patch symptoms. Then
   make the smallest fix that addresses the cause and add or update a
   regression test where the project tests that area.
5. Re-run the original reproduction unchanged and show that it now succeeds.
   Run the nearest existing tests as well.

For a dcl problem, reproduce it with the same command and add
`--debug <categories>` (api, hooks, mcp, lsp, skills, permissions or cli) or
`--debug-file <path>` to capture diagnostics.

Report the reproduction, the root cause with file and line, the fix, the
commands run and their observed results. Separate what was verified from what
remains uncertain or could not be reproduced.
