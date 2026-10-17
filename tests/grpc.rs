use bit_rs::{
    config::{CheckConfig, Config, Kind},
    daemon::Service,
    proto::{
        bit_service_client::BitServiceClient, bit_service_server::BitServiceServer, ListRequest,
        RunRequest,
    },
};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn grpc_and_cli_round_trip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (_stop, rx) = tokio::sync::watch::channel(false);
    let dir = tempfile::tempdir().unwrap();
    let command = dir.path().join("hint");
    let shell = std::env::var("BIT_TEST_SHELL").unwrap_or_else(|_| "/bin/sh".into());
    std::fs::write(
        &command,
        format!("#!{shell}\necho 'Try turning it off and on.'\n"),
    )
    .unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    let service = Service::new(
        Config {
            listen: address,
            checks: vec![CheckConfig {
                name: "hint".into(),
                kind: Kind::Regression,
                command,
                rate_hz: None,
                timeout_ms: 1000,
                config: serde_json::json!({}),
            }],
        },
        rx,
    );
    service.load_remediation().await;
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BitServiceServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let endpoint = format!("http://{address}");
    let mut client = BitServiceClient::connect(endpoint.clone()).await.unwrap();
    let checks = client
        .list(ListRequest {})
        .await
        .unwrap()
        .into_inner()
        .checks;
    assert_eq!(checks[0].remediation, "Try turning it off and on.");
    assert_eq!(
        client
            .run(RunRequest {
                names: vec!["missing".into()]
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bitctl"))
        .args(["--endpoint", &endpoint, "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed[0]["remediation"], "Try turning it off and on.");
    server.abort();
}
