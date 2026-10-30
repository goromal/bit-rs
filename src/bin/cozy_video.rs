//! Regression for Cozy's Video tab: trim a fixture clip to the frames matching
//! a prompt, then export the match, and compare both with expected answers.
//!
//! The clip and its expectations come from anixdata as a pair (`video` and
//! `expect_file`), so they change together. Cozy is handed the clip's
//! read-only store path as a host-local "remote" video, which it stages under
//! a name derived from that path: reruns reuse one copy, and a new fixture
//! version gets a new one. Analysis runs with `fresh`, so every frame really
//! goes to the vision model instead of being answered from remembered verdicts.
//!
//! Cozy has one video job slot, shared with the user. The check never
//! disturbs a running job or one awaiting review (unexported work), and
//! deletes the file its own export wrote.

use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::{cozy::CozyClient, plugin::BitCheck};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cozy {
    url: String,
    token_file: PathBuf,
    /// Fixture clip, normally an anixdata store path.
    video: PathBuf,
    /// JSON expectations for `video`: prompt, stride, precision, match, negatives.
    expect_file: PathBuf,
    /// ffprobe executable, used to verify the exported file.
    ffprobe_command: PathBuf,
    /// Cozy's output directory; the export lands under it and is deleted.
    output_dir: PathBuf,
    /// Budget for analysis plus export.
    #[serde(default = "max_seconds")]
    max_seconds: u64,
    /// Allowed difference between the exported and expected durations.
    #[serde(default = "duration_tolerance")]
    duration_tolerance: f64,
}
fn max_seconds() -> u64 {
    600
}
fn duration_tolerance() -> f64 {
    0.3
}

#[derive(Deserialize)]
struct Expect {
    prompt: String,
    stride: f64,
    precision: f64,
    #[serde(rename = "match")]
    matched: Window,
    /// [start, end] windows no segment may overlap.
    negatives: Vec<[f64; 2]>,
}
#[derive(Deserialize)]
struct Window {
    start: f64,
    end: f64,
    tolerance: f64,
}

const POLL: Duration = Duration::from_secs(2);
const BASENAME: &str = "bitd-cozy-video";
const RUNNING: [&str; 4] = ["staging", "probing", "analyzing", "exporting"];

#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    start: f64,
    end: f64,
}

fn segments(status: &Value) -> Result<Vec<Segment>> {
    status["segments"]
        .as_array()
        .context("status has no segments")?
        .iter()
        .map(|s| {
            Ok(Segment {
                start: s["start"].as_f64().context("segment start")?,
                end: s["end"].as_f64().context("segment end")?,
            })
        })
        .collect()
}

/// Index of the segment matching the expected window, after checking that no
/// segment strays into a negative window.
fn judge(segs: &[Segment], expect: &Expect) -> Result<usize> {
    for s in segs {
        for [a, b] in &expect.negatives {
            ensure!(
                !(s.start < *b && s.end > *a),
                "segment {:.2}-{:.2} s overlaps the no-match window {a}-{b} s",
                s.start,
                s.end
            );
        }
    }
    let m = &expect.matched;
    segs.iter()
        .position(|s| {
            (s.start - m.start).abs() <= m.tolerance && (s.end - m.end).abs() <= m.tolerance
        })
        .with_context(|| {
            format!(
                "no segment within {} s of {}-{} s; found {}",
                m.tolerance,
                m.start,
                m.end,
                if segs.is_empty() {
                    "none".to_owned()
                } else {
                    segs.iter()
                        .map(|s| format!("{:.2}-{:.2}", s.start, s.end))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )
        })
}

impl Cozy {
    fn expect(&self) -> Result<Expect> {
        let text = std::fs::read_to_string(&self.expect_file)
            .with_context(|| format!("read {}", self.expect_file.display()))?;
        let expect: Expect = serde_json::from_str(&text).context("parse expect_file")?;
        ensure!(
            !expect.prompt.trim().is_empty(),
            "expect_file has an empty prompt"
        );
        ensure!(
            expect.stride > 0.0 && expect.precision > 0.0,
            "expect_file stride/precision must be positive"
        );
        ensure!(
            expect.matched.end > expect.matched.start,
            "expect_file match window is empty"
        );
        Ok(expect)
    }

    /// Poll until the job leaves the running states, or cancel it at `deadline`.
    async fn wait(&self, cozy: &CozyClient, deadline: Instant, what: &str) -> Result<Value> {
        loop {
            let status = cozy.get("video/status").await?;
            let state = status["state"].as_str().unwrap_or("");
            if !RUNNING.contains(&state) {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                if state != "exporting" {
                    match cozy.post("video/cancel", &json!({})).await {
                        Ok((s, _)) if s.is_success() => println!("cancelled the video job"),
                        Ok((s, _)) => println!("cancel failed: HTTP {s}"),
                        Err(e) => println!("cancel failed: {e}"),
                    }
                }
                bail!("{what} did not finish within {} seconds", self.max_seconds);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn probe(&self, file: &Path) -> Result<(f64, bool, bool)> {
        let output = tokio::process::Command::new(&self.ffprobe_command)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(file)
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("run {}", self.ffprobe_command.display()))?;
        ensure!(output.status.success(), "ffprobe failed: {}", output.status);
        let info: Value = serde_json::from_slice(&output.stdout).context("parse ffprobe output")?;
        let duration = info["format"]["duration"]
            .as_str()
            .and_then(|d| d.parse().ok())
            .context("ffprobe reported no duration")?;
        let has = |kind: &str| {
            info["streams"]
                .as_array()
                .is_some_and(|ss| ss.iter().any(|s| s["codec_type"] == kind))
        };
        Ok((duration, has("video"), has("audio")))
    }

    async fn run(&self, cozy: &CozyClient, expect: &Expect) -> Result<()> {
        let before = cozy.get("video/status").await?;
        let state = before["state"].as_str().unwrap_or("");
        ensure!(
            !RUNNING.contains(&state),
            "Cozy's video job is busy ({state}); retry when idle"
        );
        ensure!(
            state != "review",
            "a Cozy video job is awaiting review; export or replace it, then rerun"
        );
        let deadline = Instant::now() + Duration::from_secs(self.max_seconds);
        let video = self.video.to_str().context("video path is not UTF-8")?;
        let (code, body) = cozy
            .post(
                "video/analyze",
                &json!({"prompt": expect.prompt, "remote_video": {"host": "", "path": video},
                        "stride": expect.stride, "precision": expect.precision, "fresh": true}),
            )
            .await?;
        ensure!(
            code != StatusCode::CONFLICT,
            "Cozy is busy (another job holds the GPU); retry when idle"
        );
        ensure!(
            code.is_success(),
            "analyze failed: HTTP {code} {}",
            body["error"]
        );
        println!("analyzing {video} for {:?}", expect.prompt);

        let status = self.wait(cozy, deadline, "analysis").await?;
        ensure!(
            status["state"] == "review",
            "analysis ended {}: {}",
            status["state"],
            status["error"]
        );
        let calls = status["calls_done"].as_u64().unwrap_or(0);
        let secs = status["avg_call_s"].as_f64();
        println!(
            "judged {calls} frames at {} s/frame",
            secs.map_or("?".into(), |s| format!("{s:.2}"))
        );
        ensure!(
            secs.is_some() && calls > 0,
            "no frame reached the vision model"
        );
        let segs = segments(&status)?;
        println!(
            "segments: {}",
            segs.iter()
                .map(|s| format!("{:.2}-{:.2}", s.start, s.end))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let hit = judge(&segs, expect)?;

        let keep: Vec<bool> = (0..segs.len()).map(|i| i == hit).collect();
        let (code, body) = cozy
            .post("video/export", &json!({"keep": keep, "basename": BASENAME}))
            .await?;
        ensure!(
            code.is_success(),
            "export failed: HTTP {code} {}",
            body["error"]
        );
        let status = self.wait(cozy, deadline, "export").await?;
        let output = status["output"].as_str().map(|o| self.output_dir.join(o));
        let result = async {
            ensure!(
                status["state"] == "done",
                "export ended {}: {}",
                status["state"],
                status["error"]
            );
            let file = output
                .as_deref()
                .context("export reported no output file")?;
            let (duration, video, audio) = self.probe(file).await?;
            let want = segs[hit].end - segs[hit].start;
            println!(
                "exported {:.2} s (segment {want:.2} s), video={video} audio={audio}",
                duration
            );
            ensure!(video && audio, "export lacks a video or audio stream");
            ensure!(
                (duration - want).abs() <= self.duration_tolerance,
                "export is {duration:.2} s, segment is {want:.2} s"
            );
            Ok(())
        }
        .await;
        // A cleanup failure fails an otherwise passing check: left alone it
        // would add another bitd-cozy-video_N.mp4 to the output dir every run.
        let cleanup = match output {
            Some(file) => match std::fs::remove_file(&file) {
                Ok(()) => {
                    println!("removed {}", file.display());
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(anyhow::anyhow!(
                    "could not remove the export {}: {e}",
                    file.display()
                )),
            },
            None => Ok(()),
        };
        result.and(cleanup)
    }
}

#[async_trait]
impl BitCheck for Cozy {
    fn remediation(&self) -> String {
        format!(
            "Cozy's Video tab ({url}) failed to trim {video} as {expect} expects, within {max} s.\n\
             - Services: `systemctl status cozy ollama`; `journalctl -u cozy -u ollama -n 200`.\n\
             - Vision model: `ollama ps` while it runs; `ollama list` must include the \
             model cozy's --video-model names.\n\
             - Wrong segments: the log lists them; open the Video tab to see each sampled \
             frame's verdict on the timeline. A model swap may need new expectations.\n\
             - \"awaiting review\" or \"busy\": finish or replace the job in the Video tab, \
             then rerun.\n\
             - 401/403: the token in {token} must match Cozy's api_token_hash.\n\
             - \"could not remove the export\": bitd's unit must allow writes to \
             {out}/video (services.bitd.plugins.cozy_video.writablePaths).",
            url = self.url,
            video = self.video.display(),
            expect = self.expect_file.display(),
            max = self.max_seconds,
            token = self.token_file.display(),
            out = self.output_dir.display(),
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(self.max_seconds > 0, "max_seconds must be positive");
        ensure!(
            self.duration_tolerance > 0.0,
            "duration_tolerance must be positive"
        );
        ensure!(
            self.video.is_file(),
            "fixture {} is missing",
            self.video.display()
        );
        let expect = self.expect()?;
        // No restart command: Cozy frees ComfyUI before analysis and unloads
        // the vision model afterwards on its own.
        let cozy = CozyClient::new(&self.url, &self.token_file, &[])?;
        self.run(&cozy, &expect).await
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Cozy>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect() -> Expect {
        Expect {
            prompt: "p".into(),
            stride: 2.0,
            precision: 1.0,
            matched: Window {
                start: 7.5,
                end: 10.0,
                tolerance: 1.5,
            },
            negatives: vec![[0.0, 2.0], [10.75, 13.8]],
        }
    }
    fn seg(start: f64, end: f64) -> Segment {
        Segment { start, end }
    }

    #[test]
    fn finds_the_match_among_harmless_extras() {
        assert_eq!(
            judge(&[seg(4.0, 5.0), seg(8.0, 10.0)], &expect()).unwrap(),
            1
        );
        assert_eq!(judge(&[seg(6.5, 10.7)], &expect()).unwrap(), 0);
    }

    #[test]
    fn rejects_misses_and_negative_overlaps() {
        let e = judge(&[], &expect()).unwrap_err().to_string();
        assert!(e.contains("found none"), "{e}");
        let e = judge(&[seg(8.0, 12.0)], &expect()).unwrap_err().to_string();
        assert!(e.contains("no-match window 10.75-13.8"), "{e}");
        let e = judge(&[seg(1.0, 1.05), seg(8.0, 10.0)], &expect())
            .unwrap_err()
            .to_string();
        assert!(e.contains("0-2"), "{e}");
        assert!(judge(&[seg(5.0, 9.0)], &expect()).is_err());
    }

    #[test]
    fn touching_a_negative_window_is_not_overlap() {
        assert!(judge(&[seg(8.0, 10.75)], &expect()).is_ok());
    }
}
