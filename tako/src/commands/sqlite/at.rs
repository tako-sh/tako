//! The time `tako sqlite restore --at` restores to.

use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};

/// Parses `--at`: an RFC 3339 time (`2026-11-23T10:15:00Z`), a time in `zone`
/// (`2026-11-23 10:15` or `2026-11-23 10:15:30`), or an age before `now`
/// (`90s`, `15m`, `2h`, `1d`, optionally followed by `ago`). Returns Unix
/// milliseconds.
///
/// A time in `zone` takes the offset that zone has on that date, so a time on
/// the other side of a daylight saving change than `now` keeps its meaning.
pub(super) fn parse_at<Tz: TimeZone>(
    input: &str,
    now: DateTime<Utc>,
    zone: &Tz,
) -> Result<i64, String> {
    let input = input.trim();
    let invalid = || {
        format!(
            "`{input}` is not a time. Use a time such as `2026-11-23 10:15` (local), `2026-11-23T10:15:00Z`, or an age such as `15m`, `2h` or `1d`."
        )
    };
    let at = if let Some(age) = parse_age(input) {
        now.checked_sub_signed(age)
            .ok_or_else(|| format!("`{input}` is further back than any time can be."))?
    } else if let Ok(at) = DateTime::parse_from_rfc3339(input) {
        at.with_timezone(&Utc)
    } else {
        let local = parse_local(input).ok_or_else(invalid)?;
        // A time a daylight saving change skips does not exist; one it
        // repeats is read as the earlier.
        zone.from_local_datetime(&local)
            .earliest()
            .ok_or_else(|| format!("`{input}` does not exist in the local time zone (a daylight saving change skips it)."))?
            .with_timezone(&Utc)
    };
    if at > now {
        return Err(format!("`{input}` is in the future."));
    }
    Ok(at.timestamp_millis())
}

fn parse_age(input: &str) -> Option<Duration> {
    let input = input.strip_suffix("ago").unwrap_or(input).trim_end();
    let unit = input.chars().last()?;
    let amount: i64 = input[..input.len() - unit.len_utf8()].trim().parse().ok()?;
    if amount < 0 {
        return None;
    }
    let seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return None,
    };
    Duration::try_seconds(amount.checked_mul(seconds)?)
}

/// `YYYY-MM-DD HH:MM` or `YYYY-MM-DD HH:MM:SS`, with a space or `T`.
fn parse_local(input: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(input, format).ok())
}

/// A Unix-millisecond time in the local time zone, with its offset, for
/// confirmations: what a restore will select, unambiguously.
pub(super) fn describe(at_unix_ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(at_unix_ms)
        .single()
        .map(|at| at.format("%Y-%m-%d %H:%M:%S %:z").to_string())
        .unwrap_or_else(|| format!("{at_unix_ms} (Unix ms)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_795_000_000, 0).unwrap()
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    #[test]
    fn ages_count_back_from_now() {
        let now = now();
        let ms = |seconds: i64| (now.timestamp() - seconds) * 1000;
        assert_eq!(parse_at("15m", now, &utc()), Ok(ms(15 * 60)));
        assert_eq!(parse_at("2h ago", now, &utc()), Ok(ms(2 * 3600)));
        assert_eq!(parse_at("1d", now, &utc()), Ok(ms(86_400)));
        assert_eq!(parse_at("90s", now, &utc()), Ok(ms(90)));
        assert!(parse_at("5w", now, &utc()).is_err());
    }

    #[test]
    fn local_times_use_the_zone() {
        let now = now();
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        let utc_ms = parse_at("2026-11-01 10:15", now, &utc()).unwrap();
        let local = parse_at("2026-11-01 10:15", now, &plus_two).unwrap();
        assert_eq!(utc_ms - local, 2 * 3600 * 1000);
        assert_eq!(
            parse_at("2026-11-01 10:15:30", now, &utc()).unwrap() - utc_ms,
            30_000
        );
        assert_eq!(parse_at("2026-11-01T10:15", now, &utc()), Ok(utc_ms));
    }

    #[test]
    fn rfc3339_times_keep_their_offset() {
        let now = now();
        let plus_five = FixedOffset::east_opt(5 * 3600).unwrap();
        assert_eq!(
            parse_at("2026-11-01T10:15:00Z", now, &plus_five),
            parse_at("2026-11-01 10:15", now, &utc())
        );
    }

    #[test]
    fn future_and_malformed_times_are_refused() {
        let now = now();
        assert!(parse_at("10000000000d", now, &utc()).is_err());
        assert!(parse_at("100000000000000s", now, &utc()).is_err());
        assert!(
            parse_at("2099-01-01 00:00", now, &utc())
                .unwrap_err()
                .contains("future")
        );
        assert!(parse_at("yesterday", now, &utc()).is_err());
        assert!(parse_at("2026-13-01 10:00", now, &utc()).is_err());
    }
}
