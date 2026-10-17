use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::{cozy::CozyClient, plugin::BitCheck};
use image::RgbImage;
use serde::Deserialize;
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cozy {
    url: String,
    token_file: PathBuf,
    #[serde(default = "max_seconds")]
    max_seconds: u64,
    /// anixpkgs `png` executable; generates the seeded random input image.
    png_command: PathBuf,
    /// Random per run when unset; logged so a failing input can be reproduced.
    seed: Option<String>,
    #[serde(default = "side")]
    width: u32,
    #[serde(default = "side")]
    height: u32,
    /// Minimum share of 8x8 output blocks, above the input's own baseline,
    /// whose mean colour is strongly red.
    #[serde(default = "min_red_fraction")]
    min_red_fraction: f64,
}
fn max_seconds() -> u64 {
    900
}
fn side() -> u32 {
    512
}
fn min_red_fraction() -> f64 {
    0.005
}

const PROMPT: &str = "Add several bold, bright red markings (thick strokes, circles, or X marks) on top of this image. Keep everything else unchanged.";
const BLOCK: u32 = 8;

/// Share of BLOCK x BLOCK tiles whose mean colour is saturated red. Uniform
/// noise averages toward grey per tile, so its baseline is essentially zero.
fn red_fraction(image: &RgbImage) -> f64 {
    let (bw, bh) = (image.width() / BLOCK, image.height() / BLOCK);
    if bw == 0 || bh == 0 {
        return 0.0;
    }
    let mut red = 0u64;
    for by in 0..bh {
        for bx in 0..bw {
            let mut sum = [0u64; 3];
            for y in by * BLOCK..(by + 1) * BLOCK {
                for x in bx * BLOCK..(bx + 1) * BLOCK {
                    let p = image.get_pixel(x, y);
                    for c in 0..3 {
                        sum[c] += u64::from(p[c]);
                    }
                }
            }
            let n = u64::from(BLOCK * BLOCK);
            let [r, g, b] = sum.map(|v| (v / n) as i64);
            if r >= 150 && r - g >= 70 && r - b >= 70 {
                red += 1;
            }
        }
    }
    red as f64 / f64::from(bw * bh)
}

impl Cozy {
    fn seed(&self) -> String {
        self.seed.clone().unwrap_or_else(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            format!("bitd{:x}{:x}", nanos, std::process::id())
        })
    }
    /// Run `png <seed>-<w>-<h>.random` and return the PNG bytes.
    async fn generate_input(&self, seed: &str) -> Result<Vec<u8>> {
        let dir = tempfile::tempdir().context("create temp dir")?;
        let out = dir.path().join("input.png");
        let spec = format!("{seed}-{}-{}.random", self.width, self.height);
        let status = tokio::process::Command::new(&self.png_command)
            .arg(&spec)
            .arg(&out)
            .current_dir(dir.path())
            .status()
            .await
            .with_context(|| format!("run {}", self.png_command.display()))?;
        ensure!(status.success(), "png {spec} failed: {status}");
        let bytes = std::fs::read(&out).context("read generated input")?;
        let decoded = image::load_from_memory(&bytes).context("generated input is not an image")?;
        ensure!(
            (decoded.width(), decoded.height()) == (self.width, self.height),
            "png produced {}x{}, expected {}x{}",
            decoded.width(),
            decoded.height(),
            self.width,
            self.height
        );
        Ok(bytes)
    }
    async fn run_job(&self, cozy: &CozyClient) -> Result<()> {
        cozy.ensure_idle().await?;
        let seed = self.seed();
        let png = self.generate_input(&seed).await?;
        let baseline = red_fraction(&image::load_from_memory(&png)?.to_rgb8());
        println!(
            "input seed={seed} size={}x{} red_baseline={baseline:.4}",
            self.width, self.height
        );
        let input = cozy.upload_png(png, "bitd-imgedit2.png").await?;
        let (id, output) = cozy
            .run(
                &json!({"workflow":"imgedit2", "prompt":PROMPT, "image":input}),
                Duration::from_secs(self.max_seconds),
            )
            .await?;
        let (want, got) = (
            f64::from(self.width) / f64::from(self.height),
            f64::from(output.width()) / f64::from(output.height()),
        );
        ensure!(
            (got / want - 1.0).abs() <= 0.02,
            "result {}x{} does not keep the {}x{} input's aspect ratio",
            output.width(),
            output.height(),
            self.width,
            self.height
        );
        let red = red_fraction(&output.to_rgb8());
        println!(
            "result {}x{} for job {id}: red={red:.4} baseline={baseline:.4}",
            output.width(),
            output.height()
        );
        ensure!(
            red - baseline >= self.min_red_fraction,
            "result has no clear red markings (red={red:.4}, baseline={baseline:.4}, need +{})",
            self.min_red_fraction
        );
        Ok(())
    }
}
#[async_trait]
impl BitCheck for Cozy {
    fn remediation(&self) -> String {
        format!(
            "{}\n\
             - Wrong-output failures: the log names the seed; rerun with `seed` set to \
             reproduce, and inspect the result image in Cozy.",
            bit_rs::cozy::remediation(&self.url, &self.token_file, "imgedit2", self.max_seconds)
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(self.max_seconds > 0, "max_seconds must be positive");
        ensure!(
            (64..=2048).contains(&self.width) && (64..=2048).contains(&self.height),
            "width and height must be within 64..=2048"
        );
        ensure!(
            (0.0..1.0).contains(&self.min_red_fraction),
            "min_red_fraction must be in [0, 1)"
        );
        ensure!(
            self.seed
                .as_deref()
                .map_or(true, |s| !s.is_empty() && !s.contains(['-', '/'])),
            "seed must be non-empty without '-' or '/'"
        );
        let cozy = CozyClient::new(&self.url, &self.token_file)?;
        self.run_job(&cozy).await
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Cozy>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_has_no_red_blocks_and_markings_do() {
        let mut state = 0x9e37_79b9u32;
        let mut noise = RgbImage::from_fn(512, 512, |_, _| {
            let mut px = [0u8; 3];
            for c in &mut px {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *c = (state >> 24) as u8;
            }
            image::Rgb(px)
        });
        assert!(red_fraction(&noise) < 0.001);
        for y in 100..140 {
            for x in 0..512 {
                noise.put_pixel(x, y, image::Rgb([220, 30, 30]));
            }
        }
        assert!(red_fraction(&noise) > 0.07);
    }
}
