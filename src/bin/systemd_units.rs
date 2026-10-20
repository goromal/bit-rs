use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::collections::HashMap;

/// Fails when any loaded system unit is failed or crash looping. A unit is
/// crash looping while it waits out RestartSec (`auto-restart`), or when it has
/// restarted automatically at least `max_restarts` times and the latest
/// restart was under `window_seconds` ago. systemd resets NRestarts on a
/// manual start, so restarting a fixed unit clears the check.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemdUnits {
    #[serde(default = "max_restarts")]
    max_restarts: u64,
    #[serde(default = "window_seconds")]
    window_seconds: u64,
    /// Exact unit names (e.g. `foo.service`) never reported.
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default = "systemctl")]
    systemctl: String,
}
fn max_restarts() -> u64 {
    3
}
fn window_seconds() -> u64 {
    600
}
fn systemctl() -> String {
    "systemctl".into()
}

const PROPERTIES: &str =
    "--property=Id,ActiveState,SubState,Result,NRestarts,InactiveExitTimestampMonotonic";

#[derive(Debug, PartialEq)]
struct UnitState {
    id: String,
    active: String,
    sub: String,
    result: String,
    /// Only services report these.
    restarts: u64,
    /// CLOCK_MONOTONIC microseconds of the latest start attempt; 0 if never.
    started_us: u64,
}
/// Parses `systemctl show <PROPERTIES> '*'`: one blank-line-separated block
/// per loaded unit.
fn parse(text: &str) -> Result<Vec<UnitState>> {
    text.split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .map(|b| {
            let props: HashMap<&str, &str> = b.lines().filter_map(|l| l.split_once('=')).collect();
            let get = |k: &str| {
                props
                    .get(k)
                    .map(|v| v.to_string())
                    .with_context(|| format!("systemctl show omitted {k} in block {b:?}"))
            };
            let number = |k: &str| -> Result<u64> {
                props.get(k).map_or(Ok(0), |v| {
                    v.parse()
                        .with_context(|| format!("{k}={v} is not a number"))
                })
            };
            Ok(UnitState {
                id: get("Id")?,
                active: get("ActiveState")?,
                sub: get("SubState")?,
                result: props.get("Result").unwrap_or(&"success").to_string(),
                restarts: number("NRestarts")?,
                started_us: number("InactiveExitTimestampMonotonic")?,
            })
        })
        .collect()
}

impl SystemdUnits {
    fn problem(&self, s: &UnitState, now_us: u64) -> Option<String> {
        if self.ignore.contains(&s.id) {
            None
        } else if s.active == "failed" {
            Some(format!("{} failed (result: {})", s.id, s.result))
        } else if s.sub == "auto-restart" {
            Some(format!(
                "{} is crash looping (waiting to restart, {} restarts, result: {})",
                s.id, s.restarts, s.result
            ))
        } else if s.restarts >= self.max_restarts
            && s.started_us > 0
            && now_us.saturating_sub(s.started_us) < self.window_seconds * 1_000_000
        {
            Some(format!(
                "{} is crash looping ({} restarts, latest {}s ago)",
                s.id,
                s.restarts,
                now_us.saturating_sub(s.started_us) / 1_000_000
            ))
        } else {
            None
        }
    }
}
fn monotonic_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid out-pointer and CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

#[async_trait]
impl BitCheck for SystemdUnits {
    fn remediation(&self) -> String {
        format!(
            "A system unit failed or keeps restarting (crash loop: >= {} restarts with \
             the latest under {}s ago, or waiting to restart).\n\
             - List: `systemctl --failed`; `systemctl status <unit>`\n\
             - Why: `journalctl -u <unit> -b -n 100`\n\
             - After fixing: `systemctl restart <unit>` (resets the restart count) or \
             `systemctl reset-failed <unit>` for a unit that should stay stopped.\n\
             - Expected noise: add the unit to services.bitd.plugins.systemd_units.settings.ignore \
             in anixpkgs.",
            self.max_restarts, self.window_seconds
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(self.max_restarts > 0, "max_restarts must be positive");
        let output = tokio::process::Command::new(&self.systemctl)
            .args(["show", PROPERTIES, "--", "*"])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("running {}", self.systemctl))?;
        ensure!(
            output.status.success(),
            "systemctl show failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let states = parse(&String::from_utf8_lossy(&output.stdout))?;
        ensure!(!states.is_empty(), "systemctl show listed no units");
        let now = monotonic_us();
        let problems: Vec<String> = states.iter().filter_map(|s| self.problem(s, now)).collect();
        println!("{} units loaded, {} problems", states.len(), problems.len());
        for p in &problems {
            println!("{p}");
        }
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<SystemdUnits>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(id: &str, active: &str, sub: &str, restarts: u64, started_us: u64) -> UnitState {
        UnitState {
            id: id.into(),
            active: active.into(),
            sub: sub.into(),
            result: "success".into(),
            restarts,
            started_us,
        }
    }
    fn config(ignore: &[&str]) -> SystemdUnits {
        SystemdUnits {
            max_restarts: 3,
            window_seconds: 600,
            ignore: ignore.iter().map(|s| s.to_string()).collect(),
            systemctl: systemctl(),
        }
    }

    #[test]
    fn parses_services_and_other_units() {
        let text = "Id=-.mount\nActiveState=active\nSubState=mounted\nInactiveExitTimestampMonotonic=0\nResult=success\n\n\
                    Id=sockets.target\nActiveState=active\nSubState=active\nInactiveExitTimestampMonotonic=6764349\n\n\
                    Id=a.service\nActiveState=activating\nSubState=auto-restart\nResult=exit-code\nNRestarts=7\nInactiveExitTimestampMonotonic=42\n";
        let states = parse(text).unwrap();
        assert_eq!(states.len(), 3);
        assert_eq!(
            states[1],
            state("sockets.target", "active", "active", 0, 6764349)
        );
        assert_eq!(states[2].restarts, 7);
        assert_eq!(states[2].result, "exit-code");
        assert!(parse("ActiveState=active\nSubState=running\n").is_err());
        assert!(parse("Id=a\nActiveState=active\nSubState=running\nNRestarts=x\n").is_err());
    }

    #[test]
    fn classifies_unit_states() {
        let c = config(&["ignored.service"]);
        let now = 10_000 * 1_000_000;
        let recent = now - 60 * 1_000_000;
        let old = now - 3_600 * 1_000_000;
        assert_eq!(
            c.problem(&state("a", "active", "running", 0, recent), now),
            None
        );
        assert!(c
            .problem(&state("a", "failed", "failed", 0, old), now)
            .unwrap()
            .contains("a failed"));
        assert!(c
            .problem(&state("a", "activating", "auto-restart", 1, old), now)
            .unwrap()
            .contains("crash looping"));
        assert!(c
            .problem(&state("a", "active", "running", 5, recent), now)
            .unwrap()
            .contains("5 restarts, latest 60s ago"));
        // Restarted often but stable for longer than the window: recovered.
        assert_eq!(
            c.problem(&state("a", "active", "running", 5, old), now),
            None
        );
        assert_eq!(
            c.problem(&state("a", "active", "running", 2, recent), now),
            None
        );
        assert_eq!(
            c.problem(&state("ignored.service", "failed", "failed", 0, old), now),
            None
        );
    }
}
