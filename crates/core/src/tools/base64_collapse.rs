//! Collapses long base64 data in tool output. A model cannot read base64, so a
//! long blob only wastes context; shorter data stays intact so identifiers,
//! hashes and tokens remain copyable and Edit's old_string still matches.
use base64::{
    alphabet,
    engine::{
        general_purpose::{GeneralPurpose, GeneralPurposeConfig},
        DecodePaddingMode, Engine,
    },
};

/// Ordinary runs this long are data rather than identifiers or hashes.
const RUN_THRESHOLD: usize = 1024;
/// A `data:` URI payload is unambiguous, so it collapses earlier.
pub(crate) const DATA_URI_THRESHOLD: usize = 256;
/// Enough input to decode the first 64 bytes used for the type guess.
const SAMPLE_CHARS: usize = 86;
const SAMPLE_BYTES: usize = 64;

/// Runs never contain whitespace, so scanning bytes matches character runs.
static ALPHABET: [bool; 256] = {
    let mut table = [false; 256];
    let mut index = 0;
    while index < 256 {
        table[index] = matches!(
            index as u8,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'-' | b'_'
        );
        index += 1;
    }
    table
};

fn engine(url_safe: bool) -> GeneralPurpose {
    GeneralPurpose::new(
        if url_safe {
            &alphabet::URL_SAFE
        } else {
            &alphabet::STANDARD
        },
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    )
}

/// Replaces each qualifying run. Returns the rewritten text and how many runs
/// were collapsed.
pub(crate) fn collapse(text: &str) -> (String, usize) {
    let mut out = String::new();
    let mut copied = 0;
    let runs = runs(text);
    for run in &runs {
        out.push_str(&text[copied..run.range.start]);
        out.push_str(&placeholder(run.decoded_len, run.kind));
        copied = run.range.end;
    }
    out.push_str(&text[copied..]);
    (out, runs.len())
}

pub(crate) struct Run {
    pub range: std::ops::Range<usize>,
    pub decoded_len: usize,
    pub kind: &'static str,
}

pub(crate) fn runs(text: &str) -> Vec<Run> {
    let bytes = text.as_bytes();
    let mut runs = Vec::new();
    let mut index = 0;
    // Remembers the last `data:` before the current position, so a payload
    // check does not rescan the prefix (keeping the whole pass linear).
    let mut data_scheme_seen = false;
    while index < bytes.len() {
        if index >= 5 && &bytes[index - 5..index] == b"data:" {
            data_scheme_seen = true;
        }
        if !ALPHABET[bytes[index] as usize] {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && ALPHABET[bytes[index] as usize] {
            index += 1;
        }
        let data_end = index;
        let mut padding = 0;
        while padding < 2 && index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            padding += 1;
        }
        let data = &bytes[start..data_end];
        let data_uri = data_scheme_seen && text[..start].ends_with(";base64,");
        let threshold = if data_uri {
            DATA_URI_THRESHOLD
        } else {
            RUN_THRESHOLD
        };
        if data.len() + padding < threshold {
            continue;
        }
        let Some((decoded_len, kind)) = collapsed_run(data, padding) else {
            continue;
        };
        runs.push(Run {
            range: start..index,
            decoded_len,
            kind,
        });
    }
    runs
}

/// The final note appended by Read and WebFetch when anything was replaced.
pub(crate) fn note(count: usize) -> String {
    format!("({count} long base64 blocks were collapsed; read with raw: true to see them.)")
}

fn placeholder(decoded_len: usize, kind: &str) -> String {
    format!("[base64 data omitted: about {decoded_len} bytes decoded, looks like {kind}]")
}

/// Validates a candidate run without decoding it all. Returns the decoded
/// length and the guessed type, or `None` when the run is not base64.
fn collapsed_run(data: &[u8], padding: usize) -> Option<(usize, &'static str)> {
    let mut standard = false;
    let mut url_safe = false;
    let mut hex = true;
    for &byte in data {
        match byte {
            b'+' | b'/' => standard = true,
            b'-' | b'_' => url_safe = true,
            _ => {}
        }
        hex &= byte.is_ascii_hexdigit();
    }
    // A run mixing the two alphabets decodes with neither. A long hex string
    // (a hash or identifier) is technically base64-shaped but almost never data.
    if (standard && url_safe) || hex {
        return None;
    }
    let remainder = data.len() % 4;
    if remainder == 1 {
        return None;
    }
    if padding > 0 {
        let expected = 4 - remainder;
        if remainder == 0 || padding != expected {
            return None;
        }
    }
    // Canonical encoding leaves the unused trailing bits zero, as strict
    // decoders require.
    let tail = data
        .last()
        .and_then(|byte| symbol_value(*byte))
        .unwrap_or(0);
    let canonical = match remainder {
        2 => tail & 0b1111 == 0,
        3 => tail & 0b11 == 0,
        _ => true,
    };
    if !canonical {
        return None;
    }
    let decoded_len = data.len() / 4 * 3
        + match remainder {
            2 => 1,
            3 => 2,
            _ => 0,
        };
    let take = data.len().min(SAMPLE_CHARS);
    let mut sample = engine(url_safe).decode(&data[..take]).ok()?;
    sample.truncate(SAMPLE_BYTES);
    Some((decoded_len, kind(&sample)))
}

/// The 6-bit value of a base64 symbol; standard and URL-safe symbols agree.
fn symbol_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

fn kind(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        "PNG"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "JPEG"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "GIF"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "WebP"
    } else if bytes.starts_with(b"%PDF-") {
        "PDF"
    } else if bytes.starts_with(&[0x1f, 0x8b]) {
        "gzip"
    } else if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        "zip"
    } else if utf8_prefix(bytes) {
        match bytes.iter().find(|byte| !byte.is_ascii_whitespace()) {
            Some(&b'{') | Some(&b'[') => "JSON",
            _ => "text",
        }
    } else {
        "binary"
    }
}

/// A prefix may end in the middle of a multi-byte character; that tail is
/// incomplete, not invalid.
fn utf8_prefix(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

    fn encoded(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    #[test]
    fn collapses_a_large_json_blob_inside_html() {
        let json = format!("{{\"request\":\"{}\"}}", "x".repeat(100_000));
        let blob = encoded(json.as_bytes());
        let html = format!("<html><body><pre>{blob}</pre></body></html>");
        let (out, count) = collapse(&html);
        assert_eq!(count, 1);
        assert!(out.contains("<html><body><pre>"));
        assert!(out.contains(&format!(
            "[base64 data omitted: about {} bytes decoded, looks like JSON]",
            json.len()
        )));
        assert!(!out.contains(&blob[..64]));
        assert!(out.len() < 1000);
    }

    #[test]
    fn keeps_runs_below_the_threshold() {
        let short = "QUJD".repeat(255) + "QUJ";
        assert_eq!(short.len(), 1023);
        let source = format!("token={short}");
        let (out, count) = collapse(&source);
        assert_eq!(count, 0);
        assert_eq!(out, source);
    }

    #[test]
    fn collapses_data_uri_payloads_and_keeps_the_prefix() {
        let payload = "QUJD".repeat(75);
        assert_eq!(payload.len(), 300);
        let uri = format!("data:application/json;base64,{payload}");
        let (out, count) = collapse(&uri);
        assert_eq!(count, 1);
        assert_eq!(
            out,
            format!(
                "data:application/json;base64,[base64 data omitted: about 225 bytes decoded, looks like text]"
            )
        );

        let short = "QUJD".repeat(63) + "QUJ";
        assert_eq!(short.len(), 255);
        let uri = format!("data:application/json;base64,{short}");
        let (out, count) = collapse(&uri);
        assert_eq!(count, 0);
        assert_eq!(out, uri);
    }

    #[test]
    fn keeps_long_hex_strings() {
        for hex in ["deadbeef".repeat(250), "0123456789abcdef".repeat(125)] {
            assert_eq!(hex.len(), 2000);
            let source = format!("sha={hex}");
            let (out, count) = collapse(&source);
            assert_eq!(count, 0);
            assert_eq!(out, source);
        }
    }

    #[test]
    fn recognises_png_and_gzip_magic_bytes() {
        let mut png_bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        png_bytes.resize(800, 0);
        let (out, count) = collapse(&encoded(&png_bytes));
        assert_eq!(count, 1);
        assert!(out.contains("looks like PNG]"), "{out}");

        let mut gzip_bytes = vec![0x1f, 0x8b, 0x08, 0x00];
        gzip_bytes.resize(800, 0);
        let (out, count) = collapse(&encoded(&gzip_bytes));
        assert_eq!(count, 1);
        assert!(out.contains("looks like gzip]"), "{out}");
    }

    #[test]
    fn url_safe_without_padding_is_recognised() {
        let blob = URL_SAFE_NO_PAD.encode([0xffu8; 800]);
        assert!(blob.contains('_'));
        assert!(!blob.contains('='));
        let (out, count) = collapse(&blob);
        assert_eq!(count, 1);
        assert!(out.contains("looks like binary]"), "{out}");
        assert_eq!(out.matches("base64 data omitted").count(), 1);
    }

    #[test]
    fn plain_text_is_unchanged() {
        let source = "fn main() {\n    println!(\"hello, world\");\n}\n";
        let (out, count) = collapse(source);
        assert_eq!(count, 0);
        assert_eq!(out, source);
    }
}
