---
name: verify
description: "Prove a change works end to end by exercising the real entry point, not only tests"
user-invocable: true
disable-model-invocation: false
---
Prove the requested change works: $ARGUMENTS

1. Restate the behavior that must hold and identify the real entry point: the
   executable, CLI command, HTTP endpoint, script or UI route a user would
   use.
2. Build or start only what verification needs, using the project's documented
   commands. For a long-running service, start it in the background and wait
   for readiness.
3. Exercise the change with realistic input, including an error or cancellation
   path and persistence where the behavior involves them. Compare the observed
   output with what the requirement says.
4. Use unit tests and type checks as supporting evidence, not as proof that the
   feature works end to end. When the real entry point cannot be reached in
   this environment, say why.
5. Clean up the processes, temporary files and test data you created.

Report the exact commands and inputs, the observed result with relevant output
or file paths, and the evidence for each requirement. List separately what
could not be verified and the limitation that prevented it. Do not describe a
check as passed when only a related test ran.
