//! Recovery references can arrive embedded in plain text or serialized JSON.
use std::{ops::Range, sync::LazyLock};

const PREFIX: &str = "[base64 data omitted: about ";

fn spans(text: &str) -> Vec<Range<usize>> {
    if !text.contains(PREFIX) {
        return Vec::new();
    }
    // Prefer the generated spill filename as the path terminator. Directories
    // can contain brackets, line suffixes, or even another placeholder header.
    // Non-UTF-8 Unix paths use a shell-quoted hex spelling of the same .txt
    // suffix. The last alternative also accepts simple literal recovery paths.
    static PLACEHOLDER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"\[base64 data omitted: about [0-9]+ bytes decoded, looks like [A-Za-z]+",
            r"(?:\]|; full text: (?s:.*?(?:[0-9a-f]{64}\.txt|x2e\\+x74\\+x78\\+x74')|[^\[\]]+?), line [0-9]+\])"
        )).unwrap()
    });
    PLACEHOLDER.find_iter(text).map(|m| m.range()).collect()
}

/// Keep a reference touching the head cut when it fits by borrowing bytes
/// from the tail. Otherwise drop the crossing reference in its entirety.
fn cuts(text: &str, budget: usize, spans: &[Range<usize>]) -> (usize, usize, usize) {
    let half = budget / 2;
    let mut head = text.floor_char_boundary(half);
    let mut tail_budget = budget - half;
    if let Some(span) = spans.iter().find(|r| r.start < head && head < r.end) {
        if span.end <= budget {
            head = span.end;
            tail_budget = budget - head;
        } else {
            head = span.start;
        }
    }
    let mut tail = text.ceil_char_boundary(text.len() - tail_budget);
    if let Some(span) = spans.iter().find(|r| r.start < tail && tail < r.end) {
        tail = if text.len() - span.start <= budget - head {
            span.start
        } else {
            span.end
        };
    }
    let omitted = spans
        .iter()
        .filter(|r| head <= r.start && r.end <= tail)
        .count();
    (head, tail, omitted)
}

pub(super) fn head_tail(text: &str, limit: usize) -> (String, super::PreviewRanges) {
    let mut marker = format!(
        "\n... (output truncated; {} bytes before preview) ...\n",
        text.len()
    );
    let spans = spans(text);
    let (mut head, mut tail, omitted) = cuts(text, limit.saturating_sub(marker.len()), &spans);
    if omitted > 0 {
        let note = |n| {
            format!(
                "\n... (output truncated; {} bytes before preview; placeholders omitted: {n}) ...\n",
                text.len()
            )
        };
        // Reserve enough digits even if the larger notice displaces another
        // reference. Report the actual count at the final source boundaries.
        let reserved = note(spans.len()).len();
        let count;
        (head, tail, count) = cuts(text, limit.saturating_sub(reserved), &spans);
        marker = note(count);
        if marker.len() > limit {
            marker = format!("{count} omitted");
        }
    }
    if marker.len() > limit {
        return (
            "...".chars().take(limit).collect(),
            super::PreviewRanges::whole(0),
        );
    }
    let ranges = super::PreviewRanges {
        head: 0..head,
        tail: tail..text.len(),
        tail_offset: head + marker.len(),
    };
    (
        format!("{}{marker}{}", &text[..head], &text[tail..]),
        ranges,
    )
}

/// Preserve a caller's existing one-sided preview and notice for ordinary text.
pub(crate) fn prefix(text: &str, cut: usize, marker: &str) -> String {
    let spans = spans(text);
    if spans.last().is_none_or(|span| span.end <= cut) {
        return format!("{}{marker}", &text[..cut]);
    }
    let note = |count| format!(" (placeholders omitted: {count})");
    let budget = text.floor_char_boundary(cut.saturating_sub(note(spans.len()).len()));
    let kept = spans.partition_point(|span| span.end <= budget);
    let head = spans
        .get(kept)
        .map_or(budget, |span| budget.min(span.start));
    format!("{}{marker}{}", &text[..head], note(spans.len() - kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_model_boundary_preview_preserves_ordinary_bytes_and_ranges() {
        let text = "🎉x".repeat(100);
        let marker = format!(
            "\n... (output truncated; {} bytes before preview) ...\n",
            text.len()
        );
        for limit in 0..text.len() {
            let (preview, ranges) = head_tail(&text, limit);
            if limit < marker.len() {
                assert_eq!(preview, ".".repeat(limit.min(3)));
                assert_eq!(ranges.kept(), 0);
            } else {
                let budget = limit - marker.len();
                let head = text.floor_char_boundary(budget / 2);
                let tail = text.ceil_char_boundary(text.len() - budget.div_ceil(2));
                assert_eq!(
                    preview,
                    format!("{}{marker}{}", &text[..head], &text[tail..])
                );
                assert_eq!(
                    (ranges.head, ranges.tail, ranges.tail_offset),
                    (0..head, tail..text.len(), head + marker.len())
                );
            }
        }
    }

    #[test]
    fn base64_model_boundary_preview_handles_brackets_in_recovery_paths() {
        let reference = format!("{PREFIX}768 bytes decoded, looks like text; full text: /dir.txt, line 12]/{PREFIX}2 bytes decoded, looks like text]/{}.txt, line 1]", "a".repeat(64));
        let text = format!("{}{}", reference, reference);
        let size = reference.len();
        assert_eq!(spans(&text), vec![0..size, size..text.len()]);
        for limit in 0..text.len() {
            let (preview, ranges) = head_tail(&text, limit);
            assert!(preview.len() <= limit);
            assert!([0, size, text.len()].contains(&ranges.head.end));
            assert!([0, size, text.len()].contains(&ranges.tail.start));
            if limit > 100 {
                let count = (ranges.tail.start - ranges.head.end) / size;
                assert!(preview.contains(&format!("placeholders omitted: {count}")));
            }
        }
        assert_eq!(
            prefix(&text, size + 40, "..."),
            format!("{reference}... (placeholders omitted: 1)")
        );
    }
}
