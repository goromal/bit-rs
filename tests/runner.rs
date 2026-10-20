mod common;
use bit_rs::{
    config::{CheckConfig, Config, Kind},
    daemon::Service,
    proto::State,
};
use std::time::Duration;
use tokio::sync::watch;

fn check(dir: &std::path::Path, name: &str, body: &str, timeout_ms: u64) -> CheckConfig {
    let path = dir.join(name);
    common::write_script(&path, body);
    CheckConfig {
        name: name.into(),
        command: path,
        kind: Kind::Regression,
        rate_hz: None,
        timeout_ms,
        exclusive: false,
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
async fn exclusive_regressions_run_alone_and_health_keeps_going() {
    let dir = tempfile::tempdir().unwrap();
    // Each regression records its start and end; overlaps show up as an
    // exclusive check's interval intersecting another's.
    let log = dir.path().join("log");
    let body = |tag: &str| {
        format!(
            "echo start {tag} >> '{0}'; sleep 0.3; echo end {tag} >> '{0}'",
            log.display()
        )
    };
    let mut a = check(dir.path(), "a_exclusive", &body("a"), 2000);
    a.exclusive = true;
    let mut b = check(dir.path(), "b_exclusive", &body("b"), 2000);
    b.exclusive = true;
    let c = check(dir.path(), "c_shared", &body("c"), 2000);
    let d = check(dir.path(), "d_shared", &body("d"), 2000);
    let mut health = check(dir.path(), "health", "exit 0", 1000);
    health.kind = Kind::Health;
    health.rate_hz = Some(0.001);
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        checks: vec![a, b, c, d, health],
    };
    config.validate().unwrap();
    let (_stop, rx) = watch::channel(false);
    let service = Service::new(config, rx);
    let started = service.launch(vec![]).await.unwrap();
    assert_eq!(started.len(), 4);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Health is never held back by an exclusive regression.
    service.launch(vec!["health".into()]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let health = service.snapshot().await.pop().unwrap();
    assert_eq!(health.state, State::Passed as i32);
    let finished = settled(&service).await;
    assert!(finished.iter().all(|c| c.state == State::Passed as i32));
    let log = std::fs::read_to_string(&log).unwrap();
    let events: Vec<&str> = log.lines().collect();
    assert_eq!(events.len(), 8);
    let mut running = Vec::new();
    for event in events {
        let (what, tag) = event.split_once(' ').unwrap();
        if what == "start" {
            let exclusive = ["a", "b"].contains(&tag);
            assert!(
                running.is_empty()
                    || (!exclusive && !running.iter().any(|t| ["a", "b"].contains(t))),
                "{tag} started while {running:?} ran"
            );
            running.push(tag);
        } else {
            running.retain(|t| *t != tag);
        }
    }
    // Queued time does not count against the timeout or the reported start.
    assert!(finished
        .iter()
        .all(|c| c.finished_unix_ms - c.started_unix_ms < 1000));
}
#[tokio::test]
async fn shutdown_fails_queued_regressions() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = check(dir.path(), "a", "sleep 5", 10000);
    a.exclusive = true;
    let b = check(dir.path(), "b", "exit 0", 1000);
    let (stop, rx) = watch::channel(false);
    let service = Service::new(
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            checks: vec![a, b],
        },
        rx,
    );
    service.launch(vec!["a".into()]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    service.launch(vec!["b".into()]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued = service.snapshot().await;
    assert_eq!(queued[1].state, State::Running as i32);
    assert!(
        queued[1].detail.starts_with("queued"),
        "{}",
        queued[1].detail
    );
    stop.send(true).unwrap();
    let finished = settled(&service).await;
    assert!(finished.iter().all(|c| c.state == State::Failed as i32));
    assert!(finished.iter().all(|c| c.detail == "daemon stopped"));
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
    config.checks[0].exclusive = true;
    assert!(
        config.validate().is_err(),
        "health checks cannot be exclusive"
    );
    config.checks[0].exclusive = false;
    config.checks.push(config.checks[0].clone());
    assert!(config.validate().is_err());
}
#[tokio::test]
async fn remediation_is_queried_without_running_checks() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let ran = ran.display();
    let mut answers = check(
        dir.path(),
        "answers",
        &format!("[ \"$1\" = --remediation ] || {{ touch {ran}; exit 1; }}\ncat >/dev/null\nprintf '  Restart it.\\n'"),
        1000,
    );
    answers.config = serde_json::json!({"hint": 1});
    let broken = check(dir.path(), "broken", "exit 4", 1000);
    let silent = check(dir.path(), "silent", "cat >/dev/null", 1000);
    let (_stop, rx) = watch::channel(false);
    let service = Service::new(
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            checks: vec![answers, broken, silent],
        },
        rx,
    );
    service.load_remediation().await;
    let checks = service.snapshot().await;
    let text = |name: &str| {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .remediation
            .clone()
    };
    assert_eq!(text("answers"), "Restart it.");
    assert_eq!(text("broken"), "");
    assert_eq!(text("silent"), "");
    assert!(checks
        .iter()
        .all(|c| c.state == State::Idle as i32 && c.run_id == 0));
    assert!(!dir.path().join("ran").exists());
}
