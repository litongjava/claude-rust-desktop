//! Bounded, literal source excerpts. No command or reporter grammar.
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub(crate) const MAX_NOTABLE: usize = 5;
const QUOTE_BYTES: usize = 112;
const WINDOW: usize = 4096;
static MARKERS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i:error|fatal:)|FAILED|panicked at|Traceback|AssertionError|npm ERR!|BUILD FAILURE",
    )
    .unwrap()
});

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Line {
    pub number: usize,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Lines {
    pub notable: Vec<Line>,
    pub head: Option<Line>,
    pub tail: Option<Line>,
}

fn blank(c: char) -> bool {
    c.is_whitespace() || c.is_control()
}

impl Lines {
    pub fn observe(&mut self, number: usize, raw: &str) {
        let text = raw.trim_matches(blank);
        if text.is_empty() {
            return;
        }
        if self.head.is_none() {
            self.head = Some(Line {
                number,
                text: prefix(text).to_owned(),
            });
        }
        let tail = self.tail.get_or_insert_with(|| Line {
            number,
            text: String::new(),
        });
        tail.number = number;
        tail.text.clear();
        tail.text.push_str(suffix(text));
        if self.notable.len() == MAX_NOTABLE
            || self
                .notable
                .last()
                .is_some_and(|line| line.number == number)
        {
            return;
        }
        if let Some(marker) = MARKERS.find(text) {
            let start = prefix(text);
            let quote = if marker.end() <= start.len() {
                start
            } else {
                prefix(&text[marker.start()..])
            };
            if !self.notable.iter().any(|line| line.text == quote) {
                self.notable.push(Line {
                    number,
                    text: quote.to_owned(),
                });
            }
        }
    }
}

/// A single unbounded line may cross any number of pipe reads. Keep a fixed
/// window with enough overlap for every marker and a printable excerpt.
#[derive(Default)]
pub(crate) struct LineCapture {
    lines: Lines,
    pending: Vec<u8>,
    completed: usize,
    continuation: bool,
}

impl LineCapture {
    fn observe_pending(&mut self, final_piece: bool) {
        let decoded = String::from_utf8_lossy(&self.pending);
        let mut text = decoded.as_ref();
        // An artificial window can cut a scalar. The overlapping window
        // includes the intact bytes; don't quote an invented replacement.
        if self.continuation {
            text = after_overlap_start(text);
        }
        if !final_piece {
            text = text.strip_suffix('�').unwrap_or(text);
        }
        self.lines.observe(self.completed.saturating_add(1), text);
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for part in bytes.split_inclusive(|&b| b == b'\n') {
            let mut rest = part.strip_suffix(b"\n").unwrap_or(part);
            while !rest.is_empty() {
                let n = rest.len().min(WINDOW - self.pending.len());
                self.pending.extend_from_slice(&rest[..n]);
                rest = &rest[n..];
                if self.pending.len() == WINDOW {
                    self.observe_pending(false);
                    self.pending.copy_within(WINDOW - QUOTE_BYTES.., 0);
                    self.pending.truncate(QUOTE_BYTES);
                    self.continuation = true;
                }
            }
            if part.ends_with(b"\n") {
                self.observe_pending(true);
                self.pending.clear();
                self.completed = self.completed.saturating_add(1);
                self.continuation = false;
            }
        }
    }

    pub fn finish(&self) -> Lines {
        let mut lines = self.lines.clone();
        let decoded = String::from_utf8_lossy(&self.pending);
        lines.observe(
            self.completed.saturating_add(1),
            if self.continuation {
                after_overlap_start(&decoded)
            } else {
                &decoded
            },
        );
        lines
    }
}

// At most three continuation bytes from a scalar that began in the
// previous window can decode as spurious leading replacements.
fn after_overlap_start(mut text: &str) -> &str {
    for _ in 0..3 {
        let Some(rest) = text.strip_prefix('�') else {
            break;
        };
        text = rest;
    }
    text
}

// Bound quotes after JSON escaping as well as before it. This leaves room
// for both streams' edges even if every marked line contains control bytes.
fn escaped_width(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\x08' | '\x0c' => 2,
        '\0'..='\x1f' => 6,
        _ => c.len_utf8(),
    }
}

fn prefix(s: &str) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += escaped_width(c);
        if used > QUOTE_BYTES {
            return &s[..i];
        }
    }
    s
}

fn suffix(s: &str) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        used += escaped_width(c);
        if used > QUOTE_BYTES {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}
