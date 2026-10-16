//! Executable plugin ABI v1: one JSON configuration on stdin, logs on stdout,
//! exit 0 for pass and nonzero for failure. The daemon enforces the deadline.
use anyhow::Result;
use async_trait::async_trait;
use serde::de::DeserializeOwned;

#[async_trait]
pub trait BitCheck: DeserializeOwned + Send + Sync {
    async fn check(&self) -> Result<()>;
}
pub async fn run<C: BitCheck>() -> Result<()> {
    let config: C = serde_json::from_reader(std::io::stdin())?;
    println!("check started");
    match config.check().await {
        Ok(()) => {
            println!("PASS");
            Ok(())
        }
        Err(e) => {
            println!("FAIL: {e:#}");
            Err(e)
        }
    }
}
