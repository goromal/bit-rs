use anyhow::Result;
use bit_rs::{config::Config, daemon::Service, proto::bit_service_server::BitServiceServer};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Machine health and regression daemon")]
struct Args {
    #[arg(long, default_value = "/etc/bitd.json")]
    config: std::path::PathBuf,
    #[arg(long)]
    check_config: bool,
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config: Config = serde_json::from_reader(std::fs::File::open(args.config)?)?;
    config.validate()?;
    if args.check_config {
        println!("configuration valid");
        return Ok(());
    }
    let address = config.listen;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let service = Service::new(config, rx);
    service.load_remediation().await;
    service.start_health().await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    println!("bitd listening on {address}");
    let result = tonic::transport::Server::builder()
        .add_service(BitServiceServer::new(service.clone()))
        .serve_with_shutdown(address, async {
            tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = term.recv() => () };
            let _ = stop.send(true);
        })
        .await;
    let _ = stop.send(true);
    service.drain().await;
    result?;
    Ok(())
}
