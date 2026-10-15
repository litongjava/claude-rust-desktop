//! Local read-only checks. Never spawns PATH executables or reads credential contents.
use crate::runtime::RuntimePaths;
use serde::Serialize;
use std::{io::IsTerminal, path::Path};

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: String,
    pub detail: String,
}
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}
impl DoctorReport {
    pub fn render(&self) -> String {
        self.checks
            .iter()
            .map(|c| format!("{}: {} — {}\n", c.name, c.status, c.detail))
            .collect()
    }
    fn add(&mut self, name: &str, status: &str, detail: &str) {
        self.checks.push(DoctorCheck {
            name: name.into(),
            status: status.into(),
            detail: detail.into(),
        });
    }
    fn json(&mut self, name: &str, path: &Path) {
        if !path.exists() {
            self.add(name, "absent", "Optional configuration file is not present");
            return;
        }
        // Avoid errors that can contain raw input or arbitrary values from secrets.
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() <= 4 * 1024 * 1024 => {}
            Ok(_) => {
                self.add(
                    name,
                    "warning",
                    "Configuration exceeds 4 MiB; parsing skipped",
                );
                return;
            }
            Err(_) => {
                self.add(name, "error", "Cannot inspect configuration metadata");
                return;
            }
        }
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) if value.is_object() || value.is_array() => self.add(
                    name,
                    "ok",
                    "Valid JSON container (schema and credentials not verified)",
                ),
                Ok(_) => self.add(name, "error", "Expected a JSON object or array"),
                Err(error) => self.add(
                    name,
                    "error",
                    &format!(
                        "Invalid JSON at line {}, column {}",
                        error.line(),
                        error.column()
                    ),
                ),
            },
            Err(_) => self.add(name, "error", "Cannot read configuration as UTF-8"),
        }
    }
}

pub fn inspect(workspace: &Path, paths: Option<&RuntimePaths>) -> DoctorReport {
    let mut report = DoctorReport { checks: Vec::new() };
    report.add(
        "runtime",
        "ok",
        &format!(
            "claude-core {} on {} / {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    );
    if workspace.is_dir() && std::fs::read_dir(workspace).is_ok() {
        report.add("workspace", "ok", "Directory exists and can be listed");
    } else {
        report.add("workspace", "error", "Directory is missing or unreadable");
    }
    report.add(
        "terminal",
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            "ok"
        } else {
            "info"
        },
        "Interactive terminal requires both stdin and stdout to be terminals",
    );
    report.json("project settings", &workspace.join(".claude/settings.json"));
    report.json(
        "local settings",
        &workspace.join(".claude/settings.local.json"),
    );
    report.json("project MCP", &workspace.join(".mcp.json"));
    if let Some(paths) = paths {
        report.json("application config", &paths.data_dir.join("config.json"));
        report.json("providers", &paths.data_dir.join("providers.json"));
        report.json(
            "application settings",
            &paths.data_dir.join("settings.json"),
        );
        let db = paths.data_dir.join("claude_desktop.db");
        if !db.exists() {
            report.add(
                "database",
                "absent",
                "No shared database found; no file was created",
            );
        } else {
            let check = rusqlite::Connection::open_with_flags(
                &db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .and_then(|connection| {
                connection.busy_timeout(std::time::Duration::from_millis(250))?;
                connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
            });
            match check {
                Ok(result) if result == "ok" => {
                    report.add("database", "ok", "SQLite read-only quick_check passed")
                }
                Ok(_) => report.add(
                    "database",
                    "error",
                    "SQLite integrity check reported a problem",
                ),
                Err(_) => report.add(
                    "database",
                    "error",
                    "SQLite read-only check failed or database is busy",
                ),
            }
        }
        report.add(
            "OAuth credential file",
            if paths.data_dir.join("anthropic-oauth.dat").is_file() {
                "present"
            } else {
                "absent"
            },
            "Presence only; contents and remote validity were not checked",
        );
    } else {
        report.add(
            "application data",
            "not checked",
            "Application data path was not supplied to this workspace snapshot",
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_configuration_is_redacted_and_inspection_does_not_create_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let config = paths.data_dir.join("config.json");
        let secret = "{\"apiKey\":\"DO_NOT_ECHO_THIS_SECRET\", broken}";
        std::fs::write(&config, secret).unwrap();
        let report = inspect(temp.path(), Some(&paths));
        let rendered = report.render();
        assert!(rendered.contains("application config: error"));
        assert!(!rendered.contains("DO_NOT_ECHO_THIS_SECRET"));
        assert!(!paths.data_dir.join("claude_desktop.db").exists());
        assert_eq!(std::fs::read_to_string(config).unwrap(), secret);
    }
    #[test]
    fn actual_database_integrity_is_checked() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().to_owned()).unwrap();
        let connection = rusqlite::Connection::open(temp.path().join("claude_desktop.db")).unwrap();
        connection
            .execute("CREATE TABLE fixture (id INTEGER)", [])
            .unwrap();
        drop(connection);
        assert!(inspect(temp.path(), Some(&paths))
            .render()
            .contains("database: ok"));
    }
}
