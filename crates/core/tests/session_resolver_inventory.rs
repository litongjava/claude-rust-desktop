//! Every new low-level selection needs an explicit admission or a recorded owner.
//! Include in-source fixtures and macro bodies so neither can hide a production call.
use regex::Regex;
use std::{collections::BTreeMap, path::Path};
use walkdir::WalkDir;

#[test]
fn logical_session_resolvers_have_reviewed_call_sites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let functions = Regex::new(r"\bfn\s+(\w+)").unwrap();
    let calls = Regex::new(r"\b(?:select_session|open_session|open_current_session|authorize_owned_attachment|authorize_attachment|authorize_job|worker_pair|owned_jobs|executing_in|router_pass|child_owner|recorded_child_jobs|job_execution_session|role\s*::\s*load|list)\s*\(").unwrap();
    let mut found = BTreeMap::new();
    for directory in ["crates/core/src", "crates/cli/src", "src-tauri/src"] {
        for entry in WalkDir::new(root.join(directory)) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let path = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let mut definitions = functions.captures_iter(&source).peekable();
            let mut function = "module";
            for call in calls.find_iter(&source) {
                while definitions
                    .peek()
                    .is_some_and(|d| d.get(0).unwrap().start() <= call.start())
                {
                    function = definitions.next().unwrap().get(1).unwrap().as_str();
                }
                if source[..call.start()].trim_end().ends_with("fn") {
                    continue;
                }
                let mut depth = 1;
                let arguments = &source[call.end()..];
                let Some(end) = arguments.char_indices().find_map(|(index, ch)| {
                    match ch {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(index)
                }) else {
                    continue;
                };
                let expression: String = source[call.start()..=call.end() + end]
                    .split_whitespace()
                    .collect();
                if expression.starts_with("list(")
                    && !(expression.ends_with(",None)")
                        || expression.ends_with(",None,)")
                        || expression == "list(None)"
                        || expression == "list(None,)")
                {
                    continue;
                }
                *found
                    .entry(format!("{path}\t{function}\t{expression}"))
                    .or_insert(0usize) += 1;
            }
        }
    }
    let actual = found
        .into_iter()
        .map(|(site, count)| format!("{count}\t{site}\n"))
        .collect::<String>();
    assert_eq!(
        actual,
        include_str!("storage_inventory/resolvers.tsv"),
        "Use the retained session or record why this entry selects the current occupant"
    );
}
