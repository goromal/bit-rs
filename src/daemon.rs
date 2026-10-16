use crate::{
    config::{CheckConfig, Config, Kind},
    proto::{
        self, bit_service_server::BitService, Check, ListRequest, ListResponse, RunRequest,
        RunResponse, State,
    },
};
use std::{
    collections::{BTreeMap, HashSet},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{watch, Mutex},
};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct Service {
    entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    shutdown: watch::Receiver<bool>,
}
struct Entry {
    config: CheckConfig,
    status: Check,
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl Service {
    pub fn new(config: Config, shutdown: watch::Receiver<bool>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(
                config
                    .checks
                    .into_iter()
                    .map(|c| {
                        let status = Check {
                            name: c.name.clone(),
                            kind: match c.kind {
                                Kind::Health => proto::Kind::Health as i32,
                                Kind::Regression => proto::Kind::Regression as i32,
                            },
                            state: State::Idle as i32,
                            rate_hz: c.rate_hz.unwrap_or(0.0),
                            timeout_ms: c.timeout_ms,
                            ..Default::default()
                        };
                        (c.name.clone(), Entry { config: c, status })
                    })
                    .collect(),
            )),
            shutdown,
        }
    }
    pub async fn start_health(&self) {
        for entry in self.entries.lock().await.values() {
            if let Some(rate) = entry.config.rate_hz {
                let service = self.clone();
                let name = entry.config.name.clone();
                tokio::spawn(async move {
                    let mut timer = tokio::time::interval(Duration::from_secs_f64(1.0 / rate));
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    let mut stop = service.shutdown.clone();
                    loop {
                        tokio::select! {
                            _ = stop.changed() => break,
                            _ = timer.tick() => { let _ = service.launch(vec![name.clone()]).await; }
                        }
                    }
                });
            }
        }
    }
    pub async fn snapshot(&self) -> Vec<Check> {
        self.entries
            .lock()
            .await
            .values()
            .map(|e| e.status.clone())
            .collect()
    }
    #[allow(clippy::result_large_err)] // Preserve the same error type as the gRPC API.
    pub async fn launch(&self, names: Vec<String>) -> Result<Vec<Check>, Status> {
        let mut entries = self.entries.lock().await;
        if *self.shutdown.borrow() {
            return Err(Status::unavailable("daemon is stopping"));
        }
        let names = if names.is_empty() {
            entries
                .values()
                .filter(|e| e.config.kind == Kind::Regression)
                .map(|e| e.config.name.clone())
                .collect()
        } else {
            names
        };
        let mut seen = HashSet::new();
        for name in &names {
            if !seen.insert(name) {
                return Err(Status::invalid_argument("duplicate check name"));
            }
            let e = entries
                .get(name)
                .ok_or_else(|| Status::not_found(format!("unknown check: {name}")))?;
            if e.status.state == State::Running as i32 {
                return Err(Status::failed_precondition(format!(
                    "already running: {name}"
                )));
            }
        }
        let mut started = Vec::new();
        for name in names {
            let entry = entries.get_mut(&name).unwrap();
            entry.status.run_id += 1;
            entry.status.state = State::Running as i32;
            entry.status.started_unix_ms = now();
            entry.status.finished_unix_ms = 0;
            entry.status.detail = "running; see journalctl -u bitd".into();
            started.push(entry.status.clone());
            let config = entry.config.clone();
            let service = self.clone();
            tokio::spawn(async move {
                let (state, detail) = execute(&config, service.shutdown.clone()).await;
                println!("check={} state={state:?} {detail}", config.name);
                let mut entries = service.entries.lock().await;
                let entry = entries.get_mut(&config.name).unwrap();
                entry.status.state = state as i32;
                entry.status.detail = detail;
                entry.status.finished_unix_ms = now();
            });
        }
        Ok(started)
    }
    pub async fn drain(&self) {
        while self
            .snapshot()
            .await
            .iter()
            .any(|c| c.state == State::Running as i32)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// Each check owns a process group so its helpers cannot outlive a timeout.
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
async fn execute(c: &CheckConfig, mut stop: watch::Receiver<bool>) -> (State, String) {
    if *stop.borrow() {
        return (State::Failed, "daemon stopped".into());
    }
    println!("check={} starting", c.name);
    let mut child = match Command::new(&c.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return (State::Failed, format!("cannot spawn: {e}")),
    };
    let group = ProcessGroup(child.id().unwrap());
    let input = serde_json::to_vec(&c.config).unwrap();
    let work = async {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&input).await?;
        drop(stdin);
        child.wait().await
    };
    let result = tokio::select! {
        _ = stop.changed() => (State::Failed, "daemon stopped".into()),
        result = tokio::time::timeout(Duration::from_millis(c.timeout_ms), work) => match result {
            Ok(Ok(status)) if status.success() => (State::Passed, "passed".into()),
            Ok(Ok(status)) => (State::Failed, format!("{status}; see journalctl -u bitd")),
            Ok(Err(e)) => (State::Failed, format!("plugin I/O error: {e}")),
            Err(_) => (State::TimedOut, format!("exceeded {} ms", c.timeout_ms)),
        }
    };
    drop(group);
    let _ = child.wait().await;
    result
}

#[tonic::async_trait]
impl BitService for Service {
    async fn list(&self, _: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        Ok(Response::new(ListResponse {
            checks: self.snapshot().await,
        }))
    }
    async fn run(&self, request: Request<RunRequest>) -> Result<Response<RunResponse>, Status> {
        Ok(Response::new(RunResponse {
            checks: self.launch(request.into_inner().names).await?,
        }))
    }
}
