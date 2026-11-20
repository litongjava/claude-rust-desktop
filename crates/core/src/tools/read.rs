//! Read, aligned with the reference CLI: text with line numbers, images as
//! model image content, PDFs by page range and notebooks as cells.
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Text over this size needs offset/limit or allow_large.
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_PDF_PAGES: usize = 20;
const PDF_PAGES_REQUIRED_OVER: usize = 10;

pub fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "Read".into(),
        description: "Read a file and return its text with 1-based line numbers. Text reads 2000 lines from line 1 by default; offset and limit select another range, and rendered output over 256 KiB is refused unless allow_large is true. PNG, JPEG, GIF and WebP images up to 5 MiB return as image content when the model accepts it; PDFs return page text for the pages range; .ipynb files return cells with outputs. A missing path or directory is an error, and an empty file returns only a warning.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {"description": "Path to the file; relative paths resolve against the working directory.", "type": "string"},
                "offset": {"description": "First line to return, 1-based (0 is treated as 1); defaults to 1.", "type": "integer", "minimum": 0},
                "limit": {"description": "Maximum lines to return; defaults to 2000 and must be greater than 0.", "type": "integer", "exclusiveMinimum": 0},
                "pages": {"description": "PDF page range such as \"2\" or \"3-7\", at most 20 pages, required when the PDF has more than 10 pages.", "type": "string"},
                "allow_large": {"description": "Allow text output over the 256 KiB limit; use only when the whole file is really needed.", "type": "boolean"}
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    }
}

pub fn execute(input: Value, cwd: &str) -> Result<Value> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or_else(|| anyhow!("file_path required"))?;
    if let Some(bytes) = crate::skills::bundled_resources::read(Path::new(file_path)) {
        return text(std::str::from_utf8(&bytes)?, &input);
    }
    let path = if super::screenshot_file::readable(Path::new(file_path)) {
        Path::new(file_path)
            .canonicalize()?
            .to_string_lossy()
            .into_owned()
    } else {
        super::resolve_read_path(file_path, cwd)?
    };
    let path = Path::new(&path);
    if !path.exists() {
        return Ok(
            json!({"content": format!("File not found: {}", path.display()), "is_error": true}),
        );
    }
    if path.is_dir() {
        return Ok(
            json!({"content": format!("{} is a directory, not a file. Use Bash `ls` or Glob to list it.", path.display()), "is_error": true}),
        );
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if let Some(media_type) = image_media_type(&extension) {
        return image(path, media_type);
    }
    let pages = optional_pages(&input)?;
    if extension == "pdf" {
        return pdf(path, pages);
    }
    anyhow::ensure!(pages.is_none(), "pages applies only to PDF files; omit pages for text files and use offset/limit for line ranges");
    let content = std::fs::read_to_string(path)
        .map_err(|error| anyhow!("Cannot read {} as UTF-8 text: {error}", path.display()))?;
    if extension == "ipynb" {
        if let Ok(notebook) = serde_json::from_str::<Value>(&content) {
            return Ok(
                json!({"content": notebook_cells(&notebook), "cells": notebook["cells"].as_array().map_or(0, Vec::len)}),
            );
        }
    }
    text(&content, &input)
}

// Empty optional strings from model adapters are placeholders, not page ranges.
fn optional_pages(input: &Value) -> Result<Option<&str>> {
    match input.get("pages") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim()).filter(|value| !value.is_empty())),
        _ => bail!("pages must be a PDF page-range string or null"),
    }
}

fn text(content: &str, input: &Value) -> Result<Value> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if content.is_empty() {
        return Ok(
            json!({"content": "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>", "lines": 0, "truncated": false}),
        );
    }
    // Line numbers are 1-based; 0 is accepted as the first line like the reference.
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
    anyhow::ensure!(limit > 0, "limit must be positive");
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if offset > total_lines {
        return Ok(
            json!({"content": format!("<system-reminder>Warning: the file has {total_lines} lines; offset {offset} is past the end.</system-reminder>"), "lines": total_lines, "truncated": false}),
        );
    }
    let selected: Vec<String> = lines
        .iter()
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(i, line)| format!("{:>6}\t{}", i + 1, line))
        .collect();
    let rendered = selected.join("\n");
    if rendered.len() > MAX_TEXT_BYTES && input["allow_large"].as_bool() != Some(true) {
        bail!("File content ({} KB) exceeds the {} KB read limit. Read it in parts with offset and limit, search it with Grep, or pass allow_large=true if you genuinely need all of it.", rendered.len() / 1024, MAX_TEXT_BYTES / 1024);
    }
    Ok(
        json!({"content": rendered, "lines": total_lines, "truncated": offset - 1 + selected.len() < total_lines}),
    )
}

fn image_media_type(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The tool loop turns `image` into model image content when the model accepts it.
fn image(path: &Path, media_type: &str) -> Result<Value> {
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "Image is {} MB; the limit is {} MB",
        bytes.len() / 1024 / 1024,
        MAX_IMAGE_BYTES / 1024 / 1024
    );
    use base64::Engine;
    Ok(json!({
        "content": format!("Image file {} ({media_type}, {} bytes)", path.display(), bytes.len()),
        "image": {"media_type": media_type, "data": base64::engine::general_purpose::STANDARD.encode(&bytes)}
    }))
}

fn pdf(path: &Path, pages: Option<&str>) -> Result<Value> {
    let bytes = std::fs::read(path)?;
    let texts = pdf_extract::extract_text_from_mem_by_pages(&bytes)
        .map_err(|error| anyhow!("Cannot extract PDF text from {}: {error}", path.display()))?;
    let total = texts.len();
    let (first, last) = match pages {
        Some(range) => parse_pages(range, total)?,
        None => {
            anyhow::ensure!(total <= PDF_PAGES_REQUIRED_OVER, "This PDF has {total} pages; pass pages (for example \"1-5\", at most {MAX_PDF_PAGES} pages per request).");
            (1, total.max(1))
        }
    };
    let content = (first..=last.min(total))
        .map(|page| format!("--- Page {page} ---\n{}", texts[page - 1].trim_end()))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(json!({"content": content, "pages": total}))
}

fn parse_pages(range: &str, total: usize) -> Result<(usize, usize)> {
    let parse = |value: &str| {
        value
            .trim()
            .parse::<usize>()
            .map_err(|_| anyhow!("Invalid pages value: {range}"))
    };
    let (first, last) = match range.split_once('-') {
        Some((first, last)) => (parse(first)?, parse(last)?),
        None => {
            let page = parse(range)?;
            (page, page)
        }
    };
    anyhow::ensure!(first >= 1 && first <= last, "Invalid pages range: {range}");
    anyhow::ensure!(
        last - first < MAX_PDF_PAGES,
        "At most {MAX_PDF_PAGES} pages per request"
    );
    anyhow::ensure!(first <= total, "The PDF has {total} pages");
    Ok((first, last))
}

fn source(value: &Value) -> String {
    match value {
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
        Value::String(text) => text.clone(),
        _ => String::new(),
    }
}

fn notebook_cells(notebook: &Value) -> String {
    let mut out = Vec::new();
    for (index, cell) in notebook["cells"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let id = cell["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("cell-{index}"));
        let mut text = format!(
            "<cell id=\"{id}\" type=\"{}\">\n{}\n</cell>",
            cell["cell_type"].as_str().unwrap_or("code"),
            source(&cell["source"])
        );
        for output in cell["outputs"].as_array().into_iter().flatten() {
            let body = if output["text"].is_null() {
                source(&output["data"]["text/plain"])
            } else {
                source(&output["text"])
            };
            let body = if body.is_empty() && !output["data"].is_null() {
                "[non-text output]".to_owned()
            } else {
                body
            };
            if !body.is_empty() {
                text.push_str(&format!("\n<output>\n{}\n</output>", body.trim_end()));
            }
        }
        out.push(text);
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ranges_notebooks_images_and_reports_limits() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = execute(json!({"file_path":"a.txt","offset":2,"limit":1}), cwd).unwrap();
        assert_eq!(out["content"], "     2\ttwo");
        assert_eq!(out["truncated"], true);
        assert_eq!(
            execute(json!({"file_path":"a.txt","offset":0}), cwd).unwrap()["content"],
            "     1\tone\n     2\ttwo\n     3\tthree"
        );

        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        assert!(
            execute(json!({"file_path":"empty.txt"}), cwd).unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("empty")
        );
        assert_eq!(
            execute(json!({"file_path":"."}), cwd).unwrap()["is_error"],
            true
        );

        let big = "x".repeat(1000) + "\n";
        std::fs::write(dir.path().join("big.txt"), big.repeat(400)).unwrap();
        assert!(execute(json!({"file_path":"big.txt"}), cwd)
            .unwrap_err()
            .to_string()
            .contains("allow_large"));
        assert_eq!(
            execute(json!({"file_path":"big.txt","allow_large":true}), cwd).unwrap()["lines"],
            400
        );

        std::fs::write(dir.path().join("n.ipynb"), r#"{"cells":[{"id":"c1","cell_type":"code","source":["print(1)\n"],"outputs":[{"output_type":"stream","text":["1\n"]}]}]}"#).unwrap();
        let notebook = execute(json!({"file_path":"n.ipynb"}), cwd).unwrap();
        assert!(notebook["content"].as_str().unwrap().contains(
            "<cell id=\"c1\" type=\"code\">\nprint(1)\n\n</cell>\n<output>\n1\n</output>"
        ));

        std::fs::write(dir.path().join("p.png"), b"\x89PNG").unwrap();
        let image = execute(json!({"file_path":"p.png"}), cwd).unwrap();
        assert_eq!(image["image"]["media_type"], "image/png");
        assert_eq!(image["image"]["data"], "iVBORw==");
        assert!(execute(json!({"file_path":"a.txt","pages":"1"}), cwd).is_err());
    }

    #[test]
    fn empty_page_placeholders_do_not_block_text_or_notebook_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("guide.md"), "# 中文指南\n正文").unwrap();
        std::fs::write(
            dir.path().join("notebook.ipynb"),
            r#"{"cells":[{"cell_type":"markdown","source":["中文"]}]}"#,
        )
        .unwrap();
        for pages in [Value::Null, json!(""), json!(" \t\r\n")] {
            for file in ["guide.md", "notebook.ipynb"] {
                let result = execute(json!({"file_path":file,"pages":pages}), cwd).unwrap();
                assert!(result["content"].as_str().unwrap().contains("中文"));
            }
            assert_eq!(optional_pages(&json!({"pages":pages})).unwrap(), None);
        }
        assert_eq!(
            optional_pages(&json!({"pages":" 2-4 "})).unwrap(),
            Some("2-4")
        );
        assert!(execute(json!({"file_path":"guide.md","pages":"1-2"}), cwd).is_err());
        assert!(optional_pages(&json!({"pages":3})).is_err());
    }

    #[test]
    fn page_ranges_are_bounded() {
        assert_eq!(parse_pages("3", 5).unwrap(), (3, 3));
        assert_eq!(parse_pages("2-4", 5).unwrap(), (2, 4));
        assert!(parse_pages("1-21", 30).is_err());
        assert!(parse_pages("6", 5).is_err());
        assert!(parse_pages("4-2", 5).is_err());
    }
}
