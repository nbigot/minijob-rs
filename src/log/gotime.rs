//! Go time layouts (`2006-01-02T15:04:05.000Z0700`) and `time.Duration.String`.

use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use std::fmt::Write;
use std::time::Duration;

const LONG_MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const LONG_DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

pub const ISO8601: &str = "2006-01-02T15:04:05.000Z0700";
pub const RFC3339: &str = "2006-01-02T15:04:05Z07:00";
pub const RFC3339_NANO: &str = "2006-01-02T15:04:05.999999999Z07:00";

/// Current local time with its offset.
pub fn now_local() -> DateTime<FixedOffset> {
    chrono::Local::now().fixed_offset()
}

enum Tok {
    Lit(char),
    LongMonth,
    Month,
    NumMonth,
    ZeroMonth,
    LongWeekDay,
    WeekDay,
    Day,
    UnderDay,
    ZeroDay,
    UnderYearDay,
    ZeroYearDay,
    Hour,
    Hour12,
    ZeroHour12,
    Minute,
    ZeroMinute,
    Second,
    ZeroSecond,
    LongYear,
    Year,
    PM,
    Pm,
    TZ,
    /// (Z for UTC, with colons, with seconds, hours only)
    Offset {
        z: bool,
        colon: bool,
        secs: bool,
        hours_only: bool,
    },
    /// fractional seconds: digits, trim trailing zeros, separator
    Frac(usize, bool, char),
}

fn tokenize(layout: &str) -> Vec<Tok> {
    let b = layout.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let starts = |i: usize, s: &str| layout[i..].starts_with(s);
    while i < b.len() {
        let rest = &layout[i..];
        let (tok, len) = if rest.starts_with("January") {
            (Tok::LongMonth, 7)
        } else if rest.starts_with("Jan") {
            (Tok::Month, 3)
        } else if rest.starts_with("Monday") {
            (Tok::LongWeekDay, 6)
        } else if rest.starts_with("Mon") {
            (Tok::WeekDay, 3)
        } else if rest.starts_with("MST") {
            (Tok::TZ, 3)
        } else if rest.starts_with("2006") {
            (Tok::LongYear, 4)
        } else if rest.starts_with("002") {
            (Tok::ZeroYearDay, 3)
        } else if rest.starts_with("__2") {
            (Tok::UnderYearDay, 3)
        } else if rest.starts_with("01") {
            (Tok::ZeroMonth, 2)
        } else if rest.starts_with("02") {
            (Tok::ZeroDay, 2)
        } else if rest.starts_with("03") {
            (Tok::ZeroHour12, 2)
        } else if rest.starts_with("04") {
            (Tok::ZeroMinute, 2)
        } else if rest.starts_with("05") {
            (Tok::ZeroSecond, 2)
        } else if rest.starts_with("06") {
            (Tok::Year, 2)
        } else if rest.starts_with("15") {
            (Tok::Hour, 2)
        } else if rest.starts_with("_2") {
            (Tok::UnderDay, 2)
        } else if rest.starts_with("PM") {
            (Tok::PM, 2)
        } else if rest.starts_with("pm") {
            (Tok::Pm, 2)
        } else if rest.starts_with("Z07:00:00") {
            (
                Tok::Offset {
                    z: true,
                    colon: true,
                    secs: true,
                    hours_only: false,
                },
                9,
            )
        } else if rest.starts_with("-07:00:00") {
            (
                Tok::Offset {
                    z: false,
                    colon: true,
                    secs: true,
                    hours_only: false,
                },
                9,
            )
        } else if rest.starts_with("Z070000") {
            (
                Tok::Offset {
                    z: true,
                    colon: false,
                    secs: true,
                    hours_only: false,
                },
                7,
            )
        } else if rest.starts_with("-070000") {
            (
                Tok::Offset {
                    z: false,
                    colon: false,
                    secs: true,
                    hours_only: false,
                },
                7,
            )
        } else if rest.starts_with("Z07:00") {
            (
                Tok::Offset {
                    z: true,
                    colon: true,
                    secs: false,
                    hours_only: false,
                },
                6,
            )
        } else if rest.starts_with("-07:00") {
            (
                Tok::Offset {
                    z: false,
                    colon: true,
                    secs: false,
                    hours_only: false,
                },
                6,
            )
        } else if rest.starts_with("Z0700") {
            (
                Tok::Offset {
                    z: true,
                    colon: false,
                    secs: false,
                    hours_only: false,
                },
                5,
            )
        } else if rest.starts_with("-0700") {
            (
                Tok::Offset {
                    z: false,
                    colon: false,
                    secs: false,
                    hours_only: false,
                },
                5,
            )
        } else if rest.starts_with("Z07") {
            (
                Tok::Offset {
                    z: true,
                    colon: false,
                    secs: false,
                    hours_only: true,
                },
                3,
            )
        } else if rest.starts_with("-07") {
            (
                Tok::Offset {
                    z: false,
                    colon: false,
                    secs: false,
                    hours_only: true,
                },
                3,
            )
        } else if starts(i, ".0") || starts(i, ",0") || starts(i, ".9") || starts(i, ",9") {
            let sep = b[i] as char;
            let digit = b[i + 1];
            let mut j = i + 1;
            while j < b.len() && b[j] == digit {
                j += 1;
            }
            // Go only treats it as a fraction when not followed by another digit.
            if j < b.len() && b[j].is_ascii_digit() {
                (Tok::Lit(sep), 1)
            } else {
                (Tok::Frac(j - i - 1, digit == b'9', sep), j - i)
            }
        } else if rest.starts_with('1') {
            (Tok::NumMonth, 1)
        } else if rest.starts_with('2') {
            (Tok::Day, 1)
        } else if rest.starts_with('3') {
            (Tok::Hour12, 1)
        } else if rest.starts_with('4') {
            (Tok::Minute, 1)
        } else if rest.starts_with('5') {
            (Tok::Second, 1)
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            (Tok::Lit(c), c.len_utf8())
        };
        out.push(tok);
        i += len;
    }
    out
}

/// Formats `t` with a Go layout.
pub fn format(t: &DateTime<FixedOffset>, layout: &str) -> String {
    let mut s = String::with_capacity(layout.len() + 8);
    let offset = t.offset().local_minus_utc();
    for tok in tokenize(layout) {
        match tok {
            Tok::Lit(c) => s.push(c),
            Tok::LongMonth => s.push_str(LONG_MONTHS[t.month0() as usize]),
            Tok::Month => s.push_str(&LONG_MONTHS[t.month0() as usize][..3]),
            Tok::NumMonth => {
                let _ = write!(s, "{}", t.month());
            }
            Tok::ZeroMonth => {
                let _ = write!(s, "{:02}", t.month());
            }
            Tok::LongWeekDay => s.push_str(LONG_DAYS[t.weekday().num_days_from_monday() as usize]),
            Tok::WeekDay => {
                s.push_str(&LONG_DAYS[t.weekday().num_days_from_monday() as usize][..3])
            }
            Tok::Day => {
                let _ = write!(s, "{}", t.day());
            }
            Tok::UnderDay => {
                let _ = write!(s, "{:>2}", t.day());
            }
            Tok::ZeroDay => {
                let _ = write!(s, "{:02}", t.day());
            }
            Tok::UnderYearDay => {
                let _ = write!(s, "{:>3}", t.ordinal());
            }
            Tok::ZeroYearDay => {
                let _ = write!(s, "{:03}", t.ordinal());
            }
            Tok::Hour => {
                let _ = write!(s, "{:02}", t.hour());
            }
            Tok::Hour12 | Tok::ZeroHour12 => {
                let h = match t.hour() % 12 {
                    0 => 12,
                    h => h,
                };
                if matches!(tok, Tok::Hour12) {
                    let _ = write!(s, "{h}");
                } else {
                    let _ = write!(s, "{h:02}");
                }
            }
            Tok::Minute => {
                let _ = write!(s, "{}", t.minute());
            }
            Tok::ZeroMinute => {
                let _ = write!(s, "{:02}", t.minute());
            }
            Tok::Second => {
                let _ = write!(s, "{}", t.second());
            }
            Tok::ZeroSecond => {
                let _ = write!(s, "{:02}", t.second());
            }
            Tok::LongYear => {
                let _ = write!(s, "{:04}", t.year());
            }
            Tok::Year => {
                let _ = write!(s, "{:02}", t.year().rem_euclid(100));
            }
            Tok::PM => s.push_str(if t.hour() >= 12 { "PM" } else { "AM" }),
            Tok::Pm => s.push_str(if t.hour() >= 12 { "pm" } else { "am" }),
            Tok::TZ => {
                if offset == 0 {
                    s.push_str("UTC");
                } else {
                    let (sign, abs) = if offset < 0 {
                        ('-', -offset)
                    } else {
                        ('+', offset)
                    };
                    let _ = write!(s, "{sign}{:02}{:02}", abs / 3600, abs % 3600 / 60);
                }
            }
            Tok::Offset {
                z,
                colon,
                secs,
                hours_only,
            } => {
                if z && offset == 0 {
                    s.push('Z');
                } else {
                    let (sign, abs) = if offset < 0 {
                        ('-', -offset)
                    } else {
                        ('+', offset)
                    };
                    let _ = write!(s, "{sign}{:02}", abs / 3600);
                    if !hours_only {
                        if colon {
                            s.push(':');
                        }
                        let _ = write!(s, "{:02}", abs % 3600 / 60);
                        if secs {
                            if colon {
                                s.push(':');
                            }
                            let _ = write!(s, "{:02}", abs % 60);
                        }
                    }
                }
            }
            Tok::Frac(n, trim, sep) => {
                let nanos = t.nanosecond() % 1_000_000_000;
                let digits = format!("{nanos:09}");
                let mut d = &digits[..n.min(9)];
                if trim {
                    d = d.trim_end_matches('0');
                    if d.is_empty() {
                        continue;
                    }
                }
                s.push(sep);
                s.push_str(d);
            }
        }
    }
    s
}

/// Go's `fmtFrac`: writes `.ddd` (trailing zeros removed) of `v / 10^prec`, returns the
/// integer part.
fn fmt_frac(out: &mut Vec<u8>, mut v: u64, prec: u32) -> u64 {
    let mut print = false;
    let mut digits = Vec::new();
    for _ in 0..prec {
        let digit = v % 10;
        print = print || digit != 0;
        if print {
            digits.push(b'0' + digit as u8);
        }
        v /= 10;
    }
    if print {
        digits.push(b'.');
    }
    digits.reverse();
    // `out` is built in reverse order at the end; here digits are in forward order.
    out.splice(0..0, digits);
    v
}

/// `time.Duration.String()`: `1.234567ms`, `12.5µs`, `1m2.5s`, `0s`.
pub fn duration_string(d: Duration) -> String {
    let u = d.as_nanos().min(u64::MAX as u128) as u64;
    if u == 0 {
        return "0s".to_string();
    }
    let mut tail: Vec<u8> = Vec::new();
    if u < 1_000_000_000 {
        let (prec, unit) = if u < 1_000 {
            (0, "ns")
        } else if u < 1_000_000 {
            (3, "µs")
        } else {
            (6, "ms")
        };
        let int = fmt_frac(&mut tail, u, prec);
        let mut s = int.to_string().into_bytes();
        s.extend_from_slice(&tail);
        s.extend_from_slice(unit.as_bytes());
        return String::from_utf8(s).unwrap_or_default();
    }
    let secs = fmt_frac(&mut tail, u, 9);
    let mut s = String::new();
    let (h, m, sec) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        let _ = write!(s, "{h}h");
    }
    if h > 0 || m > 0 {
        let _ = write!(s, "{m}m");
    }
    let _ = write!(s, "{sec}");
    s.push_str(std::str::from_utf8(&tail).unwrap_or_default());
    s.push('s');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(offset_secs: i32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(offset_secs)
            .unwrap()
            .with_ymd_and_hms(2026, 1, 4, 15, 4, 5)
            .unwrap()
            + chrono::Duration::nanoseconds(446_120_000)
    }

    #[test]
    fn layouts() {
        assert_eq!(format(&t(7200), ISO8601), "2026-01-04T15:04:05.446+0200");
        assert_eq!(format(&t(0), ISO8601), "2026-01-04T15:04:05.446Z");
        assert_eq!(format(&t(-3600), RFC3339), "2026-01-04T15:04:05-01:00");
        assert_eq!(format(&t(0), RFC3339_NANO), "2026-01-04T15:04:05.44612Z");
        assert_eq!(
            format(&t(7200), "2006-01-02T15:04:05-0700"),
            "2026-01-04T15:04:05+0200"
        );
        assert_eq!(
            format(&t(7200), "2006-01-02T15:04:05.000"),
            "2026-01-04T15:04:05.446"
        );
        assert_eq!(format(&t(0), "Mon Jan _2 3:04PM"), "Sun Jan  4 3:04PM");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_string(Duration::ZERO), "0s");
        assert_eq!(duration_string(Duration::from_nanos(512)), "512ns");
        assert_eq!(duration_string(Duration::from_nanos(12_500)), "12.5µs");
        assert_eq!(
            duration_string(Duration::from_nanos(1_234_567)),
            "1.234567ms"
        );
        assert_eq!(duration_string(Duration::from_millis(1500)), "1.5s");
        assert_eq!(duration_string(Duration::from_secs(62)), "1m2s");
        assert_eq!(duration_string(Duration::from_secs(3600)), "1h0m0s");
    }
}
