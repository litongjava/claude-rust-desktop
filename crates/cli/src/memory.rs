use crate::args::MemoryCommands;
use anyhow::{ensure, Result};
use claude_core::{memory::MemExClient, runtime::RuntimePaths};

pub async fn run(paths: &RuntimePaths, action: &MemoryCommands) -> Result<()> {
    let client = MemExClient::with_path(paths.data_dir.join("memory.sqlite3"), None);
    match action {
        MemoryCommands::Status => crate::services::print(&client.stats().await?),
        MemoryCommands::Clear { yes } => {
            ensure!(
                *yes,
                "Pass --yes to clear personal memory; conversations and team memory are retained"
            );
            client.clear().await?;
            crate::services::print(&serde_json::json!({"cleared": true}))
        }
        MemoryCommands::Export { output } => {
            let archive = client.export_archive().await?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            file.write_all(&serde_json::to_vec_pretty(&archive)?)?;
            file.sync_all()?;
            crate::services::print(
                &serde_json::json!({"exported": archive.memories.len(), "output": output}),
            )
        }
        MemoryCommands::Import { input, preview } => {
            let value = serde_json::from_slice(&std::fs::read(input)?)?;
            crate::services::print(&client.import_archive(value, *preview).await?)
        }
    }
}
