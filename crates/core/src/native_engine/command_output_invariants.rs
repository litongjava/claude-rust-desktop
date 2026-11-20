use super::*;
use crate::performance::command_output::shown_extent;
use crate::tools::bash_process::fixture_capture;
use std::collections::HashSet;

// Repeatable cases with the failing seed in assertion messages.
struct Generator(u64);
impl Generator {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize % n
    }

    fn stream(&mut self, size: usize) -> Vec<u8> {
        (0..size)
            .map(|_| match self.below(32) {
                0 => b'\n',
                1 => 0xff,
                2 => b'\r',
                3 => b'\t',
                _ => b'a' + self.below(26) as u8,
            })
            .collect()
    }
}

fn render_capture(
    root: &Path,
    streams: [&[u8]; 2],
    capture_limit: usize,
    tool_limit: usize,
    exit: i64,
) -> (String, String) {
    let (raw, metadata) = fixture_capture(streams, capture_limit, tool_limit, exit);
    let preview = |enabled| command_preview("Bash", &raw, Some(root), metadata.as_ref(), enabled);
    (preview(true), preview(false))
}

#[test]
fn reviewer_libtest_examples_keep_authentic_output_and_withhold_competing_totals() {
    let temp = tempfile::tempdir().unwrap();
    let captures: std::collections::BTreeMap<String, String> =
        serde_json::from_str(include_str!("test_summary_fixtures/review-02-libtest.json")).unwrap();
    assert_eq!(captures.len(), 2);
    for capture in captures.values() {
        let stdout = format!("{}\n{capture}", "0".repeat(12_100));
        let (result, _) =
            render_capture(temp.path(), [stdout.as_bytes(), b""], 100_000, 100_000, 0);
        let (body, summary) = result.split_once("Command output summary").unwrap();
        assert!(body.contains(capture));
        assert!(summary.contains("Exit status 0 | elapsed 37 ms"));
        assert!(
            !summary.contains("Test summary") && !summary.contains("failed"),
            "{summary}"
        );
    }
}

#[test]
fn reviewer_one_line_previews_count_each_stream_once_without_repeating_it() {
    let dir = tempfile::tempdir().unwrap();
    for (out, err) in [(12_000, 0), (12_001, 0), (8001, 8001)] {
        let row = |ch: u8, size: usize| {
            let mut bytes = vec![ch; size];
            if let Some(last) = bytes.last_mut() {
                *last = b'\n';
            }
            bytes
        };
        let stdout = row(if err == 0 { b'x' } else { b's' }, out);
        let stderr = row(b't', err);
        let (result, _) = render_capture(dir.path(), [&stdout, &stderr], 100_000, 100_000, 0);
        let (body, summary) = result.split_once("Command output summary").unwrap();
        for (name, bytes) in [("stdout", &stdout), ("stderr", &stderr)] {
            if bytes.is_empty() {
                continue;
            }
            let shown = body
                .lines()
                .filter(|l| {
                    l.as_bytes().first() == bytes.first() && l.bytes().all(|b| b == bytes[0])
                })
                .map(str::len)
                .sum::<usize>()
                + 1;
            assert!(
                summary.contains(&format!(
                    "{name} {shown} text bytes, 1 lines shown of {} source bytes / 1 lines",
                    bytes.len()
                )),
                "{summary}"
            );
        }
        assert!(!summary.contains("Last "), "{summary}");
    }
}

#[test]
fn reviewer_invalid_utf8_threshold_uses_raw_bytes_including_both_streams() {
    let dir = tempfile::tempdir().unwrap();
    let below = [vec![b'x'; 11_997], vec![0xff, b'\n']].concat();
    let (on, off) = render_capture(dir.path(), [&below, b""], 100_000, 100_000, 0);
    assert_eq!(on, off);
    let stdout = [vec![b'x'; 11_971], vec![b'\n']].concat();
    let stderr = b"\x1b[33mwarning: binary \xff\0\x1b[0m\n";
    let (on, _) = render_capture(dir.path(), [&stdout, stderr], 100_000, 100_000, 0);
    let (_, summary) = on.split_once("Command output summary").unwrap();
    for expected in [
        "elapsed 37 ms",
        "11972 source bytes / 1 lines",
        "28 source bytes / 1 lines",
        "stderr 30 text bytes",
    ] {
        assert!(summary.contains(expected), "{summary}");
    }
}

#[test]
fn generated_nested_ranges_equal_a_per_byte_line_identity_oracle() {
    for seed in 1..=512 {
        let mut rng = Generator(seed);
        let size = rng.below(700);
        let bytes = rng.stream(size);
        let text = String::from_utf8_lossy(&bytes);
        // Assign every projected UTF-8 byte its original source line. Neither
        // production counters nor the preview helpers participate in the oracle.
        let mut line = 0;
        let ids: Vec<_> = text
            .bytes()
            .map(|b| {
                let id = line;
                line += usize::from(b == b'\n');
                id
            })
            .collect();
        let mut projected = String::new();
        let mut original_ids = Vec::new();
        let mut sources = Vec::new();
        let mut origins = Vec::new();
        let mut pos = 0;
        while pos < text.len() {
            let end = text.ceil_char_boundary((pos + 1 + rng.below(90)).min(text.len()));
            origins.push(ids[pos]);
            sources.push(projected.len()..projected.len() + end - pos);
            projected.push_str(&text[pos..end]);
            original_ids.extend(ids[pos..end].iter().copied().map(Some));
            projected.push_str("\n...\n");
            original_ids.extend([None; 5]);
            pos = text.ceil_char_boundary((end + rng.below(75)).min(text.len()));
        }
        let mut visible = Vec::new();
        let mut pos = 0;
        while pos < projected.len() {
            let end = projected.ceil_char_boundary((pos + 1 + rng.below(80)).min(projected.len()));
            visible.push(pos..end);
            pos = projected.ceil_char_boundary((end + rng.below(65)).min(projected.len()));
        }
        let seen: Vec<_> = visible
            .iter()
            .flat_map(|r| original_ids[r.clone()].iter().flatten())
            .collect();
        let expected = (seen.len(), seen.into_iter().collect::<HashSet<_>>().len());
        let shown = shown_extent(
            &projected,
            &sources,
            &origins,
            &visible,
            &Default::default(),
        );
        assert_eq!((shown.text_bytes, shown.lines), expected, "seed {seed}");
    }
}

#[test]
fn generated_two_stream_captures_keep_raw_gate_counts_and_unique_excerpt_positions() {
    let dir = tempfile::tempdir().unwrap();

    for seed in 1..=120 {
        let mut rng = Generator(seed);
        let size = 11_996 + rng.below(9);
        let split = rng.below(size + 1);
        let mut streams = [rng.stream(split), rng.stream(size - split)];
        for stream in &mut streams {
            for sample in [
                b"\nerror: generated\n".as_slice(),
                b"\nFAILED fixture\n",
                b"\nTraceback example\n",
            ] {
                if stream.len() >= sample.len() {
                    let start = rng.below(stream.len() - sample.len() + 1);
                    stream[start..start + sample.len()].copy_from_slice(sample);
                }
            }
        }
        let (cap, tool) = match rng.below(3) {
            0 => (100_000, 100_000),
            1 => (600 + rng.below(7000), 100_000),
            _ => (100_000, 300 + rng.below(6000)),
        };
        let (result, base) = render_capture(dir.path(), [&streams[0], &streams[1]], cap, tool, 0);
        if size < 12_000 {
            assert_eq!(result, base, "seed {seed}");
            continue;
        }
        let (body, summary) = result.split_once("Command output summary").unwrap();
        assert!(
            summary.contains("Exit status 0 | elapsed 37 ms"),
            "seed {seed}"
        );
        check_factual_summary(
            summary,
            [
                &String::from_utf8_lossy(&streams[0]),
                &String::from_utf8_lossy(&streams[1]),
            ],
            body,
        );
        for bytes in &streams {
            if bytes.is_empty() {
                continue;
            }
            let lines = bytes.split_inclusive(|&b| b == b'\n').count();
            assert!(
                summary.contains(&format!("of {} source bytes / {lines} lines", bytes.len())),
                "seed {seed}: {summary}"
            );
        }
        assert!(result.len() <= MODEL_TOOL_OUTPUT_BYTES);
    }
}

#[test]
fn generated_lossy_preview_preserves_raw_byte_and_line_identity() {
    use crate::performance::command_output::ReplacementWidths;
    // Each element has exactly one decoded scalar, including a literal
    // replacement and invalid sequences consuming one, two or three bytes.
    let alphabet: &[&[u8]] = &[
        b"a",
        b"\n",
        "�".as_bytes(),
        "界".as_bytes(),
        b"\xff",
        b"\xe2\x82",
        b"\xf0\x90\x80",
    ];
    for seed in 1..=256 {
        let mut rng = Generator(seed);
        let mut bytes = Vec::new();
        let mut raw_weights = Vec::new();
        let mut line_ids = Vec::new();
        let mut line = 0;
        let mut offsets = vec![0];
        for _ in 0..100 {
            let part = alphabet[rng.below(alphabet.len())];
            bytes.extend_from_slice(part);
            raw_weights.push(part.len());
            line_ids.push(line);
            line += usize::from(part == b"\n");
            offsets.push(offsets.last().unwrap() + String::from_utf8_lossy(part).len());
        }
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text.len(), *offsets.last().unwrap());
        let a = rng.below(101);
        let b = a + rng.below(101 - a);
        let ranges = [0..offsets[a], offsets[b]..text.len()];
        let visible: Vec<_> = (0..a).chain(b..100).collect();
        let expected_raw: usize = visible.iter().map(|&i| raw_weights[i]).sum();
        let expected_lines = visible
            .iter()
            .map(|&i| line_ids[i])
            .collect::<HashSet<_>>()
            .len();
        let shown = shown_extent(
            &text,
            &[0..text.len()],
            &[0],
            &ranges,
            &ReplacementWidths::of(&bytes),
        );
        assert_eq!(
            (shown.source_bytes(), shown.lines),
            (expected_raw, expected_lines),
            "seed {seed}"
        );
    }
}

#[test]
fn spilled_json_excludes_visible_escaped_lines_from_excerpts() {
    let dir = tempfile::tempdir().unwrap();
    let stdout = format!(
        "first \"quoted\"\n{}\nerror: hidden\n{}\nwarning: emoji 🙂\n",
        "a".repeat(12500),
        "b".repeat(12500)
    );
    let raw =
        serde_json::json!({"stdout":stdout,"stderr":"warning: another stream\n","exit_code":1})
            .to_string()
            .replace("🙂", "\\uD83D\\uDE42")
            .replace("stdout", "std\\u006fut");
    let rendered = command_preview("TaskGet", &raw, Some(dir.path()), None, true);
    let (_, summary) = rendered.split_once("Command output summary").unwrap();
    assert!(
        summary.contains(r#"Notable stdout L3 | "error: hidden""#),
        "{summary}"
    );
    assert_eq!(rendered.matches("error: hidden").count(), 1);
    assert!(
        !summary.contains("emoji") && !summary.contains("another stream"),
        "{summary}"
    );
}

// Every free-form part of a summary must be a verbatim, located quotation.
// Numbers occurring inside process output are checked against the source.
fn check_factual_summary(summary: &str, streams: [&str; 2], body: &str) {
    static FACT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"^- (?:Exit status (?:-?[0-9]+|unknown)(?: \| elapsed [0-9]+ ms)?(?: \| (?:output page|log snapshot))?",
            r"|Model preview omitted [0-9]+ (?:text|serialized result) bytes",
            r"|(?:stdout|stderr) [0-9]+ text bytes(?:, [0-9]+ lines (?:shown|captured)| available before retention)",
            r"(?: of [0-9]+ (?:source|text|page) bytes(?: / [0-9]+ (?:page )?lines)?)?",
            r"(?: \| (?:source totals unknown|amount omitted unknown",
            r"|[0-9]+ source bytes shown, [0-9]+ omitted by model(?:, [0-9]+ omitted by tool)?",
            r"|[0-9]+ source bytes dropped at capture|[0-9]+ text bytes omitted by tool preview))*)$",
        )).unwrap()
    });
    let mut notable = HashSet::new();
    let mut reached_edges = false;
    let mut count = 0;
    let mut locations = HashSet::new();
    let marked = |s: &str| {
        let lower = s.to_ascii_lowercase();
        lower.contains("error")
            || lower.contains("fatal:")
            || [
                "FAILED",
                "panicked at",
                "Traceback",
                "npm ERR!",
                "BUILD FAILURE",
            ]
            .iter()
            .any(|m| s.contains(m))
    };
    for line in summary.lines().filter(|s| !s.is_empty()) {
        if FACT.is_match(line) {
            continue;
        }
        let mut words = line.strip_prefix("- ").unwrap().splitn(3, ' ');
        let kind = words.next().unwrap();
        assert!(matches!(kind, "Notable" | "Head" | "Tail"), "{line}");
        if kind == "Notable" {
            assert!(!reached_edges);
        } else {
            reached_edges = true;
        }
        let name = words.next().unwrap();
        let stream = match name {
            "stdout" => streams[0],
            "stderr" => streams[1],
            _ => panic!("{line}"),
        };
        let rest = words
            .next()
            .unwrap()
            .trim_start_matches("result ")
            .trim_start_matches("page ");
        let (number, rendered) = rest.strip_prefix('L').unwrap().split_once(' ').unwrap();
        let original = stream
            .split('\n')
            .nth(number.parse::<usize>().unwrap() - 1)
            .unwrap();
        assert!(locations.insert((name, number)), "{line}");
        if kind == "Notable" {
            count += 1;
            assert!(marked(original), "{line} / {original:?}");
        }
        if rendered == "(in preview)" {
            assert!(!body.is_empty());
            continue;
        }
        let quote: String = serde_json::from_str(rendered.strip_prefix("| ").unwrap()).unwrap();
        assert!(
            !quote.trim().is_empty() && rendered.len() <= 116 && original.contains(&quote),
            "{line} / {original:?}"
        );
        if kind == "Notable" {
            assert!(marked(&quote), "{line}");
            assert!(notable.insert(quote), "{line}");
        }
    }
    assert!(count <= 5);
}

#[test]
fn reviewer_capture_gaps_cross_stream_frames_and_conflicting_reporters_only_quote_source() {
    use std::io::Read;
    let mut json = String::new();
    flate2::read::GzDecoder::new(
        include_bytes!("test_summary_fixtures/review-03.json.gz").as_slice(),
    )
    .read_to_string(&mut json)
    .unwrap();
    let cases: std::collections::BTreeMap<String, Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(cases.len(), 16);
    let dir = tempfile::tempdir().unwrap();
    for (name, raw) in cases {
        let result = command_preview("Bash", &raw.to_string(), Some(dir.path()), None, true);
        // These old public tool-cut records have no sidecar proving the raw
        // gate was reached. Keep main's preview byte-for-byte in that case.
        if raw["stdout"].as_str().unwrap().len() + raw["stderr"].as_str().unwrap().len() < 12_000 {
            assert_eq!(
                result,
                command_preview("Bash", &raw.to_string(), Some(dir.path()), None, false),
                "{name}"
            );
            continue;
        }
        let (body, summary) = result.split_once("Command output summary").unwrap();
        check_factual_summary(
            summary,
            [
                raw["stdout"].as_str().unwrap(),
                raw["stderr"].as_str().unwrap(),
            ],
            body,
        );
        assert!(
            summary.contains(&format!("Exit status {}", raw["exit_code"])),
            "{name}"
        );
    }
}

#[test]
fn reviewer_lf_and_one_character_of_an_error_are_insufficient_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let diagnostic = "error: cannot read manifest at line 42";
    let marker = "\n... (output truncated; 18000 bytes before preview) ...\n";
    let budget = 12000 - marker.len();
    let cut = 18000 - (budget - budget / 2);
    for end in [cut - 1, cut, cut + 1] {
        let src = format!(
            "{}\n{diagnostic}\n{}",
            "H".repeat(end - diagnostic.len() - 1),
            "T".repeat(18000 - end - 1)
        );
        let (result, _) = render_capture(dir.path(), [src.as_bytes(), b""], 100_000, 100_000, 7);
        let (body, summary) = result.split_once("Command output summary").unwrap();
        assert!(!body.contains(diagnostic));
        assert!(
            summary.contains(&format!("- Notable stdout L2 | \"{diagnostic}\"")),
            "{summary}"
        );
    }
}

#[test]
fn generated_fake_footers_and_markers_have_no_synthesized_test_claims() {
    let dir = tempfile::tempdir().unwrap();
    let examples = [
        "test result: ok. 73 passed; 0 failed",
        "test result: FAILED. 0 passed; 2 failed",
        "713 passed in 0.01s",
        "Test Suites: 713 passed, 713 total",
    ];
    for seed in 1..=128 {
        let mut rng = Generator(seed);
        let streams = [0, 1].map(|id| {
            let mut lines = vec![" ".repeat(7000)];
            for i in 0..8 {
                lines.push(examples[rng.below(examples.len())].into());
                lines.push(format!("fatal: stream {id} sample {i}"));
            }
            lines.push(" ".repeat(7000));
            lines.join("\n")
        });
        for code in [0, 101] {
            let (result, _) = render_capture(
                dir.path(),
                [streams[0].as_bytes(), streams[1].as_bytes()],
                4000,
                3000,
                code,
            );
            let (body, summary) = result.split_once("Command output summary").unwrap();
            assert!(summary.contains(&format!("- Exit status {code} | elapsed 37 ms")));
            check_factual_summary(summary, [&streams[0], &streams[1]], body);
            assert!(
                summary.contains("Notable ") && summary.contains(" | \""),
                "{summary}"
            );
            assert!(result.len() <= MODEL_TOOL_OUTPUT_BYTES);
        }
    }
}

#[test]
fn visible_text_survives_blank_boundaries_and_a_marker_spanning_capture_windows() {
    let dir = tempfile::tempdir().unwrap();
    let src = format!(
        "{}{}{}",
        "\n".repeat(8192),
        " ".repeat(4094),
        "error: sole marker"
    );
    let src = format!("{src}{}{}", " ".repeat(8000), "\n".repeat(8192));
    let (result, _) = render_capture(dir.path(), [src.as_bytes(), b""], 6000, 4096, 1);
    let (body, summary) = result.split_once("Command output summary").unwrap();
    assert!(!body.contains("sole marker"));
    assert!(
        summary.contains(r#"Notable stdout L8193 | "error: sole marker""#),
        "{summary}"
    );
    let neutral = src.replace("error: sole marker", "only visible text");
    let (result, _) = render_capture(dir.path(), [neutral.as_bytes(), b""], 6000, 4096, 0);
    let (_, summary) = result.split_once("Command output summary").unwrap();
    assert!(
        summary.contains("Head stdout L8193") && summary.contains("only visible text"),
        "{summary}"
    );
}

#[test]
fn escaped_notable_lines_leave_room_for_both_stream_edges() {
    let dir = tempfile::tempdir().unwrap();
    let middle = (0..7)
        .map(|n| format!("error[{n}]{}!", "\x01".repeat(150)))
        .collect::<Vec<_>>()
        .join("\n");
    let streams = [0, 1].map(|n| {
        format!(
            "{}head{n}\n{middle}\ntail{n}{}",
            "\n".repeat(8000),
            "\n".repeat(8000)
        )
    });
    let (result, _) = render_capture(
        dir.path(),
        [streams[0].as_bytes(), streams[1].as_bytes()],
        2000,
        1000,
        1,
    );
    let (body, summary) = result.split_once("Command output summary").unwrap();
    check_factual_summary(summary, [&streams[0], &streams[1]], body);
    assert_eq!(summary.matches("- Notable ").count(), 5);
    for name in ["stdout", "stderr"] {
        for edge in ["Head", "Tail"] {
            assert!(summary.contains(&format!("- {edge} {name}")), "{summary}");
        }
    }
    assert!(summary.len() <= DIGEST_BUDGET && result.len() <= MODEL_TOOL_OUTPUT_BYTES);
}

#[test]
fn binary_replacement_text_survives_blank_capture_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let stdout = [vec![b'\n'; 8192], vec![0xff; 8192], vec![b'\n'; 8192]].concat();
    let (result, _) = render_capture(dir.path(), [&stdout, b""], 4000, 2000, 0);
    let (body, summary) = result.split_once("Command output summary").unwrap();
    assert!(!body.contains('�'));
    assert!(
        summary.contains("Head stdout L8193") && summary.contains('�'),
        "{summary}"
    );
    check_factual_summary(summary, [&String::from_utf8_lossy(&stdout), ""], body);
}
