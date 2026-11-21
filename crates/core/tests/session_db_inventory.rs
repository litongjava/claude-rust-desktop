//! Guard for SQLite access in tracked host, engine, service, and fixture source.
use std::{collections::BTreeMap, path::Path, process::Command};

const TERMS: [&str; 5] = [
    "with_conn",
    "DATABASE_FILE",
    "deepseek_code.db",
    "Connection::open",
    "peripheral_settings",
];
const SOURCE: [&str; 21] = [
    "*.rs", "*.sql", "*.ts", "*.tsx", "*.js", "*.jsx", "*.mjs", "*.cjs", "*.py", "*.sh", "*.ps1",
    "*.kt", "*.kts", "*.swift", "*.java", "*.c", "*.h", "*.cpp", "*.cs", "*.go", "*.vue",
];
const ALLOW: &str = include_str!("storage_inventory/allow.tsv");
const REMAINING: &str = include_str!("storage_inventory/pending.tsv");

fn inventory(root: &Path) -> BTreeMap<(String, String), usize> {
    // Use the index to select files, and the working tree for their current contents.
    let tracked = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "--"])
        .args(SOURCE)
        .output()
        .unwrap();
    assert!(tracked.status.success(), "git ls-files failed");
    let mut result = BTreeMap::new();
    for raw in tracked.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(raw).unwrap();
        if path.starts_with("crates/core/src/db/session_store/")
            || path == "crates/core/tests/session_db_inventory.rs"
        {
            continue;
        }
        let source = match std::fs::read(root.join(path)) {
            Ok(source) => source,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, // pending deletion
            Err(e) => panic!("{path}: {e}"),
        };
        for term in TERMS {
            let lines = source
                .split(|b| *b == b'\n')
                .filter(|line| line.windows(term.len()).any(|s| s == term.as_bytes()))
                .count();
            if lines != 0 {
                result.insert((path.to_owned(), term.to_owned()), lines);
            }
        }
    }
    result
}
fn limits(text: &str) -> BTreeMap<(String, String), usize> {
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|row| {
            let mut fields = row.split('\t');
            let path = fields.next().unwrap().to_owned();
            let term = fields.next().unwrap().to_owned();
            let count = fields.next().unwrap().parse().unwrap();
            ((path, term), count)
        })
        .collect()
}
#[test]
fn raw_store_access_is_accounted_for_in_tracked_source() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let actual = inventory(root);
    let allowed = limits(ALLOW);
    let pending = limits(REMAINING);
    let runtime = std::fs::read_to_string(root.join("crates/core/src/runtime.rs")).unwrap();
    let legacy_opener = runtime.contains(".canonicalize()?.join(DATABASE_FILE)");
    let mut violations = Vec::new();
    for (key, count) in actual {
        let max = allowed
            .get(&key)
            .or_else(|| legacy_opener.then(|| pending.get(&key)).flatten())
            .copied()
            .unwrap_or_default();
        if count > max {
            violations.push(format!(
                "{}: {} ({} lines, limit {})",
                key.0, key.1, count, max
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "Unaccounted storage access:\n{}",
        violations.join("\n")
    );
}
