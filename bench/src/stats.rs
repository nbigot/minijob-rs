//! Latency samples and server process statistics.

use crate::output::Latency;

/// All latencies of a scenario in microseconds; percentiles by nearest rank.
#[derive(Default)]
pub struct Samples {
    pub us: Vec<u32>,
}

impl Samples {
    pub fn push(&mut self, d: std::time::Duration) {
        self.us.push(d.as_micros().min(u32::MAX as u128) as u32);
    }

    pub fn merge(&mut self, other: Samples) {
        self.us.extend(other.us);
    }

    pub fn summary(&mut self) -> Latency {
        if self.us.is_empty() {
            return Latency::default();
        }
        self.us.sort_unstable();
        let n = self.us.len();
        let rank = |p: f64| {
            let i = ((p / 100.0) * n as f64).ceil() as usize;
            self.us[i.clamp(1, n) - 1] as f64 / 1000.0
        };
        let sum: u64 = self.us.iter().map(|&v| v as u64).sum();
        Latency {
            mean: round3(sum as f64 / n as f64 / 1000.0),
            p50: round3(rank(50.0)),
            p90: round3(rank(90.0)),
            p99: round3(rank(99.0)),
            max: round3(self.us[n - 1] as f64 / 1000.0),
        }
    }
}

pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// CPU time and memory of the server process, from `/proc/<pid>` (Linux only).
#[derive(Clone, Copy, Debug)]
pub struct ProcStat {
    /// utime + stime in clock ticks.
    pub cpu_ticks: u64,
    pub rss_kb: u64,
    pub hwm_kb: u64,
}

/// Clock ticks per second (`getconf CLK_TCK`), 100 on every mainstream Linux.
pub const TICKS_PER_S: f64 = 100.0;

/// CPU ticks used by this process (Linux).
pub fn self_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

/// `%` of one CPU used between two readings of [`self_cpu_ticks`].
pub fn cpu_percent(t0: Option<u64>, t1: Option<u64>, elapsed_s: f64) -> Option<f64> {
    let ticks = t1?.checked_sub(t0?)?;
    Some(round3(ticks as f64 / TICKS_PER_S / elapsed_s * 100.0))
}

pub fn proc_stat(pid: u32) -> Option<ProcStat> {
    if pid == 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces: fields start after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = |key: &str| -> Option<u64> {
        status
            .lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some(ProcStat {
        cpu_ticks: utime + stime,
        rss_kb: kb("VmRSS:")?,
        hwm_kb: kb("VmHWM:")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn nearest_rank_percentiles() {
        let mut s = Samples::default();
        for i in 1..=100 {
            s.push(Duration::from_micros(i * 1000));
        }
        let l = s.summary();
        assert_eq!((l.p50, l.p90, l.p99, l.max), (50.0, 90.0, 99.0, 100.0));
        assert_eq!(l.mean, 50.5);
        assert_eq!(Samples::default().summary().p99, 0.0);
    }
}
