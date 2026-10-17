use bit_rs::{
    config::{CheckConfig, Config, Kind},
    daemon::Service,
    proto::State,
};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::sync::watch;

fn check(dir: &std::path::Path, name: &str, body: &str, timeout_ms: u64) -> CheckConfig {
    let path = dir.join(name);
    let shell = std::env::var("BIT_TEST_SHELL").unwrap_or_else(|_| "/bin/sh".into());
    std::fs::write(&path, format!("#!{shell}\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    CheckConfig {
        name: name.into(),
        command: path,
        kind: Kind::Regression,
        rate_hz: None,
        timeout_ms,
        config: serde_json::json!({}),
    }
}
async fn settled(service: &Service) -> Vec<bit_rs::proto::Check> {
    tokio::time::timeout(Duration::from_secs(5), service.drain())
        .await
        .unwrap();
    service.snapshot().await
}
#[tokio::test]
async fn parallel_outcomes_and_atomic_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        checks: vec![
            check(dir.path(), "pass", "sleep 0.2; exit 0", 2000),
            check(dir.path(), "fail", "exit 7", 2000),
            check(dir.path(), "timeout", "sleep 20", 100),
        ],
    };
    config.validate().unwrap();
    let (_stop, rx) = watch::channel(false);
    let service = Service::new(config, rx);
    assert!(service
        .launch(vec!["pass".into(), "missing".into()])
        .await
        .is_err());
    assert!(service
        .launch(vec!["pass".into(), "pass".into()])
        .await
        .is_err());
    assert!(service
        .snapshot()
        .await
        .iter()
        .all(|c| c.state == State::Idle as i32));
    let started = service.launch(vec![]).await.unwrap();
    assert_eq!(started.len(), 3);
    assert!(started.iter().all(|c| c.state == State::Running as i32));
    assert!(service.launch(vec!["pass".into()]).await.is_err());
    let finished = settled(&service).await;
    assert_eq!(
        finished
            .iter()
            .map(|c| (c.name.as_str(), c.state))
            .collect::<Vec<_>>(),
        vec![
            ("fail", State::Failed as i32),
            ("pass", State::Passed as i32),
            ("timeout", State::TimedOut as i32)
        ]
    );
    assert!(finished
        .iter()
        .all(|c| c.finished_unix_ms >= c.started_unix_ms && c.run_id == 1));
}
#[tokio::test]
async fn health_schedule_never_overlaps_and_regressions_stay_idle() {
    let dir = tempfile::tempdir().unwrap();
    let mut health = check(dir.path(), "health", "sleep 0.1", 1000);
    health.kind = Kind::Health;
    health.rate_hz = Some(50.0);
    let (stop, rx) = watch::channel(false);
    let service = Service::new(
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            checks: vec![health, check(dir.path(), "regression", "exit 0", 1000)],
        },
        rx,
    );
    service.start_health().await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    let checks = service.snapshot().await;
    assert!((2..=4).contains(&checks[0].run_id));
    assert_eq!(checks[1].run_id, 0);
    stop.send(true).unwrap();
    settled(&service).await;
    assert!(service.launch(vec![]).await.is_err());
}
#[tokio::test]
async fn shutdown_and_timeout_kill_helpers() {
    for shutdown in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("orphan");
        let body = format!("(sleep 0.4; touch '{}') &\nwait", marker.display());
        let (stop, rx) = watch::channel(false);
        let service = Service::new(
            Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                checks: vec![check(
                    dir.path(),
                    "child",
                    &body,
                    if shutdown { 5000 } else { 100 },
                )],
            },
            rx,
        );
        service.launch(vec![]).await.unwrap();
        if shutdown {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop.send(true).unwrap();
        }
        settled(&service).await;
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(!marker.exists(), "plugin helper outlived its process group");
    }
}
#[test]
fn reject_invalid_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        checks: vec![check(dir.path(), "test", "exit 0", 1000)],
    };
    config.validate().unwrap();
    config.listen = "0.0.0.0:40041".parse().unwrap();
    assert!(config.validate().is_err());
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.checks[0].kind = Kind::Health;
    assert!(config.validate().is_err());
    for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        config.checks[0].rate_hz = Some(rate);
        assert!(config.validate().is_err());
    }
    config.checks[0].rate_hz = Some(0.2);
    config.validate().unwrap();
    config.checks.push(config.checks[0].clone());
    assert!(config.validate().is_err());
}
