---
name: test-dcl
description: "Test the DeepSeek Code dcl CLI: pure logic, binary behavior, process-level mock runs, and real-terminal REPL panels."
whenToUse: "Use when verifying a dcl change, reproducing a CLI bug, or adding regression coverage for crates/cli or shared core behavior."
user-invocable: true
disable-model-invocation: false
---

# Test dcl

Pick the cheapest layer that can falsify the claim. A unit test cannot prove the REPL renders, and a PTY run cannot pin parsing logic.

## Choose the layer

| Claim | Run |
| --- | --- |
| Core parsing, state, discovery | `cargo test --locked -p deepseek-code-core --lib <filter>` |
| CLI formatting, state files, slash dispatch | `cargo test --locked -p deepseek-code-cli --bin dcl <filter>` |
| Real process, HTTP shape, session DB | `cargo test --locked -p deepseek-code-cli --test <file> <filter>` |
| REPL prompt, panel rendering, key handling | PTY harness below |

Read `docs/cli-regression-tests.md` before changing `crates/cli` or `crates/core`; it names the required commands per area. For `src/` or `src-tauri/`, use `docs/desktop-regression-tests.md`.

## Build and run the binary under test

```bash
cargo build --locked -p deepseek-code-cli --bin dcl
./target/debug/dcl.exe --version
```

Drive `target/debug/dcl.exe` (or `target/release/dcl.exe`), not an installed `dcl`, unless the installed copy itself is the subject.

Isolate state with `--data-dir <temp>` or `DEEPSEEK_CODE_DATA_DIR`. Never point a test at real sessions or credentials; use local mock endpoints.

## What makes dcl tests fail for the wrong reason

- **Piped stdin is not a REPL.** `dcl` refuses it (`repl requires interactive stdin`). Use `scripts/drive_dcl.py` or `references/pty-harness.md`.
- **A fresh data dir is not ready.** It shows the provider onboarding wizard, then the folder trust prompt. Copy a working `config.json` into the temp data dir and pre-seed `<data>/startup-prompts.json`; the reference has the exact shape.
- **Windows path tests can fail at HEAD.** Path-separator assertions and hard-coded hashes can fail before your change. Re-run the same filter after `git stash push` before blaming the change.
- **Parallel SQLite tests can flake.** `SQLITE_BUSY` failures that pass when run alone are contention, not regressions. Re-run the exact filter alone.
- **Panel output is not stdout-only.** `stderr` carries the TUI. Capture both, render with `pyte`, and read the rendered screen; stripping escapes by hand produces duplicated fragments.

## REPL panels

`/skills`, `/plugins`, `/tools` open full-page panels:

- `←/→` switch tabs, `Space` toggles, `Tab` toggles on-demand (Tools → Built-in only), `Enter` opens details or the state picker, `Esc` closes.
- `/skills` lists skills only; `/plugins` lists plugin bundles; `/tools` has Built-in, Skills, Mcp and Plugin tabs.
- Non-terminal equivalents: `dcl skills list`, `dcl config show`, `dcl plugin list`, `dcl mcp list`.

## Inspect a run instead of guessing

- `dcl requests <session>` lists recorded HTTP requests; `dcl sessions list` finds the ID.
- `dcl config show` shows effective tool switches (`tools.<name>.enabled/on_demand`).
- `dcl skills list` prints every skill with its full content; summarize with Python or jq instead of pasting it.

## Report

Name the exact command and filter, the observed test count, and the result. Keep layers separate: unit, binary, integration, PTY. A passing unit filter is not evidence that the panel renders or that a real request carries the tool.
