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
    sync::{watch, Mutex, OwnedSemaphorePermit, Semaphore},
};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct Service {
    entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    /// Admission for regression checks: a shared one takes one slot, an
    /// exclusive one takes them all. The semaphore is FIFO, so a waiting
    /// exclusive check holds back regressions requested after it.
    regressions: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
}
const REGRESSION_SLOTS: u32 = 1 << 16;
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
            regressions: Arc::new(Semaphore::new(REGRESSION_SLOTS as usize)),
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
    /// Asks every plugin for its remediation text, concurrently. A plugin that
    /// cannot answer just leaves the field empty; checks run regardless.
    pub async fn load_remediation(&self) {
        let configs: Vec<CheckConfig> = self
            .entries
            .lock()
            .await
            .values()
            .map(|e| e.config.clone())
            .collect();
        let mut queries = tokio::task::JoinSet::new();
        for config in configs {
            queries.spawn(async move { (config.name.clone(), remediation(&config).await) });
        }
        while let Some(Ok((name, result))) = queries.join_next().await {
            match result {
                Ok(text) => {
                    if let Some(entry) = self.entries.lock().await.get_mut(&name) {
                        entry.status.remediation = text;
                    }
                }
                Err(e) => println!("check={name} no remediation: {e:#}"),
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
            entry.status.detail = RUNNING.into();
            started.push(entry.status.clone());
            let config = entry.config.clone();
            let service = self.clone();
            tokio::spawn(async move {
                let (state, detail) = match service.admit(&config).await {
                    // The permit is held until the plugin has finished.
                    Some(_permit) => execute(&config, service.shutdown.clone()).await,
                    None => (State::Failed, "daemon stopped".into()),
                };
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
    /// Wait for a regression's turn; `None` if the daemon stops first. Health
    /// checks are lightweight by contract and are never held back.
    async fn admit(&self, c: &CheckConfig) -> Option<Option<OwnedSemaphorePermit>> {
        if c.kind != Kind::Regression {
            return Some(None);
        }
        let slots = if c.exclusive { REGRESSION_SLOTS } else { 1 };
        if let Ok(permit) = self.regressions.clone().try_acquire_many_owned(slots) {
            return Some(Some(permit));
        }
        println!("check={} waiting for other regressions", c.name);
        self.set_detail(
            &c.name,
            if c.exclusive {
                "queued: waits for exclusive access"
            } else {
                "queued: waits for an exclusive check"
            },
        )
        .await;
        let mut stop = self.shutdown.clone();
        if *stop.borrow() {
            return None;
        }
        let permit = tokio::select! {
            _ = stop.changed() => return None,
            permit = self.regressions.clone().acquire_many_owned(slots) => permit.ok()?,
        };
        // The deadline and reported start begin once the check really runs.
        let mut entries = self.entries.lock().await;
        let entry = entries.get_mut(&c.name).unwrap();
        entry.status.started_unix_ms = now();
        entry.status.detail = RUNNING.into();
        Some(Some(permit))
    }
    async fn set_detail(&self, name: &str, detail: &str) {
        if let Some(entry) = self.entries.lock().await.get_mut(name) {
            entry.status.detail = detail.into();
        }
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

const RUNNING: &str = "running; see journalctl -u bitd";

// Each check owns a process group so its helpers cannot outlive a timeout.
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
const REMEDIATION_TIMEOUT: Duration = Duration::from_secs(5);
const REMEDIATION_MAX_BYTES: usize = 4096;
async fn remediation(c: &CheckConfig) -> anyhow::Result<String> {
    use anyhow::Context;
    let mut child = Command::new(&c.command)
        .arg(crate::plugin::REMEDIATION_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("cannot spawn")?;
    let _group = ProcessGroup(child.id().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let input = serde_json::to_vec(&c.config).unwrap();
    let output = tokio::time::timeout(REMEDIATION_TIMEOUT, async {
        stdin.write_all(&input).await?;
        drop(stdin);
        child.wait_with_output().await
    })
    .await
    .context("timed out")??;
    anyhow::ensure!(output.status.success(), "{}", output.status);
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.len() > REMEDIATION_MAX_BYTES {
        let cut = (0..=REMEDIATION_MAX_BYTES)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(cut);
        text.push('…');
    }
    Ok(text)
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
