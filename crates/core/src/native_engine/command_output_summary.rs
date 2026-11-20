//! Model-facing digest of the text already available to the tool renderer.
use crate::performance::command_lines::{Line, Lines, MAX_NOTABLE};
use crate::performance::command_output::{shown_extent, ByteLines, ReplacementWidths};
use std::{fmt::Write as _, ops::Range};

#[derive(Default)]
pub(super) struct Stream<'a> {
    pub text: &'a str,
    pub bytes: Option<usize>,
    pub window_bytes: Option<usize>,
    pub lines: Option<usize>,
    pub capture_elided: Option<usize>,
    pub tool_elided: Option<usize>,
    /// The provided text is partial. Exact source totals may still be known.
    pub partial: bool,
    pub source_ranges: Option<Vec<Range<usize>>>,
    pub first_lines: Vec<usize>,
    pub widths: ReplacementWidths,
    /// Source text ranges included in the final model preview.
    pub visible: Option<Vec<Range<usize>>>,
    pub page_continuation: bool,
    pub relative: bool,
    pub excerpts: Option<Lines>,
}

pub(super) struct CommandOutput<'a> {
    pub streams: [(&'static str, Stream<'a>); 2],
    pub exit: Option<i64>,
    pub duration_ms: Option<u64>,
    pub page: bool,
    pub log_snapshot: bool,
    pub serialized_elided: usize,
}

pub(super) fn summarize(input: &CommandOutput<'_>) -> String {
    let mut summary = String::from("Command output summary");
    let _ = write!(
        summary,
        "\n- Exit status {}",
        input
            .exit
            .map_or_else(|| "unknown".into(), |code| code.to_string())
    );
    if let Some(ms) = input.duration_ms {
        let _ = write!(summary, " | elapsed {ms} ms");
    }
    if input.page {
        summary.push_str(" | output page");
    } else if input.log_snapshot {
        summary.push_str(" | log snapshot");
    }
    let mut model_elided = 0;
    let mut notable = Vec::new();
    let mut edges = Vec::new();
    for (name, stream) in &input.streams {
        // Known ranges exclude synthetic markers at both shell capture stages.
        let all = [0..stream.text.len()];
        let source = stream.source_ranges.as_deref().unwrap_or(&all);
        let visible = stream
            .visible
            .as_deref()
            .unwrap_or(if input.serialized_elided == 0 {
                source
            } else {
                &[]
            });
        let source_bytes = source.iter().map(Range::len).sum::<usize>();
        // Pre-line-map sidecars retained a head and a tail. Recover their
        // origins from total lines; new captures carry each origin explicitly.
        let origins = if stream.first_lines.len() == source.len() {
            stream.first_lines.clone()
        } else {
            source
                .iter()
                .map(|r| {
                    if r.start == 0 {
                        0
                    } else {
                        stream.lines.unwrap_or(0).saturating_sub(
                            ByteLines::measure(&stream.text.as_bytes()[r.start..]).lines(),
                        )
                    }
                })
                .collect()
        };
        let shown = shown_extent(stream.text, source, &origins, visible, &stream.widths);
        let (shown_bytes, shown_lines) = (shown.text_bytes, shown.lines);
        if stream.visible.is_some() {
            let _ = write!(
                summary,
                "\n- {name} {shown_bytes} text bytes, {shown_lines} lines shown"
            );
            model_elided += source_bytes.saturating_sub(shown_bytes);
        } else if input.serialized_elided > 0 {
            let _ = write!(
                summary,
                "\n- {name} {source_bytes} text bytes available before retention"
            );
        } else {
            let _ = write!(
                summary,
                "\n- {name} {source_bytes} text bytes, {shown_lines} lines captured"
            );
        }
        match (stream.bytes, stream.lines) {
            (Some(bytes), Some(lines)) => {
                let _ = write!(summary, " of {bytes} source bytes / {lines} lines");
            }
            (Some(bytes), None) => {
                let _ = write!(summary, " of {bytes} source bytes");
            }
            (None, _) if input.page => {
                let _ = write!(
                    summary,
                    " of {} page bytes / {} page lines | source totals unknown",
                    stream.window_bytes.unwrap_or(stream.text.len()),
                    ByteLines::measure(stream.text.as_bytes()).lines()
                );
            }
            (None, Some(lines)) if !stream.partial => {
                let _ = write!(
                    summary,
                    " of {} text bytes / {lines} lines",
                    stream.text.len()
                );
            }
            _ if stream.partial => summary.push_str(" | source totals unknown"),
            _ => {}
        }
        // The source balance uses raw byte widths even when a model-facing
        // replacement occupies three UTF-8 bytes. Text costs above/below
        // describe the independent UTF-8 presentation budget.
        let kept_raw = stream.widths.raw_bytes(source_bytes);
        let shown_raw = shown.source_bytes();
        let tool_raw = stream
            .bytes
            .zip(stream.capture_elided)
            .map(|(total, dropped)| total.saturating_sub(dropped).saturating_sub(kept_raw));
        if kept_raw != source_bytes || tool_raw != stream.tool_elided {
            let _ = write!(
                summary,
                " | {shown_raw} source bytes shown, {} omitted by model",
                kept_raw.saturating_sub(shown_raw)
            );
            if let Some(bytes) = tool_raw {
                let _ = write!(summary, ", {bytes} omitted by tool");
            }
        }
        if let Some(bytes) = stream.capture_elided.filter(|bytes| *bytes > 0) {
            let _ = write!(summary, " | {bytes} source bytes dropped at capture");
        }
        if let Some(bytes) = stream.tool_elided.filter(|bytes| *bytes > 0) {
            let _ = write!(summary, " | {bytes} text bytes omitted by tool preview");
        }
        if stream.partial && stream.capture_elided.is_none() && stream.tool_elided.is_none() {
            summary.push_str(" | amount omitted unknown");
        }

        let samples = stream.excerpts.clone().unwrap_or_else(|| {
            let mut samples = Lines::default();
            for (part, &origin) in source.iter().zip(&origins) {
                for (i, raw) in stream.text[part.clone()].split_terminator('\n').enumerate() {
                    samples.observe(origin + i + 1, raw);
                }
            }
            samples
        });
        let scope = if stream.page_continuation {
            "page "
        } else if stream.relative {
            "result "
        } else {
            ""
        };
        let locate = |line: Line| {
            let in_preview = visible.iter().any(|part| {
                source.iter().zip(&origins).any(|(src, origin)| {
                    let start = part.start.max(src.start);
                    let end = part.end.min(src.end);
                    if start >= end {
                        return false;
                    }
                    let first = origin
                        + ByteLines::measure(&stream.text.as_bytes()[src.start..start]).newlines
                        + 1;
                    line.number
                        .checked_sub(first)
                        .and_then(|index| stream.text[start..end].split_terminator('\n').nth(index))
                        .is_some_and(|text| text.contains(&line.text))
                })
            });
            Quote {
                stream: name,
                scope,
                line,
                in_preview,
            }
        };
        notable.extend(samples.notable.into_iter().map(locate));
        // With a single long source line, keep one useful location even when
        // independent capture windows sampled different ends of that line.
        let mut head = samples.head;
        let mut tail = samples.tail;
        if let (Some(first), Some(last)) = (&head, &tail) {
            if first.number == last.number {
                if first.text.len() < last.text.len() {
                    head = tail.take();
                }
                tail = None;
            }
        }
        if let Some(head) = head {
            edges.push(("Head", locate(head)));
        }
        if let Some(tail) = tail {
            edges.push(("Tail", locate(tail)));
        }
    }
    if model_elided > 0 {
        let _ = write!(
            summary,
            "\n- Model preview omitted {model_elided} text bytes"
        );
    }
    if input.serialized_elided > 0 {
        let _ = write!(
            summary,
            "\n- Model preview omitted {} serialized result bytes",
            input.serialized_elided
        );
    }
    // Spend the bounded list on missing text first. Already visible source
    // text only needs a location; a newline or short suffix cannot cover it.
    notable.sort_by_key(|quote| quote.in_preview);
    let mut emitted = Vec::new();
    for quote in &notable {
        if emitted.len() == MAX_NOTABLE {
            break;
        }
        if !emitted
            .iter()
            .any(|other: &&Quote| other.line.text == quote.line.text)
        {
            quote.write(&mut summary, "Notable");
            emitted.push(quote);
        }
    }
    for (kind, quote) in &edges {
        if !emitted
            .iter()
            .any(|other| other.stream == quote.stream && other.line.number == quote.line.number)
        {
            quote.write(&mut summary, kind);
            emitted.push(quote);
        }
    }
    summary
}

struct Quote {
    stream: &'static str,
    scope: &'static str,
    line: Line,
    in_preview: bool,
}

impl Quote {
    fn write(&self, out: &mut String, kind: &str) {
        let _ = write!(
            out,
            "\n- {kind} {} {}L{}",
            self.stream, self.scope, self.line.number
        );
        if self.in_preview {
            out.push_str(" (in preview)");
        } else {
            out.push_str(" | ");
            out.push_str(&serde_json::to_string(&self.line.text).expect("string serialization"));
        }
    }
}
