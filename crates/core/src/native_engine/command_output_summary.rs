//! Facts about source output missing from the model's retained preview.
use crate::performance::command_lines::{Line, Lines, MAX_NOTABLE};
use crate::performance::command_output::{shown_extent, ByteLines, ReplacementWidths};
use std::{fmt::Write as _, ops::Range};

#[derive(Default)]
pub(super) struct Stream<'a> {
    pub text: &'a str,
    pub bytes: Option<usize>,
    pub window_bytes: Option<usize>,
    pub lines: Option<usize>,
    pub source_ranges: Option<Vec<Range<usize>>>,
    pub first_lines: Vec<usize>,
    pub widths: ReplacementWidths,
    pub visible: Vec<Range<usize>>,
    pub page_continuation: bool,
    pub relative: bool,
    pub excerpts: Option<Lines>,
}

pub(super) fn summarize(
    streams: &[(&str, Stream<'_>); 2],
    duration_ms: Option<u64>,
    preview: &str,
    base64_collapsed: bool,
) -> String {
    let mut summary = String::new();
    let mut notable: Vec<(&str, &str, Line)> = Vec::new();
    for (name, stream) in streams {
        let all = [0..stream.text.len()];
        let source = stream.source_ranges.as_deref().unwrap_or(&all);
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
        let shown = shown_extent(
            stream.text,
            source,
            &origins,
            &stream.visible,
            &stream.widths,
        );
        let total = stream
            .bytes
            .or(stream.window_bytes)
            .unwrap_or_else(|| stream.widths.raw_bytes(source.iter().map(Range::len).sum()));
        let omitted = total.saturating_sub(shown.source_bytes());
        if omitted == 0 {
            continue;
        }
        let _ = write!(summary, "\n{name}: {omitted} bytes");
        if let Some(lines) = stream.lines {
            let n = lines.saturating_sub(shown.lines);
            let _ = write!(summary, ", {n} whole line{}", if n == 1 { "" } else { "s" });
        }

        // Captured markers can survive a tool-level cut. Also scan retained
        // source lines, skipping visible ones before spending the marker cap.
        let mut missing = Lines::default();
        let blocks = if base64_collapsed {
            crate::tools::base64_collapse::runs(stream.text)
        } else {
            Vec::new()
        };
        let visible_text = |text: &str| {
            let quoted = serde_json::to_string(text).expect("string");
            // A short diagnostic quote may overlap a much longer block.
            // Use decoded stream offsets even if the JSON escaped its bytes.
            let overlaps_block = !blocks.is_empty()
                && stream.text.match_indices(text).any(|(at, _)| {
                    let next = blocks.partition_point(|b| b.range.end <= at);
                    blocks
                        .get(next)
                        .is_some_and(|b| b.range.start < at + text.len())
                });
            overlaps_block
                || preview.contains(text)
                || preview.contains(&quoted[1..quoted.len() - 1])
                || streams
                    .iter()
                    .any(|(_, s)| s.visible.iter().any(|r| s.text[r.clone()].contains(text)))
        };
        if let Some(saved) = &stream.excerpts {
            for line in &saved.notable {
                if !visible_text(&line.text) {
                    missing.observe(line.number, &line.text);
                }
            }
        }
        for (range, origin) in source.iter().zip(origins) {
            let mut offset = range.start;
            for (i, line) in stream.text[range.clone()].split_inclusive('\n').enumerate() {
                if missing.notable.len() == MAX_NOTABLE {
                    break;
                }
                let end = offset + line.len();
                if !stream
                    .visible
                    .iter()
                    .any(|r| r.start <= offset && end <= r.end)
                {
                    missing.observe_if(origin + i + 1, line, |quote| !visible_text(quote));
                }
                offset = end;
            }
        }
        let scope = if stream.page_continuation {
            "page "
        } else if stream.relative {
            "result "
        } else {
            ""
        };
        for line in missing.notable {
            if notable.len() < MAX_NOTABLE
                && !notable.iter().any(|(_, _, other)| other.text == line.text)
            {
                notable.push((*name, scope, line));
            }
        }
    }
    for (name, scope, line) in notable {
        let _ = write!(
            summary,
            "\n{name} {scope}L{} | {}",
            line.number,
            serde_json::to_string(&line.text).expect("string")
        );
    }
    if !summary.is_empty() {
        summary.insert_str(0, "Omitted output:");
    }
    if let Some(ms) = duration_ms.filter(|ms| *ms >= 10_000) {
        if !summary.is_empty() {
            summary.push('\n');
        }
        let _ = write!(summary, "Elapsed: {ms} ms");
    }
    summary
}
