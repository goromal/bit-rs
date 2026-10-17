use anyhow::{bail, Result};
use serde::Deserialize;
use std::{collections::HashSet, net::SocketAddr, path::PathBuf};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub checks: Vec<CheckConfig>,
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Health,
    Regression,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    pub name: String,
    pub kind: Kind,
    pub command: PathBuf,
    #[serde(default)]
    pub rate_hz: Option<f64>,
    pub timeout_ms: u64,
    #[serde(default = "empty_object")]
    pub config: serde_json::Value,
}
fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if !self.listen.ip().is_loopback() {
            bail!("bitd must listen on loopback; use SSH forwarding for remote access");
        }
        let mut names = HashSet::new();
        for c in &self.checks {
            if c.name.is_empty()
                || !c
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || !names.insert(&c.name)
            {
                bail!("invalid or duplicate check name: {}", c.name);
            }
            if !c.command.is_absolute() || !c.command.is_file() {
                bail!("{}: command must be an existing absolute file", c.name);
            }
            if c.timeout_ms == 0 || c.timeout_ms > 86_400_000 {
                bail!("{}: timeout must be in 1..=86400000 ms", c.name);
            }
            match (c.kind, c.rate_hz) {
                (Kind::Health, Some(rate))
                    if rate.is_finite() && (0.00001..=100.0).contains(&rate) => {}
                (Kind::Regression, None) => (),
                _ => bail!(
                    "{}: health requires rate_hz in 0.00001..=100; regression must omit it",
                    c.name
                ),
            }
            if !c.config.is_object() {
                bail!("{}: config must be an object", c.name);
            }
        }
        Ok(())
    }
}
