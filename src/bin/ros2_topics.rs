use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ros2Topics {
    /// The `ros2` CLI from the machine's ROS environment.
    ros2: PathBuf,
    /// Topics that must each deliver a message.
    topics: Vec<Topic>,
    #[serde(default = "max_seconds")]
    max_seconds: f64,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Topic {
    name: String,
    /// Message type, e.g. `builtin_interfaces/msg/Time`. Optional, but setting
    /// it lets the subscription wait out discovery instead of failing early.
    #[serde(rename = "type")]
    msg_type: Option<String>,
}
fn max_seconds() -> f64 {
    8.0
}

/// Receive one message on `topic` through `ros2 topic echo --once`.
/// `--no-daemon` keeps the CLI from spawning a ros2 daemon that would outlive
/// this check; its `--timeout` exits 0, so the deadline is enforced here.
async fn receive(ros2: PathBuf, topic: Topic, max_seconds: f64, ros_home: PathBuf) -> Result<()> {
    let mut command = tokio::process::Command::new(&ros2);
    command
        .args([
            "topic",
            "echo",
            "--once",
            "--no-daemon",
            "--no-arr",
            "--no-str",
        ])
        .arg(&topic.name)
        .args(&topic.msg_type)
        .env("ROS_HOME", &ros_home)
        .env("ROS_LOG_DIR", ros_home.join("log"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs_f64(max_seconds), command.output())
        .await
        .with_context(|| format!("no message on {} within {} s", topic.name, max_seconds))?
        .with_context(|| format!("run {}", ros2.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "ros2 topic echo {} failed ({}): {}",
            topic.name,
            output.status,
            text.trim()
        );
    }
    ensure!(
        !text.trim().is_empty(),
        "ros2 topic echo {} exited without a message",
        topic.name
    );
    println!("{}: message received", topic.name);
    Ok(())
}

#[async_trait]
impl BitCheck for Ros2Topics {
    fn remediation(&self) -> String {
        let names: Vec<_> = self.topics.iter().map(|t| t.name.as_str()).collect();
        format!(
            "Expected a message on each of {} within {} s.\n\
             - Graph: `ros2 topic list` and `ros2 topic hz <topic>` as the same user.\n\
             - Ardupilot topics (/ap/*): `systemctl status microxrce-agent ardusitl`; \
             `journalctl -u microxrce-agent -n 100` should show a session from AP_DDS.\n\
             - ROS_DOMAIN_ID and RMW_IMPLEMENTATION must match the publishers.",
            names.join(", "),
            self.max_seconds
        )
    }
    async fn check(&self) -> Result<()> {
        ensure!(!self.topics.is_empty(), "topics must not be empty");
        ensure!(
            self.max_seconds.is_finite() && self.max_seconds > 0.0,
            "max_seconds must be positive"
        );
        for t in &self.topics {
            ensure!(
                t.name.starts_with('/') && !t.name.contains(char::is_whitespace),
                "invalid topic name: {:?}",
                t.name
            );
        }
        // bitd's sandbox has a read-only home; keep ROS logs and state here.
        let ros_home = tempfile::tempdir().context("create ROS_HOME")?;
        let mut receivers = tokio::task::JoinSet::new();
        for topic in &self.topics {
            receivers.spawn(receive(
                self.ros2.clone(),
                topic.clone(),
                self.max_seconds,
                ros_home.path().to_owned(),
            ));
        }
        let mut errors = Vec::new();
        while let Some(result) = receivers.join_next().await {
            if let Err(e) = result? {
                errors.push(format!("{e:#}"));
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Ros2Topics>().await
}
