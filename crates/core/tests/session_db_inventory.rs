//! Guard for SQLite access in tracked host, engine, service, and fixture source.
use sha2::{Digest, Sha256};
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

fn inventory(root: &Path) -> BTreeMap<(String, String), (usize, String)> {
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
            let mut matching = source
                .split(|b| *b == b'\n')
                .filter(|line| line.windows(term.len()).any(|s| s == term.as_bytes()))
                .map(|line| std::str::from_utf8(line).unwrap().trim())
                .collect::<Vec<_>>();
            if !matching.is_empty() {
                matching.sort_unstable();
                let mut digest = Sha256::new();
                for line in &matching {
                    digest.update(line.as_bytes());
                    digest.update(b"\n");
                }
                result.insert(
                    (path.into(), term.into()),
                    (matching.len(), format!("{:x}", digest.finalize())),
                );
            }
        }
    }
    result
}
fn limits(text: &str) -> BTreeMap<(String, String), (usize, String)> {
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|row| {
            let mut fields = row.split('\t');
            let path = fields.next().unwrap().to_owned();
            let term = fields.next().unwrap().to_owned();
            let count = fields.next().unwrap().parse().unwrap();
            let digest = fields
                .next()
                .expect("Missing reviewed SQL fingerprint")
                .to_owned();
            assert!(
                fields
                    .next()
                    .is_some_and(|reason| !reason.trim().is_empty()),
                "Inventory entry lacks a reason"
            );
            ((path, term), (count, digest))
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
    let mut violations = Vec::new();
    for (key, fingerprint) in &actual {
        if allowed.get(key) != Some(fingerprint) {
            violations.push(format!(
                "{}: {} ({} matching lines)",
                key.0, key.1, fingerprint.0
            ));
        }
    }
    for key in allowed.keys().filter(|key| !actual.contains_key(*key)) {
        violations.push(format!(
            "Remove obsolete allowance for {}: {}",
            key.0, key.1
        ));
    }
    assert!(
        violations.is_empty(),
        "Unaccounted storage access:\n{}",
        violations.join("\n")
    );
}
