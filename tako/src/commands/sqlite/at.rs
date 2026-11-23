//! The time `tako sqlite restore --at` restores to.

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// Parses `--at`: an RFC 3339 time (`2026-11-23T10:15:00Z`), a local time
/// (`2026-11-23 10:15` or `2026-11-23 10:15:30`), or an age before `now`
/// (`90s`, `15m`, `2h`, `1d`, optionally followed by `ago`). Returns Unix
/// milliseconds.
pub(super) fn parse_at(input: &str, now: OffsetDateTime, local: UtcOffset) -> Result<i64, String> {
    let input = input.trim();
    let invalid = || {
        format!(
            "`{input}` is not a time. Use a time such as `2026-11-23 10:15` (local), `2026-11-23T10:15:00Z`, or an age such as `15m`, `2h` or `1d`."
        )
    };
    let at = if let Some(age) = parse_age(input) {
        now - age
    } else if let Ok(at) =
        OffsetDateTime::parse(input, &time::format_description::well_known::Rfc3339)
    {
        at
    } else {
        parse_local(input).ok_or_else(invalid)?.assume_offset(local)
    };
    if at > now {
        return Err(format!("`{input}` is in the future."));
    }
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).map_err(|_| invalid())
}

fn parse_age(input: &str) -> Option<time::Duration> {
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
    Some(time::Duration::seconds(amount.checked_mul(seconds)?))
}

/// `YYYY-MM-DD HH:MM` or `YYYY-MM-DD HH:MM:SS`.
fn parse_local(input: &str) -> Option<PrimitiveDateTime> {
    let (date, clock) = input.split_once([' ', 'T'])?;
    let mut date = date.split('-');
    let year: i32 = date.next()?.parse().ok()?;
    let month: u8 = date.next()?.parse().ok()?;
    let day: u8 = date.next()?.parse().ok()?;
    if date.next().is_some() {
        return None;
    }
    let mut clock = clock.split(':');
    let hour: u8 = clock.next()?.parse().ok()?;
    let minute: u8 = clock.next()?.parse().ok()?;
    let second: u8 = match clock.next() {
        Some(second) => second.parse().ok()?,
        None => 0,
    };
    if clock.next().is_some() {
        return None;
    }
    Some(PrimitiveDateTime::new(
        Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?,
        Time::from_hms(hour, minute, second).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_795_000_000).unwrap()
    }

    #[test]
    fn ages_count_back_from_now() {
        let now = now();
        let ms = |seconds: i64| (now.unix_timestamp() - seconds) * 1000;
        assert_eq!(parse_at("15m", now, UtcOffset::UTC), Ok(ms(15 * 60)));
        assert_eq!(parse_at("2h ago", now, UtcOffset::UTC), Ok(ms(2 * 3600)));
        assert_eq!(parse_at("1d", now, UtcOffset::UTC), Ok(ms(86_400)));
        assert_eq!(parse_at("90s", now, UtcOffset::UTC), Ok(ms(90)));
        assert!(parse_at("5w", now, UtcOffset::UTC).is_err());
    }

    #[test]
    fn local_times_use_the_local_offset() {
        let now = now();
        let plus_two = UtcOffset::from_hms(2, 0, 0).unwrap();
        let utc = parse_at("2026-11-01 10:15", now, UtcOffset::UTC).unwrap();
        let local = parse_at("2026-11-01 10:15", now, plus_two).unwrap();
        assert_eq!(utc - local, 2 * 3600 * 1000);
        assert_eq!(
            parse_at("2026-11-01 10:15:30", now, UtcOffset::UTC).unwrap() - utc,
            30_000
        );
    }

    #[test]
    fn rfc3339_times_keep_their_offset() {
        let now = now();
        assert_eq!(
            parse_at(
                "2026-11-01T10:15:00Z",
                now,
                UtcOffset::from_hms(5, 0, 0).unwrap()
            ),
            parse_at("2026-11-01 10:15", now, UtcOffset::UTC)
        );
    }

    #[test]
    fn future_and_malformed_times_are_refused() {
        let now = now();
        assert!(
            parse_at("2099-01-01 00:00", now, UtcOffset::UTC)
                .unwrap_err()
                .contains("future")
        );
        assert!(parse_at("yesterday", now, UtcOffset::UTC).is_err());
        assert!(parse_at("2026-13-01 10:00", now, UtcOffset::UTC).is_err());
    }
}
