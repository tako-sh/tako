use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};

use crate::ChannelError;

/// One database epoch and stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorScope {
    epoch: [u8; 16],
    stream: [u8; 16],
}

/// An opaque replay position. Positions belong to one stream of one database
/// epoch: a reset, restore or import gives the history a new epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayCursor {
    pub(crate) scope: Option<CursorScope>,
    pub(crate) position: i64,
}

fn stream_id(channel: &str) -> [u8; 16] {
    let digest = Sha256::digest(channel.as_bytes());
    let mut stream = [0; 16];
    stream.copy_from_slice(&digest[..16]);
    stream
}

impl ReplayCursor {
    pub(crate) fn scoped(epoch: [u8; 16], channel: &str, position: i64) -> Self {
        Self {
            scope: Some(CursorScope {
                epoch,
                stream: stream_id(channel),
            }),
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
}

impl fmt::Display for ReplayCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.scope {
            Some(CursorScope { epoch, stream }) => write!(
                f,
                "s1.{}.{}.{}",
                URL_SAFE_NO_PAD.encode(epoch),
                URL_SAFE_NO_PAD.encode(stream),
                self.position
            ),
            None => write!(f, "{}", self.position),
        }
    }
}

impl FromStr for ReplayCursor {
    type Err = ChannelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ChannelError::BadRequest("invalid replay cursor".into());
        let (components, value) = match (value.strip_prefix("s1."), value.strip_prefix("p1.")) {
            (Some(value), _) => (2, value),
            // Cursors of the retired shared Postgres history name no epoch:
            // they are stale everywhere, so their clients resync.
            (_, Some(value)) => (1, value),
            _ => (0, value),
        };
        let mut parts = value.split('.');
        let mut scope = Vec::with_capacity(components);
        for _ in 0..components {
            let component: [u8; 16] = URL_SAFE_NO_PAD
                .decode(parts.next().ok_or_else(invalid)?)
                .map_err(|_| invalid())?
                .try_into()
                .map_err(|_| invalid())?;
            scope.push(component);
        }
        let position = parts
            .next()
            .ok_or_else(invalid)?
            .parse::<i64>()
            .map_err(|_| invalid())?;
        if position < 0 || parts.next().is_some() {
            return Err(invalid());
        }
        Ok(match scope[..] {
            [epoch, stream] => Self {
                scope: Some(CursorScope { epoch, stream }),
                position,
            },
            _ => Self::unscoped(position),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_stay_bound_to_their_stream() {
        let cursor = ReplayCursor::scoped([7; 16], "chat", 42);
        for cursor in [cursor, ReplayCursor::unscoped(42)] {
            assert_eq!(cursor.to_string().parse::<ReplayCursor>().unwrap(), cursor);
        }
        assert_eq!(
            cursor.to_string(),
            "s1.BwcHBwcHBwcHBwcHBwcHBw.MeBvfYn-uZoObAr_4Zh0jA.42"
        );
        assert_ne!(cursor, ReplayCursor::scoped([7; 16], "other", 42));
        assert_ne!(cursor, ReplayCursor::scoped([8; 16], "chat", 42));
        // A retired shared-Postgres cursor names no epoch, so every store
        // finds it stale.
        assert_eq!(
            "p1.MeBvfYn-uZoObAr_4Zh0jA.42"
                .parse::<ReplayCursor>()
                .unwrap(),
            ReplayCursor::unscoped(42)
        );
    }

    #[test]
    fn malformed_cursors_are_rejected() {
        for value in [
            "",
            "-1",
            "s1.x",
            "p1.x.1",
            "p1.AAAAAAAAAAAAAAAAAAAAAA.-1",
            "s1.AAAAAAAAAAAAAAAAAAAAAA.1",
        ] {
            assert!(value.parse::<ReplayCursor>().is_err(), "{value}");
        }
        let extra = format!("{}.1", ReplayCursor::scoped([7; 16], "chat", 1));
        assert!(extra.parse::<ReplayCursor>().is_err());
    }
}
