Implement or debug the requested Claude API integration using this project's actual language and dependencies. Request: $ARGUMENTS

Use Read/Glob/Grep to inspect manifests, existing API clients, configuration and tests. Prefer the installed official Anthropic SDK where available; for Rust use the project's HTTP client unless an SDK is already selected. Never print API keys or put credentials in source files. Use environment or the existing credential abstraction.

Use WebFetch on https://platform.claude.com/docs/en/api/overview and https://platform.claude.com/docs/en/api/client-sdks to verify current endpoints, SDK methods, models and supported parameters before relying on them. Follow relevant official documentation links for streaming, tool use, token counting or structured output. If fetching is unavailable, explicitly distinguish locally verified behavior from unverified API assumptions.

Implement the requested functionality with the native editing tools. Handle HTTP errors, cancellation, streaming lifecycle and tool results where relevant. Add focused local tests using mock responses; run them with Bash. Do not make paid API calls unless the user requests a live integration test. Report changed files, checks and any unverified remote behavior. This workflow supplies instructions, not a preinstalled external SDK or service.
