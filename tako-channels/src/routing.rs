use percent_encoding::percent_decode_str;

use crate::{CHANNELS_BASE_PATH, ChannelError, ChannelOperation, ChannelRoute, ReplayCursor};

pub fn parse_channel_route(path: &str) -> Result<Option<ChannelRoute>, ChannelError> {
    if path == crate::shared::PATH || !path.starts_with(CHANNELS_BASE_PATH) {
        return Ok(None);
    }

    let rest = &path[CHANNELS_BASE_PATH.len()..];
    let (operation, rest) = if let Some(name) = rest.strip_prefix("sse/") {
        (ChannelOperation::Subscribe, name)
    } else if let Some(name) = rest.strip_prefix("ws/") {
        (ChannelOperation::Connect, name)
    } else {
        return Err(ChannelError::InvalidPath);
    };
    if rest.is_empty() || rest.contains('/') {
        return Err(ChannelError::InvalidPath);
    }
    let channel = percent_decode_str(rest)
        .decode_utf8()
        .map_err(|_| ChannelError::InvalidPath)?
        .into_owned();
    if crate::reserved_stream_name(&channel) {
        return Err(ChannelError::InvalidPath);
    }

    Ok(Some(ChannelRoute { channel, operation }))
}

pub fn parse_message_id_cursor(
    value: Option<&str>,
    field_name: &str,
) -> Result<Option<ReplayCursor>, ChannelError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    value
        .parse::<ReplayCursor>()
        .map(Some)
        .map_err(|_| ChannelError::BadRequest(format!("invalid '{field_name}' cursor")))
}

pub fn parse_ws_last_message_id(query: Option<&str>) -> Result<Option<ReplayCursor>, ChannelError> {
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

/// Where a reader starts: after an explicit cursor, at the oldest retained
/// message, or at the head.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplayFrom {
    #[default]
    Head,
    Start,
    After(ReplayCursor),
}

impl ReplayFrom {
    /// An explicit cursor wins over `from=start`.
    pub fn new(cursor: Option<ReplayCursor>, from_start: bool) -> Self {
        match cursor {
            Some(cursor) => Self::After(cursor),
            None if from_start => Self::Start,
            None => Self::Head,
        }
    }

    /// Prefer `cursor`, such as one from a WebSocket auth frame, when given.
    pub fn or_cursor(self, cursor: Option<ReplayCursor>) -> Self {
        cursor.map_or(self, Self::After)
    }
}

/// Read the reserved `from` query key of a channel route. `from=start`
/// replays every retained message; any other value is rejected so a param
/// cannot hide behind the name.
pub fn parse_replay_from(query: Option<&str>) -> Result<bool, ChannelError> {
    let mut from_start = false;
    for pair in query.unwrap_or_default().split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |part: &str| {
            percent_decode_str(&part.replace('+', " "))
                .decode_utf8()
                .map(|part| part.into_owned())
                .map_err(|_| ChannelError::BadRequest("invalid query encoding".to_string()))
        };
        if decode(key)? != "from" {
            continue;
        }
        if decode(value)? != "start" {
            return Err(ChannelError::BadRequest(
                "invalid 'from': expected 'start'".to_string(),
            ));
        }
        from_start = true;
    }
    Ok(from_start)
}
