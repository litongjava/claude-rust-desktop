# Third-party bundled skills

The skill folders in `bundled/skills/` are installed into the data directory as
built-in skills. This file records the provenance and license of every bundled
skill that did not originate in this repository.

## anthropics/skills

Source repository: <https://github.com/anthropics/skills>
Commit: `e4bb5d960d87126ff3b3a8af1a104b7db67560ba`
License: Apache License 2.0 (each folder carries its own `LICENSE.txt`).

The following folders are copied unmodified. Each has an Apache-2.0
`LICENSE.txt` and a `NOTICE.md` recording the source repository, commit and
path.

| Skill | Source path | License |
| --- | --- | --- |
| algorithmic-art | `skills/algorithmic-art` | Apache-2.0 |
| canvas-design | `skills/canvas-design` | Apache-2.0 (fonts: SIL OFL, see `canvas-fonts/*-OFL.txt`) |
| frontend-design | `skills/frontend-design` | Apache-2.0 |
| internal-comms | `skills/internal-comms` | Apache-2.0 |
| mcp-builder | `skills/mcp-builder` | Apache-2.0 |
| slack-gif-creator | `skills/slack-gif-creator` | Apache-2.0 |
| theme-factory | `skills/theme-factory` | Apache-2.0 |
| webapp-testing | `skills/webapp-testing` | Apache-2.0 |
| docx | `skills/docx` | Apache-2.0 |
| pdf | `skills/pdf` | Apache-2.0 |
| pptx | `skills/pptx` | Apache-2.0 |
| xlsx | `skills/xlsx` | Apache-2.0 |
| claude-api | `skills/claude-api` | Apache-2.0 |

## skill-creator

`bundled/skills/skill-creator/` is adapted from OpenAI Codex's `skill-creator`
sample under the Apache License 2.0. It is modified for DeepSeek Code; see
`bundled/skills/skill-creator/NOTICE.md` and `license.txt`.
