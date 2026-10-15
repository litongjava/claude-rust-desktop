//! Canonical multimodal user input shared by CLI persistence and model history.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

pub fn normalize_user_content(content: &Value) -> Result<Value> {
    if let Some(text) = content.as_str() {
        ensure!(!text.trim().is_empty(), "User message is empty");
        return Ok(content.clone());
    }
    let blocks = content
        .as_array()
        .context("User content must be text or a block array")?;
    ensure!(
        !blocks.is_empty() && blocks.len() <= 64,
        "User content requires 1..64 blocks"
    );
    ensure!(
        serde_json::to_vec(content)?.len() <= 8 * 1024 * 1024,
        "User content exceeds 8 MiB"
    );
    let mut result = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().context("Text block needs text")?;
                ensure!(
                    block
                        .as_object()
                        .unwrap()
                        .keys()
                        .all(|k| matches!(k.as_str(), "type" | "text")),
                    "Unsupported text block metadata"
                );
                result.push(json!({"type":"text","text":text}));
            }
            Some("image") | Some("image_url") => {
                let source_key = if block["type"] == "image" {
                    "source"
                } else {
                    "image_url"
                };
                ensure!(
                    block
                        .as_object()
                        .unwrap()
                        .keys()
                        .all(|k| k == "type" || k == source_key),
                    "Unsupported image block metadata"
                );
                let (mime, data) = if block["type"] == "image" {
                    let source = &block["source"];
                    ensure!(source["type"]=="base64","Only embedded base64 images are supported; remote image URLs are not fetched");
                    ensure!(
                        source
                            .as_object()
                            .context("image source must be an object")?
                            .keys()
                            .all(|k| matches!(k.as_str(), "type" | "media_type" | "data")),
                        "Unsupported image source metadata"
                    );
                    (
                        source["media_type"]
                            .as_str()
                            .context("Image needs media_type")?,
                        source["data"].as_str().context("Image needs base64 data")?,
                    )
                } else {
                    let image = &block["image_url"];
                    ensure!(
                        image
                            .as_object()
                            .context("image_url must be an object")?
                            .keys()
                            .all(|k| k == "url"),
                        "Image detail/extra image_url fields are not supported"
                    );
                    image["url"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("data:"))
                        .and_then(|s| s.split_once(";base64,"))
                        .context("image_url must contain an embedded base64 data URL")?
                };
                ensure!(
                    matches!(
                        mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ),
                    "Unsupported image media type"
                );
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .context("Invalid image base64")?;
                ensure!(
                    !decoded.is_empty() && decoded.len() <= 5 * 1024 * 1024,
                    "Image must contain 1 byte to 5 MiB"
                );
                let valid = match mime {
                    "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "image/jpeg" => decoded.starts_with(&[0xff, 0xd8, 0xff]),
                    "image/gif" => decoded.starts_with(b"GIF87a") || decoded.starts_with(b"GIF89a"),
                    "image/webp" => {
                        decoded.starts_with(b"RIFF") && decoded.get(8..12) == Some(b"WEBP")
                    }
                    _ => false,
                };
                ensure!(valid, "Image bytes do not match declared media type");
                result.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}));
            }
            other => bail!("Unsupported user content block: {other:?}"),
        }
    }
    ensure!(
        result.iter().any(|b| b["type"] == "image_url"
            || b["text"].as_str().is_some_and(|t| !t.trim().is_empty())),
        "User message is empty"
    );
    Ok(Value::Array(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_images_and_unhandled_content_are_rejected_without_dropping_fields() {
        for value in [
            json!([{"type":"image","source":{"type":"url","url":"https://example.invalid/p.png"}}]),
            json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"not-base64!"}}]),
            json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]),
            json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}}]),
            json!([{"type":"text","text":"hello","citations":[{}]}]),
            json!([{"type":"document","source":{}}]),
            json!([]),
        ] {
            assert!(normalize_user_content(&value).is_err(), "{value}");
        }
        assert_eq!(
            normalize_user_content(&json!("你好")).unwrap(),
            json!("你好")
        );
    }
}
