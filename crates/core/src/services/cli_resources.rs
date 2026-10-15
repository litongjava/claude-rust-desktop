//! Public resource links and cached reference release notes. No account data is sent.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

pub const RADIO: &str = "https://clau.de/radio";
pub const STICKERS: &str = "https://www.stickermule.com/claudecode";
pub const IOS: &str = "https://apps.apple.com/app/claude-by-anthropic/id6473753684";
pub const ANDROID: &str = "https://play.google.com/store/apps/details?id=com.anthropic.claude";
pub const CHANGELOG: &str =
    "https://raw.githubusercontent.com/anthropics/claude-code/refs/heads/main/CHANGELOG.md";
const NOTES_LIMIT: usize = 2 * 1024 * 1024;

pub fn open_link(url: &str) -> Result<()> {
    ensure!(
        [RADIO, STICKERS, IOS, ANDROID].contains(&url),
        "Unknown resource link"
    );
    open::that_detached(url).context("Opening browser")
}

pub fn mobile_qr(android: bool) -> Result<String> {
    let url = if android { ANDROID } else { IOS };
    let qr = qrcode::QrCode::new(url.as_bytes())?;
    Ok(qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

#[derive(Serialize, Deserialize)]
struct CachedNotes {
    fetched_at: String,
    text: String,
}

pub struct ReleaseNotes {
    pub text: String,
    pub fetched_at: String,
    pub cached: bool,
}

pub async fn release_notes(data_dir: &Path, refresh: bool) -> Result<ReleaseNotes> {
    release_notes_from(data_dir, refresh, CHANGELOG).await
}

async fn release_notes_from(
    data_dir: &Path,
    refresh: bool,
    endpoint: &str,
) -> Result<ReleaseNotes> {
    let cache_path = data_dir.join("reference-release-notes.json");
    let cached = (|| -> Result<CachedNotes> {
        ensure!(
            cache_path.metadata()?.len() <= (NOTES_LIMIT * 6 + 65536) as u64,
            "Release cache is too large"
        );
        let notes: CachedNotes = serde_json::from_slice(&std::fs::read(&cache_path)?)?;
        ensure!(
            notes.text.len() <= NOTES_LIMIT && notes.text.starts_with('#'),
            "Invalid release cache content"
        );
        chrono::DateTime::parse_from_rfc3339(&notes.fetched_at)?;
        Ok(notes)
    })()
    .ok();
    let deadline = if cached.is_some() && !refresh {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(10)
    };
    let fresh = tokio::time::timeout(deadline, async {
        let mut response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .get(endpoint)
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response.status().is_success(),
            "Unexpected release notes status"
        );
        ensure!(
            response.content_length().unwrap_or(0) <= NOTES_LIMIT as u64,
            "Release notes are too large"
        );
        let mut content = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                content.len() + chunk.len() <= NOTES_LIMIT,
                "Release notes are too large"
            );
            content.extend_from_slice(&chunk);
        }
        let text = String::from_utf8(content)?;
        ensure!(text.starts_with('#'), "Release notes are not Markdown");
        Ok::<_, anyhow::Error>(CachedNotes {
            text,
            fetched_at: chrono::Utc::now().to_rfc3339(),
        })
    })
    .await;
    match fresh {
        Ok(Ok(notes)) => {
            let saved = (|| -> Result<()> {
                std::fs::create_dir_all(data_dir)?;
                let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
                std::io::Write::write_all(&mut file, &serde_json::to_vec(&notes)?)?;
                file.persist(&cache_path)?;
                Ok(())
            })();
            if saved.is_err() {
                tracing::warn!("Fetched release notes could not be cached; displaying fresh notes");
            }
            Ok(ReleaseNotes {
                text: notes.text,
                fetched_at: notes.fetched_at,
                cached: false,
            })
        }
        failure => match cached {
            Some(notes) => Ok(ReleaseNotes {
                text: notes.text,
                fetched_at: notes.fetched_at,
                cached: true,
            }),
            None => {
                failure.context("Release notes request timed out")??;
                unreachable!()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_notes_survive_unwritable_cache_location() {
        let data = tempfile::tempdir().unwrap();
        let occupied = data.path().join("not-a-directory");
        std::fs::write(&occupied, "preserve").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/", axum::routing::get(|| async { "# Fresh notes" })),
            )
            .await
            .unwrap();
        });
        let notes = release_notes_from(&occupied, true, &endpoint)
            .await
            .unwrap();
        assert!(!notes.cached);
        assert_eq!(notes.text, "# Fresh notes");
        assert_eq!(std::fs::read_to_string(occupied).unwrap(), "preserve");
        server.abort();
    }

    #[tokio::test]
    async fn release_cache_survives_network_failure() {
        let data = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/",
                    axum::routing::get(|| async { "# 2.1\n\nExample release" }),
                ),
            )
            .await
            .unwrap();
        });
        let first = release_notes_from(data.path(), false, &endpoint)
            .await
            .unwrap();
        assert!(!first.cached);
        server.abort();
        let _ = server.await;
        let fallback = release_notes_from(data.path(), false, &endpoint)
            .await
            .unwrap();
        assert!(fallback.cached);
        assert_eq!(fallback.text, first.text);
        assert_eq!(fallback.fetched_at, first.fetched_at);
    }
}
