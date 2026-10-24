use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;

/// Fails when avahi-daemon is down, when it serves this machine under a name
/// other than `hostname`, or when `hostname` does not resolve through it.
/// Avahi renames the host (`<name>-2.local`, `-3`, ...) when it sees a
/// conflicting record while probing, e.g. its own stale announcement after an
/// address change at boot; it never switches back by itself, so mDNS peers
/// (Nexus included) list the machine under the new name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Avahi {
    hostname: String,
    #[serde(default = "systemctl")]
    systemctl: String,
    #[serde(default = "busctl")]
    busctl: String,
    #[serde(default = "avahi_resolve_host_name")]
    avahi_resolve_host_name: String,
}
fn systemctl() -> String {
    "systemctl".into()
}
fn busctl() -> String {
    "busctl".into()
}
fn avahi_resolve_host_name() -> String {
    "avahi-resolve-host-name".into()
}

/// Parses `busctl call ... GetHostNameFqdn` output: `s "name.local"`.
fn parse_fqdn(text: &str) -> Result<String> {
    text.trim()
        .strip_prefix("s \"")
        .and_then(|s| s.strip_suffix('"'))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .with_context(|| format!("unexpected GetHostNameFqdn reply {:?}", text.trim()))
}

#[async_trait]
impl BitCheck for Avahi {
    fn remediation(&self) -> String {
        format!(
            "Avahi could not serve {host}.\n\
             - `systemctl status avahi-daemon`; restart it if inactive.\n\
             - Renamed (e.g. -2.local): Avahi saw a name conflict while probing, often \
             its own stale record after an address change at boot. \
             `sudo systemctl restart avahi-daemon` re-probes; if it renames again, another \
             LAN device claims {host} (`journalctl -u avahi-daemon | grep conflict`). \
             Then forget the stale entry in Nexus.\n\
             - `{resolve} -4 {host}` should print this machine's IP.\n\
             - Ensure the firewall allows mDNS (UDP 5353).",
            host = self.hostname,
            resolve = self.avahi_resolve_host_name,
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(
            self.hostname.ends_with(".local") && !self.hostname.starts_with('-'),
            "hostname must be an mDNS .local name"
        );
        let active = tokio::process::Command::new(&self.systemctl)
            .args(["is-active", "--quiet", "avahi-daemon.service"])
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| format!("running {}", self.systemctl))?;
        ensure!(active.success(), "avahi-daemon is not active");
        let output = tokio::process::Command::new(&self.busctl)
            .args([
                "--system",
                "call",
                "org.freedesktop.Avahi",
                "/",
                "org.freedesktop.Avahi.Server",
                "GetHostNameFqdn",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("running {}", self.busctl))?;
        if !output.status.success() {
            bail!(
                "querying Avahi's host name failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let served = parse_fqdn(&String::from_utf8_lossy(&output.stdout))?;
        ensure!(
            served.eq_ignore_ascii_case(&self.hostname),
            "Avahi serves this machine as {served}, not {} (mDNS host name conflict)",
            self.hostname
        );
        let resolved = tokio::process::Command::new(&self.avahi_resolve_host_name)
            .args(["-4", &self.hostname])
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| format!("running {}", self.avahi_resolve_host_name))?;
        ensure!(
            resolved.success(),
            "Avahi could not resolve {}",
            self.hostname
        );
        println!("Avahi serves and resolves {served}");
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Avahi>().await
}

#[cfg(test)]
mod tests {
    use super::parse_fqdn;

    #[test]
    fn parses_busctl_string_reply() {
        assert_eq!(parse_fqdn("s \"box-3.local\"\n").unwrap(), "box-3.local");
        assert!(parse_fqdn("s \"\"").is_err());
        assert!(parse_fqdn("garbage").is_err());
    }
}
