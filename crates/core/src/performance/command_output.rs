//! Shell accounting is in source bytes. A line includes its terminating LF.
//! Lossy decoding preserves LF boundaries; preview offsets are UTF-8 text bytes.
use std::ops::Range;

pub(crate) const LONG_COMMAND_OUTPUT_BYTES: usize = 12_000;

pub(crate) fn qualifies(bytes: [usize; 2]) -> bool {
    bytes[0].saturating_add(bytes[1]) >= LONG_COMMAND_OUTPUT_BYTES
}

#[derive(Default)]
pub(crate) struct ByteLines {
    pub bytes: usize,
    pub newlines: usize,
    unterminated: bool,
}

impl ByteLines {
    pub fn measure(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.len(),
            newlines: bytes.iter().filter(|&&b| b == b'\n').count(),
            unterminated: bytes.last().is_some_and(|&b| b != b'\n'),
        }
    }

    pub fn append(&mut self, chunk: &[u8]) {
        let part = Self::measure(chunk);
        self.bytes = self.bytes.saturating_add(part.bytes);
        self.newlines = self.newlines.saturating_add(part.newlines);
        if part.bytes != 0 {
            self.unterminated = part.unterminated;
        }
    }

    pub fn lines(&self) -> usize {
        self.newlines.saturating_add(usize::from(self.unterminated))
    }
}

#[derive(Default)]
pub(crate) struct PreviewExtent {
    pub text_bytes: usize,
    pub lines: usize,
    pub widths: ReplacementWidths,
}

impl PreviewExtent {
    pub fn source_bytes(&self) -> usize {
        self.widths.raw_bytes(self.text_bytes)
    }
}

/// Each source range starts on a known zero-based line of the original stream.
/// Count the union of line identities touched by the ordered visible ranges.
pub(crate) fn shown_extent(
    text: &str,
    sources: &[Range<usize>],
    first_lines: &[usize],
    visible: &[Range<usize>],
    widths: &ReplacementWidths,
) -> PreviewExtent {
    let mut bytes = 0;
    let mut lines = 0;
    let mut previous_end = 0;
    for (source, &origin) in sources.iter().zip(first_lines) {
        let mut pos = source.start;
        let mut line = origin;
        for range in visible {
            let start = source.start.max(range.start);
            let end = source.end.min(range.end);
            if start >= end {
                continue;
            }
            line += ByteLines::measure(&text.as_bytes()[pos..start]).newlines;
            let part = ByteLines::measure(&text.as_bytes()[start..end]);
            let line_end = line + part.lines();
            bytes += part.bytes;
            lines += line_end.saturating_sub(previous_end.max(line));
            previous_end = previous_end.max(line_end);
            line += part.newlines;
            pos = end;
        }
    }
    PreviewExtent {
        text_bytes: bytes,
        lines,
        widths: widths.project(text, visible),
    }
}

/// Original byte width of each U+FFFD, in order. ASCII digits keep a lossy
/// sidecar bounded to one byte per replacement and distinguish literal U+FFFD.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct ReplacementWidths(String);

impl ReplacementWidths {
    pub fn of(raw: &[u8]) -> Self {
        if std::str::from_utf8(raw).is_ok() {
            return Self::default();
        }
        let mut widths = String::new();
        for chunk in raw.utf8_chunks() {
            widths.extend(chunk.valid().matches('�').map(|_| '3'));
            if !chunk.invalid().is_empty() {
                widths.push(char::from(b'0' + chunk.invalid().len() as u8));
            }
        }
        Self(widths)
    }

    pub fn project(&self, text: &str, ranges: &[Range<usize>]) -> Self {
        if self.0.is_empty() {
            return Self::default();
        }
        let mut ranges = ranges.iter().peekable();
        let mut widths = String::new();
        for ((offset, _), width) in text.match_indices('�').zip(self.0.bytes()) {
            while ranges.next_if(|r| r.end <= offset).is_some() {}
            let Some(range) = ranges.peek() else { break };
            if range.start <= offset {
                widths.push(char::from(width));
            }
        }
        Self(widths)
    }

    pub fn raw_bytes(&self, text_bytes: usize) -> usize {
        let extra: usize = self
            .0
            .bytes()
            .map(|width| b'3'.saturating_sub(width) as usize)
            .sum();
        text_bytes.saturating_sub(extra)
    }
}

/// Source byte coordinates of a file-backed snapshot, before UTF-8 replacement.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SnapshotSize {
    pub read: usize,
    pub omitted: usize,
    pub from_tail: bool,
    pub widths: ReplacementWidths,
}

impl SnapshotSize {
    pub fn read_log(
        path: &std::path::Path,
        limit: u64,
        tail: bool,
    ) -> std::io::Result<(String, Self)> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(e) => return Err(e),
        };
        let length = file.metadata()?.len();
        let offset = if tail {
            length.saturating_sub(limit)
        } else {
            0
        };
        if offset != 0 {
            file.seek(SeekFrom::Start(offset))?;
        }
        let mut raw = Vec::new();
        file.take(length.min(limit)).read_to_end(&mut raw)?;
        let omitted = if tail {
            offset
        } else {
            length.saturating_sub(limit)
        };
        Ok((
            String::from_utf8_lossy(&raw).into_owned(),
            Self {
                read: raw.len(),
                omitted: usize::try_from(omitted).unwrap_or(usize::MAX),
                from_tail: offset != 0,
                widths: ReplacementWidths::of(&raw),
            },
        ))
    }
}
