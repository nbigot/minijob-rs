//! minijob-bench: load generator for the minijob HTTP API.
//!
//! The Go repository has the same tool (`cmd/benchmark`): same options, scenarios and result
//! files, so either one can measure either server and `report` reads files from both.

mod bench;
mod client;
mod output;
mod report;
mod stats;

use std::process::ExitCode;

const USAGE: &str = "Usage:
  minijob-bench [options]             run the scenarios against a server
  minijob-bench report FILE.json...   median and spread of several result files (Markdown)

Options:
  -url string          server base URL (default \"http://127.0.0.1:8080\")
  -scenarios string    comma-separated list or \"all\" (default \"all\"):
                       ping, create, pull-batch, get, list, list-filter, cycle
  -concurrency int     connections, one request at a time each (default 32)
  -duration int        measured seconds per timed scenario (default 10)
  -warmup int          unmeasured seconds before each timed scenario (default 2)
  -preload int         jobs created before the read scenarios (default 10000)
  -batch-jobs int      jobs created for pull-batch (default 100000)
  -batch-size int      numjobs of each pull in pull-batch (default 100)
  -pid int             server process id, to read its CPU time and memory in /proc (Linux)
  -label string        name of this run in reports (default: the URL)
  -wait int            seconds to wait for the server to answer /ping (default 0)
  -json string         write the result to this file
";

struct Args {
    opts: bench::Opts,
    json: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut url = "http://127.0.0.1:8080".to_string();
    let mut scenarios = "all".to_string();
    let (mut concurrency, mut duration, mut warmup) = (32usize, 10u64, 2u64);
    let (mut preload, mut batch_jobs, mut batch_size, mut pid) =
        (10_000usize, 100_000usize, 100usize, 0u32);
    let mut wait = 0u64;
    let mut label = String::new();
    let mut json = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let flag = a
            .strip_prefix("--")
            .or_else(|| a.strip_prefix('-'))
            .ok_or_else(|| format!("unexpected argument {a:?}"))?;
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => {
                if matches!(flag, "h" | "help") {
                    return Err(String::new());
                }
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| format!("flag needs an argument: -{flag}"))?;
                (flag.to_string(), v.clone())
            }
        };
        let num = |v: &str| -> Result<u64, String> {
            v.parse()
                .map_err(|_| format!("invalid value {v:?} for flag -{name}"))
        };
        match name.as_str() {
            "url" => url = value.trim_end_matches('/').to_string(),
            "scenarios" => scenarios = value,
            "concurrency" => concurrency = num(&value)?.max(1) as usize,
            "duration" => duration = num(&value)?.max(1),
            "warmup" => warmup = num(&value)?,
            "preload" => preload = num(&value)?.max(1) as usize,
            "batch-jobs" => batch_jobs = num(&value)?.max(1) as usize,
            "batch-size" => batch_size = num(&value)?.clamp(1, 100) as usize,
            "pid" => pid = num(&value)? as u32,
            "label" => label = value,
            "wait" => wait = num(&value)?,
            "json" => json = Some(value),
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
        i += 1;
    }
    let hostport = url
        .strip_prefix("http://")
        .ok_or("only http:// URLs are supported")?
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let addr = if hostport.contains(':') {
        hostport.clone()
    } else {
        format!("{hostport}:80")
    };
    let scenarios: Vec<String> = if scenarios == "all" {
        bench::SCENARIOS.iter().map(|s| s.to_string()).collect()
    } else {
        scenarios.split(',').map(|s| s.trim().to_string()).collect()
    };
    if let Some(s) = scenarios
        .iter()
        .find(|s| !bench::SCENARIOS.contains(&s.as_str()))
    {
        return Err(format!("unknown scenario {s:?}"));
    }
    if label.is_empty() {
        label = url.clone();
    }
    Ok(Args {
        opts: bench::Opts {
            url,
            addr,
            host: hostport,
            scenarios,
            concurrency,
            duration,
            warmup,
            preload,
            batch_jobs,
            batch_size,
            pid,
            label,
            wait,
        },
        json,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("report") {
        return match report::report(&args[1..]) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let a = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprint!("{USAGE}");
            return if e.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    println!(
        "minijob-bench (rust) {} -> {}, concurrency {}, {} s per scenario after {} s warm-up",
        env!("CARGO_PKG_VERSION"),
        a.opts.url,
        a.opts.concurrency,
        a.opts.duration,
        a.opts.warmup
    );
    match rt.block_on(bench::run(a.opts)) {
        Ok(result) => {
            if let Some(path) = a.json {
                let text = serde_json::to_string_pretty(&result).expect("serializable");
                if let Err(e) = std::fs::write(&path, text + "\n") {
                    eprintln!("cannot write {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            let failed = result
                .scenarios
                .iter()
                .any(|s| s.check.as_ref().is_some_and(|c| !c.ok));
            if failed {
                eprintln!("a consistency check failed");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> Result<Args, String> {
        parse(&s.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn go_style_flags() {
        let a = p(&[
            "-url",
            "http://h:9/",
            "--concurrency=4",
            "-scenarios",
            "ping,get",
        ])
        .unwrap();
        assert_eq!(a.opts.addr, "h:9");
        assert_eq!(a.opts.concurrency, 4);
        assert_eq!(a.opts.scenarios, ["ping", "get"]);
        assert_eq!(a.opts.label, "http://h:9");
        assert_eq!(p(&["-url", "http://h"]).unwrap().opts.addr, "h:80");
        assert!(p(&["-nope", "1"]).is_err());
        assert!(p(&["-scenarios", "x"]).is_err());
        assert!(p(&["-duration"]).is_err());
    }
}
