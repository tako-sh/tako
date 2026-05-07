use time::OffsetDateTime;

#[cfg(test)]
pub(super) fn parse_uptime_since(line: &str) -> Option<OffsetDateTime> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    let time_parts: Vec<&str> = parts[1].split(':').collect();
    if date_parts.len() < 3 || time_parts.len() < 3 {
        return None;
    }

    let year: i32 = date_parts[0].parse().ok()?;
    let month: u8 = date_parts[1].parse().ok()?;
    let day: u8 = date_parts[2].parse().ok()?;
    let hour: u8 = time_parts[0].parse().ok()?;
    let minute: u8 = time_parts[1].parse().ok()?;
    let second: u8 = time_parts[2].parse().ok()?;

    let month = time::Month::try_from(month).ok()?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let time = time::Time::from_hms(hour, minute, second).ok()?;
    Some(OffsetDateTime::new_utc(date, time))
}

pub(super) fn format_deployed_at(unix_secs: i64) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(unix_secs).ok()?;
    let local =
        dt.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
    let month = local.month();
    Some(format!(
        "{} {}, {} {:02}:{:02}",
        month_abbrev(month),
        local.day(),
        local.year(),
        local.hour(),
        local.minute(),
    ))
}

fn month_abbrev(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}

pub(super) fn format_duration_human(total_secs: u64) -> String {
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;

    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

#[cfg(test)]
pub(super) fn format_unix_timestamp_local(unix_secs: i64) -> Option<String> {
    super::format_unix_timestamp_with_date_command(unix_secs)
        .or_else(|| format_unix_timestamp_with_offset(unix_secs, super::local_offset()))
}

#[cfg(test)]
pub(super) fn format_unix_timestamp_with_offset(
    unix_secs: i64,
    offset: time::UtcOffset,
) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()?
        .to_offset(offset);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}
