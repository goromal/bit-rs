use anyhow::{bail, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Internet {
    #[serde(default = "urls")]
    urls: Vec<String>,
}
fn urls() -> Vec<String> {
    vec![
        "https://www.gstatic.com/generate_204".into(),
        "https://cp.cloudflare.com/generate_204".into(),
    ]
}
#[async_trait]
impl BitCheck for Internet {
    fn remediation(&self) -> String {
        format!(
            "No endpoint ({}) returned HTTP 204.\n\
             - Check the link: `ip addr`, `nmcli device status`, Wi-Fi/Ethernet cable.\n\
             - Check DNS: `resolvectl query www.gstatic.com`.\n\
             - A captive portal (hotel/guest Wi-Fi) needs a browser login first.\n\
             - `journalctl -u bitd` shows the error for each endpoint.",
            self.urls.join(", ")
        )
    }
    async fn check(&self) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        for url in &self.urls {
            match client.get(url).send().await {
                Ok(r) if r.status() == reqwest::StatusCode::NO_CONTENT => {
                    println!("internet reachable via {url}");
                    return Ok(());
                }
                Ok(r) => println!("{url}: unexpected status {}", r.status()),
                Err(e) => println!("{url}: {e}"),
            }
        }
        bail!("no internet endpoint returned HTTP 204")
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<Internet>().await
}
