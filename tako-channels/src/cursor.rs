use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{fmt, str::FromStr};

use crate::ChannelError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorScope {
    /// One SQLite database epoch and stream.
    Sqlite([u8; 16], [u8; 16]),
    /// One stream of a shared PostgreSQL history.
    Postgres([u8; 16]),
}

/// An opaque replay position. Positions belong to one stream; SQLite positions
/// also belong to one database epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayCursor {
    pub(crate) scope: Option<CursorScope>,
    pub(crate) position: i64,
}

fn stream_id(channel: &str) -> [u8; 16] {
    let digest = openssl::sha::sha256(channel.as_bytes());
    let mut stream = [0; 16];
    stream.copy_from_slice(&digest[..16]);
    stream
}

impl ReplayCursor {
    pub(crate) fn sqlite(epoch: [u8; 16], channel: &str, position: i64) -> Self {
        Self {
            scope: Some(CursorScope::Sqlite(epoch, stream_id(channel))),
            position,
        }
    }

    pub(crate) fn postgres(channel: &str, position: i64) -> Self {
        Self {
            scope: Some(CursorScope::Postgres(stream_id(channel))),
            position,
        }
    }

    /// A bare position names no stream, so no store resumes from it.
    pub(crate) fn unscoped(position: i64) -> Self {
        Self {
            scope: None,
            position,
        }
    }

    pub(crate) fn pg_position(self, channel: &str) -> Result<i64, ChannelError> {
        if self.scope != Some(CursorScope::Postgres(stream_id(channel))) {
            return Err(ChannelError::StaleCursor);
        }
        Ok(self.position)
    }
}

impl fmt::Display for ReplayCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.scope {
            Some(CursorScope::Sqlite(epoch, stream)) => write!(
                f,
                "s1.{}.{}.{}",
                URL_SAFE_NO_PAD.encode(epoch),
                URL_SAFE_NO_PAD.encode(stream),
                self.position
            ),
            Some(CursorScope::Postgres(stream)) => {
                write!(f, "p1.{}.{}", URL_SAFE_NO_PAD.encode(stream), self.position)
            }
            None => write!(f, "{}", self.position),
        }
    }
}

impl FromStr for ReplayCursor {
    type Err = ChannelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ChannelError::BadRequest("invalid replay cursor".into());
        let (sqlite, value) = match (value.strip_prefix("s1."), value.strip_prefix("p1.")) {
            (Some(value), _) => (true, value),
            (_, Some(value)) => (false, value),
            _ => {
                let position = value.parse::<i64>().map_err(|_| invalid())?;
                if position < 0 {
                    return Err(invalid());
                }
                return Ok(Self::unscoped(position));
            }
        };
        let mut parts = value.split('.');
        let mut component = || -> Result<[u8; 16], ChannelError> {
            URL_SAFE_NO_PAD
                .decode(parts.next().ok_or_else(invalid)?)
                .map_err(|_| invalid())?
                .try_into()
                .map_err(|_| invalid())
        };
        let scope = if sqlite {
            CursorScope::Sqlite(component()?, component()?)
        } else {
            CursorScope::Postgres(component()?)
        };
        let position = parts
            .next()
            .ok_or_else(invalid)?
            .parse::<i64>()
            .map_err(|_| invalid())?;
        if position < 0 || parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            scope: Some(scope),
            position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_stay_bound_to_their_stream() {
        let sqlite = ReplayCursor::sqlite([7; 16], "chat", 42);
        let postgres = ReplayCursor::postgres("chat", 42);
        for cursor in [sqlite, postgres, ReplayCursor::unscoped(42)] {
            assert_eq!(cursor.to_string().parse::<ReplayCursor>().unwrap(), cursor);
        }
        assert!(sqlite.to_string().starts_with("s1."));
        assert!(postgres.to_string().starts_with("p1."));
        assert_eq!(postgres.pg_position("chat").unwrap(), 42);
        for foreign in [
            ReplayCursor::postgres("other", 42),
            ReplayCursor::unscoped(42),
            sqlite,
        ] {
            assert!(matches!(
                foreign.pg_position("chat"),
                Err(ChannelError::StaleCursor)
            ));
        }
        assert_ne!(sqlite, ReplayCursor::sqlite([7; 16], "other", 42));
    }

    #[test]
    fn malformed_cursors_are_rejected() {
        for value in ["", "-1", "s1.x", "p1.x.1", "p1.AAAAAAAAAAAAAAAAAAAAAA.-1"] {
            assert!(value.parse::<ReplayCursor>().is_err(), "{value}");
        }
        let extra = format!("{}.1", ReplayCursor::postgres("chat", 1));
        assert!(extra.parse::<ReplayCursor>().is_err());
    }
}
