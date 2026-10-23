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
    let mut out = std::io::stdout().lock();
    let colors = out.is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb")
        && std::env::var("NO_COLOR").as_deref() != Ok("1");
    let line = line(colors, &time, status, remote, latency, method, path);
    let _ = out.write_all(line.as_bytes());
}

fn line(
    colors: bool,
    time: &str,
    status: u16,
    remote: SocketAddr,
    latency: Duration,
    method: &str,
    path: &str,
) -> String {
    let latency = format!("{:>13}", gotime::duration_string(latency));
    if colors {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_like_fiber() {
        let remote: SocketAddr = "[::1]:50312".parse().unwrap();
        let latency = Duration::from_nanos(1_234_567);
        let time = "2026-10-23T09:50:00+0200";
        assert_eq!(
            line(false, time, 201, remote, latency, "POST", "/api/v1/job/"),
            "[2026-10-23T09:50:00+0200] 201 - ::1:50312 -    1.234567ms POST /api/v1/job/\n"
        );
        let remote: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let colored = |status, method| line(true, time, status, remote, latency, method, "/");
        assert_eq!(
            colored(200, "GET"),
            "[2026-10-23T09:50:00+0200] \x1b[92m200\x1b[0m - 127.0.0.1:8080 -    1.234567ms \
             \x1b[96mGET\x1b[0m /\n"
        );
        for (status, color) in [(204, 92), (304, 94), (404, 93), (500, 91), (101, 91)] {
            assert!(colored(status, "GET").contains(&format!("\x1b[{color}m{status}\x1b[0m")));
        }
        for (method, color) in [
            ("POST", 92),
            ("PUT", 93),
            ("DELETE", 91),
            ("PATCH", 97),
            ("HEAD", 95),
            ("OPTIONS", 94),
            ("TRACE", 0),
        ] {
            assert!(colored(200, method).contains(&format!("\x1b[{color}m{method}\x1b[0m")));
        }
    }
}
