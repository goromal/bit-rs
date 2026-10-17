//! Executable plugin ABI v2: one JSON configuration on stdin, logs on stdout,
//! exit 0 for pass and nonzero for failure. The daemon enforces the deadline.
//! Invoked with `--remediation`, a plugin instead prints its suggestions for a
//! failing check (given the same configuration) and exits 0 without checking.
use anyhow::Result;
use async_trait::async_trait;
use serde::de::DeserializeOwned;

pub const REMEDIATION_FLAG: &str = "--remediation";

#[async_trait]
pub trait BitCheck: DeserializeOwned + Send + Sync {
    async fn check(&self) -> Result<()>;
    /// What an operator should try when this check fails, in plain text.
    fn remediation(&self) -> String;
}
pub async fn run<C: BitCheck>() -> Result<()> {
    let config: C = serde_json::from_reader(std::io::stdin())?;
    if std::env::args().nth(1).as_deref() == Some(REMEDIATION_FLAG) {
        print!("{}", config.remediation());
        return Ok(());
    }
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
