//! `/monitor` (Fiber monitor middleware): HTML page, or JSON statistics when
//! `Accept: application/json`.

use super::http::{Req, Resp, json, response, text};
use parking_lot::Mutex;
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const INDEX_HTML: &str = include_str!("../../assets/monitor.html");

#[derive(Serialize)]
struct PidStats {
    cpu: f64,
    ram: u64,
    conns: i64,
}

#[derive(Serialize)]
struct OsStats {
    cpu: f64,
    ram: u64,
    total_ram: u64,
    load_avg: f64,
    conns: i64,
}

#[derive(Serialize)]
struct Stats {
    pid: PidStats,
    os: OsStats,
}

pub struct Monitor {
    sys: Mutex<System>,
}

impl Default for Monitor {
    fn default() -> Self {
        Monitor {
            sys: Mutex::new(System::new()),
        }
    }
}

/// Number of TCP connections of the host (Linux), like gopsutil's `net.Connections`.
fn os_connections() -> Option<i64> {
    let mut n = 0;
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = std::fs::read_to_string(f).ok()?;
        n += text.lines().skip(1).count() as i64;
    }
    Some(n)
}

impl Monitor {
    /// Resident memory of this process in bytes.
    pub fn process_memory(&self) -> u64 {
        let mut sys = self.sys.lock();
        let pid = Pid::from_u32(std::process::id());
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        sys.process(pid).map_or(0, |p| p.memory())
    }

    fn stats(&self, conns: i64) -> Stats {
        let mut sys = self.sys.lock();
        let pid = Pid::from_u32(std::process::id());
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let (pcpu, pram) = sys
            .process(pid)
            .map_or((0.0, 0), |p| (f64::from(p.cpu_usage()), p.memory()));
        Stats {
            pid: PidStats {
                cpu: pcpu,
                ram: pram,
                conns,
            },
            os: OsStats {
                cpu: f64::from(sys.global_cpu_usage()),
                ram: sys.used_memory(),
                total_ram: sys.total_memory(),
                load_avg: System::load_average().one,
                conns: os_connections().unwrap_or(conns),
            },
        }
    }

    pub fn handle(&self, req: &Req, conns: i64) -> Resp {
        if req.method != http::Method::GET {
            return text(405, "Method Not Allowed");
        }
        if req.header("accept") == "application/json" {
            return json(200, &self.stats(conns));
        }
        response(200, "text/html; charset=utf-8", INDEX_HTML)
    }
}
