//! Project exactly one model-facing string, after the ordinary output renderer.
use crate::{performance::save_output_in, tools::base64_collapse};
use sha2::{Digest, Sha256};
use std::{fmt::Write, path::Path};

pub(crate) fn monitor_notification(text: &str) -> bool {
    text.starts_with(crate::tools::background_shell::NOTIFICATION_START)
        && text.lines().any(|line| line == "<kind>monitor</kind>")
}

/// None keeps the caller's original string (and avoids a copy on ordinary results).
pub(crate) fn project_for_model(
    name: &str,
    text: &str,
    root: Option<&Path>,
    session: &str,
) -> Option<String> {
    if !matches!(
        name,
        "Bash" | "TaskGet" | "TaskOutput" | "Monitor" | "ContextSearch"
    ) {
        return None;
    }
    let runs = base64_collapse::runs(text);
    if runs.is_empty() {
        return None;
    }
    let saved = root
        .map(Path::to_path_buf)
        .map_or_else(crate::runtime::resolve_default_data_dir, Ok)
        .and_then(|root| {
            let directory = std::path::absolute(root)?
                .join("tool-outputs")
                .join(format!("{:x}", Sha256::digest(session)));
            Ok(save_output_in(&directory, text.as_bytes())?)
        });
    let path = match saved {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "Could not save model output");
            return Some(format!(
                "[Could not present {name} output: unable to save the original text ({error})]"
            ));
        }
    };
    let saved_preview = text.lines().any(|line| {
        line.starts_with("Saved tool result (")
            && line.contains(" bytes, may already be truncated by the tool): ")
    });
    let mut view = String::new();
    let mut previous = 0;
    let mut line = 1;
    let mut bytes = 0;
    for run in &runs {
        let prefix = &text[previous..run.range.start];
        line += prefix.bytes().filter(|b| *b == b'\n').count();
        view.push_str(prefix);
        write!(
            view,
            "[base64 data omitted: about {} bytes decoded, looks like {}; full text: {}, line {}]",
            run.decoded_len,
            run.kind,
            path.display(),
            line
        )
        .unwrap();
        if saved_preview && adjacent_cut(&text[..run.range.start], &text[run.range.end..]) {
            view.push_str(" (partial at output cut)");
        }
        bytes += run.range.len();
        previous = run.range.end;
    }
    // Copy I6's existing saved-result path and omission summary verbatim.
    view.push_str(&text[previous..]);
    let count = runs.len();
    let blocks = if count == 1 { "block" } else { "blocks" };
    write!(
        view,
        "\nbase64: {count} {blocks} collapsed ({bytes} encoded bytes)"
    )
    .unwrap();
    Some(view)
}

fn adjacent_cut(before: &str, after: &str) -> bool {
    fn marker(line: &str) -> bool {
        line == "... (capture truncated; middle omitted) ..."
            || line
                .strip_prefix("... (output truncated; ")
                .and_then(|s| s.strip_suffix(" bytes before preview) ..."))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }
    before
        .strip_suffix('\n')
        .is_some_and(|s| marker(s.rsplit('\n').next().unwrap_or("")))
        || after
            .strip_prefix('\n')
            .is_some_and(|s| marker(s.split('\n').next().unwrap_or("")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_shell_thresholds_and_recovery_failure() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["Bash", "TaskGet", "TaskOutput", "Monitor"] {
            for (prefix, size) in [("", 1024), ("data:text/plain;base64,", 256)] {
                let full = format!("{prefix}{}", "Q".repeat(size));
                assert!(project_for_model(
                    name,
                    &full[..full.len() - 1],
                    Some(temp.path()),
                    "session"
                )
                .is_none());
                let projected =
                    project_for_model(name, &full, Some(temp.path()), "session").unwrap();
                assert!(projected.contains("base64: 1 block collapsed"));
                let blocked = temp.path().join("file");
                std::fs::write(&blocked, "ordinary file").unwrap();
                let failure = project_for_model(name, &full, Some(&blocked), "session").unwrap();
                assert!(failure.contains("unable to save"));
                assert!(!failure.contains("full text:"));
                assert!(!failure.contains(&full));
            }
        }
        for name in ["Read", "Edit"] {
            assert!(
                project_for_model(name, &"Q".repeat(1024), Some(temp.path()), "session").is_none()
            );
        }
        let bash = crate::tools::get_tool_definitions()
            .into_iter()
            .find(|t| t.name == "Bash")
            .unwrap();
        assert!(bash.input_schema["properties"].get("raw").is_none());
    }
}
