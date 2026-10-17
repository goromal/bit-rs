//! Shared Cozy queue client for the cozy_* regression plugins.
//!
//! A check requires an idle queue, submits exactly one job, and tracks it by
//! ID. If no valid output arrives within the deadline (or the job fails), it
//! cancels only its own job and leaves other work and finished results alone.

use anyhow::{bail, ensure, Context, Result};
use image::DynamicImage;
use reqwest::{multipart, Client};
use serde_json::{json, Value};
use std::{io::Cursor, path::Path, time::Duration};

const POLL: Duration = Duration::from_secs(2);
const MAX_RESULT_BYTES: usize = 50 * 1024 * 1024;

/// Shared remediation text for the cozy_* regression plugins.
pub fn remediation(url: &str, token_file: &Path, workflow: &str, max_seconds: u64) -> String {
    format!(
        "The {workflow} job through Cozy ({url}) failed or exceeded {max_seconds} s.\n\
         - Services: `systemctl status cozy comfyui`; errors: `journalctl -u comfyui -n 200`.\n\
         - Slow runs: check the nvidia_power health check (a capped GPU makes jobs \
         ~10x slower) and compare s/it in the ComfyUI log with past runs.\n\
         - \"Cozy queue is busy\": wait for other Cozy jobs to finish, then rerun.\n\
         - 401/403: the token in {} must match Cozy's api_token_hash.",
        token_file.display()
    )
}

pub struct CozyClient {
    api: String,
    client: Client,
}

impl CozyClient {
    pub fn new(url: &str, token_file: &Path) -> Result<Self> {
        let token = std::fs::read_to_string(token_file).context("read Cozy API token file")?;
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
        Ok(Self {
            api: format!("{}/api", url.trim_end_matches('/')),
            client,
        })
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        Ok(self
            .client
            .get(format!("{}/{path}", self.api))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        Ok(self
            .client
            .post(format!("{}/{path}", self.api))
            .json(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn ensure_idle(&self) -> Result<()> {
        let status = self.get_json("queue/status").await?;
        ensure!(
            status["active"] == false
                && status["current"].is_null()
                && status["jobs"].as_array().is_some_and(|a| a.is_empty()),
            "Cozy queue is busy; retry when idle"
        );
        Ok(())
    }

    /// Upload a PNG to Cozy's input directory; returns the picker value.
    pub async fn upload_png(&self, png: Vec<u8>, file_name: &str) -> Result<String> {
        let form = multipart::Form::new().part(
            "file",
            multipart::Part::bytes(png)
                .file_name(file_name.to_owned())
                .mime_str("image/png")?,
        );
        let uploaded: Value = self
            .client
            .post(format!("{}/upload-image", self.api))
            .multipart(form)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(uploaded["value"]
            .as_str()
            .context("upload response missing value")?
            .to_owned())
    }

    /// Queue `spec`, start the queue, and return the decoded result image.
    /// Fails as soon as `max` passes without a result, cancelling the job.
    pub async fn run(&self, spec: &Value, max: Duration) -> Result<(String, DynamicImage)> {
        let queued = self.post_json("queue/add", spec).await?;
        let id = queued["id"]
            .as_str()
            .context("queue response missing id")?
            .to_owned();
        ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid queue id"
        );
        println!("cozy job={id} workflow={}", spec["workflow"]);
        match tokio::time::timeout(max, self.await_result(&id)).await {
            Ok(Ok(image)) => Ok((id, image)),
            other => {
                self.cancel(&id).await;
                match other {
                    Ok(Err(e)) => Err(e),
                    _ => bail!(
                        "job {id} produced no output within {} seconds; cancelled",
                        max.as_secs()
                    ),
                }
            }
        }
    }

    async fn await_result(&self, id: &str) -> Result<DynamicImage> {
        let started = self
            .client
            .post(format!("{}/queue/start", self.api))
            .send()
            .await?;
        if started.status() != reqwest::StatusCode::CONFLICT {
            started.error_for_status()?;
        }
        loop {
            let status = self.get_json("queue/status").await?;
            if let Some(result) = status["results"]
                .as_array()
                .and_then(|rs| rs.iter().find(|r| r["id"] == id))
            {
                ensure!(
                    result["status"] == "success",
                    "Cozy job failed: {}",
                    result["error"]
                );
                return self.download(id).await;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn download(&self, id: &str) -> Result<DynamicImage> {
        let mut response = self
            .client
            .get(format!("{}/queue/image", self.api))
            .query(&[("id", id)])
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_RESULT_BYTES,
                "result image exceeds 50 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits);
        let output = reader.decode().context("result is not a valid image")?;
        ensure!(output.width() > 0 && output.height() > 0, "empty image");
        Ok(output)
    }

    /// Cancel our job whether pending or running; best-effort.
    async fn cancel(&self, id: &str) {
        match self
            .client
            .post(format!("{}/queue/cancel", self.api))
            .json(&json!({"id": id}))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => println!("cancelled Cozy job {id}"),
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {}
            Ok(r) => println!("cancel of Cozy job {id} failed: HTTP {}", r.status()),
            Err(e) => println!("cancel of Cozy job {id} failed: {e}"),
        }
    }
}
