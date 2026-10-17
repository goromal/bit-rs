use anyhow::{ensure, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Avahi {
    hostname: String,
}
#[async_trait]
impl BitCheck for Avahi {
    async fn check(&self) -> Result<()> {
        ensure!(
            self.hostname.ends_with(".local") && !self.hostname.starts_with('-'),
            "hostname must be an mDNS .local name"
        );
        let active = tokio::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "avahi-daemon.service"])
            .kill_on_drop(true)
            .status()
            .await?;
        ensure!(active.success(), "avahi-daemon is not active");
        let resolved = tokio::process::Command::new("avahi-resolve-host-name")
            .args(["-4", &self.hostname])
            .kill_on_drop(true)
            .status()
            .await?;
        ensure!(
            resolved.success(),
            "Avahi could not resolve {}",
            self.hostname
        );
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Avahi>().await
}
