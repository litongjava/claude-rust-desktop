---
name: task-template
description: "Prepare a brief after deciding to delegate substantial work through Agent, to a teammate, or to a background agent. Skip ordinary background shell commands."
user-invocable: true
disable-model-invocation: false
---
After deciding to delegate substantial work through `Agent`, to a teammate, or to a background agent, write a self-contained brief for the recipient who will do the work. Skip ordinary background shell commands. Use the task details if supplied: $ARGUMENTS

1. **Problem** — Say what needs to be solved, where it occurs, and what evidence supports it. Include the relevant workspace, symptoms, and reproduction steps or log references.
2. **Exact change** — Name the files and functions to change, the expected behavior, and the scope limits. Flag any locations the delegate still needs to confirm.
3. **How to verify** — Give the exact commands and test cases to run, the expected results, and any required setup.
4. **Report to write** — Specify the report path or ask the delegate to include the report in the task result. Use requirement → evidence → unverified. For each requirement, record the command or scenario, actual result, and evidence path. Mark skipped or inconclusive checks as unverified and give the reason.
