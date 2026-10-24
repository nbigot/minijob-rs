//! Go's `net.Listen("tcp4", address)` as Fiber calls it for `webserver.http.address`: same
//! address parsing (`net.SplitHostPort`, `LookupPort`) and same error messages, wrapped by
//! Fiber in `failed to listen: ...`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;

/// `net.SplitHostPort`. The error is an `*AddrError` message.
pub fn split_host_port(hostport: &str) -> Result<(&str, &str), String> {
    let addr_err = |why: &str| Err(format!("address {hostport}: {why}"));
    let Some(i) = hostport.rfind(':') else {
        return addr_err("missing port in address");
    };
    let (host, j, k) = if hostport.starts_with('[') {
        // the first ']' must be just before the last ':'
        let Some(end) = hostport.find(']') else {
            return addr_err("missing ']' in address");
        };
        if end + 1 == hostport.len() {
            return addr_err("missing port in address");
        }
        if end + 1 != i {
            if hostport.as_bytes()[end + 1] == b':' {
                return addr_err("too many colons in address");
            }
            return addr_err("missing port in address");
        }
        (&hostport[1..end], 1, end + 1)
    } else {
        let host = &hostport[..i];
        if host.contains(':') {
            return addr_err("too many colons in address");
        }
        (host, 0, 0)
    };
    if hostport[j..].contains('[') {
        return addr_err("unexpected '[' in address");
    }
    if hostport[k..].contains(']') {
        return addr_err("unexpected ']' in address");
    }
    Ok((host, &hostport[i + 1..]))
}

/// `net.parsePort`: the port number, or `None` when `service` is a name to look up.
/// Out-of-range numbers saturate, as in Go, and are refused by the caller.
fn parse_port(service: &str) -> Option<i64> {
    const CUTOFF: u64 = 1 << 30;
    let (neg, digits) = match service.as_bytes().first() {
        Some(b'+') => (false, &service[1..]),
        Some(b'-') => (true, &service[1..]),
        _ => (false, service),
    };
    let mut n: u64 = 0;
    for d in digits.bytes() {
        if !d.is_ascii_digit() {
            return None;
        }
        n = (n * 10 + u64::from(d - b'0')).min(CUTOFF);
    }
    let n = n as i64;
    Some(if neg { -n } else { n })
}

/// `net.LookupPort("tcp4", service)`. Names come from Go's built-in table, which the
/// services files of Linux and Windows agree with.
pub fn lookup_port(service: &str) -> Result<u16, String> {
    let port = match parse_port(service) {
        Some(p) => p,
        None => match service.to_ascii_lowercase().as_str() {
            "ftp" => 21,
            "ftps" => 990,
            "gopher" => 70,
            "http" => 80,
            "https" => 443,
            "imap2" => 143,
            "imap3" => 220,
            "imaps" => 993,
            "pop3" => 110,
            "pop3s" => 995,
            "smtp" => 25,
            "submissions" => 465,
            "ssh" => 22,
            "telnet" => 23,
            _ => return Err(format!("lookup tcp4/{service}: unknown port")),
        },
    };
    u16::try_from(port).map_err(|_| format!("address {service}: invalid port"))
}

/// The IPv4 address of `host` for `tcp4`: a literal, else a lookup. Go does not read
/// `127.1` or `0x7f000001` as addresses, the system resolver does: they are refused here.
async fn lookup_ipv4(host: &str) -> Result<Ipv4Addr, String> {
    let no_suitable = || format!("address {host}: no suitable address found");
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => Ok(v4),
            IpAddr::V6(v6) => v6.to_ipv4_mapped().ok_or_else(no_suitable),
        };
    }
    // an IPv6 address with a zone (`fe80::1%eth0`) is not for tcp4
    if let Some((ip, zone)) = host.split_once('%')
        && !zone.is_empty()
        && ip.parse::<std::net::Ipv6Addr>().is_ok()
    {
        return Err(no_suitable());
    }
    let no_such_host = || format!("lookup {host}: no such host");
    let numeric = host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || host.starts_with("0x")
        || host.starts_with("0X");
    if numeric || host.contains(|c: char| c.is_whitespace() || c == ':' || c == '%') {
        return Err(no_such_host());
    }
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|_| no_such_host())?;
    let mut found = false;
    for a in addrs {
        found = true;
        if let SocketAddr::V4(v4) = a {
            return Ok(*v4.ip());
        }
    }
    Err(if found { no_suitable() } else { no_such_host() })
}

/// Fiber's `app.Listen(address)` on the `tcp4` network.
pub async fn listen_tcp4(address: &str) -> Result<TcpListener, String> {
    let fail = |e: String| format!("failed to listen: listen tcp4: {e}");
    // an empty address listens on all interfaces, on a free port
    let (host, port) = if address.is_empty() {
        ("", "")
    } else {
        split_host_port(address).map_err(fail)?
    };
    let port = lookup_port(port).map_err(fail)?;
    let ip = if host.is_empty() {
        Ipv4Addr::UNSPECIFIED
    } else {
        lookup_ipv4(host).await.map_err(fail)?
    };
    TcpListener::bind((ip, port)).await.map_err(|e| {
        // Go prints the address it bound: without a host, only the port
        let laddr = if host.is_empty() {
            format!(":{port}")
        } else {
            format!("{ip}:{port}")
        };
        format!(
            "failed to listen: listen tcp4 {laddr}: bind: {}",
            crate::config::go_errno_text(&e)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_like_go() {
        fn ok(s: &str) -> (&str, &str) {
            split_host_port(s).unwrap()
        }
        assert_eq!(ok("127.0.0.1:80"), ("127.0.0.1", "80"));
        assert_eq!(ok(":80"), ("", "80"));
        assert_eq!(ok("127.0.0.1:"), ("127.0.0.1", ""));
        assert_eq!(ok("[::1]:80"), ("::1", "80"));
        assert_eq!(ok("[127.0.0.1]:0"), ("127.0.0.1", "0"));
        let err = |s: &str| split_host_port(s).unwrap_err();
        assert_eq!(err("[::1]"), "address [::1]: missing port in address");
        assert_eq!(
            err("[::1]x:80"),
            "address [::1]x:80: missing port in address"
        );
        assert_eq!(
            err("[::1]::80"),
            "address [::1]::80: too many colons in address"
        );
        assert_eq!(err("a[b:80"), "address a[b:80: unexpected '[' in address");
        assert_eq!(err("[a]:8]0"), "address [a]:8]0: unexpected ']' in address");
    }

    #[test]
    fn ports_like_go() {
        assert_eq!(lookup_port(""), Ok(0));
        assert_eq!(lookup_port("+8080"), Ok(8080));
        assert_eq!(lookup_port("-0"), Ok(0));
        assert_eq!(lookup_port("HTTPS"), Ok(443));
        assert_eq!(
            lookup_port("65536"),
            Err("address 65536: invalid port".into())
        );
        assert_eq!(
            lookup_port("99999999999999999999999"),
            Err("address 99999999999999999999999: invalid port".into())
        );
        assert_eq!(lookup_port("-1"), Err("address -1: invalid port".into()));
        assert_eq!(
            lookup_port("8o"),
            Err("lookup tcp4/8o: unknown port".into())
        );
    }

    /// Messages of Go 1.27 (`net.Listen("tcp4", ...)` wrapped as Fiber does).
    #[tokio::test]
    async fn listen_like_go() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let used = held.local_addr().unwrap().port();
        let cases = [
            (
                "127.0.0.1",
                "listen tcp4: address 127.0.0.1: missing port in address".to_string(),
            ),
            (
                "127.0.0.1:99999",
                "listen tcp4: address 99999: invalid port".into(),
            ),
            (
                "127.0.0.1:-1",
                "listen tcp4: address -1: invalid port".into(),
            ),
            (
                "127.0.0.1:abc",
                "listen tcp4: lookup tcp4/abc: unknown port".into(),
            ),
            (
                "127.0.0.1:80:80",
                "listen tcp4: address 127.0.0.1:80:80: too many colons in address".into(),
            ),
            (
                "nohost.invalid:80",
                "listen tcp4: lookup nohost.invalid: no such host".into(),
            ),
            (
                "[::1]:0",
                "listen tcp4: address ::1: no suitable address found".into(),
            ),
            (
                "::1:80",
                "listen tcp4: address ::1:80: too many colons in address".into(),
            ),
            (
                "[127.0.0.1:80",
                "listen tcp4: address [127.0.0.1:80: missing ']' in address".into(),
            ),
            (
                "127.0.0.1]:80",
                "listen tcp4: address 127.0.0.1]:80: unexpected ']' in address".into(),
            ),
            (
                " 127.0.0.1:0",
                "listen tcp4: lookup  127.0.0.1: no such host".into(),
            ),
            (
                "0x7f000001:0",
                "listen tcp4: lookup 0x7f000001: no such host".into(),
            ),
            ("127.1:0", "listen tcp4: lookup 127.1: no such host".into()),
            (
                "256.0.0.1:0",
                "listen tcp4: lookup 256.0.0.1: no such host".into(),
            ),
            (
                "0X7f000001:0",
                "listen tcp4: lookup 0X7f000001: no such host".into(),
            ),
            (
                "01.2.3.4:0",
                "listen tcp4: lookup 01.2.3.4: no such host".into(),
            ),
            (
                "+",
                "listen tcp4: address +: missing port in address".into(),
            ),
            (
                "127.0.0.1:0x50",
                "listen tcp4: lookup tcp4/0x50: unknown port".into(),
            ),
            (
                "[fe80::1%eth0]:0",
                "listen tcp4: address fe80::1%eth0: no suitable address found".into(),
            ),
        ];
        for (address, want) in cases {
            let got = listen_tcp4(address).await.map(|_| ()).unwrap_err();
            assert_eq!(got, format!("failed to listen: {want}"), "{address:?}");
        }
        let got = listen_tcp4(&format!("127.0.0.1:{used}"))
            .await
            .map(|_| ())
            .unwrap_err();
        let prefix = format!("failed to listen: listen tcp4 127.0.0.1:{used}: bind: ");
        assert!(got.starts_with(&prefix), "{got}");
        if cfg!(windows) {
            assert_eq!(
                &got[prefix.len()..],
                "Only one usage of each socket address (protocol/network address/port) is \
                 normally permitted."
            );
        } else {
            assert_eq!(&got[prefix.len()..], "address already in use");
        }
        for (address, ip) in [
            ("[127.0.0.1]:0", "127.0.0.1"),
            ("localhost:0", "127.0.0.1"),
            (":0", "0.0.0.0"),
            ("127.0.0.1:", "127.0.0.1"),
            ("", "0.0.0.0"),
            ("[::ffff:127.0.0.1]:0", "127.0.0.1"),
            ("127.0.0.1:+", "127.0.0.1"),
            ("localhost.:0", "127.0.0.1"),
        ] {
            let l = listen_tcp4(address).await.unwrap();
            assert_eq!(l.local_addr().unwrap().ip().to_string(), ip, "{address:?}");
        }
    }
}
