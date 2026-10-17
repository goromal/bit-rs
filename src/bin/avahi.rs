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
    fn remediation(&self) -> String {
        format!(
            "Avahi could not serve {host}.\n\
             - `systemctl status avahi-daemon`; restart it if inactive.\n\
             - `avahi-resolve-host-name -4 {host}` should print this machine's IP.\n\
             - A hostname clash on the LAN renames this host (e.g. -2.local); check \
             `journalctl -u avahi-daemon`.\n\
             - Ensure the firewall allows mDNS (UDP 5353).",
            host = self.hostname
        )
    }
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
