---
name: run
description: "Start the project's app or service so a change can be exercised, then stop it"
user-invocable: true
disable-model-invocation: false
---
Start this project and see the requested change working: $ARGUMENTS

1. Find the documented way to run it in README, AGENTS.md, package scripts,
   Cargo metadata, a Makefile or project scripts. Prefer the project's own
   command over a guessed one, and check prerequisites before starting.
2. Run one-shot builds and checks normally. Start a server, watcher or
   long-running app as a background task (Bash `run_in_background`, or
   `detach` when it must outlive the call) and note its task id, port and log
   path.
3. Wait for readiness from evidence: a listening port, a startup log line or a
   successful health request. Do not assume it is ready because the process
   started.
4. Exercise the change through the running app or service, then inspect the
   logs for errors that the response alone would hide.
5. Stop everything you started (TaskStop or the matching shutdown command) and
   confirm the process is gone. Remove temporary data directories you created.

Inspect project scripts before executing them and use the normal permission
flow for installs or network calls.

Report the command used, the URL or port, what was exercised and the result,
plus the stopping evidence. If the app cannot start here, give the exact error
and mark the run unverified.
