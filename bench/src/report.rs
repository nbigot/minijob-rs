//! `minijob-bench report <files...>`: median and spread (min–max) over runs, grouped by
//! label then scenario, as Markdown tables.

use crate::output::RunResult;
use std::fmt::Write;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// `median (min–max)`, or `median` alone for a single run.
fn cell(mut v: Vec<f64>, decimals: usize) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let m = median(&mut v);
    let (lo, hi) = (v[0], v[v.len() - 1]);
    if v.len() == 1 || lo == hi {
        format!("{m:.decimals$}")
    } else {
        format!("{m:.decimals$} ({lo:.decimals$}–{hi:.decimals$})")
    }
}

pub fn report(files: &[String]) -> Result<String, String> {
    let mut runs: Vec<RunResult> = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
        runs.push(serde_json::from_str(&text).map_err(|e| format!("{f}: {e}"))?);
    }
    // Labels and scenarios in order of first appearance.
    let mut labels: Vec<String> = Vec::new();
    let mut scenarios: Vec<String> = Vec::new();
    for r in &runs {
        if !labels.contains(&r.label) {
            labels.push(r.label.clone());
        }
        for s in &r.scenarios {
            if !scenarios.contains(&s.name) {
                scenarios.push(s.name.clone());
            }
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Median over runs, (min–max) when they differ. Latency in ms, server CPU in µs per request.\n"
    );
    for sc in &scenarios {
        let _ = writeln!(out, "### {sc}\n");
        let _ = writeln!(
            out,
            "| label | runs | req/s | items/s | p50 | p99 | server CPU | load gen CPU % | errors | check |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|");
        for label in &labels {
            let rs: Vec<_> = runs
                .iter()
                .filter(|r| &r.label == label)
                .filter_map(|r| r.scenarios.iter().find(|s| &s.name == sc))
                .collect();
            if rs.is_empty() {
                continue;
            }
            let col = |f: &dyn Fn(&crate::output::ScenarioResult) -> Option<f64>| -> Vec<f64> {
                rs.iter().filter_map(|s| f(s)).collect()
            };
            let errors: u64 = rs.iter().map(|s| s.errors).sum();
            let checks = rs
                .iter()
                .filter_map(|s| s.check.as_ref())
                .collect::<Vec<_>>();
            let check = if checks.is_empty() {
                "-".to_string()
            } else if checks.iter().all(|c| c.ok) {
                "OK".to_string()
            } else {
                format!(
                    "FAILED: {}",
                    checks
                        .iter()
                        .find(|c| !c.ok)
                        .map(|c| c.detail.as_str())
                        .unwrap_or("")
                )
            };
            let _ = writeln!(
                out,
                "| {label} | {} | {} | {} | {} | {} | {} | {} | {errors} | {check} |",
                rs.len(),
                cell(col(&|s| Some(s.requests_per_s)), 0),
                cell(col(&|s| Some(s.items_per_s)), 0),
                cell(col(&|s| Some(s.latency_ms.p50)), 3),
                cell(col(&|s| Some(s.latency_ms.p99)), 3),
                cell(col(&|s| s.server_cpu_us_per_request), 1),
                cell(col(&|s| s.client_cpu_percent), 0),
            );
        }
        out.push('\n');
    }
    let _ = writeln!(out, "### Server memory (MiB)\n");
    let _ = writeln!(out, "| label | runs | idle RSS | peak RSS |");
    let _ = writeln!(out, "|---|---|---|---|");
    for label in &labels {
        let rs: Vec<_> = runs.iter().filter(|r| &r.label == label).collect();
        let mib = |v: Option<u64>| v.map(|kb| kb as f64 / 1024.0);
        let idle: Vec<f64> = rs
            .iter()
            .filter_map(|r| mib(r.server.idle_rss_kb))
            .collect();
        let peak: Vec<f64> = rs
            .iter()
            .filter_map(|r| mib(r.server.peak_rss_kb))
            .collect();
        let _ = writeln!(
            out,
            "| {label} | {} | {} | {} |",
            rs.len(),
            cell(idle, 1),
            cell(peak, 1)
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_spread() {
        assert_eq!(cell(vec![3.0, 1.0, 2.0], 0), "2 (1–3)");
        assert_eq!(cell(vec![2.0, 4.0], 1), "3.0 (2.0–4.0)");
        assert_eq!(cell(vec![5.0], 2), "5.00");
        assert_eq!(cell(vec![], 2), "-");
    }
}
