use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use bit_rs::{cozy::CozyClient, plugin::BitCheck};
use serde::Deserialize;
use serde_json::json;
use std::{path::PathBuf, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cozy {
    url: String,
    token_file: PathBuf,
    #[serde(default = "max_seconds")]
    max_seconds: u64,
    #[serde(default = "workflow")]
    workflow: String,
    #[serde(default = "side")]
    width: u32,
    #[serde(default = "side")]
    height: u32,
}
fn max_seconds() -> u64 {
    60
}
fn workflow() -> String {
    "imggen-quantized".into()
}
fn side() -> u32 {
    500
}

const PROMPT: &str = "A red apple on a wooden table, soft daylight, photograph.";

/// The whole factor k (1..=8) with got == base * k on both axes, where base is
/// the requested size either exact or floored to a multiple of 8.
fn scale_factor(got: (u32, u32), want: (u32, u32)) -> Option<u32> {
    let bases = [want, (want.0 - want.0 % 8, want.1 - want.1 % 8)];
    (1..=8).find(|k| bases.iter().any(|b| (b.0 * k, b.1 * k) == got))
}

#[async_trait]
impl BitCheck for Cozy {
    fn remediation(&self) -> String {
        bit_rs::cozy::remediation(
            &self.url,
            &self.token_file,
            &self.workflow,
            self.max_seconds,
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(self.max_seconds > 0, "max_seconds must be positive");
        ensure!(
            (64..=2048).contains(&self.width) && (64..=2048).contains(&self.height),
            "width and height must be within 64..=2048"
        );
        let cozy = CozyClient::new(&self.url, &self.token_file)?;
        cozy.ensure_idle().await?;
        let (id, output) = cozy
            .run(
                &json!({"workflow":self.workflow, "prompt":PROMPT,
                        "width":self.width, "height":self.height,
                        "basename":"bitd-imggen-quantized"}),
                Duration::from_secs(self.max_seconds),
            )
            .await?;
        // Latent models emit the requested size floored to their 8-px grid
        // (500 -> 496), and some workflows then upscale by a whole factor
        // (imggen-quantized: ImageScaleBy 4x -> 1984). Anything else means the
        // request was not honoured.
        let scale = scale_factor((output.width(), output.height()), (self.width, self.height));
        let Some(scale) = scale else {
            bail!(
                "result {}x{} for job {id} is not {}x{} (floored to a multiple of 8) times a whole factor",
                output.width(),
                output.height(),
                self.width,
                self.height
            );
        };
        let luma = output.to_luma8();
        let (lo, hi) = luma.pixels().fold((u8::MAX, u8::MIN), |(lo, hi), p| {
            (lo.min(p[0]), hi.max(p[0]))
        });
        ensure!(
            hi - lo >= 16,
            "result for job {id} is blank (luma {lo}..={hi})"
        );
        println!(
            "valid {}x{} result for job {id} ({scale}x upscale, luma {lo}..={hi})",
            output.width(),
            output.height()
        );
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Cozy>().await
}

#[cfg(test)]
mod tests {
    use super::scale_factor;

    #[test]
    fn accepts_grid_floor_and_whole_upscales_only() {
        assert_eq!(scale_factor((500, 500), (500, 500)), Some(1));
        assert_eq!(scale_factor((496, 496), (500, 500)), Some(1));
        assert_eq!(scale_factor((1984, 1984), (500, 500)), Some(4));
        assert_eq!(scale_factor((2000, 2000), (500, 500)), Some(4));
        assert_eq!(scale_factor((1984, 992), (500, 500)), None);
        assert_eq!(scale_factor((1000, 1000), (512, 512)), None);
        assert_eq!(scale_factor((1024, 768), (512, 512)), None);
    }
}
