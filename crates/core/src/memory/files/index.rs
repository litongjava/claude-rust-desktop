use super::store::Entry;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Copy)]
pub struct IndexLimits {
    pub lines: usize,
    pub bytes: usize,
    pub tokens: usize,
}
impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            lines: 200,
            bytes: 25 * 1024,
            tokens: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexContext {
    pub text: String,
    pub included: usize,
    pub omitted: usize,
    pub truncated: bool,
    /// False only when even the omission marker cannot fit (e.g. zero budget).
    pub marker_included: bool,
}

impl IndexContext {
    /// Caller may pass the actual model tokenizer. The default byte counter is
    /// conservative for UTF-8; never assume chars/4 for Chinese text.
    pub fn from_index(
        index: &str,
        path: &str,
        limits: IndexLimits,
        count_tokens: impl Fn(&str) -> usize,
    ) -> Self {
        let rows: Vec<_> = index.lines().collect();
        let fits = |text: &str| {
            text.lines().count() <= limits.lines
                && text.len() <= limits.bytes
                && count_tokens(text) <= limits.tokens
        };
        if fits(index) {
            return Self {
                text: index.into(),
                included: rows.len(),
                omitted: 0,
                truncated: false,
                marker_included: false,
            };
        }
        // No tokenizer monotonicity assumptions: test each complete prefix.
        let mut prefix = String::new();
        let mut best = Self {
            text: String::new(),
            included: 0,
            omitted: rows.len(),
            truncated: true,
            marker_included: false,
        };
        let path = path.replace(['\r', '\n'], " ");
        for included in 0..rows.len() {
            let candidate = format!("{prefix}[Memory index truncated: {} entries omitted; full index: {path}; limits: {} lines / {} bytes / {} tokens.]\n", rows.len() - included, limits.lines, limits.bytes, limits.tokens);
            if fits(&candidate) {
                best = Self {
                    text: candidate,
                    included,
                    omitted: rows.len() - included,
                    truncated: true,
                    marker_included: true,
                };
            }
            prefix.push_str(rows[included]);
            prefix.push('\n');
            if prefix.len() > limits.bytes || included + 1 > limits.lines {
                break;
            }
        }
        best
    }
}

fn escape(text: &str) -> String {
    let mut output = String::new();
    for c in text.chars() {
        if "\\[]*_`<>!".contains(c) {
            output.push('\\');
        }
        output.push(c);
    }
    output
}

pub(super) fn build(entries: &[Entry], previous: &[u8]) -> String {
    let mut remaining: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| (entry.filename.as_str(), entry))
        .collect();
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(previous) = std::str::from_utf8(previous) {
        for row in previous.lines() {
            if let Some((_, tail)) = row.rsplit_once("](") {
                if let Some((filename, _)) = tail.split_once(')') {
                    if seen.insert(filename) {
                        if let Some(entry) = remaining.remove(filename) {
                            ordered.push(entry);
                        }
                    }
                }
            }
        }
    }
    let mut rest: Vec<_> = remaining.into_values().collect();
    rest.sort_by_key(|entry| {
        (
            super::format::name_key(&entry.document.name),
            entry.filename.clone(),
        )
    });
    ordered.extend(rest);
    ordered
        .into_iter()
        .map(|entry| {
            format!(
                "- [{}]({}) — {}\n",
                escape(&entry.document.name),
                entry.filename,
                escape(&entry.document.description)
            )
        })
        .collect()
}
