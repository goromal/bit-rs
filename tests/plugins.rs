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
/// Executable script with `body`, for fake external tools.
fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let shell = std::env::var("BIT_TEST_SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let path = dir.join(name);
    std::fs::write(&path, format!("#!{shell}\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
/// Fake ComfyUI restart: appends its arguments to `<dir>/restarts`.
fn fake_restart(dir: &std::path::Path) -> (Value, std::path::PathBuf) {
    let log = dir.join("restarts");
    let path = script(
        dir,
        "restart",
        &format!("echo \"$@\" >> '{}'", log.display()),
    );
    (json!([path, "restart", "comfyui.service"]), log)
}
fn restarts(log: &std::path::Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
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
        let (restart, log) = fake_restart(dir.path());
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-cozy-imgedit2"),
            json!({"url":url,"token_file":token,"max_seconds":1,"png_command":png,
                   "seed":"seed7","width":64,"height":64,"restart_command":restart}),
        )
        .await;
        // Every submitted job ends with exactly one restart; a busy queue
        // means someone else's job, which must not be disturbed.
        let expected = if mode == "busy" {
            ""
        } else {
            "restart comfyui.service\n"
        };
        assert_eq!(restarts(&log), expected, "{mode}");
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
    let (restart, log) = fake_restart(dir.path());
    let output = plugin(
        env!("CARGO_BIN_EXE_bit-check-cozy-imgedit2"),
        json!({"url":url,"token_file":token,"png_command":png,"seed":"other",
               "width":64,"height":64,"restart_command":restart}),
    )
    .await;
    assert!(!output.status.success());
    assert_eq!(restarts(&log), "");
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
        let (restart, log) = fake_restart(dir.path());
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-cozy-imggen-quantized"),
            json!({"url":url,"token_file":token,"max_seconds":1,"width":130,"height":130,
                   "restart_command":restart}),
        )
        .await;
        let expected = if mode == "busy" {
            ""
        } else {
            "restart comfyui.service\n"
        };
        assert_eq!(restarts(&log), expected, "{mode}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            mode == "success",
            "{mode}: {stderr}"
        );
        let requests = mock.requests.lock().unwrap();
        match mode {
            "aspect" => assert!(stderr.contains("times a whole factor"), "{stderr}"),
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
#[tokio::test]
async fn cozy_fails_when_comfyui_restart_fails_but_reports_job_errors_first() {
    for (mode, restart, expected) in [
        ("success", "exit 1", "ComfyUI restart failed"),
        ("failed", "exit 1", "Cozy job failed"),
        ("success", "exit 0", ""),
    ] {
        let (url, _mock, server) = server(mode).await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "test-token\n").unwrap();
        let restart = script(dir.path(), "restart", restart);
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-cozy-imggen-quantized"),
            json!({"url":url,"token_file":token,"max_seconds":1,"width":130,"height":130,
                   "restart_command":[restart]}),
        )
        .await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), expected.is_empty(), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
        server.abort();
    }
}
/// MAVLink v2 HEARTBEAT from `system_id` with `autopilot`, CRC included.
fn heartbeat(system_id: u8, autopilot: u8) -> Vec<u8> {
    let mut frame = vec![
        0xfd, 9, 0, 0, 0, system_id, 1, 0, 0, 0, 0, 0, 0, 0, 2, autopilot, 0x51, 3, 3,
    ];
    let mut crc: u16 = 0xffff;
    for &b in frame[1..].iter().chain(std::iter::once(&50u8)) {
        let mut tmp = b ^ (crc & 0xff) as u8;
        tmp ^= tmp << 4;
        crc = (crc >> 8) ^ (u16::from(tmp) << 8) ^ (u16::from(tmp) << 3) ^ (u16::from(tmp) >> 4);
    }
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}
/// A one-connection MAVLink TCP endpoint that streams `frames` in small chunks.
async fn mavlink_server(frames: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            for chunk in frames.chunks(7) {
                if stream.write_all(chunk).await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            // Hold the connection open like a quiet router.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
    (address, handle)
}
#[tokio::test]
async fn ardupilot_needs_an_autopilot_heartbeat() {
    let binary = env!("CARGO_BIN_EXE_bit-check-ardupilot");
    let mut gcs_then_copter = heartbeat(255, 8);
    gcs_then_copter.extend(heartbeat(1, 3));
    for (frames, system_id, passes) in [
        (gcs_then_copter.clone(), json!(1), true),
        (gcs_then_copter, json!(2), false),
        (heartbeat(255, 8), json!(null), false),
        (vec![], json!(null), false),
    ] {
        let (address, server) = mavlink_server(frames).await;
        let output = plugin(
            binary,
            json!({"address":address,"max_seconds":0.5,"system_id":system_id}),
        )
        .await;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.success(), passes, "{stdout}");
        if !passes {
            assert!(stdout.contains("no autopilot heartbeat"), "{stdout}");
        }
        server.abort();
    }
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap().to_string();
    drop(closed);
    let output = plugin(binary, json!({"address":address})).await;
    assert!(!output.status.success());
    assert!(!plugin(binary, json!({"address":"x:1","max_seconds":0}))
        .await
        .status
        .success());
}
/// Fake `ros2`: `topic echo --once --no-daemon ... <topic> [type]` behaves per topic.
fn fake_ros2(dir: &std::path::Path) -> std::path::PathBuf {
    script(
        dir,
        "ros2",
        r#"[ "$1 $2 $3 $4" = "topic echo --once --no-daemon" ] || exit 9
[ -n "$ROS_HOME" ] && [ -n "$ROS_LOG_DIR" ] || exit 8
for topic; do :; done
case "$*" in
  */ap/time*) echo "sec: 1"; echo "nanosec: 2" ;;
  */silent*) exec sleep 30 ;;
  */empty*) exit 0 ;;
  *) echo "WARNING: topic [$topic] does not appear to be published yet"; exit 1 ;;
esac"#,
    )
}
#[tokio::test]
async fn ros2_topics_need_a_message_on_every_topic() {
    let dir = tempfile::tempdir().unwrap();
    let ros2 = fake_ros2(dir.path());
    let binary = env!("CARGO_BIN_EXE_bit-check-ros2-topics");
    let topic = |name: &str| json!({"name":name, "type":"builtin_interfaces/msg/Time"});
    for (topics, expected) in [
        (json!([topic("/ap/time")]), ""),
        (json!([{"name":"/ap/time"}, topic("/ap/time")]), ""),
        (
            json!([topic("/ap/time"), topic("/silent")]),
            "no message on /silent within 0.5 s",
        ),
        (json!([topic("/empty")]), "exited without a message"),
        (
            json!([{"name":"/missing"}]),
            "does not appear to be published",
        ),
        (json!([]), "topics must not be empty"),
        (json!([{"name":"no_slash"}]), "invalid topic name"),
    ] {
        let started = std::time::Instant::now();
        let output = plugin(
            binary,
            json!({"ros2":ros2,"topics":topics,"max_seconds":0.5}),
        )
        .await;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.success(), expected.is_empty(), "{stdout}");
        assert!(stdout.contains(expected), "{stdout}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
fn fake_nvidia_smi(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let shell = std::env::var("BIT_TEST_SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let path = dir.join("nvidia-smi");
    std::fs::write(
        &path,
        format!(
            "#!{shell}\n[ \"$1\" = --query-gpu=index,enforced.power.limit,power.default_limit,name ] || exit 3\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
#[tokio::test]
async fn nvidia_power_flags_capped_gpus() {
    let cases = [
        ("echo '0, 30.00, 30.00, RTX 500'", true, ""),
        ("echo '0, 20.00, 30.00, RTX 500'", true, ""),
        (
            "echo '0, 6.00, 30.00, RTX 500'",
            false,
            "capped at 6.00 W of 30.00 W",
        ),
        (
            "echo '0, [N/A], [N/A], A'; echo '1, 6.00, 30.00, B'",
            false,
            "GPU 1 (B) capped",
        ),
        ("echo '0, [N/A], [N/A], RTX 500'", false, "no GPU reported"),
        ("echo 'garbage'", false, "unexpected nvidia-smi row"),
        (
            "echo 'no devices' >&2; exit 9",
            false,
            "nvidia-smi failed: no devices",
        ),
    ];
    for (body, pass, message) in cases {
        let dir = tempfile::tempdir().unwrap();
        let smi = fake_nvidia_smi(dir.path(), body);
        let output = plugin(
            env!("CARGO_BIN_EXE_bit-check-nvidia-power"),
            json!({"nvidia_smi":smi}),
        )
        .await;
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.success(), pass, "{body}: {text}");
        assert!(text.contains(message), "{body}: {text}");
    }
    let output = plugin(
        env!("CARGO_BIN_EXE_bit-check-nvidia-power"),
        json!({"nvidia_smi":"/nonexistent/nvidia-smi"}),
    )
    .await;
    assert!(!output.status.success());
    for config in [json!({"bogus":1}), json!({"min_fraction":0.0})] {
        let output = plugin(env!("CARGO_BIN_EXE_bit-check-nvidia-power"), config).await;
        assert!(!output.status.success());
    }
}
async fn remediation(binary: &str, config: Value) -> String {
    let mut child = tokio::process::Command::new(binary)
        .arg("--remediation")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{binary}");
    String::from_utf8(output.stdout).unwrap()
}
#[tokio::test]
async fn every_plugin_gives_config_aware_remediation_without_checking() {
    let (url, mock, server) = server("success").await;
    let cases = [
        (
            env!("CARGO_BIN_EXE_bit-check-internet"),
            json!({"urls":["https://example.invalid/204"]}),
            "https://example.invalid/204",
        ),
        (
            env!("CARGO_BIN_EXE_bit-check-avahi"),
            json!({"hostname":"box.local"}),
            "avahi-resolve-host-name -4 box.local",
        ),
        (
            env!("CARGO_BIN_EXE_bit-check-nvidia-power"),
            json!({"nvidia_smi":"/opt/nvidia-smi"}),
            "/opt/nvidia-smi -q -d POWER",
        ),
        (
            env!("CARGO_BIN_EXE_bit-check-cozy-imgedit2"),
            json!({"url":url,"token_file":"/secret/token","png_command":"/bin/png"}),
            "/secret/token",
        ),
        (
            env!("CARGO_BIN_EXE_bit-check-cozy-imggen-quantized"),
            json!({"url":url,"token_file":"/secret/token","workflow":"wf9"}),
            "The wf9 job",
        ),
        (
            env!("CARGO_BIN_EXE_bit-check-ardupilot"),
            json!({"address":"127.0.0.1:5790"}),
            "ss -tlnp | grep 5790",
        ),
        (
            env!("CARGO_BIN_EXE_bit-check-ros2-topics"),
            json!({"ros2":"/bin/ros2","topics":[{"name":"/ap/time"}]}),
            "each of /ap/time",
        ),
    ];
    for (binary, config, expected) in cases {
        let text = remediation(binary, config).await;
        assert!(text.contains(expected), "{binary}: {text}");
        assert!(!text.contains("check started"), "{binary}: {text}");
    }
    assert!(mock.requests.lock().unwrap().is_empty());
    server.abort();
}
