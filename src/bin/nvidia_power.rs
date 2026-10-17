use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;

/// Fails when a GPU's enforced power limit sits far below its default. Laptop
/// firmware can leave the dGPU capped after a resume (e.g. 6 W of 30 W), which
/// pins it near idle clocks and silently makes GPU jobs an order of magnitude
/// slower until reboot.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NvidiaPower {
    #[serde(default = "nvidia_smi")]
    nvidia_smi: String,
    /// Minimum enforced / default power-limit ratio considered healthy.
    #[serde(default = "min_fraction")]
    min_fraction: f64,
}
fn nvidia_smi() -> String {
    "nvidia-smi".into()
}
fn min_fraction() -> f64 {
    0.5
}

#[derive(Debug, PartialEq)]
struct Gpu {
    index: String,
    name: String,
    enforced: Option<f64>,
    default: Option<f64>,
}
fn watts(field: &str) -> Option<f64> {
    field.trim().parse().ok()
}
/// Parses `--query-gpu=index,enforced.power.limit,power.default_limit,name
/// --format=csv,noheader,nounits`; the name is last because it may contain commas.
fn parse(csv: &str) -> Result<Vec<Gpu>> {
    csv.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.splitn(4, ',').collect();
            ensure!(f.len() == 4, "unexpected nvidia-smi row: {line:?}");
            Ok(Gpu {
                index: f[0].trim().into(),
                enforced: watts(f[1]),
                default: watts(f[2]),
                name: f[3].trim().into(),
            })
        })
        .collect()
}

#[async_trait]
impl BitCheck for NvidiaPower {
    fn remediation(&self) -> String {
        format!(
            "A capped GPU runs near idle clocks, so GPU jobs (e.g. ComfyUI) take many \
             times longer.\n\
             - Reboot: firmware can leave the limit capped after a resume, notably \
             hybrid-sleep, and only a reboot resets it.\n\
             - Confirm AC power is connected; on battery the platform caps the GPU.\n\
             - Inspect: `{smi} -q -d POWER,PERFORMANCE` (Current vs Default Power \
             Limit, Clocks Event Reasons).",
            smi = self.nvidia_smi
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(
            self.min_fraction > 0.0 && self.min_fraction <= 1.0,
            "min_fraction must be in (0, 1]"
        );
        let output = tokio::process::Command::new(&self.nvidia_smi)
            .args([
                "--query-gpu=index,enforced.power.limit,power.default_limit,name",
                "--format=csv,noheader,nounits",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("running {}", self.nvidia_smi))?;
        ensure!(
            output.status.success(),
            "nvidia-smi failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let gpus = parse(&String::from_utf8_lossy(&output.stdout))?;
        let mut checked = 0;
        let mut capped = Vec::new();
        for gpu in &gpus {
            let (Some(enforced), Some(default)) = (gpu.enforced, gpu.default) else {
                println!(
                    "GPU {} ({}): power limits not reported, skipped",
                    gpu.index, gpu.name
                );
                continue;
            };
            checked += 1;
            println!(
                "GPU {} ({}): enforced power limit {enforced:.2} W of default {default:.2} W",
                gpu.index, gpu.name
            );
            if enforced < default * self.min_fraction {
                capped.push(format!(
                    "GPU {} ({}) capped at {enforced:.2} W of {default:.2} W",
                    gpu.index, gpu.name
                ));
            }
        }
        if !capped.is_empty() {
            bail!(
                "{}; GPU work will run far slower than normal (a reboot resets the limit)",
                capped.join("; ")
            );
        }
        ensure!(checked > 0, "no GPU reported its power limits");
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<NvidiaPower>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits_and_names_with_commas() {
        let gpus =
            parse("0, 6.00, 30.00, NVIDIA RTX 500, Laptop\n1, [N/A], [N/A], Other\n\n").unwrap();
        assert_eq!(
            gpus,
            [
                Gpu {
                    index: "0".into(),
                    name: "NVIDIA RTX 500, Laptop".into(),
                    enforced: Some(6.0),
                    default: Some(30.0),
                },
                Gpu {
                    index: "1".into(),
                    name: "Other".into(),
                    enforced: None,
                    default: None,
                },
            ]
        );
        assert!(parse("garbage").is_err());
    }
}
