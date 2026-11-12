use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bit_rs::plugin::BitCheck;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

/// Fails when the agent sessions on the given tmux sockets (Agent UI and the
/// app companions) together hold too much of the machine's RAM. Sessions stay
/// alive until explicitly ended (closing a browser tab only detaches), and an
/// idle Claude/Codex session with its MCP servers costs ~1 GB.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSessions {
    /// tmux `-L` socket names whose sessions are agent sessions.
    sockets: Vec<String>,
    #[serde(default = "max_memory_percent")]
    max_memory_percent: f64,
}
fn max_memory_percent() -> f64 {
    40.0
}

struct Proc {
    pid: u32,
    ppid: u32,
    comm: String,
    cmdline: Vec<String>,
    rss_anon_kb: u64,
}

#[derive(Debug, PartialEq)]
struct Tally {
    socket: String,
    sessions: usize,
    rss_anon_kb: u64,
}

/// The `-L` socket a tmux server was started with: its cmdline is the argv of
/// the client that spawned it (`tmux -L agent-ui -f conf new-session ...`).
fn tmux_socket(p: &Proc) -> Option<&str> {
    if p.comm != "tmux: server" {
        return None;
    }
    let mut args = p.cmdline.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-L" {
            return args.next().map(String::as_str);
        }
        if let Some(socket) = arg.strip_prefix("-L") {
            return Some(socket);
        }
    }
    None
}

/// Per configured socket: its server's direct children (one pane process per
/// session) and the anonymous memory of everything below the server.
fn tally(procs: &[Proc], sockets: &[String]) -> Vec<Tally> {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut out: Vec<Tally> = sockets
        .iter()
        .map(|s| Tally {
            socket: s.clone(),
            sessions: 0,
            rss_anon_kb: 0,
        })
        .collect();
    for server in procs {
        let Some(socket) = tmux_socket(server) else {
            continue;
        };
        let Some(t) = out.iter_mut().find(|t| t.socket == socket) else {
            continue;
        };
        let panes = children.get(&server.pid).map_or(&[][..], Vec::as_slice);
        t.sessions += panes.len();
        let mut stack: Vec<&Proc> = panes.to_vec();
        while let Some(p) = stack.pop() {
            t.rss_anon_kb += p.rss_anon_kb;
            if let Some(kids) = children.get(&p.pid) {
                stack.extend(kids.iter().copied());
            }
        }
    }
    out
}

fn status_kb(status: &str, key: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// ppid from /proc/<pid>/stat; comm may contain spaces/parens, so parse after
/// the last ')'.
fn stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn read_procs() -> Result<Vec<Proc>> {
    let mut procs = Vec::new();
    for entry in fs::read_dir("/proc").context("reading /proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let dir = entry.path();
        // Processes can exit mid-scan: skip whatever vanished.
        let (Ok(stat), Ok(status), Ok(comm)) = (
            fs::read_to_string(dir.join("stat")),
            fs::read_to_string(dir.join("status")),
            fs::read_to_string(dir.join("comm")),
        ) else {
            continue;
        };
        let Some(ppid) = stat_ppid(&stat) else {
            continue;
        };
        let cmdline = fs::read(dir.join("cmdline"))
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        procs.push(Proc {
            pid,
            ppid,
            comm: comm.trim_end().to_string(),
            cmdline,
            // Kernel threads have no RssAnon line.
            rss_anon_kb: status_kb(&status, "RssAnon:").unwrap_or(0),
        });
    }
    Ok(procs)
}

#[async_trait]
impl BitCheck for AgentSessions {
    fn remediation(&self) -> String {
        format!(
            "Agent sessions (tmux sockets: {}) hold more than {}% of RAM.\n\
             - Sessions run until ended (closing a browser tab only detaches); each idle one costs ~1 GB.\n\
             - End finished ones in their UI: Agent Terminal (Terminate), Folio/Budget/Assistant (Kill).\n\
             - `tmux -L <socket> ls` lists them; `journalctl -u bitd` shows per-socket counts and memory.",
            self.sockets.join(", "),
            self.max_memory_percent
        )
    }
    async fn check(&self) -> Result<()> {
        let meminfo = fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
        let total_kb = status_kb(&meminfo, "MemTotal:").context("no MemTotal in /proc/meminfo")?;
        let tallies = tally(&read_procs()?, &self.sockets);
        let used_kb: u64 = tallies.iter().map(|t| t.rss_anon_kb).sum();
        for t in &tallies {
            println!(
                "{}: {} session(s), {} MiB",
                t.socket,
                t.sessions,
                t.rss_anon_kb / 1024
            );
        }
        let percent = used_kb as f64 * 100.0 / total_kb as f64;
        println!(
            "agent sessions: {} MiB of {} MiB RAM ({percent:.1}%, limit {}%)",
            used_kb / 1024,
            total_kb / 1024,
            self.max_memory_percent
        );
        if percent > self.max_memory_percent {
            bail!(
                "agent sessions hold {percent:.1}% of RAM (limit {}%)",
                self.max_memory_percent
            );
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    bit_rs::plugin::run::<AgentSessions>().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, comm: &str, cmdline: &str, rss: u64) -> Proc {
        Proc {
            pid,
            ppid,
            comm: comm.into(),
            cmdline: cmdline.split(' ').map(String::from).collect(),
            rss_anon_kb: rss,
        }
    }

    #[test]
    fn tallies_sessions_and_descendant_memory_per_socket() {
        let procs = vec![
            p(
                10,
                1,
                "tmux: server",
                "tmux -L agent-ui -f c new-session",
                3000,
            ),
            p(11, 10, "codex", "codex", 700_000),
            p(12, 11, "python3", "python3 mcp", 50_000),
            p(13, 10, ".claude-wrapped", "claude", 250_000),
            p(
                20,
                1,
                "tmux: server",
                "tmux -Lfolio-agent new-session",
                3000,
            ),
            p(21, 20, "codex", "codex", 600_000),
            // Not configured / not a tmux server: ignored.
            p(30, 1, "tmux: server", "tmux -L other new-session", 3000),
            p(31, 30, "codex", "codex", 900_000),
            p(40, 1, "tmux", "tmux -L agent-ui attach", 1000),
        ];
        let sockets = vec![
            "agent-ui".to_string(),
            "folio-agent".to_string(),
            "budget-agent".to_string(),
        ];
        assert_eq!(
            tally(&procs, &sockets),
            vec![
                Tally {
                    socket: "agent-ui".into(),
                    sessions: 2,
                    rss_anon_kb: 1_000_000
                },
                Tally {
                    socket: "folio-agent".into(),
                    sessions: 1,
                    rss_anon_kb: 600_000
                },
                Tally {
                    socket: "budget-agent".into(),
                    sessions: 0,
                    rss_anon_kb: 0
                },
            ]
        );
    }

    #[test]
    fn parses_proc_fields() {
        assert_eq!(stat_ppid("1234 (tmux: server) S 1 1234 1234 0"), Some(1));
        assert_eq!(stat_ppid("7 (a) b) R 42 7"), Some(42));
        assert_eq!(
            status_kb("Name:\tx\nRssAnon:\t  2048 kB\n", "RssAnon:"),
            Some(2048)
        );
        assert_eq!(status_kb("Name:\tx\n", "RssAnon:"), None);
    }

    #[test]
    fn config_requires_sockets_and_rejects_unknown_fields() {
        let ok: AgentSessions = serde_json::from_str(r#"{"sockets": ["agent-ui"]}"#).unwrap();
        assert_eq!(ok.max_memory_percent, 40.0);
        assert!(serde_json::from_str::<AgentSessions>("{}").is_err());
        assert!(serde_json::from_str::<AgentSessions>(r#"{"sockets": [], "x": 1}"#).is_err());
    }
}
