use anyhow::{ensure, Result};
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

#[async_trait]
impl BitCheck for Cozy {
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
        // (500 -> 496); anything else means the request was not honoured.
        let fits = |got: u32, want: u32| got <= want && got >= want - want % 8;
        ensure!(
            fits(output.width(), self.width) && fits(output.height(), self.height),
            "result {}x{} for job {id}, expected {}x{} (or floored to a multiple of 8)",
            output.width(),
            output.height(),
            self.width,
            self.height
        );
        let luma = output.to_luma8();
        let (lo, hi) = luma.pixels().fold((u8::MAX, u8::MIN), |(lo, hi), p| {
            (lo.min(p[0]), hi.max(p[0]))
        });
        ensure!(
            hi - lo >= 16,
            "result for job {id} is blank (luma {lo}..={hi})"
        );
        println!(
            "valid {}x{} result for job {id} (luma {lo}..={hi})",
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
