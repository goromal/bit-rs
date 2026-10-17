use bit_rs::{
    config::Config,
    daemon::Service,
    proto::{
        bit_service_client::BitServiceClient, bit_service_server::BitServiceServer, ListRequest,
        RunRequest,
    },
};

#[tokio::test]
async fn grpc_and_cli_round_trip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (_stop, rx) = tokio::sync::watch::channel(false);
    let service = Service::new(
        Config {
            listen: address,
            checks: vec![],
        },
        rx,
    );
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BitServiceServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let endpoint = format!("http://{address}");
    let mut client = BitServiceClient::connect(endpoint.clone()).await.unwrap();
    assert!(client
        .list(ListRequest {})
        .await
        .unwrap()
        .into_inner()
        .checks
        .is_empty());
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
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!([])
    );
    server.abort();
}
