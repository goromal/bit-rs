use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use serde_json::{json, Value};
use std::{
    io::Cursor,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::io::AsyncWriteExt;

#[derive(Clone)]
struct Mock {
    mode: &'static str,
    requests: Arc<Mutex<Vec<String>>>,
}
async fn endpoint(
    State(mock): State<Mock>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    mock.requests.lock().unwrap().push(path.clone());
    if path == "204" {
        return StatusCode::NO_CONTENT.into_response();
    }
    if path == "portal" {
        return "login page".into_response();
    }
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer test-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let id = "0123456789abcdef0123456789abcdef";
    let value = match path.as_str() {
        "api/queue/status" => {
            let uploaded = mock
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r == "api/queue/add");
            if !uploaded {
                json!({"active":mock.mode == "busy", "current":null, "jobs":[], "results":[]})
            } else if mock.mode == "timeout" {
                json!({"results":[{"id":"another-job", "status":"success"}]})
            } else {
                json!({"results":[{"id":id, "status":if mock.mode == "failed" {"failed"} else {"success"}, "error":"mock failure"}]})
            }
        }
        "api/upload-image" => {
            assert!(body.windows(8).any(|w| w == b"\x89PNG\r\n\x1a\n"));
            json!({"value":"input-fixture.png"})
        }
        "api/queue/add" => {
            let data: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(data["workflow"], "imgedit2");
            assert_eq!(data["image"], "input-fixture.png");
            json!({"id":id})
        }
        "api/queue/start" | "api/queue/remove" => json!({"ok":true}),
        "api/queue/image" => {
            if mock.mode == "invalid" {
                return "not an image".into_response();
            }
            let image = image::RgbImage::new(8, 8);
            let mut bytes = Cursor::new(Vec::new());
            image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            return bytes.into_inner().into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(value).into_response()
}
async fn server(mode: &'static str) -> (String, Mock, tokio::task::JoinHandle<()>) {
    let mock = Mock {
        mode,
        requests: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/*path", any(endpoint))
        .with_state(mock.clone());
    let handle = tokio::spawn(async {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), mock, handle)
}
async fn plugin(binary: &str, config: Value) -> std::process::Output {
    let mut child = tokio::process::Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(config.to_string().as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn internet_success_fallback_captive_portal_and_bad_config() {
    let (url, _, server) = server("success").await;
    let binary = env!("CARGO_BIN_EXE_bit-check-internet");
    assert!(plugin(
        binary,
        json!({"urls":[format!("{url}/portal"), format!("{url}/204")]})
    )
    .await
    .status
    .success());
    assert!(!plugin(binary, json!({"urls":[format!("{url}/portal")]}))
        .await
        .status
        .success());
    assert!(!plugin(binary, json!({"unknown":true}))
        .await
        .status
        .success());
    server.abort();
}
#[tokio::test]
async fn cozy_tracks_result_validates_image_and_cleans_up_only_its_job() {
    for mode in ["success", "busy", "failed", "invalid", "timeout"] {
        let (url, mock, server) = server(mode).await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-token\n").unwrap();
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-cozy-imgedit2"),
            json!({"url":url,"token_file":token,"max_seconds":1}),
        )
        .await;
        assert_eq!(
            output.status.success(),
            mode == "success",
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = mock.requests.lock().unwrap();
        if mode == "busy" {
            assert_eq!(requests.as_slice(), ["api/queue/status"]);
        }
        if ["failed", "invalid", "timeout"].contains(&mode) {
            assert!(requests.contains(&"api/queue/remove".to_string()));
        }
        assert!(!requests
            .iter()
            .any(|r| r.contains("clear") || r.contains("stop")));
        server.abort();
    }
}
