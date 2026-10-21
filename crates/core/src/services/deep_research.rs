//! Typed, durable state for the explicitly invoked deep-research workflow.
//! Ported from the normally materialized Claude Code 2.1.295 workflow.
use super::ServiceContext;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PARALLELISM: usize = 4;
pub const MAX_SOURCES: usize = 15;
pub const MAX_CLAIMS: usize = 25;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Angle {
    pub label: String,
    pub query: String,
    #[serde(default)]
    pub rationale: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scope {
    pub angles: Vec<Angle>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub snippet: String,
    pub relevance: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Search {
    pub results: Vec<SearchHit>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub claim: String,
    pub quote: String,
    pub importance: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extraction {
    pub source_quality: String,
    #[serde(default)]
    pub publish_date: String,
    pub claims: Vec<Claim>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub title: String,
    pub angle: String,
    pub extraction: Extraction,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vote {
    pub assessed: bool,
    pub refuted: bool,
    pub confidence: String,
    pub evidence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Adjudication {
    Confirmed,
    Refuted,
    Unverified,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Panel {
    pub claim: Claim,
    pub source: String,
    pub source_quality: String,
    pub votes: Vec<Vote>,
    pub errors: Vec<String>,
    pub outcome: Adjudication,
}
impl Panel {
    pub fn adjudicate(&mut self) {
        self.outcome = if self
            .votes
            .iter()
            .filter(|v| v.assessed && v.refuted)
            .count()
            >= 2
        {
            Adjudication::Refuted
        } else if self
            .votes
            .iter()
            .filter(|v| v.assessed && !v.refuted)
            .count()
            >= 2
        {
            Adjudication::Confirmed
        } else {
            Adjudication::Unverified
        };
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub finding: String,
    pub confidence: String,
    pub sources: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub summary: String,
    pub findings: Vec<Finding>,
    pub caveats: Vec<String>,
    pub open_questions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Partial,
    Inconclusive,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskResult {
    pub phase: String,
    pub label: String,
    pub child_session_id: Option<String>,
    pub output: Option<Value>,
    pub web_evidence: Vec<Value>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub parent_session_id: String,
    pub question: String,
    pub phase: String,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub tasks: Vec<TaskResult>,
    pub sources: Vec<Source>,
    pub panels: Vec<Panel>,
    pub report: Option<Report>,
    pub errors: Vec<String>,
    pub url_duplicates: usize,
    pub dropped_sources: usize,
}
pub struct RunGuard {
    pub run: Run,
    context: ServiceContext,
}
impl RunGuard {
    pub fn start(context: ServiceContext, session: &str, question: &str) -> Result<Self> {
        let value = Self {
            context,
            run: Run {
                id: format!("research-{}", uuid::Uuid::new_v4()),
                parent_session_id: session.into(),
                question: question.into(),
                phase: "Scope".into(),
                status: RunStatus::Running,
                started_at: chrono::Utc::now().to_rfc3339(),
                finished_at: None,
                tasks: vec![],
                sources: vec![],
                panels: vec![],
                report: None,
                errors: vec![],
                url_duplicates: 0,
                dropped_sources: 0,
            },
        };
        value.save()?;
        Ok(value)
    }
    pub fn save(&self) -> Result<()> {
        self.context
            .save_setting(&format!("workflow.{}", self.run.id), &self.run)
    }
    pub fn finish(&mut self, status: RunStatus) -> Result<()> {
        self.run.status = status;
        self.run.finished_at = Some(chrono::Utc::now().to_rfc3339());
        self.save()
    }
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.run.status == RunStatus::Running {
            self.run.status = RunStatus::Cancelled;
            self.run.finished_at = Some(chrono::Utc::now().to_rfc3339());
            if let Err(error) = self.save() {
                tracing::warn!("Unable to persist cancelled research: {error}");
            }
        }
    }
}

/// Canonical URL for deduplication; actual tool fetch keeps the original URL.
pub fn canonical_url(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(|c| c.is_control() || is_invisible(c)),
        "URL contains control or directional characters"
    );
    let mut url = reqwest::Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "https" | "http") && url.host_str().is_some(),
        "Only HTTP(S) research sources are supported"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Credential-bearing source URL rejected"
    );
    url.set_fragment(None);
    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !matches!(k.as_ref(), "fbclid" | "gclid"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    query.sort();
    url.set_query(None);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    if url.path() != "/" {
        let trimmed = url.path().trim_end_matches('/').to_string();
        url.set_path(&trimmed);
    }
    Ok(url.to_string())
}
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}
pub fn label(text: &str) -> String {
    let clean: String = text
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .take(60)
        .collect();
    // Always quoted: a shortened or IDN label must not masquerade as a literal host.
    format!(
        "{}{}",
        serde_json::to_string(&clean).unwrap_or_default(),
        if clean.chars().count() < text.chars().count() {
            "…"
        } else {
            ""
        }
    )
}
pub fn validate_report(report: &Report, panels: &[Panel]) -> Result<()> {
    let allowed: std::collections::HashSet<&str> = panels
        .iter()
        .filter(|p| p.outcome == Adjudication::Confirmed)
        .map(|p| p.source.as_str())
        .collect();
    for finding in &report.findings {
        ensure!(
            !finding.sources.is_empty(),
            "Synthesis finding has no cited source"
        );
        for source in &finding.sources {
            ensure!(
                allowed.contains(source.as_str()),
                "Synthesis cited an unverified or invented source: {}",
                label(source)
            );
        }
    }
    // Citation URLs cannot be smuggled into prose instead of the sources field.
    let prose = std::iter::once(report.summary.as_str())
        .chain(report.findings.iter().map(|f| f.finding.as_str()))
        .chain(report.caveats.iter().map(String::as_str))
        .chain(report.open_questions.iter().map(String::as_str));
    let pattern = regex::Regex::new(r#"https?://[^\s<>\"\)\]]+"#)?;
    for text in prose {
        for url in pattern.find_iter(text) {
            ensure!(
                allowed.contains(url.as_str().trim_end_matches(['.', ',', ';'])),
                "Synthesis prose contains an unverified URL"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_urls_and_quoted_labels() {
        assert_eq!(
            canonical_url("https://EXAMPLE.com/a/?utm_source=x&b=2&a=1#x").unwrap(),
            "https://example.com/a?a=1&b=2"
        );
        assert!(canonical_url("file:///tmp/a").is_err());
        assert!(canonical_url("https://user:secret@example.com").is_err());
        assert!(canonical_url("https://exam\u{202e}ple.com").is_err());
        assert!(!label("x\x1b[31m\u{202e}y").contains('\x1b'));
        assert!(label(&"a".repeat(100)).ends_with('…'));
    }
    #[test]
    fn missing_votes_are_not_refutations() {
        let mut p = Panel {
            claim: Claim {
                claim: "c".into(),
                quote: "q".into(),
                importance: "central".into(),
            },
            source: "https://example.com".into(),
            source_quality: "primary".into(),
            votes: vec![],
            errors: vec!["API failed".into()],
            outcome: Adjudication::Confirmed,
        };
        p.adjudicate();
        assert_eq!(p.outcome, Adjudication::Unverified);
        p.votes = vec![
            Vote {
                assessed: true,
                refuted: false,
                confidence: "high".into(),
                evidence: "e".into()
            };
            2
        ];
        p.adjudicate();
        assert_eq!(p.outcome, Adjudication::Confirmed);
        p.votes[0].refuted = true;
        p.adjudicate();
        assert_eq!(p.outcome, Adjudication::Unverified); // Split vote + missing third vote.
        p.votes[1].refuted = true;
        p.adjudicate();
        assert_eq!(p.outcome, Adjudication::Refuted);
    }
}
