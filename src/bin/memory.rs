use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

/// Fails when the machine is low on memory headroom or already stalling on it.
///
/// Headroom is MemAvailable + SwapFree (so zram counts): below
/// `min_headroom_percent` of RAM there is little left before earlyoom (which
/// acts near 10% free) has to kill something. Pressure is the kernel's PSI
/// "full" share over the last minute: all non-idle tasks stalled on memory at
/// once, i.e. thrashing. "Used %" is deliberately not a criterion; page cache
/// makes it near 100% on a healthy box.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Memory {
    #[serde(default = "min_headroom_percent")]
    min_headroom_percent: f64,
    #[serde(default = "max_full_pressure_percent")]
    max_full_pressure_percent: f64,
    /// How many top memory users (by systemd unit) to log.
    #[serde(default = "top")]
    top: usize,
}
fn min_headroom_percent() -> f64 {
    15.0
}
fn max_full_pressure_percent() -> f64 {
    10.0
}
fn top() -> usize {
    5
}

fn meminfo_kb(meminfo: &str, key: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// `full avg60=` from /proc/pressure/memory, in percent.
fn full_avg60(pressure: &str) -> Option<f64> {
    pressure
        .lines()
        .find(|l| l.starts_with("full "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg60="))?
        .parse()
        .ok()
}

/// Group a process under its innermost systemd unit (service/scope), or its
/// cgroup path when it has none, e.g.
/// `0::/user.slice/user-1000.slice/user@1000.service/app.slice/lorri.service`
/// -> `lorri.service`.
fn unit_of(cgroup: &str) -> String {
    let path = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or("")
        .trim();
    path.rsplit('/')
        .find(|seg| seg.ends_with(".service") || seg.ends_with(".scope"))
        .map(str::to_string)
        .unwrap_or_else(|| {
            if path.is_empty() {
                "?".into()
            } else {
                path.into()
            }
        })
}

fn top_units(usage: HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut units: Vec<(String, u64)> = usage.into_iter().collect();
    units.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    units.truncate(n);
    units
}

fn usage_by_unit() -> HashMap<String, u64> {
    let mut usage: HashMap<String, u64> = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return usage;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let dir = entry.path();
        // Processes can exit mid-scan: skip whatever vanished.
        let (Ok(status), Ok(cgroup)) = (
            fs::read_to_string(dir.join("status")),
            fs::read_to_string(dir.join("cgroup")),
        ) else {
            continue;
        };
        if let Some(kb) = meminfo_kb(&status, "RssAnon:") {
            *usage.entry(unit_of(&cgroup)).or_default() += kb;
        }
    }
    usage
}

#[async_trait]
impl BitCheck for Memory {
    fn remediation(&self) -> String {
        format!(
            "Memory headroom (MemAvailable + SwapFree) is under {}% of RAM, or tasks are stalling\n\
             on memory (PSI full avg60 over {}%). earlyoom kills the largest process near 10% free.\n\
             - `journalctl -u bitd` lists the top memory users by systemd unit for this check.\n\
             - Idle agent sessions are a common cause (~1 GB each): see the agent_sessions check.\n\
             - Postpone deploys and other big nix builds until headroom recovers.\n\
             - `systemd-cgtop -m` and `ps -eo rss,comm --sort=-rss | head` show live usage.",
            self.min_headroom_percent, self.max_full_pressure_percent
        )
    }
    async fn check(&self) -> Result<()> {
        let meminfo = fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
        let total = meminfo_kb(&meminfo, "MemTotal:").context("no MemTotal")?;
        let available = meminfo_kb(&meminfo, "MemAvailable:").context("no MemAvailable")?;
        let swap_free = meminfo_kb(&meminfo, "SwapFree:").unwrap_or(0);
        let headroom = (available + swap_free) as f64 * 100.0 / total as f64;
        // PSI can be compiled out or disabled (psi=0): judge headroom alone then.
        let pressure = fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|p| full_avg60(&p));
        println!(
            "headroom {headroom:.1}% (available {} MiB + swap free {} MiB of {} MiB RAM); \
             full pressure avg60 {}",
            available / 1024,
            swap_free / 1024,
            total / 1024,
            pressure.map_or("n/a".into(), |p| format!("{p:.2}%"))
        );
        for (unit, kb) in top_units(usage_by_unit(), self.top) {
            println!("  {:>7} MiB  {unit}", kb / 1024);
        }
        if headroom < self.min_headroom_percent {
            bail!(
                "memory headroom {headroom:.1}% is under {}%",
                self.min_headroom_percent
            );
        }
        if let Some(p) = pressure.filter(|p| *p > self.max_full_pressure_percent) {
            bail!(
                "tasks stalled on memory {p:.2}% of the last minute (limit {}%)",
                self.max_full_pressure_percent
            );
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Memory>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo_and_pressure() {
        let meminfo = "MemTotal:       16150000 kB\nMemAvailable:    1900000 kB\nSwapFree:              0 kB\n";
        assert_eq!(meminfo_kb(meminfo, "MemTotal:"), Some(16_150_000));
        assert_eq!(meminfo_kb(meminfo, "MemAvailable:"), Some(1_900_000));
        assert_eq!(meminfo_kb(meminfo, "Missing:"), None);
        let psi = "some avg10=1.00 avg60=20.50 avg300=3.00 total=1\nfull avg10=0.50 avg60=12.25 avg300=1.00 total=1\n";
        assert_eq!(full_avg60(psi), Some(12.25));
        assert_eq!(
            full_avg60("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
            None
        );
    }

    #[test]
    fn groups_by_innermost_unit() {
        assert_eq!(
            unit_of("0::/user.slice/user-1000.slice/user@1000.service/app.slice/lorri.service\n"),
            "lorri.service"
        );
        assert_eq!(
            unit_of("0::/system.slice/agent-ui.service\n"),
            "agent-ui.service"
        );
        assert_eq!(
            unit_of("0::/user.slice/user-1000.slice/session-3.scope\n"),
            "session-3.scope"
        );
        assert_eq!(unit_of("0::/init.scope\n"), "init.scope");
        assert_eq!(unit_of("0::/\n"), "/");
        assert_eq!(unit_of(""), "?");
    }

    #[test]
    fn ranks_top_units() {
        let usage = HashMap::from([
            ("a.service".to_string(), 10),
            ("b.service".to_string(), 30),
            ("c.service".to_string(), 20),
        ]);
        assert_eq!(
            top_units(usage, 2),
            vec![("b.service".to_string(), 30), ("c.service".to_string(), 20)]
        );
    }

    #[test]
    fn config_defaults_and_rejects_unknown_fields() {
        let c: Memory = serde_json::from_str("{}").unwrap();
        assert_eq!(
            (c.min_headroom_percent, c.max_full_pressure_percent, c.top),
            (15.0, 10.0, 5)
        );
        assert!(serde_json::from_str::<Memory>(r#"{"x": 1}"#).is_err());
    }
}
