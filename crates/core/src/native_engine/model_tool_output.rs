//! Model-only rendering. Stored results, hooks and UI events keep their original data.
use crate::performance::{
    preview_with_ranges, retain_tool_output_with_ranges, PreviewRanges, LONG_COMMAND_OUTPUT_BYTES,
    MODEL_TOOL_OUTPUT_BYTES,
};
use crate::tools::bash_process::CommandMetadata;
use serde_json::Value;
use std::{ops::Range, path::Path};

const DIGEST_BUDGET: usize = 2048;

#[path = "command_output_summary.rs"]
mod command_output_summary;

#[path = "base64_preview.rs"]
pub(crate) mod base64_preview;

pub(crate) fn monitor_line(text: &str, root: &Path) -> Option<String> {
    base64_preview::Collapsed::prepare("Monitor", text, root).map(|preview| preview.text)
}

impl super::ToolLoopExecutor {
    /// Called by both provider builders, after typed MCP/image restoration.
    pub(super) fn prepare_history_tool_outputs(&self, messages: &mut [Value]) {
        let mut names = std::collections::HashMap::<String, String>::new();
        for message in messages {
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    if let (Some(id), Some(name)) =
                        (call["id"].as_str(), call["function"]["name"].as_str())
                    {
                        names.insert(id.into(), name.into());
                    }
                }
            }
            if message["role"] == "tool" {
                if let (Some(name), Some(text)) = (
                    message["tool_call_id"]
                        .as_str()
                        .and_then(|id| names.get(id)),
                    message["content"].as_str(),
                ) {
                    message["content"] = Value::String(self.preview_tool_output_for_id(
                        name,
                        text,
                        message["tool_call_id"].as_str(),
                    ));
                }
            } else if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if block["type"] == "tool_use" {
                        if let (Some(id), Some(name)) =
                            (block["id"].as_str(), block["name"].as_str())
                        {
                            names.insert(id.into(), name.into());
                        }
                    } else if block["type"] == "tool_result" {
                        if let Some(name) =
                            block["tool_use_id"].as_str().and_then(|id| names.get(id))
                        {
                            if let Some(text) = block["content"].as_str() {
                                block["content"] = Value::String(self.preview_tool_output_for_id(
                                    name,
                                    text,
                                    block["tool_use_id"].as_str(),
                                ));
                            } else {
                                self.retain_content(name, &mut block["content"]);
                            }
                        }
                    }
                }
            }
        }
    }

    pub(super) fn retain_content(&self, name: &str, content: &mut Value) {
        if let Some(text) = content.as_str() {
            *content = Value::String(self.preview_tool_output(name, text));
        } else if let Some(blocks) = content.as_array() {
            // Text-only MCP blocks share one budget. Images and opaque blocks stay intact.
            if blocks.iter().all(|block| {
                block["type"] == "text"
                    && block["text"].is_string()
                    && block.as_object().is_some_and(|o| o.len() == 2)
            }) {
                let text = blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                *content = Value::String(self.preview_tool_output(name, &text));
            }
        }
    }
}

pub(super) fn render(name: &str, output: &str, body_budget: Option<usize>) -> String {
    render_with_spans(name, output, body_budget, &mut [None, None])
}

struct BodySpan {
    truncated: bool,
    start: usize,
    ranges: PreviewRanges,
}

fn render_with_spans(
    name: &str,
    output: &str,
    body_budget: Option<usize>,
    spans: &mut [Option<BodySpan>; 2],
) -> String {
    // ToolLoad's structured result (loaded names, schemas) is kept for the
    // transcript, the UI and the SDK. The model only needs the content line:
    // the next request already carries the loaded definitions, and repeating
    // them here would grow every later request.
    if name == "ToolLoad" {
        return tool_load_content(output).unwrap_or_else(|| output.into());
    }
    if !matches!(
        name,
        "Bash" | "Read" | "Write" | "Edit" | "MultiEdit" | "ApplyPatch"
    ) {
        return output.into();
    }
    let (prefix, json) = output
        .strip_prefix("Error: ")
        .map_or(("", output), |s| ("Error: ", s));
    // Hooks can append context after the JSON value; preserve that suffix verbatim.
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(Value::Object(mut fields))) = stream.next() else {
        return output.into();
    };
    let suffix = &json[stream.byte_offset()..];
    if name == "ApplyPatch" {
        // Per-file diffs stay in the UI/records; the model keeps the counts and
        // path lists, including every unrestored path.
        fields.remove("file_change");
        if let Some(Value::Array(changes)) = fields.get_mut("changes") {
            for change in changes {
                if let Some(object) = change.as_object_mut() {
                    object.remove("file_change");
                }
            }
        }
        return format!("{prefix}{}{suffix}", Value::Object(fields));
    }
    if matches!(name, "Write" | "Edit" | "MultiEdit") {
        if fields.remove("file_change").is_none() {
            return output.into();
        }
        return format!("{prefix}{}{suffix}", Value::Object(fields));
    }
    let mut parts = Vec::new();
    let truncated = [
        fields.get("stdout_truncated") == Some(&Value::Bool(true)),
        fields.get("stderr_truncated") == Some(&Value::Bool(true)),
    ];
    let mut bodies = Vec::new();
    let mut span_parts = Vec::new();
    if name == "Read" {
        let Some(content) = fields
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return output.into();
        };
        // Images are handled by the multimodal path, never flattened here.
        if fields.contains_key("image") {
            return output.into();
        }
        fields.remove("content");
        if let Some(lines) = fields.remove("lines") {
            if fields.get("truncated") == Some(&Value::Bool(true)) {
                parts.push(format!(
                    "File has {lines} lines; more lines remain. Use offset/limit to continue."
                ));
            }
        }
        fields.remove("truncated");
        bodies.push(content);
    } else {
        if !fields.contains_key("stdout")
            && !fields.contains_key("stderr")
            && !fields.contains_key("error")
        {
            return output.into();
        }
        let requested_shell = fields
            .remove("shell")
            .and_then(|shell| shell.as_str().map(str::to_owned));
        // A zero exit code is the normal case; only a failure needs naming.
        let mut exit_shown = false;
        if fields.get("exit_code").is_none_or(Value::is_null) {
            if let Some(signal) = fields
                .get("signal")
                .and_then(Value::as_u64)
                .filter(|s| *s > 0)
            {
                parts.push(format!("Command terminated by signal {signal}"));
                fields.remove("signal");
                exit_shown = true;
            }
        }
        if let Some(code) = fields.remove("exit_code") {
            if !exit_shown && code.as_i64().is_some_and(|code| code != 0 && code != -1) {
                parts.push(format!("Exit code: {code}"));
                exit_shown = true;
            }
        }
        if fields.get("success") == Some(&Value::Bool(false)) && !exit_shown {
            parts.push("Command failed".into());
        }
        fields.remove("success");
        for (key, label) in [
            ("timed_out", "Command timed out"),
            ("is_error", "Tool reported an error"),
            (
                "stdout_truncated",
                "Stdout is incomplete (capture or tool limit)",
            ),
            (
                "stderr_truncated",
                "Stderr is incomplete (capture or tool limit)",
            ),
        ] {
            if fields.remove(key) == Some(Value::Bool(true)) {
                parts.push(label.into());
            }
        }
        // A pipeline closed early is not a failure; say so in words instead of
        // leaving a raw flag for the model to decode.
        if fields.remove("pipeline_sigpipe") == Some(Value::Bool(true)) {
            parts.push("Output ended early: a later command in the pipeline closed it".into());
        }
        // The executable that ran is only news when it is not the one asked
        // for; a matching shell needs no line at all.
        if let Some(Value::String(path)) = fields.remove("shell_path") {
            if let Some(requested) = requested_shell.as_deref() {
                if !shell_matches(requested, &path) {
                    parts.push(format!("Shell: {path} (requested {requested})"));
                }
            }
        }
        if let Some(error) = fields.remove("error") {
            parts.push(format!(
                "Error: {}",
                error
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string())
            ));
        }
        let streams = ["stdout", "stderr"]
            .iter()
            .filter(|key| {
                fields
                    .get(**key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
            .count()
            .max(1);
        for (index, (key, label)) in [("stdout", "Output"), ("stderr", "Stderr")]
            .into_iter()
            .enumerate()
        {
            if let Some(Value::String(text)) = fields.remove(key) {
                if !text.is_empty() {
                    let budget = body_budget.map_or(text.len(), |budget| budget / streams);
                    let (text, ranges) = preview_with_ranges(&text, budget);
                    span_parts.push((index, label.len() + 2, ranges));
                    bodies.push(format!("{label}:\n{text}"));
                }
            }
        }
        // Without a status line or a second stream, the `Output:` label adds
        // nothing: the body is simply the command's output.
        if prefix.is_empty() && parts.is_empty() && fields.is_empty() {
            if let [(0, label_len, ranges)] = span_parts.as_slice() {
                spans[0] = Some(BodySpan {
                    truncated: truncated[0],
                    start: 0,
                    ranges: ranges.clone(),
                });
                return format!("{}{suffix}", &bodies[0][*label_len..]);
            }
            if bodies.is_empty() {
                // A successful command with nothing to say must still read as
                // finished rather than as an unfinished task.
                return format!("(Completed with no output){suffix}");
            }
        }
    }
    // Preserve extensions (diagnostics, paths, task IDs) rather than dropping unknown fields.
    if !fields.is_empty() {
        parts.push(format!("Metadata: {}", Value::Object(fields)));
    }
    let mut offset = prefix.len() + parts.iter().map(|part| part.len() + 1).sum::<usize>();
    for ((index, label_len, ranges), body) in span_parts.into_iter().zip(&bodies) {
        spans[index] = Some(BodySpan {
            truncated: truncated[index],
            start: offset + label_len,
            ranges,
        });
        offset += body.len() + 1;
    }
    parts.extend(bodies);
    format!("{prefix}{}{suffix}", parts.join("\n"))
}

/// ToolLoad's model-visible text: the result's own `content` line. A hook
/// context appended after the JSON stays, but the loaded schemas do not.
fn tool_load_content(output: &str) -> Option<String> {
    let json = output.strip_prefix("Error: ").unwrap_or(output);
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(value)) = stream.next() else {
        return None;
    };
    let content = value.get("content")?.as_str()?;
    Some(format!("{content}{}", &json[stream.byte_offset()..]))
}

pub(super) fn is_shell_result(name: &str) -> bool {
    matches!(name, "Bash" | "PowerShell" | "TaskGet" | "TaskOutput")
}

pub(super) fn metadata_path(root: &Path, id: &str) -> std::path::PathBuf {
    // Transcript row ids prefix the occurrence with the assistant message id.
    let id = id.rsplit(':').next().unwrap_or(id);
    let digest = ring::digest::digest(&ring::digest::SHA256, id.as_bytes());
    let key: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    root.join("tool-output-metadata")
        .join(format!("{key}.json"))
}

pub(super) fn save_metadata(
    root: &Path,
    id: &str,
    mut capture: CommandMetadata,
) -> std::io::Result<()> {
    use std::io::Write;
    let path = metadata_path(root, id);
    let dir = root.join("tool-output-metadata");
    std::fs::create_dir_all(&dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    if let CommandMetadata::Shell(shell) = &mut capture {
        if let Some((path, _)) = shell.recovery(root)? {
            shell.source_file = Some(path);
        }
    }
    file.write_all(&serde_json::to_vec(&capture)?)?;
    file.persist(path)?;
    Ok(())
}

pub(crate) fn read_metadata(root: &Path, id: &str) -> Option<CommandMetadata> {
    serde_json::from_slice(&std::fs::read(metadata_path(root, id)).ok()?).ok()
}

/// Keep ordinary stored history in its existing format. For an encoded result,
/// use the same renderer and occurrence-specific source metadata as a live turn.
pub(crate) fn history_projection(
    name: &str,
    raw: &str,
    root: &Path,
    id: &str,
    enabled: bool,
) -> Option<String> {
    if !matches!(name, "Bash" | "TaskGet" | "TaskOutput") {
        return None;
    }
    let metadata = read_metadata(root, id);
    command_preview_inner(name, raw, Some(root), metadata.as_ref(), enabled, 0, true)
}

pub(crate) fn command_preview(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    metadata: Option<&CommandMetadata>,
    enabled: bool,
) -> String {
    command_preview_with_room(name, raw, root, metadata, enabled, 0)
}

fn command_preview_with_room(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    metadata: Option<&CommandMetadata>,
    enabled: bool,
    reserved: usize,
) -> String {
    command_preview_inner(name, raw, root, metadata, enabled, reserved, false)
        .expect("unconditional preview")
}

fn command_preview_inner(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    metadata: Option<&CommandMetadata>,
    enabled: bool,
    reserved: usize,
    only_collapsed: bool,
) -> Option<String> {
    use command_output_summary::Stream;
    let bash = name == "Bash";
    let capture = match metadata {
        Some(CommandMetadata::Shell(s)) => Some(s),
        _ => None,
    };
    let page_bytes = match metadata {
        Some(CommandMetadata::Page { bytes, widths, .. }) => Some((bytes, widths)),
        _ => None,
    };
    let recovery = root
        .zip(capture)
        .filter(|_| bash)
        .and_then(|(root, c)| c.recovery(root).ok().flatten());
    let streams = recovery.as_ref().zip(capture).and_then(|((_, bytes), c)| {
        let [out, err] = c.source_ranges();
        Some([(out.start, bytes.get(out)?), (err.start, bytes.get(err)?)])
    });
    let restored = streams.as_ref().zip(capture).and_then(|(streams, c)| {
        let (prefix, json) = raw
            .strip_prefix("Error: ")
            .map_or(("", raw), |json| ("Error: ", json));
        let mut input = serde_json::Deserializer::from_str(json).into_iter::<Value>();
        let mut value = input.next()?.ok()?;
        for ((name, (_, bytes)), stats) in ["stdout", "stderr"]
            .into_iter()
            .zip(streams)
            .zip([&c.stdout, &c.stderr])
        {
            let shown = value[name].as_str()?;
            if stats.preview_digest
                != Some(crate::tools::bash_process::StreamCapture::digest(shown))
            {
                return None;
            }
            value[name] = String::from_utf8_lossy(bytes).into_owned().into();
            value[format!("{name}_truncated")] = stats.capture_prefix.into();
        }
        Some(format!("{prefix}{value}{}", &json[input.byte_offset()..]))
    });
    let streams = streams.filter(|_| restored.is_some());
    let raw = restored.as_deref().unwrap_or(raw);
    let mut spans = [None, None];
    let cleaned = render_with_spans(name, raw, None, &mut spans);
    let collapsed = root
        .filter(|_| matches!(name, "Bash" | "TaskGet" | "TaskOutput"))
        .and_then(|root| {
            let mut locations = std::collections::HashMap::new();
            let blocks = (bash && spans.iter().any(Option::is_some)).then(|| {
                let mut blocks = Vec::new();
                for (index, span) in spans.iter().enumerate() {
                    let Some(span) = span else { continue };
                    let prefix = capture
                        .is_some_and(|c| [c.stdout.capture_prefix, c.stderr.capture_prefix][index]);
                    if span.truncated && !prefix {
                        continue;
                    }
                    let body = &cleaned[span.start..span.start + span.ranges.kept()];
                    let found = crate::tools::base64_collapse::captured_runs(body, prefix);
                    let physical = streams
                        .as_ref()
                        .map(|streams| base64_preview::raw_ranges(streams[index].1, &found));
                    let mut line = streams.as_ref().map_or(1, |streams| {
                        if index == 1 && streams[1].0 > 0 {
                            2 + streams[0].1.iter().filter(|&&b| b == b'\n').count()
                        } else {
                            1
                        }
                    });
                    let mut previous = 0;
                    for (i, mut block) in found.into_iter().enumerate() {
                        if let (Some(ranges), Some(streams)) = (&physical, &streams) {
                            let (offset, bytes) = streams[index];
                            let range = &ranges[i];
                            line += bytes[previous..range.start]
                                .iter()
                                .filter(|&&b| b == b'\n')
                                .count();
                            previous = range.end;
                            locations.insert(
                                span.start + block.range.start,
                                base64_preview::Location {
                                    path: None,
                                    line,
                                    range: offset + range.start..offset + range.end,
                                },
                            );
                        }
                        block.range = span.start + block.range.start..span.start + block.range.end;
                        blocks.push(block);
                    }
                }
                blocks
            });
            base64_preview::Collapsed::prepare_runs(
                name,
                &cleaned,
                root,
                blocks,
                recovery
                    .as_ref()
                    .filter(|_| streams.is_some())
                    .map(|(path, _)| path.as_path()),
                locations,
                match metadata {
                    Some(CommandMetadata::Page { sources, .. }) => Some(sources),
                    _ => None,
                },
                match metadata {
                    Some(CommandMetadata::ShellSnapshot { cuts, .. }) => *cuts,
                    _ => [false; 2],
                },
            )
        });
    if only_collapsed && collapsed.is_none() {
        return None;
    }
    let limit = MODEL_TOOL_OUTPUT_BYTES - reserved;
    let (mut base, retained) = if let Some(preview) = &collapsed {
        let (text, kept) = preview.retain(if bash {
            limit.min(LONG_COMMAND_OUTPUT_BYTES)
        } else {
            limit
        });
        (text, Some(kept))
    } else if let Some(root) = root {
        let draft = if bash {
            render_with_spans(name, raw, Some(LONG_COMMAND_OUTPUT_BYTES), &mut spans)
        } else {
            cleaned.clone()
        };
        retain_tool_output_with_ranges(root, &cleaned, &draft, limit)
    } else {
        (cleaned.clone(), Some(PreviewRanges::whole(cleaned.len())))
    };
    if !enabled {
        return Some(base);
    }
    let task = matches!(name, "TaskGet" | "TaskOutput");
    let json = raw.strip_prefix("Error: ").unwrap_or(raw);
    let mut input = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(value @ Value::Object(_))) = input.next() else {
        return Some(base);
    };
    if task && value["metadata"]["interaction"] == "pty" {
        return Some(base);
    }
    if !bash && value.get("model_output_summary").is_some() {
        return Some(base);
    }
    let json_end = raw.len() - json.len() + input.byte_offset();
    // TaskGet renders managed-shell result strings as stdout, which can itself
    // be JSON. Obtain their status from private capture metadata.
    let nested = if task && value["metadata"]["managed_shell"] != true {
        value["result"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
    } else {
        None
    };
    let command = nested
        .as_ref()
        .or(value["result"]
            .as_object()
            .filter(|_| task)
            .map(|_| &value["result"]))
        .unwrap_or(&value);
    let legacy_capture = &command["output_capture"];
    let strings = |key: &str| value[key].as_str().unwrap_or("");
    let (stdout, stderr) = (strings("stdout"), strings("stderr"));
    let streams = if task && stdout.is_empty() && stderr.is_empty() {
        [("output", strings("output")), ("stderr", stderr)]
    } else {
        [("stdout", stdout), ("stderr", stderr)]
    };
    let page = value["output_mode"] == "page";
    let page_end = value["next_cursor"]
        .as_str()
        .filter(|_| page)
        .and_then(|cursor| crate::tools::output_page::Cursor::decode(cursor).ok());
    let state = value["status"]
        .as_str()
        .or_else(|| (value["timed_out"] == true).then_some("timed out"));
    let mut streams = streams.map(|(key, text)| {
        let index = usize::from(key == "stderr");
        let snapshot = match metadata {
            Some(
                CommandMetadata::Snapshot(streams) | CommandMetadata::ShellSnapshot { streams, .. },
            ) => Some(&streams[index]),
            _ => None,
        };
        let stats = capture.and_then(|capture| match key {
            "stdout" => Some(&capture.stdout),
            "stderr" => Some(&capture.stderr),
            _ => None,
        });
        // Snapshot/page text belongs to this log read. Stored command totals
        // may describe a different or more complete result.
        let legacy = |field: &str| {
            if snapshot.is_some() || page {
                return None;
            }
            usize::try_from(legacy_capture[key][field].as_u64()?).ok()
        };
        let flagged = match key {
            "stdout" => value["stdout_truncated"] == true,
            "stderr" => value["stderr_truncated"] == true,
            _ => false,
        };
        let bytes = snapshot
            .filter(|s| !s.from_tail || s.omitted > 0)
            .map(|s| s.read.saturating_add(s.omitted))
            .or_else(|| stats.map(|s| s.bytes))
            .or_else(|| legacy("bytes"));
        let lines = stats.map(|s| s.lines).or_else(|| legacy("lines"));
        let source_ranges = stats.map(|s| s.ranges.clone()).filter(|ranges| {
            ranges
                .iter()
                .all(|r| r.start <= r.end && text.get(r.clone()).is_some())
        });
        let offset = page_end.as_ref().map(|end| {
            if key == "stderr" {
                end.err()
            } else {
                end.out()
            }
        });
        let partial = snapshot.is_some_and(|s| s.omitted > 0 || s.from_tail)
            || page
            || flagged
            || (task && value["output_truncated"] == true)
            || bytes.is_some_and(|n| n > text.len())
            || state.is_some_and(|state| {
                matches!(state, "running" | "pending" | "starting" | "timed out")
            })
            || value["backgrounded"] == true;
        (
            key,
            Stream {
                text,
                excerpts: stats.and_then(|s| s.excerpts.clone()),
                relative: flagged && source_ranges.is_none()
                    || snapshot.is_some_and(|s| s.from_tail),
                window_bytes: page_bytes
                    .map(|(bytes, _)| bytes[index])
                    .or_else(|| snapshot.map(|s| s.read)),
                bytes,
                lines: lines.or_else(|| {
                    (page || !partial || snapshot.is_some_and(|s| !s.from_tail && s.omitted == 0))
                        .then(|| {
                            crate::performance::command_output::ByteLines::measure(text.as_bytes())
                                .lines()
                        })
                }),
                first_lines: stats.map(|s| s.first_lines.clone()).unwrap_or_default(),
                widths: stats
                    .map(|s| &s.widths)
                    .or_else(|| snapshot.map(|s| &s.widths))
                    .or_else(|| page_bytes.map(|(_, w)| &w[usize::from(key == "stderr")]))
                    .cloned()
                    .unwrap_or_default(),
                source_ranges,
                visible: Vec::new(),
                page_continuation: page
                    && (offset
                        != page_bytes
                            .map(|(bytes, _)| bytes[usize::from(key == "stderr")] as u64)
                            .or_else(|| u64::try_from(text.len()).ok())
                        || page_bytes.is_none() && !value["output_gap"].is_null()),
            },
        )
    });
    // A shell's authoritative source length is measured before replacement.
    // Text-only backends supply UTF-8 stream values as their source.
    let bytes = page_bytes.map(|(bytes, _)| *bytes).unwrap_or_else(|| {
        streams.each_ref().map(|(_, stream)| {
            stream.window_bytes.or(stream.bytes).unwrap_or_else(|| {
                // A legacy Bash transcript may lack raw capture metadata. A
                // replacement could represent just one byte; qualify only
                // when even the lower bound reaches the threshold.
                if bash
                    || (task && value["metadata"]["managed_shell"] == true)
                    || value["output_gap"]["reason"] == "invalid_utf8"
                {
                    stream.text.len() - 2 * stream.text.matches('�').count()
                } else {
                    stream.text.len()
                }
            })
        })
    });
    if !crate::performance::command_output::qualifies(bytes) {
        return Some(base);
    }

    let json_result = !bash
        && (base == raw
            || collapsed
                .as_ref()
                .is_some_and(|c| c.json_end.is_some() && base == c.text))
        && base.len() <= MODEL_TOOL_OUTPUT_BYTES;
    let outer = retained.unwrap_or_else(|| {
        if bash {
            // Without a spill, the retained result is the full cleaned output.
            render_with_spans(name, raw, None, &mut spans);
        }
        PreviewRanges::whole(cleaned.len())
    });
    let outer = match &collapsed {
        Some(preview) => preview.source_ranges(&outer),
        None => vec![outer.head, outer.tail],
    };
    let serialized_elided = if bash || json_result {
        0
    } else {
        cleaned
            .len()
            .saturating_sub(outer.iter().map(Range::len).sum())
    };
    // RawValue borrows the original JSON spelling, including escaped keys and
    // strings. Serialized byte cuts must be projected before counting lines.
    let json_fields = (serialized_elided > 0)
        .then(|| {
            serde_json::from_str::<std::collections::HashMap<String, &serde_json::value::RawValue>>(
                &raw[raw.len() - json.len()..json_end],
            )
            .ok()
        })
        .flatten();
    let mut copies = Vec::new();
    if let Some(fields) = &json_fields {
        for (key, field) in fields {
            if let Some(text) = value[key].as_str() {
                let encoded = field.get();
                let start = encoded.as_ptr() as usize - raw.as_ptr() as usize;
                let visible = visible_json_string(text, encoded, start, &outer);
                if !visible.is_empty() {
                    copies.push((key.as_str(), text, visible));
                }
            }
        }
    }
    for ((key, stream), span) in streams.iter_mut().zip(spans) {
        let all = [0..stream.text.len()];
        let sources = stream.source_ranges.as_deref().unwrap_or(&all);
        if json_result {
            stream.visible = sources.to_vec();
        } else if let Some(span) = span {
            stream.visible = visible_ranges(sources, &span, &outer);
            copies.push((*key, stream.text, stream.visible.clone()));
        }
    }
    if !json_result {
        for (name, stream) in &mut streams {
            if stream.text.is_empty() {
                continue;
            }
            for (key, alias, kept) in &copies {
                if key == name {
                    stream.visible.extend_from_slice(kept);
                } else {
                    let managed = task
                        && value["metadata"]["managed_shell"] == true
                        && matches!(*key, "output" | "result");
                    project_alias(name, stream.text, alias, kept, managed, &mut stream.visible);
                }
            }
            stream.visible.sort_unstable_by_key(|r| r.start);
            stream.visible.dedup_by(|next, previous| {
                if next.start > previous.end {
                    return false;
                }
                previous.end = previous.end.max(next.end);
                true
            });
        }
    }

    let duration_ms = capture
        .map(|s| s.duration_ms)
        .or_else(|| legacy_capture["duration_ms"].as_u64())
        .or_else(|| value["duration_ms"].as_u64())
        .or_else(|| {
            value["finished_ms"]
                .as_u64()?
                .checked_sub(value["started_ms"].as_u64()?)
        });
    // A complete JSON message already exposes an explicit duration field.
    let duration_ms = duration_ms.filter(|ms| {
        !(json_result && value["duration_ms"].as_u64() == Some(*ms))
            && !base.contains(&format!(r#""duration_ms":{ms}"#))
    });
    let summary =
        command_output_summary::summarize(&streams, duration_ms, &base, collapsed.is_some());
    if summary.is_empty() {
        return Some(base);
    }
    let available = MODEL_TOOL_OUTPUT_BYTES
        .saturating_sub(base.len())
        .min(DIGEST_BUDGET);
    let compact_json_end = collapsed
        .as_ref()
        .and_then(|c| c.json_end)
        .filter(|_| json_result);
    let appendix = if compact_json_end.is_some() {
        // Extend the final model_output_summary string after its base64 count.
        let quoted = serde_json::to_string(&summary).expect("string serialization");
        format!("\\n{}", &quoted[1..quoted.len() - 1])
    } else if json_result {
        format!(
            ",\"model_output_summary\":{}",
            serde_json::to_string(&summary).expect("string serialization")
        )
    } else {
        format!("\n\n{summary}")
    };
    if appendix.len() > available && root.is_some() && reserved == 0 {
        return command_preview_inner(
            name,
            raw,
            root,
            metadata,
            enabled,
            DIGEST_BUDGET,
            only_collapsed,
        );
    }
    if appendix.len() <= available {
        if json_result {
            base.insert_str(
                compact_json_end.map_or(json_end - 1, |end| end - 2),
                &appendix,
            );
        } else {
            base.push_str(&appendix);
        }
    }
    Some(base)
}

/// A stored managed result can include status text, labels and the shell's
/// own head/tail cut. Map only verbatim source spans into stream coordinates.
fn project_alias(
    name: &str,
    stream: &str,
    alias: &str,
    kept: &[Range<usize>],
    managed: bool,
    out: &mut Vec<Range<usize>>,
) {
    alias_parts(name, stream, alias, managed, |at, source| {
        for r in kept {
            let start = r.start.max(at);
            let end = r.end.min(at + source.len());
            if start < end {
                out.push(source.start + (start - at)..source.start + (end - at));
            }
        }
    });
}

fn alias_parts(
    name: &str,
    stream: &str,
    alias: &str,
    managed: bool,
    mut project: impl FnMut(usize, Range<usize>),
) {
    if stream.is_empty() {
        return;
    }
    if alias.contains(stream) {
        for (at, _) in alias.match_indices(stream) {
            project(at, 0..stream.len());
        }
        return;
    }
    if !managed {
        return;
    }
    let body = alias_body(name, alias);
    let at = body.as_ptr() as usize - alias.as_ptr() as usize;
    // An alias may have been clipped before TaskGet read its log. Its prefix
    // and suffix have known origins only when the source bytes agree.
    if let Some((head, cut)) = body.split_once("\n... (") {
        if let Some((marker, tail)) = cut.split_once(") ...\n") {
            if marker.starts_with("output truncated; ")
                || marker == "capture truncated; middle omitted"
            {
                if stream.starts_with(head) {
                    project(at, 0..head.len());
                }
                if stream.ends_with(tail) {
                    project(
                        at + body.len() - tail.len(),
                        stream.len() - tail.len()..stream.len(),
                    );
                }
            }
        }
    }
}

fn alias_body<'a>(name: &str, alias: &'a str) -> &'a str {
    let label = if name == "stdout" {
        "Output:\n"
    } else {
        "Stderr:\n"
    };
    let body = match alias.split_once(label) {
        Some((_, s)) => s,
        None => alias,
    };
    if name == "stdout" {
        body.split_once("\nStderr:\n").map_or(body, |(s, _)| s)
    } else {
        body
    }
}

fn json_char_width(ch: char, spelling: &[u8]) -> usize {
    match spelling {
        [b'\\', b'u', ..] if ch.len_utf8() == 4 => 12,
        [b'\\', b'u', ..] => 6,
        [b'\\', ..] => 2,
        _ => ch.len_utf8(),
    }
}

/// Map complete visible JSON string scalars back to the decoded stream.
fn visible_json_string(
    text: &str,
    encoded: &str,
    start: usize,
    outer: &[Range<usize>],
) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut pos = 1; // opening quote
    for (offset, ch) in text.char_indices() {
        let width = json_char_width(ch, &encoded.as_bytes()[pos..]);
        let end = pos + width;
        if outer
            .get(outer.partition_point(|r| r.end <= start + pos))
            .is_some_and(|r| r.start <= start + pos && start + end <= r.end)
        {
            if let Some(last) = spans.last_mut().filter(|r| r.end == offset) {
                last.end += ch.len_utf8();
            } else {
                spans.push(offset..offset + ch.len_utf8());
            }
        }
        pos = end;
    }
    spans
}

fn visible_ranges(
    sources: &[Range<usize>],
    span: &BodySpan,
    outer: &[Range<usize>],
) -> Vec<Range<usize>> {
    let mut visible = Vec::new();
    for (inner, offset) in span.ranges.parts() {
        let start = span.start + offset;
        for kept in outer {
            for source in sources {
                let from = inner
                    .start
                    .max(source.start)
                    .max(inner.start + kept.start.saturating_sub(start));
                let to = inner
                    .end
                    .min(source.end)
                    .min(inner.start + kept.end.saturating_sub(start));
                if from < to {
                    visible.push(from..to);
                }
            }
        }
    }
    visible
}

/// The executable name a shell path or request resolves to. Both separators
/// are accepted because a stored result can be replayed on another platform.
fn shell_stem(value: &str) -> String {
    let name = value
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(value);
    let name = if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".exe")
    {
        &name[..name.len() - 4]
    } else {
        name
    };
    name.to_ascii_lowercase()
}

/// Whether the shell that actually ran is the one that was requested. The
/// names are compared as executable stems, so `powershell.exe` matches a
/// request for `powershell` while `pwsh` stays distinct.
fn shell_matches(requested: &str, shell_path: &str) -> bool {
    shell_stem(requested) == shell_stem(shell_path)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::tools::bash_process::ShellCapture;

    #[test]
    fn apply_patch_keeps_paths_and_counts_but_not_per_file_diffs() {
        let raw = json!({
            "success": false,
            "content": "Applied 1 change(s); 1 could not be restored",
            "applied": [{"path":"/w/a.txt","raw_path":"a.txt","action":"updated","destination":null}],
            "unchanged": [],
            "rolled_back": [],
            "unrestored": [{"path":"/w/b.txt","raw_path":"b.txt","message":"external edit","backup":"/w/.b.backup"}],
            "changes": [{
                "path": "/w/a.txt",
                "raw_path": "a.txt",
                "action": "updated",
                "file_change": {"version": 1, "added": 1, "removed": 1, "rows": [{"text": "secret diff row"}]}
            }],
            "changes_truncated": false,
            "file_change": {"version": 1, "added": 1, "removed": 1}
        })
        .to_string();
        let rendered = render("ApplyPatch", &raw, None);
        let value: Value = serde_json::from_str(&rendered).unwrap();
        // Unresolved paths and counts survive; bulk diff rows do not.
        assert_eq!(value["unrestored"][0]["path"], "/w/b.txt");
        assert_eq!(value["changes"][0]["path"], "/w/a.txt");
        assert!(value["changes"][0].get("file_change").is_none());
        assert!(value.get("file_change").is_none());
        assert!(!rendered.contains("secret diff row"));
        // Hook context after the JSON is preserved verbatim.
        let with_context = format!("{raw}\nPreToolUse context: keep me");
        let rendered = render("ApplyPatch", &with_context, None);
        assert!(rendered.contains("keep me"), "{rendered}");
    }

    #[test]
    fn model_tool_output_preserves_payload_errors_and_hook_suffix() {
        let raw = json!({"stdout":"{\"user_json\": 1}\n", "stderr":"", "exit_code":0,"success":true,"shell":"bash","stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(render("Bash", &raw, None), "{\"user_json\": 1}\n");
        assert_eq!(render("mcp__fixture__Bash", &raw, None), raw);
        let error = format!(
            "Error: {}\nPostToolUseFailure context: inspect dependency",
            json!({"stdout":"partial","stderr":"last error","exit_code":7,"success":false,"timed_out":true,"stdout_truncated":true,"task_id":"task-1"})
        );
        let rendered = render("Bash", &error, None);
        for expected in [
            "Exit code: 7",
            "Command timed out",
            "Stdout is incomplete",
            "last error",
            "task-1",
            "PostToolUseFailure context: inspect dependency",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        assert_eq!(
            render("Bash", "Error: Starting shell command", None),
            "Error: Starting shell command"
        );
        let read = json!({"content":"   51\tcode", "lines":100,"truncated":true,"lsp_diagnostics":{"error":"missing name"}}).to_string();
        let rendered = render("Read", &read, None);
        assert!(rendered.contains("more lines remain"));
        assert!(rendered.contains("missing name"));
        assert!(rendered.ends_with("   51\tcode"));
    }

    #[test]
    fn model_tool_output_short_or_disabled_shell_output_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let stdout = include_str!("test_summary_fixtures/vitest-pass.stdout");
        let raw = format!(
            "{}\nHook context: preserve this",
            json!({"stdout":stdout, "stderr":"diagnostic", "exit_code":0})
        );
        let original = render("Bash", &raw, None);
        assert_eq!(engine.retained_tool_output("Bash", &raw), original);
        assert_eq!(engine.retained_tool_output("mcp__fixture__Bash", &raw), raw);
        let hook_only = format!("{}\n{stdout}", json!({"stdout":"hello", "exit_code":0}));
        assert_eq!(
            engine.retained_tool_output("Bash", &hook_only),
            render("Bash", &hook_only, None)
        );
        let raw = json!({"stdout":"line\n".repeat(3000), "exit_code":0}).to_string();
        let original = render("Bash", &raw, None);
        assert_eq!(command_preview("Bash", &raw, None, None, false), original);
    }

    #[test]
    fn model_tool_output_short_literal_capture_marker_is_exact() {
        let stdout = "... (capture truncated; middle omitted) ...\n";
        assert_eq!(stdout.len(), 44);
        let raw = json!({"stdout":stdout,"stderr":"","stdout_truncated":false,"stderr_truncated":false,"success":true,"exit_code":0,"shell":"bash"}).to_string();
        let temp = tempfile::tempdir().unwrap();
        let expected = stdout;
        assert_eq!(
            command_preview("Bash", &raw, Some(temp.path()), None, true),
            expected
        );
        assert_eq!(
            command_preview("Bash", &raw, Some(temp.path()), None, false),
            expected
        );
    }

    #[test]
    fn base64_review_task_provenance_keeps_ordinary_and_agent_results() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "QUJD".repeat(256);
        let session = uuid::Uuid::new_v4().to_string();
        let call = |name, args| {
            crate::tools::tasks::execute_for_session(name, args, dir.path(), &session).unwrap()
        };
        let id = call("TaskCreate", json!({"subject":"ordinary record"}))["id"].clone();
        call("TaskUpdate", json!({"id":id,"output":blob}));
        let record = call("TaskGet", json!({"id":id}));
        assert_eq!(record["output"], blob);
        for value in [record, json!({"stdout":blob,"backend":"agent"})] {
            let raw = value.to_string();
            assert_eq!(command_preview("TaskGet",&raw,Some(dir.path()),None,true),raw);
        }
    }

    #[test]
    fn base64_review_capture_classifies_before_tool_and_capture_cuts() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "QUJD".repeat(32768);
        for (text, ceiling, expected) in [
            (format!("!!!{blob}Z!"), 1024 * 1024, None),
            (
                blob.clone(),
                1024 * 1024,
                Some("base64: 1 block collapsed (128 KB)"),
            ),
            (blob.clone(), 65536, Some("capture ceiling")),
            (
                format!("{}QQ==", "QUJD".repeat(256)),
                1027,
                Some("capture ceiling"),
            ),
        ] {
            let (raw, capture) = crate::tools::bash_process::fixture_capture(
                [text.as_bytes(), b""],
                ceiling,
                102400,
                0,
            );
            let view = command_preview("Bash", &raw, Some(dir.path()), capture.as_ref(), true);
            if let Some(expected) = expected {
                assert!(view.contains(expected));
                assert!(!view.contains("QUJDQUJD"));
                if ceiling > text.len() {
                    assert!(!view.contains("Omitted output:"));
                    let path = view
                        .split_once("full output: ")
                        .unwrap()
                        .1
                        .split_once(", line ")
                        .unwrap()
                        .0;
                    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
                }
            } else {
                assert!(!view.contains("[base64 data omitted:"));
            }
        }
    }

    #[test]
    fn base64_review_capture_preserves_raw_stream_bytes_and_terminal_preview() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "QUJD".repeat(32768);
        let stdout = [b"\xff!\n".as_slice(), blob.as_bytes()].concat();
        let (raw, capture) = crate::tools::bash_process::fixture_capture(
            [&stdout, b"diagnostic"],
            1024 * 1024,
            102400,
            0,
        );
        let row: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(row["stdout_truncated"], true);
        assert!(row["stdout"].as_str().unwrap().contains("output truncated"));
        let capture = capture.unwrap();
        let live = command_preview("Bash", &raw, Some(dir.path()), Some(&capture), true);
        let path = live
            .split_once("full output: ")
            .unwrap()
            .1
            .split_once(", line 2]")
            .unwrap()
            .0;
        assert_eq!(
            std::fs::read(path).unwrap(),
            [stdout.as_slice(), b"\ndiagnostic"].concat()
        );
        save_metadata(dir.path(), "occurrence", capture).unwrap();
        let restored = read_metadata(dir.path(), "occurrence").unwrap();
        assert_eq!(
            live,
            command_preview("Bash", &raw, Some(dir.path()), Some(&restored), true)
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("tool-outputs"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn base64_review_clipped_aliases_reference_distinct_physical_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "QUJD".repeat(32768);
        let stdout = format!("{blob}\n{blob}");
        let alias = preview_with_ranges(&stdout, 102400).0;
        let raw = serde_json::to_string_pretty(&json!({
            "stdout":stdout,"output":alias,"result":alias,"metadata":{"managed_shell":true}
        }))
        .unwrap();
        let view = command_preview("TaskGet", &raw, Some(dir.path()), None, true);
        assert!(!view.contains("QUJD"));
        assert!(view.contains("base64: 2 blocks collapsed (256 KB)"));
        let stdout_line = raw
            .lines()
            .position(|line| line.contains("\"stdout\""))
            .unwrap()
            + 1;
        assert_eq!(view.matches(&format!(", line {stdout_line}]")).count(), 6);
    }

    #[test]
    fn base64_review_shell_snapshot_discloses_a_cut_at_the_capture_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let blob = format!("{}Q", "QUJD".repeat(1024));
        let alias = preview_with_ranges(&format!("{blob}!!!\n"), 2048).0;
        let raw =
            json!({"stdout":blob,"output":alias,"result":alias,"metadata":{"managed_shell":true}})
                .to_string();
        let metadata = CommandMetadata::ShellSnapshot {
            streams: Default::default(),
            cuts: [true, false],
        };
        let view = command_preview("TaskGet", &raw, Some(dir.path()), Some(&metadata), false);
        assert!(view.contains("run cut at capture ceiling"));
        assert!(view.contains("base64: 1 block collapsed (4097 bytes)"));
        assert!(!view.contains("QUJDQUJD"));
    }

    #[test]
    fn base64_shell_thresholds_and_fallback_preserve_original_output() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["Bash", "TaskGet"] {
            for (prefix, limit) in [("", 1024), ("data:text/plain;base64,", 256)] {
                for len in [limit - 1, limit] {
                    let data = "QUJD".repeat(limit / 4);
                    let payload = if len == limit {
                        data.clone()
                    } else {
                        format!("{}I", &data[..len - 1])
                    };
                    let text = format!("{prefix}{payload}");
                    let raw = if name == "TaskGet" { json!({"metadata":{"managed_shell":true},"stdout": text}) } else { json!({"stdout": text}) }.to_string();
                    let full = render(name, &raw, None);
                    let view = command_preview(name, &raw, Some(dir.path()), None, false);
                    if len < limit {
                        assert_eq!(view, full);
                    } else {
                        assert!(view.contains("base64: 1 block collapsed"), "{view}");
                        assert!(!view.contains(&data[..64]));
                        let bad_root = dir.path().join("blocked");
                        std::fs::write(&bad_root, "").unwrap();
                        assert_eq!(
                            command_preview(name, &raw, Some(&bad_root), None, true),
                            full
                        );
                    }
                }
            }
        }
        let input = crate::tools::get_tool_definitions()
            .into_iter()
            .find(|t| t.name == "Bash")
            .unwrap()
            .input_schema;
        assert!(input["properties"].get("raw").is_none());
    }

    #[test]
    fn base64_shell_and_task_get_save_100_kib_before_previewing() {
        let dir = tempfile::tempdir().unwrap();
        let data = "QUJD".repeat(25 * 1024);
        let text = format!("before\n{data}\nafter");
        for name in ["Bash", "TaskGet"] {
            let value = if name == "Bash" {
                json!({"stdout":text})
            } else {
                json!({"metadata":{"managed_shell":true},"stdout":text,"output":text,"result":text,"status":"completed"})
            };
            let raw = serde_json::to_string_pretty(&value).unwrap();
            let full = render(name, &raw, None);
            let view = command_preview(name, &raw, Some(dir.path()), None, true);
            assert!(!view.contains(&data[..16]));
            assert!(!view.contains("Omitted output:"));
            assert!(view.contains("base64: 1 block collapsed (100 KB)"));
            let body = if name == "TaskGet" {
                serde_json::from_str::<Value>(&view).unwrap()["stdout"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            } else {
                view.clone()
            };
            let (path, line) = body
                .split_once("76800 bytes decoded, looks like text; full output: ")
                .unwrap()
                .1
                .split_once(", line ")
                .unwrap();
            let line = line.split(']').next().unwrap().parse::<usize>().unwrap();
            let saved = std::fs::read_to_string(path).unwrap();
            assert_eq!(saved, full);
            assert!(saved.lines().nth(line - 1).unwrap().contains(&data));
            assert_eq!(
                command_preview(name, &raw, Some(dir.path()), None, false),
                view
            );
        }
    }

    #[test]
    fn base64_and_i6_count_disjoint_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "QUJD".repeat(512);
        let dots = ".".repeat(20000);
        let stdout = format!("{blob}\n{dots}\n{blob}");
        let (raw, capture) = crate::tools::bash_process::fixture_capture(
            [stdout.as_bytes(), b""],
            100_000,
            100_000,
            0,
        );
        let view = command_preview("Bash", &raw, Some(dir.path()), capture.as_ref(), true);
        let (body, summary) = view.split_once("\n\nOmitted output:").unwrap();
        assert_eq!(body.matches("[base64 data omitted:").count(), 2);
        assert!(body.contains("base64: 2 blocks collapsed (4 KB)"));
        let kept_dots = body
            .lines()
            .filter(|l| l.starts_with("...."))
            .map(str::len)
            .sum::<usize>();
        assert!(kept_dots > 0 && kept_dots < dots.len());
        assert_eq!(
            summary.trim(),
            format!("stdout: {} bytes, 0 whole lines", dots.len() - kept_dots)
        );
    }

    #[test]
    fn base64_marker_survives_a_middle_cut_and_json_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let blob = "QUJD".repeat(512);
        let dots = ".".repeat(16000);
        let stdout = format!("{dots}\nerror: {blob}\n{dots}");
        for name in ["Bash", "TaskGet"] {
            let value = if name == "TaskGet" { json!({"metadata":{"managed_shell":true},"stdout":stdout}) } else { json!({"stdout":stdout}) };
            let raw = serde_json::to_string_pretty(&value)
                .unwrap()
                .replace("stdout", "std\\u006fut")
                .replace("QUJD", "QU\\u004aD");
            let view = command_preview(name, &raw, Some(dir.path()), None, true);
            assert_eq!(view.matches("[base64 data omitted:").count(), 1);
            assert!(!view.contains("QUJD"));
            assert!(view.contains("base64: 1 block collapsed (2 KB)"));
            let (body, summary) = view.split_once("\n\nOmitted output:").unwrap();
            let shown = body
                .split(|c| c != '.')
                .filter(|run| run.len() > 3)
                .map(str::len)
                .sum::<usize>();
            // Both surrounding newlines fall inside the middle cut.
            assert_eq!(
                summary.trim(),
                format!(
                    "stdout: {} bytes, 0 whole lines",
                    dots.len() * 2 + "error: ".len() + 2 - shown
                )
            );
        }
    }

    #[test]
    fn model_tool_output_command_lines_follow_unlabeled_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = format!(
            "error: head\n{}\nfatal: middle\n{}\nFAILED tail\n",
            ".".repeat(8000),
            "!".repeat(8000)
        );
        let raw = json!({"stdout":stdout,"stderr":"","shell":"bash","shell_path":"/bin/bash","success":true,"exit_code":0,"pipeline_sigpipe":false}).to_string();
        let result = command_preview("Bash", &raw, Some(dir.path()), None, true);
        let (body, summary) = result.split_once("\n\nOmitted output:").unwrap();
        assert!(body.starts_with("error: head\n"));
        assert!(!body.contains("fatal: middle"));
        assert!(!summary.contains("error: head"));
        assert!(summary.contains(r#"stdout L3 | "fatal: middle""#));
        assert!(!summary.contains("FAILED tail"));
    }

    #[test]
    fn model_tool_output_preserves_long_console_task_outcomes_and_managed_shell_status() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = r#"{"exit_code":91,"final_drain_complete":true}"#;
        let mut task = json!({"task_id":"fixture","stdout":".".repeat(12000),"stderr":"","output":outcome,"result":outcome,"metadata":{"managed_shell":true,"interaction":"pty"}});
        for name in ["TaskGet", "TaskOutput"] {
            let raw = task.to_string();
            assert_eq!(
                command_preview(name, &raw, Some(dir.path()), None, true),
                raw
            );
        }
        // A plain managed shell can print JSON with a fabricated exit code.
        task["metadata"]["interaction"] = json!("pipe");
        let raw = task.to_string();
        assert_eq!(
            command_preview("TaskGet", &raw, Some(dir.path()), None, true),
            raw
        );
    }

    #[test]
    fn model_tool_output_counts_source_once_across_snapshot_aliases() {
        let dir = tempfile::tempdir().unwrap();
        for count in [600, 1100] {
            let stdout = (0..count)
                .map(|i| format!("row-{i:04}-diagnostic\n"))
                .collect::<String>();
            let raw = json!({"status":"completed","metadata":{"managed_shell":true},"stdout":stdout,"stderr":"","output":stdout,"result":stdout}).to_string();
            let view = command_preview("TaskGet", &raw, Some(dir.path()), None, true);
            let (body, appendix) = view
                .split_once("\n\nOmitted output:")
                .unwrap_or((&view, ""));
            let (head, tail) = body.split_once("\n... (output truncated; ").unwrap();
            let tail = tail
                .split_once(" bytes before preview) ...\n")
                .unwrap()
                .1
                .split_once("\nSaved tool result")
                .unwrap()
                .0;
            // Independent oracle: locate every ASCII source byte in each
            // actual JSON field, counting a two-byte LF escape atomically.
            let mut seen = vec![false; stdout.len()];
            for field in ["stdout", "result", "output"] {
                let key = format!(r#""{field}":""#);
                let mut pos = raw.find(&key).unwrap() + key.len();
                for (i, ch) in stdout.bytes().enumerate() {
                    let next = pos + if ch == b'\n' { 2 } else { 1 };
                    seen[i] |= next <= head.len() || pos >= raw.len() - tail.len();
                    pos = next;
                }
            }
            let missing = seen.iter().filter(|b| !**b).count();
            if count == 600 {
                assert_eq!(missing, 0);
                assert!(appendix.is_empty());
            } else {
                let lines = seen
                    .chunks_exact(20)
                    .filter(|line| !line.contains(&true))
                    .count();
                assert_eq!(
                    appendix.trim(),
                    format!("stdout: {missing} bytes, {lines} whole lines")
                );
            }
        }
    }

    #[test]
    fn model_tool_output_elapsed_requires_ten_seconds_and_hidden_information() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = vec![b'.'; 12_000];
        let (raw, capture) =
            crate::tools::bash_process::fixture_capture([&stdout, b""], 100_000, 100_000, 0);
        let mut meta = capture.unwrap();
        for ms in [9999, 10000] {
            let CommandMetadata::Shell(ref mut capture) = meta else {
                unreachable!()
            };
            capture.duration_ms = ms;
            let view = command_preview("Bash", &raw, Some(dir.path()), Some(&meta), true);
            if ms == 9999 {
                assert_eq!(view.as_bytes(), stdout);
            } else {
                assert_eq!(view, format!("{}\n\nElapsed: 10000 ms", ".".repeat(12_000)));
            }
        }
    }

    #[test]
    fn model_tool_output_reviewed_capture_counts_exclude_markers_and_keep_excerpts_unique() {
        for large in [false, true] {
            let (stdout, capture) = crate::tools::bash_process::review_capture(large);
            let raw = json!({"stdout":stdout,"stderr":"","stdout_truncated":true,"exit_code":0})
                .to_string();
            let temp = tempfile::tempdir().unwrap();
            let metadata = CommandMetadata::Shell(capture);
            let result = command_preview("Bash", &raw, Some(temp.path()), Some(&metadata), true);
            let CommandMetadata::Shell(capture) = metadata else {
                unreachable!()
            };
            if !large {
                assert_eq!(result, render("Bash", &raw, None));
                continue;
            }
            let (body, summary) = result.split_once("\n\nOmitted output:").unwrap();
            let stream = &capture.stdout;
            assert_eq!((stream.bytes, stream.lines), (52_428_888, 819_204));
            let (_, stdout) = body.split_once("Output:\n").unwrap();
            let (stdout, _) = stdout.rsplit_once("\nSaved tool result (").unwrap();
            let (head, clipped) = stdout.split_once("\n... (output truncated; ").unwrap();
            let (_, tail) = clipped.split_once(" bytes before preview) ...\n").unwrap();
            let shown = head.len() + tail.len();
            let lines = head.lines().count() + tail.lines().count();
            // One invalid source byte became three UTF-8 preview bytes.
            assert!(
                summary.contains(&format!(
                    "stdout: {} bytes, {} whole lines",
                    stream.bytes - (shown - 2),
                    stream.lines - lines
                )),
                "{summary}"
            );
            assert!(!summary.contains("stderr:"));
            assert_eq!(
                result.matches("warning: final fixture").count(),
                1,
                "{summary}"
            );
            assert!(
                summary.contains(r#"stdout L409602 | "error: midpoint sentinel""#),
                "{summary}"
            );
            assert!(result.len() <= MODEL_TOOL_OUTPUT_BYTES);
        }
    }

    #[tokio::test]
    async fn model_tool_output_desktop_runtime_uses_selected_data_dir_for_opt_out() {
        use crate::native_engine::{engine_core::ChatRequest, provider_manager::ProviderManager};
        use crate::runtime;
        use axum::{routing::post, Json, Router};

        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = runtime::SessionStore::new(runtime.open_database().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, mut requests) = mpsc::channel(4);
        let app = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<Value>| {
            let tx = tx.clone();
            async move {
                tx.send(body).await.unwrap();
                ([("content-type", "text/event-stream")], concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolved = executor(&workspace).provider;
        let mut provider = resolved.provider;
        provider.models = vec![resolved.model];
        provider.base_url = base;
        let providers =
            ProviderManager::in_memory(vec![provider], runtime.data_dir.join("providers.json"));
        // The Desktop Bridge builds its NativeEngine with this same data-root path.
        let engine = runtime::create_engine(
            &runtime,
            store.db.clone(),
            Arc::new(Mutex::new(providers)),
            runtime::default_permissions(),
            Arc::new(crate::cache::FileCache::new()),
        );
        for enabled in [true, false] {
            if !enabled {
                crate::config::command_output::set(&runtime.data_dir, false).unwrap();
            }
            let id = store.create(&workspace, "fixture", "fixture").unwrap();
            let raw = json!({"stdout": "test line\n".repeat(1400), "exit_code":0}).to_string();
            let request: ChatRequest = serde_json::from_value(json!({
                "conversation_id": id, "provider_id": "fixture", "model": "fixture",
                "workspace_path": workspace,
                "messages": [
                    {"role":"user","content":"run"},
                    {"role":"assistant","content":"","tool_calls":[
                        {"id":"fixture-shell","type":"function","function":{"name":"Bash","arguments":"{}"}}
                    ]},
                    {"role":"tool","tool_call_id":"fixture-shell","content":raw},
                    {"role":"user","content":"continue"}
                ]
            })).unwrap();
            let mut events = engine.send_message(request).await.unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    match events.recv().await.unwrap() {
                        EngineEvent::MessageStop { .. } => break,
                        EngineEvent::Error(message) => panic!("{message}"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            engine.finish_turn(&id).await;
            let request = requests.recv().await.unwrap();
            let rendered = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert_eq!(rendered.contains("Omitted output:"), enabled, "{rendered}");
        }
        server.abort();
    }

    #[test]
    fn model_tool_output_long_powershell_and_taskget_keep_their_json_contract() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        for name in ["PowerShell", "TaskGet"] {
            let raw = json!({
                "stdout":format!("{}\nlast line", ".".repeat(12_300)),
                "stderr":"warning: retrying\nerror: stopped",
                "exit_code":3,
                "duration_ms":129,
                "next_cursor":"fixture"
            });
            let rendered = engine.retained_tool_output(name, &raw.to_string());
            assert_eq!(engine.retained_tool_output(name, &rendered), rendered);
            assert_eq!(rendered, raw.to_string());
        }

        let short_page = json!({"stdout":"tiny","output_mode":"page","output_truncated":true,"next_cursor":"fixture"}).to_string();
        assert_eq!(
            engine.retained_tool_output("TaskGet", &short_page),
            short_page
        );
    }

    #[test]
    fn model_tool_output_spilled_taskget_keeps_preview_and_uses_nested_status() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let raw = json!({
            "stdout":"line\n".repeat(6000),
            "stderr":"error: failed",
            "status":"failed",
            "result":json!({"exit_code":7,"output_capture":{"duration_ms":314}}).to_string(),
        })
        .to_string();
        let rendered = engine.retained_tool_output("TaskGet", &raw);
        assert!(rendered.len() <= MODEL_TOOL_OUTPUT_BYTES);
        let (preview, summary) = rendered.split_once("\n\nOmitted output:").unwrap();
        let (_, file) = preview.rsplit_once("): ").unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), raw);
        assert!(summary.contains("stdout: ") && summary.contains("whole lines"));
        assert!(
            !summary.contains("Exit")
                && !summary.contains("stderr")
                && !summary.contains("Elapsed")
        );
    }

    #[test]
    fn model_tool_output_complete_near_capacity_json_stays_intact() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let raw = json!({"stdout":".".repeat(15_900), "next_cursor":"fixture", "exit_code":0})
            .to_string();
        assert!(raw.len() < MODEL_TOOL_OUTPUT_BYTES);
        assert_eq!(engine.retained_tool_output("TaskGet", &raw), raw);
    }

    #[test]
    fn model_tool_output_shows_only_output_and_required_status() {
        // A lone stdout body needs neither its label nor an exit-code line.
        let raw = json!({"stdout":"ok\n","stderr":"","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash","pipeline_sigpipe":false,"stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok\n");
        // Two streams keep their labels so the model can tell them apart.
        let raw = json!({"stdout":"out","stderr":"err","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "Output:\nout\nStderr:\nerr");
        let raw = json!({"stdout":"","stderr":"err\n","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "Stderr:\nerr\n");
        // A status line keeps the label even for a lone stdout body.
        let raw = json!({"stdout":"ok\n","stderr":"","exit_code":0,"success":true,"stdout_truncated":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Stdout is incomplete (capture or tool limit)\nOutput:\nok\n"
        );
        // A silent success must not read as an unfinished task.
        let raw = json!({"stdout":"","stderr":"","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash","pipeline_sigpipe":false,"stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(render("Bash", &raw, None), "(Completed with no output)");
        // A failure still names its non-zero code.
        let raw = json!({"stdout":"","stderr":"boom\n","exit_code":2,"success":false,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "Exit code: 2\nStderr:\nboom\n");
        // A failure without a code keeps the existing line.
        let raw = json!({"stderr":"denied\n","success":false,"shell":"bash"}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Command failed\nStderr:\ndenied\n"
        );
    }

    #[test]
    fn model_tool_output_explains_a_pipeline_cut_short() {
        let raw = json!({"stdout":"first\n","stderr":"","exit_code":0,"success":true,"pipeline_sigpipe":true,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        let rendered = render("Bash", &raw, None);
        assert_eq!(
            rendered,
            "Output ended early: a later command in the pipeline closed it\nOutput:\nfirst\n"
        );
        assert!(!rendered.contains("pipeline_sigpipe"), "{rendered}");
        assert!(!rendered.contains("Metadata:"), "{rendered}");
        // A normal success never mentions the flag.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"pipeline_sigpipe":false,"shell":"bash","shell_path":"/bin/bash"}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok");
    }

    #[test]
    fn model_tool_output_names_only_a_fallback_shell() {
        // The running executable matches the request, so its path stays hidden.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"shell":"pwsh","shell_path":"C:\\Program Files\\PowerShell\\7\\pwsh.exe"}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok");
        // Windows PowerShell is not a request for pwsh.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"shell":"pwsh","shell_path":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Shell: C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe (requested pwsh)\nOutput:\nok"
        );
        // Case and the .exe suffix do not make powershell a fallback.
        let raw = json!({"stdout":"ok","exit_code":0,"success":true,"shell":"PowerShell","shell_path":"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.EXE"}).to_string();
        assert_eq!(render("Bash", &raw, None), "ok");
    }

    #[test]
    fn model_tool_output_keeps_unknown_fields_in_metadata() {
        let raw = json!({"stdout":"ok\n","stderr":"","exit_code":0,"success":true,"shell":"bash","shell_path":"/bin/bash","task_id":"task-1","note":"kept"}).to_string();
        let rendered = render("Bash", &raw, None);
        assert!(rendered.starts_with("Metadata: {"), "{rendered}");
        assert!(rendered.contains("\"note\":\"kept\""), "{rendered}");
        assert!(rendered.contains("\"task_id\":\"task-1\""), "{rendered}");
        assert!(rendered.ends_with("Output:\nok\n"), "{rendered}");
        assert!(!rendered.contains("shell_path"), "{rendered}");
        assert!(!rendered.contains("exit_code"), "{rendered}");
    }

    #[test]
    fn model_tool_output_leaves_task_results_alone() {
        // TaskGet/TaskOutput snapshots keep their structured shape here; the
        // managed-shell payload inside them is rendered by the task layer.
        let snapshot = json!({"task_id":"task-1","status":"completed","stdout":"done\n","stderr":"","result":"{\"exit_code\":0,\"shell_path\":\"/bin/bash\"}"}).to_string();
        assert_eq!(render("TaskGet", &snapshot, None), snapshot);
        assert_eq!(render("TaskOutput", &snapshot, None), snapshot);
    }

    #[test]
    fn tool_load_reply_is_the_content_line_without_loaded_schemas() {
        let raw = json!({
            "success": true,
            "loaded_tools": ["WebFetch"],
            "tools": [{
                "name": "WebFetch",
                "description": "Fetch a URL and return its readable text.",
                "input_schema": {"type":"object","properties":{"url":{"type":"string"}}}
            }],
            "content": "Loaded WebFetch. Available from the next request."
        })
        .to_string();
        assert_eq!(
            render("ToolLoad", &raw, None),
            "Loaded WebFetch. Available from the next request."
        );
        assert!(!render("ToolLoad", &raw, None).contains("input_schema"));

        let failed = json!({
            "success": false,
            "loaded_tools": [],
            "tools": [],
            "unknown": ["Nope"],
            "unavailable": [{"name":"computer_use","reason":"disabled by tool settings"}],
            "content": "Unknown tools: Nope. Unavailable tools: computer_use (disabled by tool settings).",
        })
        .to_string();
        let rendered = render("ToolLoad", &failed, None);
        assert_eq!(
            rendered,
            "Unknown tools: Nope. Unavailable tools: computer_use (disabled by tool settings)."
        );
        assert!(!rendered.contains("\"unknown\""), "{rendered}");

        // Hook context appended after the JSON survives the lean rendering.
        let with_context = format!("{raw}\nPostToolUse context: keep me");
        assert_eq!(
            render("ToolLoad", &with_context, None),
            "Loaded WebFetch. Available from the next request.\nPostToolUse context: keep me"
        );
        // A non-JSON legacy result is left as it is.
        assert_eq!(render("ToolLoad", "plain text", None), "plain text");
    }

    fn executor(root: &std::path::Path) -> ToolLoopExecutor {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (tx, _rx) = mpsc::channel(16);
        ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            root.to_string_lossy().into(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(root.join("config.json"))
    }

    #[test]
    fn model_tool_output_replay_matches_fresh_for_both_protocols_and_forks() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let id = store.create(temp.path(), "fixture", "fixture").unwrap();
        let assistant = store.append(&id, "assistant", "").unwrap();
        let stdout = format!(
            "START\n{}\nTEST SUMMARY\n{}",
            "中🙂".repeat(30_000),
            include_str!("test_summary_fixtures/vitest-pass.stdout")
        );
        let raw = format!(
            "Error: {}\nHook context: keep this",
            json!({"stdout":stdout,"stderr":"FINAL ERROR","exit_code":7,"success":false})
        );
        let occurrence = "inv-fixture-occurrence";
        let row_id = format!("{assistant}:{occurrence}");
        let stream = |text: &str| crate::tools::bash_process::StreamCapture {
            capture_prefix: false,
            source: None,
            preview_digest: None,
            bytes: text.len(),
            lines: text.lines().count(),
            first_lines: vec![0],
            excerpts: None,
            widths: Default::default(),
            capture_elided_bytes: 0,
            tool_elided_text_bytes: 0,
            ranges: std::iter::once(0..text.len()).collect(),
        };
        let capture = ShellCapture {
            source_file: None,
            duration_ms: 314,
            stdout: stream(&stdout),
            stderr: stream("FINAL ERROR"),
        };
        save_metadata(temp.path(), occurrence, CommandMetadata::Shell(capture)).unwrap();
        store
            .db
            .fixture_shard(&id, |conn| {
                crate::db::message_repo::insert_tool_call(
                    conn,
                    &row_id,
                    &assistant,
                    "Bash",
                    Some("{}"),
                    Some(&raw),
                    true,
                    0,
                )
            })
            .unwrap()
            .unwrap();
        let mut engine = executor(temp.path());
        let fresh = engine.retained_tool_output_for_id("Bash", &raw, Some(occurrence));
        assert!(fresh.len() <= 16_000);
        for expected in [
            "Exit code: 7",
            "START",
            "TEST SUMMARY",
            "FINAL ERROR",
            "Hook context: keep this",
            "Saved tool result",
            "Omitted output:",
            "stdout: ",
        ] {
            assert!(fresh.contains(expected), "missing {expected}");
        }
        assert!(
            !fresh.contains("Test summary"),
            "nonzero exit with a pass-only footer"
        );
        let fork = store.fork(&id).unwrap();
        for session in [&id, &fork] {
            for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
                engine.messages = store.model_history(session, &format).unwrap();
                let built = match format {
                    ApiFormat::OpenAI => {
                        serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap()
                    }
                    ApiFormat::Anthropic => {
                        serde_json::to_value(engine.build_anthropic_messages().unwrap()).unwrap()
                    }
                };
                let actual = if matches!(format, ApiFormat::OpenAI) {
                    &built[1]["content"]
                } else {
                    &built[1]["content"][0]["content"]
                };
                assert_eq!(actual, &json!(fresh));
                assert_eq!(engine.messages[1]["content"].as_str().unwrap_or(&raw), raw);
            }
        }
        assert_eq!(
            std::fs::read_dir(temp.path().join("tool-outputs"))
                .unwrap()
                .count(),
            1
        );
        let spill = std::fs::read_dir(temp.path().join("tool-outputs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(spill).unwrap(),
            render("Bash", &raw, None)
        );
        let stored = store
            .db
            .fixture_shard(&id, |conn| {
                crate::db::message_repo::get_tool_calls_for_message(conn, &assistant)
            })
            .unwrap()
            .unwrap();
        assert_eq!(stored[0].output.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn model_tool_output_mcp_text_budget_preserves_images_and_opaque_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let mut content =
            json!([{"type":"text","text":"头".repeat(20_000)},{"type":"text","text":"TAIL"}]);
        engine.retain_content("mcp__fixture__logs", &mut content);
        assert!(content.as_str().unwrap().len() <= 16_000);
        assert!(content.as_str().unwrap().contains("TAIL"));
        for original in [
            json!([{"type":"image","source":{"data":"abc"}},{"type":"text","text":"caption"}]),
            json!([{"type":"text","text":"signed","signature":"opaque"}]),
        ] {
            let mut content = original.clone();
            engine.retain_content("mcp__fixture__logs", &mut content);
            assert_eq!(content, original);
        }
    }

    #[tokio::test]
    async fn model_tool_output_reaches_actual_provider_requests() {
        use axum::{http::header, routing::post, Json, Router};
        for format in ["openai", "anthropic"] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source.txt");
            std::fs::write(&source, "source line\n").unwrap();
            let calls = vec![
                (
                    "shell",
                    "Bash",
                    json!({"command":concat!(
                        "node -e \"process.stdout.write('START'+ '.'.repeat(200000)+'TEST SUMMARY\\n",
                        "    Running tests/fail.rs (target/debug/deps/fail-abc)\\n\\nrunning 1 test\\n",
                        "test failing_case ... FAILED\\n",
                        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\\n",
                        "');process.stderr.write('FINAL ERROR');process.exitCode=7\"",
                    )}),
                ),
                ("read", "Read", json!({"file_path":source})),
            ];
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let requests = captured.clone();
            let app = Router::new().route(if format == "anthropic" { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                let calls = calls.clone();
                async move {
                    let mut requests = requests.lock().await;
                    let first = requests.is_empty();
                    requests.push(body);
                    let mut events = Vec::new();
                    if format == "anthropic" {
                        events.push(json!({"type":"message_start","message":{"id":"fixture","model":"fixture","usage":{"input_tokens":1}}}));
                        if first {
                            for (index, (id, name, input)) in calls.iter().enumerate() {
                                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
                                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
                                events.push(json!({"type":"content_block_stop","index":index}));
                            }
                        } else {
                            events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}));
                            events.push(json!({"type":"content_block_stop","index":0}));
                        }
                        events.push(json!({"type":"message_delta","delta":{"stop_reason":if first {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}));
                        events.push(json!({"type":"message_stop"}));
                    } else if first {
                        let calls: Vec<_> = calls.iter().enumerate().map(|(index, (id, name, input))| json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect();
                        events.push(json!({"id":"fixture","model":"fixture","choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}));
                        events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
                    } else {
                        events.push(json!({"id":"fixture","model":"fixture","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}));
                        events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
                    }
                    let mut stream = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                    if format == "openai" { stream.push_str("data: [DONE]\n\n"); }
                    ([(header::CONTENT_TYPE, "text/event-stream")], stream)
                }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut engine = executor(temp.path());
            let (tx, _events) = mpsc::channel(128);
            engine.event_tx = tx;
            engine.provider.provider.base_url = base;
            engine.provider.provider.api_format = if format == "anthropic" {
                ApiFormat::Anthropic
            } else {
                ApiFormat::OpenAI
            };
            engine.messages = vec![json!({"role":"user","content":"run fixture"})];
            let result = tokio::time::timeout(Duration::from_secs(20), engine.execute()).await;
            server.abort();
            assert_eq!(result.unwrap().unwrap().0, "done");
            let requests = captured.lock().await;
            assert_eq!(requests.len(), 2);
            let mut outputs = Vec::new();
            for message in requests[1]["messages"].as_array().unwrap() {
                if message["role"] == "tool" {
                    outputs.push(message["content"].as_str().unwrap());
                }
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        if block["type"] == "tool_result" {
                            outputs.push(block["content"].as_str().unwrap());
                        }
                    }
                }
            }
            assert_eq!(outputs.len(), 2);
            for expected in [
                "Exit code: 7",
                "START",
                "TEST SUMMARY",
                "FINAL ERROR",
                "Stdout is incomplete",
            ] {
                assert!(
                    outputs[0].contains(expected),
                    "{format}: missing {expected}: {}",
                    outputs[0]
                );
            }
            assert!(outputs[0].len() <= 16_000);
            assert!(
                !outputs[0].contains("Test summary") && outputs[0].contains("stdout: "),
                "{format}: {}",
                outputs[0]
            );
            let (_, appendix) = outputs[0].split_once("Omitted output:").unwrap();
            assert!(
                !appendix.contains("Exit")
                    && !appendix.contains("Elapsed")
                    && !appendix.contains("stderr")
            );
            assert_eq!(outputs[1], "     1\tsource line");
        }
    }
}

#[cfg(test)]
#[path = "command_output_invariants.rs"]
mod invariants;
