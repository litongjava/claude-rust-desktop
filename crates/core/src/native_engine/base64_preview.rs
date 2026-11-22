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
    index: LineIndex,
}

/// Map decoded string offsets back into the JSON source kept in the spill.
fn encode_ranges(original: &str, field: &RawValue, text: &str, blocks: &mut [Run]) {
    let encoded = field.get();
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
    for block in blocks {
        block.range = advance(block.range.start)..advance(block.range.end);
    }
}

#[derive(Clone)]
pub(super) struct Location {
    pub(super) path: Option<std::path::PathBuf>,
    pub(super) line: usize,
    pub(super) range: Range<usize>,
}

/// The byte ranges are half-open and refer to the immutable saved file.
pub(crate) struct LineIndex {
    root: std::path::PathBuf,
    output: std::path::PathBuf,
    entries: std::collections::BTreeSet<(Option<std::path::PathBuf>, usize, usize, usize)>,
}

impl LineIndex {
    pub(crate) fn new(root: &Path, output: &Path) -> Self {
        Self {
            root: root.into(),
            output: output.into(),
            entries: Default::default(),
        }
    }

    fn record(&mut self, path: &Path, line: usize, range: Range<usize>) {
        self.entries.insert((
            (path != self.output).then(|| path.into()),
            line,
            range.start,
            range.end,
        ));
    }

    pub(crate) fn monitor_line(&mut self, bytes: &[u8], number: usize, start: usize) {
        let text = String::from_utf8_lossy(bytes);
        let blocks = runs(&text);
        for range in raw_ranges(bytes, &blocks) {
            self.entries
                .insert((None, number, start + range.start, start + range.end));
        }
    }

    pub(crate) fn pointer(&self) -> std::io::Result<String> {
        use std::fmt::Write;
        let mut text = format!("full output: {}\n", self.output.display());
        for (path, line, start, end) in &self.entries {
            write!(text, "{line} {start}..{end}").unwrap();
            if let Some(path) = path {
                write!(text, " {}", path.display()).unwrap();
            }
            text.push('\n');
        }
        let path = crate::performance::save_tool_output(&self.root, &text)?;
        Ok(format!("base64 line index: {}", path.display()))
    }
}

pub(super) fn raw_ranges(bytes: &[u8], blocks: &[Run]) -> Vec<Range<usize>> {
    let mut chunks = bytes.utf8_chunks();
    let (mut raw, mut decoded) = (0, 0);
    let mut chunk = chunks.next();
    let mut offset = |at| {
        while let Some(part) = chunk.as_ref() {
            if at <= decoded + part.valid().len() {
                break;
            }
            raw += part.valid().len() + part.invalid().len();
            decoded += part.valid().len()
                + if part.invalid().is_empty() {
                    0
                } else {
                    '�'.len_utf8()
                };
            chunk = chunks.next();
        }
        raw + (at - decoded)
    };
    blocks
        .iter()
        .map(|b| offset(b.range.start)..offset(b.range.end))
        .collect()
}

struct Blocks {
    runs: Vec<Run>,
    locations: HashMap<usize, Location>,
    count: usize,
    bytes: usize,
    json_end: Option<usize>,
}

type PageSources<'a> = [Option<&'a crate::tools::bash_process::PageSource>; 2];

fn task_blocks(original: &str, pages: Option<PageSources>, cuts: [bool; 2]) -> Option<Blocks> {
    let json = original.strip_prefix("Error: ").unwrap_or(original);
    let mut objects =
        serde_json::Deserializer::from_str(json).into_iter::<HashMap<String, &RawValue>>();
    let fields = objects.next()?.ok()?;
    let metadata: serde_json::Value = serde_json::from_str(fields.get("metadata")?.get()).ok()?;
    if metadata["managed_shell"] != true && metadata["monitor"] != true {
        return None;
    }
    let field = |key: &str| {
        let raw = *fields.get(key)?;
        Some((raw, serde_json::from_str::<String>(raw.get()).ok()?))
    };
    let mut result = Blocks {
        runs: Vec::new(),
        locations: HashMap::new(),
        count: 0,
        bytes: 0,
        json_end: Some(original.len() - json.len() + objects.byte_offset()),
    };
    let page = fields
        .get("output_mode")
        .is_some_and(|v| v.get() == r#""page""#);
    let newlines: Vec<_> = original.match_indices('\n').map(|(i, _)| i).collect();
    let mut aliases: HashMap<&str, Vec<(Run, Location)>> = HashMap::new();
    let keys = if ["stdout", "stderr"].iter().any(|k| {
        fields
            .get(*k)
            .is_some_and(|v| v.get().starts_with('"') && v.get().len() > 2)
    }) {
        ["stdout", "stderr"]
    } else {
        ["output", "stderr"]
    };
    for (index, key) in keys.into_iter().enumerate() {
        let Some((raw, text)) = field(key) else {
            continue;
        };
        let mut blocks = Vec::new();
        let mut origins = Vec::new();
        if page || pages.is_some() {
            let Some(source) = pages.and_then(|p| p[index]) else {
                continue;
            };
            use std::io::{Read, Seek, SeekFrom};
            let mut file = std::fs::File::open(&source.path).ok()?;
            file.seek(SeekFrom::Start(source.origin as u64)).ok()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).ok()?;
            let full = String::from_utf8_lossy(&bytes);
            let start = source.window.start;
            let end = source.window.end;
            let window = bytes.get(start..end)?;
            if String::from_utf8_lossy(window) != text {
                continue;
            }
            let mut page_pos = 0;
            let mut decoded_pos = 0;
            let mut line = source.first_line;
            let mut pos = 0;
            let found = crate::tools::base64_collapse::captured_runs(&full, source.cut);
            let ranges = raw_ranges(&bytes, &found);
            for (mut run, physical) in found.into_iter().zip(ranges) {
                if physical.start >= end {
                    break;
                }
                if physical.end <= start {
                    continue;
                }
                line += full[pos..run.range.start]
                    .bytes()
                    .filter(|&b| b == b'\n')
                    .count();
                pos = run.range.end;
                origins.push(Location {
                    path: Some(source.path.clone()),
                    line,
                    range: source.origin + physical.start..source.origin + physical.end,
                });
                if !page || physical.start >= start {
                    result.count += 1;
                    result.bytes += run.range.len();
                }
                let left = physical.start.saturating_sub(start);
                let right = physical.end.min(end) - start;
                decoded_pos += String::from_utf8_lossy(&window[page_pos..left]).len();
                run.range = decoded_pos..decoded_pos + right - left;
                page_pos = right;
                decoded_pos = run.range.end;
                blocks.push(run);
            }
        } else {
            blocks = crate::tools::base64_collapse::captured_runs(&text, cuts[index]);
            result.count += blocks.len();
            result.bytes += blocks.iter().map(|b| b.range.len()).sum::<usize>();
        }
        if blocks.is_empty() {
            continue;
        }
        let mut encoded = blocks.clone();
        encode_ranges(original, raw, &text, &mut encoded);
        if !page && pages.is_none() {
            origins = encoded
                .iter()
                .map(|b| Location {
                    path: None,
                    line: 1 + newlines.partition_point(|&n| n < b.range.start),
                    range: b.range.clone(),
                })
                .collect();
            for alias in ["output", "result"] {
                if alias == key {
                    continue;
                }
                let Some((raw_copy, copy)) = field(alias) else {
                    continue;
                };
                super::alias_parts(key, &text, &copy, true, |at, span| {
                    for (b, origin) in blocks.iter().zip(&origins) {
                        let start = span.start.max(b.range.start);
                        let end = span.end.min(b.range.end);
                        if start < end {
                            let mut part = b.clone();
                            part.range = at + (start - span.start)..at + (end - span.start);
                            aliases
                                .entry(alias)
                                .or_default()
                                .push((part, origin.clone()));
                        }
                    }
                });
                // The log stopped mid-run. A completion alias may also hold a
                // suffix from beyond that ceiling. Its boundary is unknown;
                // point to the retained fragment without counting another run.
                if let Some(cut) = blocks.last().filter(|b| b.incomplete) {
                    let body = super::alias_body(key, &copy);
                    if let Some((_, tail)) = body.split_once(") ...\n") {
                        let len = crate::tools::base64_collapse::leading_fragment(tail);
                        if len > 0 && !text.ends_with(tail) {
                            let start = tail.as_ptr() as usize - copy.as_ptr() as usize;
                            let mut fragment = cut.clone();
                            fragment.range = start..start + len;
                            let mut physical = [fragment.clone()];
                            encode_ranges(original, raw_copy, &copy, &mut physical);
                            let range = physical[0].range.clone();
                            let line = 1 + newlines.partition_point(|&n| n < range.start);
                            aliases.entry(alias).or_default().push((
                                fragment,
                                Location {
                                    path: None,
                                    line,
                                    range,
                                },
                            ));
                        }
                    }
                }
            }
        }
        result
            .locations
            .extend(encoded.iter().zip(origins).map(|(r, l)| (r.range.start, l)));
        result.runs.extend(encoded);
    }
    for (key, mut parts) in aliases {
        let (raw, text) = field(key)?;
        parts.sort_by_key(|(b, _)| b.range.start);
        let mut end = 0;
        parts.retain(|(b, _)| {
            if b.range.start < end {
                false
            } else {
                end = b.range.end;
                true
            }
        });
        if cuts.contains(&true) {
            let newlines_before = 1 + newlines
                .partition_point(|&n| n < raw.get().as_ptr() as usize - original.as_ptr() as usize);
            for mut run in runs(&text) {
                if parts
                    .iter()
                    .any(|(p, _)| p.range.start < run.range.end && p.range.end > run.range.start)
                {
                    continue;
                }
                run.incomplete = true;
                let mut encoded = [run.clone()];
                encode_ranges(original, raw, &text, &mut encoded);
                parts.push((
                    run,
                    Location {
                        path: None,
                        line: newlines_before,
                        range: encoded[0].range.clone(),
                    },
                ));
            }
            parts.sort_by_key(|(b, _)| b.range.start);
        }
        let (mut blocks, origins): (Vec<_>, Vec<_>) = parts.into_iter().unzip();
        encode_ranges(original, raw, &text, &mut blocks);
        result
            .locations
            .extend(blocks.iter().zip(origins).map(|(r, l)| (r.range.start, l)));
        result.runs.extend(blocks);
    }
    result.runs.sort_unstable_by_key(|b| b.range.start);
    Some(result)
}

impl Collapsed {
    pub fn prepare(name: &str, original: &str, root: &Path) -> Option<Self> {
        if original.len() < crate::tools::base64_collapse::DATA_URI_THRESHOLD {
            return None;
        }
        Self::prepare_runs(
            name,
            original,
            root,
            None,
            None,
            HashMap::new(),
            None,
            [false; 2],
        )
    }

    pub(super) fn prepare_runs(
        name: &str,
        original: &str,
        root: &Path,
        stream_runs: Option<Vec<Run>>,
        source_file: Option<&Path>,
        stream_locations: HashMap<usize, Location>,
        pages: Option<PageSources>,
        cuts: [bool; 2],
    ) -> Option<Self> {
        let task = matches!(name, "TaskGet" | "TaskOutput");
        let Blocks {
            runs: blocks,
            locations,
            count,
            bytes,
            mut json_end,
        } = if task {
            task_blocks(original, pages, cuts)?
        } else {
            let runs = stream_runs.unwrap_or_else(|| runs(original));
            Blocks {
                locations: stream_locations,
                count: runs.len(),
                bytes: runs.iter().map(|b| b.range.len()).sum(),
                runs,
                json_end: None,
            }
        };
        if blocks.is_empty() {
            return None;
        }
        let path = if let Some(path) = source_file {
            path.to_owned()
        } else if let Some(source) = pages.and_then(|p| p.into_iter().flatten().next()) {
            source.path.clone()
        } else {
            crate::performance::save_tool_output(root, original).ok()?
        };
        let note = if count == 0 {
            "base64: continuation of a previously counted block".to_owned()
        } else {
            summary(count, bytes)
        };
        let insert = json_end.map_or(original.len(), |end| end - 1);
        let mut index = LineIndex::new(root, &path);
        let (mut text, mut changes) = render_runs(
            &original[..insert],
            &blocks,
            &path,
            1,
            json_end.is_some(),
            &locations,
            Some(&mut index),
        );
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
            index,
            json_end,
            note,
        })
    }

    /// Never split a file reference at a preview boundary. Repeat references
    /// from the missing middle in the space reserved below the preview.
    pub fn retain(&self, limit: usize) -> (String, PreviewRanges) {
        let ((_, note_range), marks) = self.changes.split_last().expect("summary insertion");
        let (view, kept, missing_index) = retain_with_references(
            &self.text,
            marks,
            Some((&self.note, note_range)),
            limit,
            || self.index.pointer().ok(),
        );
        if missing_index {
            // As with a failed I6 spill, preserve the only complete reference list.
            return (self.text.clone(), PreviewRanges::whole(self.text.len()));
        }
        (view, kept)
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
    index_pointer: impl FnOnce() -> Option<String>,
) -> (String, PreviewRanges, bool) {
    use crate::performance::preview_with_ranges;
    if text.len() <= limit {
        return (text.to_owned(), PreviewRanges::whole(text.len()), false);
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
    let missing = marks.iter().filter(|(_, r)| hidden(r, &kept));
    let missing_bytes = missing.clone().map(|(_, r)| 1 + r.len()).sum::<usize>();
    let indexed = view.len() + missing_bytes > available;
    let mut missing_index = false;
    if indexed {
        if let Some(pointer) = index_pointer() {
            view.push('\n');
            view.push_str(&pointer);
        } else {
            missing_index = true;
        }
    } else {
        for (_, r) in missing {
            view.push('\n');
            view.push_str(&text[r.clone()]);
        }
    }
    if let Some((note, _)) = missing_note {
        view.push('\n');
        view.push_str(note);
    }
    (view, kept, missing_index)
}

fn render_runs(
    original: &str,
    blocks: &[Run],
    path: &Path,
    mut line: usize,
    json: bool,
    locations: &HashMap<usize, Location>,
    mut index: Option<&mut LineIndex>,
) -> (String, Vec<(Range<usize>, Range<usize>)>) {
    let mut text = String::new();
    let mut changes = Vec::new();
    let mut copied = 0;

    for Run {
        range,
        decoded_len,
        kind,
        incomplete,
    } in blocks
    {
        let gap = &original[copied..range.start];
        text.push_str(gap);
        line += gap.bytes().filter(|&b| b == b'\n').count();
        let origin = locations.get(&range.start);
        let path = origin.and_then(|l| l.path.as_deref()).unwrap_or(path);
        let line = origin.map_or(line, |l| l.line);
        if let Some(index) = index.as_deref_mut() {
            index.record(path, line, origin.map_or(range, |l| &l.range).clone());
        }
        let location = if *incomplete {
            "run cut at capture ceiling; captured output"
        } else {
            "full output"
        };
        let marker = format!("[base64 data omitted: about {decoded_len} bytes decoded, looks like {kind}; {location}: {}, line {line}]", path.display());
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
    pub needs_index: bool,
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
    let (text, marks) = render_runs(line, &blocks, path, number, false, &HashMap::new(), None);
    let (text, _, needs_index) = retain_with_references(&text, &marks, None, limit, || None);
    Some(MonitorLine {
        text,
        needs_index,
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
