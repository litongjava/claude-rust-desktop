Diagnose the user's CLI problem: $ARGUMENTS

Use the attached actual local diagnostic snapshot as initial evidence. Explain failed checks and inspect relevant configuration with Read only when needed; never echo secrets, tokens, authorization headers or full provider configuration. Configuration validity does not prove network authentication or model availability.

Use normal tools for targeted follow-up diagnostics. Do not execute executables from untrusted PATH merely to discover their version. Inspect project scripts before running them. Separate observed failures from hypotheses. Repairs, dependency installs, credential changes and network requests must use the ordinary permission flow and require a relevant user request. Do not silently repair or claim a remote service is healthy from local checks.
