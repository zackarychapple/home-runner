//! Just enough UTC date handling for the job ledger, without pulling in a date/time crate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

pub fn unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

pub fn now() -> i64 {
    unix(SystemTime::now())
}

/// `2026-10-29T10:05:00Z`
pub fn rfc3339(secs: i64) -> String {
    let (y, m, d, hh, mm, ss) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// `2026-10-29 10:05` for tables.
pub fn short(secs: i64) -> String {
    let (y, m, d, hh, mm, _) = civil(secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

pub fn day(secs: i64) -> String {
    let (y, m, d, ..) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn month(secs: i64) -> String {
    let (y, m, ..) = civil(secs);
    format!("{y:04}-{m:02}")
}

/// Parses GitHub API timestamps (`2026-10-29T10:05:00Z`, optionally with fractions or an offset).
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(y, mo, d) * 86400 + hh * 3600 + mm * 60 + ss - offset)
}

/// `7d`, `12h`, `90m`, `2w`, or `all`.
pub fn parse_since(s: &str) -> Result<Option<Duration>> {
    if s.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {s:?} (try 7d, 24h, all)"))?;
    let secs = match unit {
        "m" => 60,
        "h" | "" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => bail!("invalid duration {s:?} (try 7d, 24h, all)"),
    };
    Ok(Some(Duration::from_secs(n * secs)))
}

fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

// Howard Hinnant's algorithms: http://howardhinnant.github.io/date_algorithms.html
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_timestamps() {
        for secs in [0, 951_782_400, 1_793_268_312, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&rfc3339(secs)), Some(secs));
        }
        assert_eq!(rfc3339(1_793_268_312), "2026-10-29T10:05:12Z");
        assert_eq!(
            parse_rfc3339("2026-10-29T12:05:12.123+02:00"),
            Some(1_793_268_312)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn parses_since() {
        assert_eq!(
            parse_since("7d").unwrap(),
            Some(Duration::from_secs(7 * 86400))
        );
        assert_eq!(parse_since("all").unwrap(), None);
        assert!(parse_since("7y").is_err());
    }
}
