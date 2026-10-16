use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use image::{ImageBuffer, Rgb};
use reqwest::{multipart, Client};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{io::Cursor, path::PathBuf, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cozy {
    url: String,
    token_file: PathBuf,
    #[serde(default = "max_seconds")]
    max_seconds: u64,
}
fn max_seconds() -> u64 {
    900
}
impl Cozy {
    async fn run_job(&self, client: &Client) -> Result<()> {
        let api = format!("{}/api", self.url.trim_end_matches('/'));
        let status: Value = client
            .get(format!("{api}/queue/status"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            status["active"] == false
                && status["current"].is_null()
                && status["jobs"].as_array().is_some_and(|a| a.is_empty()),
            "Cozy queue is busy; retry when idle"
        );
        // Fixed, reproducible fixture: a red square on a neutral background.
        let image = ImageBuffer::from_fn(256, 256, |x, y| {
            if (64..192).contains(&x) && (64..192).contains(&y) {
                Rgb([220u8, 30, 30])
            } else {
                Rgb([220u8, 220, 220])
            }
        });
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png)?;
        let form = multipart::Form::new().part(
            "file",
            multipart::Part::bytes(png.into_inner())
                .file_name("bitd-imgedit2.png")
                .mime_str("image/png")?,
        );
        let uploaded: Value = client
            .post(format!("{api}/upload-image"))
            .multipart(form)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let input = uploaded["value"]
            .as_str()
            .context("upload response missing value")?;
        let queued: Value = client.post(format!("{api}/queue/add")).json(&json!({"workflow":"imgedit2", "prompt":"Change the red square to blue. Keep the background unchanged.", "image":input})).send().await?.error_for_status()?.json().await?;
        let id = queued["id"].as_str().context("queue response missing id")?;
        ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid queue id"
        );
        println!("cozy_imgedit2 job={id}");
        let result = tokio::time::timeout(Duration::from_secs(self.max_seconds), async {
            let started = client.post(format!("{api}/queue/start")).send().await?;
            if started.status() != reqwest::StatusCode::CONFLICT {
                started.error_for_status()?;
            }
            loop {
                let status: Value = client
                    .get(format!("{api}/queue/status"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if let Some(result) = status["results"]
                    .as_array()
                    .and_then(|rs| rs.iter().find(|r| r["id"] == id))
                {
                    ensure!(
                        result["status"] == "success",
                        "Cozy job failed: {}",
                        result["error"]
                    );
                    let mut response = client
                        .get(format!("{api}/queue/image"))
                        .query(&[("id", id)])
                        .send()
                        .await?
                        .error_for_status()?;
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await? {
                        ensure!(
                            bytes.len() + chunk.len() <= 50 * 1024 * 1024,
                            "result image exceeds 50 MiB"
                        );
                        bytes.extend_from_slice(&chunk);
                    }
                    let mut reader =
                        image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(8192);
                    limits.max_image_height = Some(8192);
                    limits.max_alloc = Some(256 * 1024 * 1024);
                    reader.limits(limits);
                    let output = reader.decode().context("result is not a valid image")?;
                    ensure!(output.width() > 0 && output.height() > 0, "empty image");
                    println!(
                        "valid result {}x{} for job {id}",
                        output.width(),
                        output.height()
                    );
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            other => {
                // Cancel only our job, whether still pending or already running
                // on the GPU; leave other work and completed results intact.
                match client
                    .post(format!("{api}/queue/cancel"))
                    .json(&json!({"id":id}))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => println!("cancelled Cozy job {id}"),
                    Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {}
                    Ok(r) => println!("cancel of Cozy job {id} failed: HTTP {}", r.status()),
                    Err(e) => println!("cancel of Cozy job {id} failed: {e}"),
                }
                match other {
                    Ok(Err(e)) => Err(e),
                    _ => bail!(
                        "job {id} produced no output within {} seconds; cancelled",
                        self.max_seconds
                    ),
                }
            }
        }
    }
}
#[async_trait]
impl BitCheck for Cozy {
    async fn check(&self) -> Result<()> {
        ensure!(self.max_seconds > 0, "max_seconds must be positive");
        let token =
            std::fs::read_to_string(&self.token_file).context("read Cozy API token file")?;
        ensure!(!token.trim().is_empty(), "empty Cozy API token");
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        self.run_job(&client).await
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Cozy>().await
}
