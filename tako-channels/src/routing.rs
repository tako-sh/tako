use percent_encoding::percent_decode_str;

use crate::{CHANNELS_BASE_PATH, ChannelError, ChannelRoute};

pub fn parse_channel_route(path: &str) -> Result<Option<ChannelRoute>, ChannelError> {
    if !path.starts_with(CHANNELS_BASE_PATH) {
        return Ok(None);
    }

    let rest = &path[CHANNELS_BASE_PATH.len()..];
    if rest.is_empty() || rest.contains('/') {
        return Err(ChannelError::InvalidPath);
    }
    let channel = percent_decode_str(rest)
        .decode_utf8()
        .map_err(|_| ChannelError::InvalidPath)?
        .into_owned();

    Ok(Some(ChannelRoute { channel }))
}

pub fn parse_message_id_cursor(
    value: Option<&str>,
    field_name: &str,
) -> Result<Option<i64>, ChannelError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    value
        .parse::<i64>()
        .map(Some)
        .map_err(|_| ChannelError::BadRequest(format!("invalid '{field_name}' cursor")))
}

pub fn parse_ws_last_message_id(query: Option<&str>) -> Result<Option<i64>, ChannelError> {
    let Some(query) = query else {
        return Ok(None);
    };

    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or_default();
        if key != "last_message_id" {
            continue;
        }
        let value = percent_decode_str(parts.next().unwrap_or_default())
            .decode_utf8()
            .map_err(|_| ChannelError::BadRequest("invalid query encoding".to_string()))?;
        return parse_message_id_cursor(Some(value.as_ref()), "last_message_id");
    }

    Ok(None)
}
