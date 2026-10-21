use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::path::Path;

/// Fails when an SSH host that unattended jobs depend on (e.g. github.com for
/// git-over-SSH syncs) has no entry in any known_hosts file. Non-interactive
/// ssh cannot prompt to trust a key, so a deleted or truncated
/// `~/.ssh/known_hosts` silently breaks every such job with "Host key
/// verification failed".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SshKnownHosts {
    hosts: Vec<String>,
    /// A host passes when any existing file has an entry for it.
    files: Vec<String>,
    /// Backup known_hosts files (e.g. the copy a machine's ~/.ssh was seeded
    /// from) that remediation suggests restoring from before ssh-keyscan.
    #[serde(default)]
    seeds: Vec<String>,
    #[serde(default = "ssh_keygen")]
    ssh_keygen: String,
}
fn ssh_keygen() -> String {
    "ssh-keygen".into()
}

impl SshKnownHosts {
    /// `ssh-keygen -F` handles hashed entries, ports, and wildcards like ssh.
    async fn known(&self, host: &str, file: &str) -> Result<bool> {
        let output = tokio::process::Command::new(&self.ssh_keygen)
            .args(["-F", host, "-f", file])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("running {}", self.ssh_keygen))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "{} -F {host} -f {file} failed: {}",
                self.ssh_keygen,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
}

#[async_trait]
impl BitCheck for SshKnownHosts {
    fn remediation(&self) -> String {
        let user_file = self
            .files
            .first()
            .map(String::as_str)
            .unwrap_or("~/.ssh/known_hosts");
        let hosts = self.hosts.join(" ");
        let mut text = String::from(
            "Unattended ssh (git sync jobs, etc.) cannot trust a host missing from \
             known_hosts and fails with \"Host key verification failed\".\n\
             - Usual cause: `rm ~/.ssh/known_hosts` to clear one changed key. Remove \
             a single entry instead: `ssh-keygen -R '[localhost]:4444'`.\n",
        );
        for seed in &self.seeds {
            text += &format!(
                "- Restore from the seed copy: `cat {seed} >> {user_file}`, then \
                 `ssh-keygen -R <host>` any entry that now warns of a changed key.\n"
            );
        }
        text += &format!(
            "- Otherwise: `ssh-keyscan -t ed25519,ecdsa,rsa {hosts} > /tmp/k; ssh-keygen -lf /tmp/k`, \
             compare the fingerprints with the host's published ones (GitHub: \
             docs.github.com \"GitHub's SSH key fingerprints\"), then \
             `cat /tmp/k >> {user_file}`.\n\
             - Own machines belong in the OS-managed /etc/ssh/ssh_known_hosts, which \
             survives deleting the user file; record a missing key there instead.\n\
             - Then re-run failed jobs (e.g. `systemctl start launchpad-sync`)."
        );
        text
    }
    async fn check(&self) -> Result<()> {
        ensure!(!self.hosts.is_empty(), "hosts must not be empty");
        ensure!(!self.files.is_empty(), "files must not be empty");
        ensure!(
            self.hosts
                .iter()
                .all(|h| !h.is_empty() && !h.starts_with('-')),
            "host names must be non-empty and not start with '-'"
        );
        let files: Vec<&String> = self
            .files
            .iter()
            .filter(|f| {
                let exists = Path::new(f).is_file();
                if !exists {
                    println!("{f}: absent");
                }
                exists
            })
            .collect();
        let mut missing = Vec::new();
        for host in &self.hosts {
            let mut found = None;
            for file in &files {
                if self.known(host, file).await? {
                    found = Some(file);
                    break;
                }
            }
            match found {
                Some(file) => println!("{host}: known in {file}"),
                None => missing.push(host.as_str()),
            }
        }
        if !missing.is_empty() {
            bail!(
                "no known_hosts entry for {} in {}",
                missing.join(", "),
                self.files.join(", ")
            );
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<SshKnownHosts>().await
}
