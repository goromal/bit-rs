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
            if data["workflow"] == "imgedit2" {
                assert_eq!(data["image"], "input-fixture.png");
            } else {
                assert_eq!(data["workflow"], "imggen-quantized");
                assert_eq!(
                    (data["width"].as_u64(), data["height"].as_u64()),
                    (Some(130), Some(130))
                );
            }
            json!({"id":id})
        }
        "api/queue/start" | "api/queue/cancel" => json!({"ok":true}),
        "api/queue/image" => {
            if mock.mode == "invalid" {
                return "not an image".into_response();
            }
            // Edited result: grey 2x upscale (as an edit model may resize),
            // with a red block unless the mode tests a missing marking.
            let (w, h) = if mock.mode == "aspect" {
                (128, 96)
            } else {
                (128, 128)
            };
            let image = image::RgbImage::from_fn(w, h, |x, y| {
                if mock.mode != "nored" && (16..64).contains(&x) && (16..64).contains(&y) {
                    image::Rgb([230, 20, 25])
                } else {
                    image::Rgb([120, 120, 120])
                }
            });
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
/// Fake anixpkgs `png`: checks the `<seed>-<w>-<h>.random <out>` call and
/// writes a 64x64 noise fixture.
fn fake_png(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut state = 0x2545_f491u32;
    let noise = image::RgbImage::from_fn(64, 64, |_, _| {
        let mut px = [0u8; 3];
        for c in &mut px {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *c = (state >> 24) as u8;
        }
        image::Rgb(px)
    });
    let fixture = dir.join("noise.png");
    noise.save(&fixture).unwrap();
    let shell = std::env::var("BIT_TEST_SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let path = dir.join("png");
    std::fs::write(
        &path,
        format!(
            "#!{shell}\n[ \"$1\" = seed7-64-64.random ] || exit 3\nexec cp {} \"$2\"\n",
            fixture.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
#[tokio::test]
async fn cozy_tracks_result_validates_image_and_cleans_up_only_its_job() {
    for mode in [
        "success", "busy", "failed", "invalid", "timeout", "nored", "aspect",
    ] {
        let (url, mock, server) = server(mode).await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-token\n").unwrap();
        let png = fake_png(dir.path());
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-cozy-imgedit2"),
            json!({"url":url,"token_file":token,"max_seconds":1,"png_command":png,
                   "seed":"seed7","width":64,"height":64}),
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
        // Jobs that never yielded a usable result are cancelled; a finished
        // job whose output fails validation has nothing left to cancel.
        let cancelled = requests.contains(&"api/queue/cancel".to_string());
        assert_eq!(
            cancelled,
            ["failed", "invalid", "timeout"].contains(&mode),
            "{mode}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        if mode == "nored" {
            assert!(stderr.contains("no clear red markings"), "{stderr}");
        }
        if mode == "aspect" {
            assert!(stderr.contains("aspect ratio"), "{stderr}");
        }
        if mode == "timeout" {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("no output within 1 seconds"), "{stderr}");
            assert!(!requests.iter().any(|r| r == "api/queue/image"));
        }
        assert!(!requests
            .iter()
            .any(|r| r.contains("clear") || r.contains("stop")));
        server.abort();
    }
}
#[tokio::test]
async fn cozy_fails_before_queueing_when_png_fails() {
    let (url, mock, server) = server("success").await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "test-token\n").unwrap();
    let png = fake_png(dir.path());
    let output = plugin(
        env!("CARGO_BIN_EXE_bit-check-cozy-imgedit2"),
        json!({"url":url,"token_file":token,"png_command":png,"seed":"other",
               "width":64,"height":64}),
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(
        mock.requests.lock().unwrap().as_slice(),
        ["api/queue/status"]
    );
    server.abort();
}
#[tokio::test]
async fn cozy_imggen_checks_size_content_and_deadline() {
    for mode in ["success", "aspect", "nored", "timeout", "failed", "busy"] {
        let (url, mock, server) = server(mode).await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-token\n").unwrap();
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-cozy-imggen-quantized"),
            json!({"url":url,"token_file":token,"max_seconds":1,"width":130,"height":130}),
        )
        .await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            mode == "success",
            "{mode}: {stderr}"
        );
        let requests = mock.requests.lock().unwrap();
        match mode {
            "aspect" => assert!(stderr.contains("expected 130x130"), "{stderr}"),
            "nored" => assert!(stderr.contains("blank"), "{stderr}"),
            "timeout" => {
                assert!(stderr.contains("no output within 1 seconds"), "{stderr}");
                assert!(requests.contains(&"api/queue/cancel".to_string()));
            }
            "busy" => assert_eq!(requests.as_slice(), ["api/queue/status"]),
            _ => {}
        }
        assert!(!requests.iter().any(|r| r.contains("upload")));
        server.abort();
    }
}
