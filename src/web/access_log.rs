//! Access log of `webserver.logs.enable` (Fiber logger with the Go server's format):
//! `[${time}] ${status} - ${ip}:${port} - ${latency} ${method} ${path}`.

use crate::log::gotime;
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::time::Duration;

const TIME_FORMAT: &str = "2006-01-02T15:04:05-0700";

fn status_color(status: u16) -> &'static str {
    match status {
        200..=299 => "\x1b[92m",
        300..=399 => "\x1b[94m",
        400..=499 => "\x1b[93m",
        _ => "\x1b[91m",
    }
}

fn method_color(method: &str) -> &'static str {
    match method {
        "GET" => "\x1b[96m",
        "POST" => "\x1b[92m",
        "PUT" => "\x1b[93m",
        "DELETE" => "\x1b[91m",
        "PATCH" => "\x1b[97m",
        "HEAD" => "\x1b[95m",
        "OPTIONS" => "\x1b[94m",
        _ => "\x1b[0m",
    }
}

const RESET: &str = "\x1b[0m";

/// Writes one line to stdout (colors only on a terminal, like Fiber).
pub fn write(status: u16, remote: SocketAddr, latency: Duration, method: &str, path: &str) {
    let time = gotime::format(&gotime::now_local(), TIME_FORMAT);
    let latency = format!("{:>13}", gotime::duration_string(latency));
    let mut out = std::io::stdout().lock();
    let colors = out.is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb")
        && std::env::var("NO_COLOR").as_deref() != Ok("1");
    let line = if colors {
        format!(
            "[{time}] {}{status:3}{RESET} - {}:{} - {latency} {}{method}{RESET} {path}\n",
            status_color(status),
            remote.ip(),
            remote.port(),
            method_color(method),
        )
    } else {
        format!(
            "[{time}] {status:3} - {}:{} - {latency} {method} {path}\n",
            remote.ip(),
            remote.port()
        )
    };
    let _ = out.write_all(line.as_bytes());
}
