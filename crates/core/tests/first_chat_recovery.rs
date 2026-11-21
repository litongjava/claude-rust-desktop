use anyhow::{ensure, Result};
use deepseek_code_core::db::session_store::{Phase, Stores};
use serde_json::json;

#[test]
fn first_chat_recovery() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let fault = stores.with_fault_hook(|p| {
        if p == Phase::BeforePublishCommit {
            return Err(std::io::Error::other("simulated recoverable publish I/O failure").into());
        }
        Ok(())
    });
    let content = json!("first turn");
    let result = fault.admit_chat_execution("publication", "model", Some(&content));
    let error = result.err().expect("first admission should fail");
    println!("First admission returned: {error:#}");
    assert_eq!(stores.conversation_detail("publication")?.1.len(), 1);
    let (lease, sealed) = stores.admit_chat_execution("publication", "model", Some(&content))?;
    if !sealed {
        stores.admit_chat_message("publication", "model", Some(&content))?;
    }
    drop(lease);
    let (_, messages) = stores.conversation_detail("publication")?;
    let texts: Vec<_> = messages.iter().map(|m| &m["content"]).collect();
    println!("Retry sealed_first={sealed}, messages={texts:?}");
    ensure!(messages.len() == 1 && sealed);
    let (_lease, sealed_again) =
        stores.admit_chat_execution("publication", "model", Some(&content))?;
    assert!(!sealed_again);
    Ok(())
}
