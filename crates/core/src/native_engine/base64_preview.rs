//! Collapse before head/tail selection and map retained text back to I6 source offsets.
use super::{json_char_width, PreviewRanges};
use crate::tools::base64_collapse::{runs, Run};
use serde_json::value::RawValue;
use std::{collections::HashMap, ops::Range, path::Path};

pub(super) struct Collapsed {
    pub text: String,
    /// Original and displayed byte ranges for each replacement, in order.
    changes: Vec<(Range<usize>, Range<usize>)>,
    pub json_end: Option<usize>,
    note: String,
}

/// Map decoded string offsets back into the JSON source kept in the spill.
fn field_runs(original: &str, field: &RawValue, counts: &mut HashMap<String, usize>) -> Vec<Run> {
    let encoded = field.get();
    let Ok(text) = serde_json::from_str::<String>(encoded) else {
        return Vec::new();
    };
    let mut blocks = runs(&text);
    let mut pos = 1;
    let mut decoded = 0;
    let start = encoded.as_ptr() as usize - original.as_ptr() as usize;
    let mut advance = |end| {
        for ch in text[decoded..end].chars() {
            pos += json_char_width(ch, &encoded.as_bytes()[pos..]);
        }
        decoded = end;
        start + pos
    };
    for run in &mut blocks {
        *counts
            .entry(text[run.range.clone()].to_owned())
            .or_default() += 1;
        run.range = advance(run.range.start)..advance(run.range.end);
    }
    blocks
}

struct Blocks {
    runs: Vec<Run>,
    count: usize,
    bytes: usize,
    json_end: Option<usize>,
}

fn task_blocks(original: &str) -> Option<Blocks> {
    let json = original.strip_prefix("Error: ").unwrap_or(original);
    let mut objects =
        serde_json::Deserializer::from_str(json).into_iter::<HashMap<String, &RawValue>>();
    let fields = objects.next()?.ok()?;
    let mut runs = Vec::new();
    let mut counts = HashMap::new();
    // A snapshot can repeat stdout and stderr in output and result. Count
    // physical streams together and take the maximum across those aliases.
    for keys in [&["stdout", "stderr"][..], &["output"], &["result"]] {
        let mut group = HashMap::new();
        for field in keys.iter().filter_map(|&k| fields.get(k)) {
            runs.extend(field_runs(original, field, &mut group));
        }
        for (blob, n) in group {
            let count = counts.entry(blob).or_insert(0usize);
            *count = (*count).max(n);
        }
    }
    runs.sort_unstable_by_key(|b| b.range.start);
    let count = counts.values().sum();
    let bytes = counts.iter().map(|(blob, n)| blob.len() * n).sum();
    Some(Blocks {
        runs,
        count,
        bytes,
        json_end: Some(original.len() - json.len() + objects.byte_offset()),
    })
}

impl Collapsed {
    pub fn prepare(name: &str, original: &str, root: &Path) -> Option<Self> {
        if original.len() < crate::tools::base64_collapse::DATA_URI_THRESHOLD {
            return None;
        }
        let task = matches!(name, "TaskGet" | "TaskOutput");
        let Blocks {
            runs: blocks,
            count,
            bytes,
            mut json_end,
        } = task
            .then(|| task_blocks(original))
            .flatten()
            .unwrap_or_else(|| {
                let runs = runs(original);
                Blocks {
                    count: runs.len(),
                    bytes: runs.iter().map(|b| b.range.len()).sum(),
                    runs,
                    json_end: None,
                }
            });
        if blocks.is_empty() {
            return None;
        }
        let path = crate::performance::save_tool_output(root, original).ok()?;
        let note = summary(count, bytes);
        let insert = json_end.map_or(original.len(), |end| end - 1);
        let (mut text, mut changes) =
            render_runs(&original[..insert], &blocks, &path, 1, json_end.is_some());
        let start = text.len();
        if json_end.is_some() {
            text.push_str(&format!(",\"model_output_summary\":\"{note}\""));
            json_end = Some(text.len() + 1);
        } else {
            text.push('\n');
            text.push_str(&note);
        }
        changes.push((insert..insert, start..text.len()));
        text.push_str(&original[insert..]);
        Some(Self {
            text,
            changes,
            json_end,
            note,
        })
    }

    /// Never split a file reference at a preview boundary. Repeat references
    /// from the missing middle in the space reserved below the preview.
    pub fn retain(&self, limit: usize) -> (String, PreviewRanges) {
        let ((_, note_range), marks) = self.changes.split_last().expect("summary insertion");
        retain_with_references(&self.text, marks, Some((&self.note, note_range)), limit)
    }

    /// I6 counts only missing ordinary bytes. Each encoded run is already
    /// accounted for by the base64 summary, even if the preview cuts its line.
    pub fn source_ranges(&self, kept: &PreviewRanges) -> Vec<Range<usize>> {
        // retain() snaps cuts to replacement boundaries. Between two
        // replacements ordinary text has the same width in both versions.
        let source_offset = |shown| {
            let n = self.changes.partition_point(|(_, r)| r.end <= shown);
            match n.checked_sub(1) {
                Some(i) => {
                    let (source, replacement) = &self.changes[i];
                    source.end + (shown - replacement.end)
                }
                None => shown,
            }
        };
        let head = 0..source_offset(kept.head.end);
        let tail = source_offset(kept.tail.start)..source_offset(kept.tail.end);
        let mut visible = vec![head.clone()];
        visible.extend(self.changes.iter().filter_map(|(r, _)| {
            (!r.is_empty() && r.start >= head.end && r.end <= tail.start).then(|| r.clone())
        }));
        visible.push(tail);
        visible
    }
}

fn retain_with_references(
    text: &str,
    marks: &[(Range<usize>, Range<usize>)],
    note: Option<(&str, &Range<usize>)>,
    limit: usize,
) -> (String, PreviewRanges) {
    use crate::performance::preview_with_ranges;
    if text.len() <= limit {
        return (text.to_owned(), PreviewRanges::whole(text.len()));
    }
    let note_bytes = note.map_or(0, |(s, _)| s.len() + 1);
    let body_limit = limit - note_bytes;
    let (_, initial) = preview_with_ranges(text, body_limit);
    let hidden =
        |r: &Range<usize>, kept: &PreviewRanges| r.end > kept.head.end && r.start < kept.tail.start;
    let reserve = if marks.iter().any(|(_, r)| hidden(r, &initial)) {
        marks
            .iter()
            .map(|(_, r)| r.len() + 1)
            .sum::<usize>()
            .min(limit / 2)
    } else {
        0
    };
    let (draft, mut kept) = preview_with_ranges(text, body_limit - reserve);
    let cut = &draft[kept.head.len()..kept.tail_offset];
    for r in marks.iter().map(|(_, r)| r).chain(note.map(|(_, r)| r)) {
        if r.start < kept.head.end && kept.head.end < r.end {
            kept.head.end = r.start;
        }
        if r.start < kept.tail.start && kept.tail.start < r.end {
            kept.tail.start = r.end;
        }
    }
    let mut view = format!(
        "{}{cut}{}",
        &text[kept.head.clone()],
        &text[kept.tail.clone()]
    );
    let missing_note = note.filter(|(_, r)| hidden(r, &kept));
    let available = limit
        - if missing_note.is_some() {
            note_bytes
        } else {
            0
        };
    for (_, r) in marks.iter().filter(|(_, r)| hidden(r, &kept)) {
        if view.len() + 1 + r.len() > available {
            break;
        }
        view.push('\n');
        view.push_str(&text[r.clone()]);
    }
    if let Some((note, _)) = missing_note {
        view.push('\n');
        view.push_str(note);
    }
    (view, kept)
}

fn render_runs(
    original: &str,
    blocks: &[Run],
    path: &Path,
    mut line: usize,
    json: bool,
) -> (String, Vec<(Range<usize>, Range<usize>)>) {
    let mut text = String::new();
    let mut changes = Vec::new();
    let mut copied = 0;

    for Run {
        range,
        decoded_len,
        kind,
    } in blocks
    {
        let gap = &original[copied..range.start];
        text.push_str(gap);
        line += gap.bytes().filter(|&b| b == b'\n').count();
        let marker = format!("[base64 data omitted: about {decoded_len} bytes decoded, looks like {kind}; full output: {}, line {line}]", path.display());
        let start = text.len();
        if json {
            let quoted = serde_json::to_string(&marker).expect("JSON string");
            text.push_str(&quoted[1..quoted.len() - 1]);
        } else {
            text.push_str(&marker);
        }
        copied = range.end;
        changes.push((range.clone(), start..text.len()));
    }
    text.push_str(&original[copied..]);
    (text, changes)
}

#[derive(Default)]
pub(crate) struct MonitorLine {
    pub text: String,
    pub blocks: usize,
    pub encoded_bytes: usize,
}

pub(crate) fn monitor_output_line(
    line: &str,
    path: &Path,
    number: usize,
    limit: usize,
) -> Option<MonitorLine> {
    let blocks = runs(line);
    if blocks.is_empty() {
        return None;
    }
    let (text, marks) = render_runs(line, &blocks, path, number, false);
    let text = if text.len() > limit {
        retain_with_references(&text, &marks, None, limit).0
    } else {
        text
    };
    Some(MonitorLine {
        text,
        blocks: blocks.len(),
        encoded_bytes: blocks.iter().map(|b| b.range.len()).sum(),
    })
}

pub(crate) fn summary(count: usize, bytes: usize) -> String {
    let size = if bytes % 1024 == 0 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("base64: {count} block{plural} collapsed ({size})")
}
