//! Fast timestamp parsing/formatting without a calendar dependency.
//!
//! Jetstream v2 renders event times as RFC 3339 UTC with exactly six
//! fractional digits; record `createdAt` values are client supplied and can be
//! any RFC 3339 flavour, so the parser accepts variable fractional digits and
//! numeric offsets.

/// Days since 1970-01-01 for a proleptic Gregorian civil date.
#[inline]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01.
#[inline]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[inline]
fn digits(b: &[u8]) -> Option<i64> {
    let mut v = 0i64;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

/// Parse an RFC 3339 timestamp into unix microseconds.
///
/// Also accepts a bare date (`2026-11-12`) and a missing offset (treated as
/// UTC), which is convenient for filter literals.
pub fn parse_rfc3339_us(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&b[0..4])?;
    let mo = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    if b.len() == 10 {
        return Some(days * 86_400_000_000);
    }
    if b.len() < 16 || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') || b[13] != b':' {
        return None;
    }
    let h = digits(&b[11..13])?;
    let mi = digits(&b[14..16])?;
    let mut pos = 16;
    let mut sec = 0;
    let mut micros = 0i64;
    if pos < b.len() && b[pos] == b':' {
        if b.len() < pos + 3 {
            return None;
        }
        sec = digits(&b[pos + 1..pos + 3])?;
        pos += 3;
        if pos < b.len() && (b[pos] == b'.' || b[pos] == b',') {
            pos += 1;
            let start = pos;
            let mut scale = 100_000i64;
            while pos < b.len() && b[pos].is_ascii_digit() {
                micros += (b[pos] - b'0') as i64 * scale;
                scale /= 10;
                pos += 1;
            }
            if pos == start {
                return None;
            }
        }
    }
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut offset_s = 0i64;
    if pos < b.len() {
        match b[pos] {
            b'Z' | b'z' => {
                if pos + 1 != b.len() {
                    return None;
                }
            }
            sign @ (b'+' | b'-') => {
                let rest = &b[pos + 1..];
                let (oh, om) = match rest.len() {
                    5 if rest[2] == b':' => (digits(&rest[0..2])?, digits(&rest[3..5])?),
                    4 => (digits(&rest[0..2])?, digits(&rest[2..4])?),
                    2 => (digits(&rest[0..2])?, 0),
                    _ => return None,
                };
                offset_s = (oh * 3600 + om * 60) * if sign == b'+' { 1 } else { -1 };
            }
            _ => return None,
        }
    }
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_s;
    Some(secs * 1_000_000 + micros)
}

/// Broken-down UTC time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub micros: u32,
}

pub fn civil_from_us(us: i64) -> Civil {
    let secs = us.div_euclid(1_000_000);
    let micros = us.rem_euclid(1_000_000) as u32;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    Civil { year, month, day, hour: sod / 3600, minute: sod % 3600 / 60, second: sod % 60, micros }
}

/// `2026-08-13T06:47:43.959305Z`
pub fn format_rfc3339_us(us: i64) -> String {
    let c = civil_from_us(us);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.micros
    )
}

/// Current unix time in microseconds.
pub fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Parse a duration literal such as `90s`, `5m`, `2h`, `1d`, `250ms` into
/// microseconds. A bare number is seconds.
pub fn parse_duration_us(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mul = match unit.trim() {
        "us" | "µs" => 1.0,
        "ms" => 1e3,
        "" | "s" | "sec" | "secs" => 1e6,
        "m" | "min" | "mins" => 60e6,
        "h" | "hr" | "hrs" => 3600e6,
        "d" | "day" | "days" => 86_400e6,
        "w" => 7.0 * 86_400e6,
        _ => return None,
    };
    Some((n * mul) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wire_layout() {
        assert_eq!(parse_rfc3339_us("1970-01-01T00:00:00.000000Z"), Some(0));
        assert_eq!(parse_rfc3339_us("2026-08-13T06:47:43.959305Z"), Some(1_786_603_663_959_305));
        assert_eq!(parse_rfc3339_us("2026-05-25T14:23:28.778Z"), Some(1_779_719_008_778_000));
    }

    #[test]
    fn parses_offsets_and_variants() {
        let base = parse_rfc3339_us("2024-02-29T12:00:00Z").unwrap();
        assert_eq!(parse_rfc3339_us("2024-02-29T21:00:00+09:00"), Some(base));
        assert_eq!(parse_rfc3339_us("2024-02-29T07:00:00-0500"), Some(base));
        assert_eq!(parse_rfc3339_us("2024-02-29T12:00Z"), Some(base));
        assert_eq!(parse_rfc3339_us("2024-02-29T12:00:00.123456789Z"), Some(base + 123_456));
        assert_eq!(parse_rfc3339_us("2024-02-29"), Some(base - 12 * 3_600_000_000));
        assert_eq!(parse_rfc3339_us("garbage"), None);
        assert_eq!(parse_rfc3339_us("2024-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_us("2024-02-29T12:00:00Zjunk"), None);
    }

    #[test]
    fn round_trips() {
        for us in [0, 1_786_603_663_959_305, 951_782_400_000_000, -1, 4_102_444_800_000_001] {
            assert_eq!(parse_rfc3339_us(&format_rfc3339_us(us)), Some(us), "{us}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_us("5m"), Some(300_000_000));
        assert_eq!(parse_duration_us("1.5h"), Some(5_400_000_000));
        assert_eq!(parse_duration_us("250ms"), Some(250_000));
        assert_eq!(parse_duration_us("30"), Some(30_000_000));
        assert_eq!(parse_duration_us("5 parsecs"), None);
    }
}
