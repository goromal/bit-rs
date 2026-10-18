use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::collections::HashMap;

/// Fails when any of the plugin user's systemd user units failed, never ran,
/// or is not installed. Built for the oneshot units that merge Nix-managed
/// agent settings (e.g. `codex-settings-update`): when one fails, agents
/// silently keep stale configuration such as missing MCP servers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserUnits {
    units: Vec<String>,
    #[serde(default = "systemctl")]
    systemctl: String,
    /// Defaults to /run/user/<uid> of the plugin process.
    runtime_dir: Option<String>,
}
fn systemctl() -> String {
    "systemctl".into()
}

#[derive(Debug, PartialEq)]
struct UnitState {
    load: String,
    active: String,
    sub: String,
    result: String,
}
/// Parses `systemctl show -p LoadState,ActiveState,SubState,Result u1 u2 ...`:
/// one blank-line-separated block per unit, in argument order.
fn parse(text: &str, count: usize) -> Result<Vec<UnitState>> {
    let blocks: Vec<HashMap<&str, &str>> = text
        .split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .map(|b| b.lines().filter_map(|l| l.split_once('=')).collect())
        .collect();
    ensure!(
        blocks.len() == count,
        "expected {count} unit blocks from systemctl show, got {}",
        blocks.len()
    );
    blocks
        .into_iter()
        .map(|b| {
            let get = |k: &str| {
                b.get(k)
                    .map(|v| v.to_string())
                    .with_context(|| format!("systemctl show omitted {k}"))
            };
            Ok(UnitState {
                load: get("LoadState")?,
                active: get("ActiveState")?,
                sub: get("SubState")?,
                result: get("Result")?,
            })
        })
        .collect()
}
fn problem(unit: &str, s: &UnitState) -> Option<String> {
    if s.load != "loaded" {
        Some(format!("{unit} is {} (not installed?)", s.load))
    } else if s.active == "failed" || s.result != "success" {
        Some(format!("{unit} failed (result: {})", s.result))
    } else if s.active == "inactive" {
        Some(format!("{unit} has not run since the user manager started"))
    } else {
        None
    }
}

impl UserUnits {
    fn runtime_dir(&self) -> String {
        self.runtime_dir
            .clone()
            .unwrap_or_else(|| format!("/run/user/{}", unsafe { libc::getuid() }))
    }
}

#[async_trait]
impl BitCheck for UserUnits {
    fn remediation(&self) -> String {
        let units = self.units.join(" ");
        format!(
            "A user unit that applies Nix-managed configuration failed, so the app it \
             configures keeps stale settings (e.g. agents missing MCP servers).\n\
             - Why: `journalctl --user -u <unit> -n 50` for each of: {units}\n\
             - State: `systemctl --user status {units}`\n\
             - Retry after fixing: `systemctl --user restart <unit>`; the unit is \
             defined in anixpkgs (agent units: pkgs/nixos/components/agent-lib.nix), so \
             fix it there and redeploy.\n\
             - Not installed: the unit is missing from this generation; check the \
             profile's agent frameworks and redeploy."
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(!self.units.is_empty(), "units must not be empty");
        ensure!(
            self.units
                .iter()
                .all(|u| !u.is_empty() && !u.starts_with('-')),
            "unit names must be non-empty and not start with '-'"
        );
        let runtime_dir = self.runtime_dir();
        let bus = std::path::Path::new(&runtime_dir).join("bus");
        if !bus.exists() {
            // These units only run under the user manager; without a session
            // (and no linger) there is nothing to have failed.
            println!(
                "user manager not running ({} absent); units run at login, skipped",
                bus.display()
            );
            return Ok(());
        }
        let output = tokio::process::Command::new(&self.systemctl)
            .args([
                "--user",
                "show",
                "--property=LoadState,ActiveState,SubState,Result",
            ])
            .args(&self.units)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus.display()),
            )
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("running {}", self.systemctl))?;
        ensure!(
            output.status.success(),
            "systemctl --user show failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let states = parse(&String::from_utf8_lossy(&output.stdout), self.units.len())?;
        let mut problems = Vec::new();
        for (unit, state) in self.units.iter().zip(&states) {
            println!(
                "{unit}: {} {}/{} (result: {})",
                state.load, state.active, state.sub, state.result
            );
            problems.extend(problem(unit, state));
        }
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<UserUnits>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(load: &str, active: &str, sub: &str, result: &str) -> UnitState {
        UnitState {
            load: load.into(),
            active: active.into(),
            sub: sub.into(),
            result: result.into(),
        }
    }

    #[test]
    fn parses_blocks_in_order_regardless_of_property_order() {
        let text = "LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\n\n\
                    Result=exit-code\nSubState=failed\nActiveState=failed\nLoadState=loaded\n";
        assert_eq!(
            parse(text, 2).unwrap(),
            [
                state("loaded", "active", "exited", "success"),
                state("loaded", "failed", "failed", "exit-code"),
            ]
        );
        assert!(parse(text, 3).is_err());
        assert!(parse("LoadState=loaded\n", 1).is_err());
    }

    #[test]
    fn classifies_unit_states() {
        assert_eq!(
            problem("a", &state("loaded", "active", "exited", "success")),
            None
        );
        assert_eq!(
            problem("a", &state("loaded", "activating", "start", "success")),
            None
        );
        assert!(
            problem("a", &state("loaded", "failed", "failed", "exit-code"))
                .unwrap()
                .contains("failed (result: exit-code)")
        );
        assert!(
            problem("a", &state("not-found", "inactive", "dead", "success"))
                .unwrap()
                .contains("not-found")
        );
        assert!(
            problem("a", &state("loaded", "inactive", "dead", "success"))
                .unwrap()
                .contains("has not run")
        );
    }
}
