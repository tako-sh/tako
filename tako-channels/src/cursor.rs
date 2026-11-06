use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{fmt, str::FromStr};

use crate::ChannelError;

/// An opaque replay position. SQLite positions belong to one database epoch and stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayCursor {
    pub(crate) scope: Option<([u8; 16], [u8; 16])>,
    pub(crate) position: i64,
}

impl ReplayCursor {
    pub(crate) fn sqlite(epoch: [u8; 16], channel: &str, position: i64) -> Self {
        let digest = openssl::sha::sha256(channel.as_bytes());
        let mut stream = [0; 16];
        stream.copy_from_slice(&digest[..16]);
        Self {
            scope: Some((epoch, stream)),
            position,
        }
    }

    pub(crate) fn postgres(position: i64) -> Self {
        Self {
            scope: None,
            position,
        }
    }

    pub(crate) fn pg_position(self) -> Result<i64, ChannelError> {
        if self.scope.is_some() {
            return Err(ChannelError::StaleCursor);
        }
        Ok(self.position)
    }
}

impl fmt::Display for ReplayCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((epoch, stream)) = self.scope {
            write!(
                f,
                "s1.{}.{}.{}",
                URL_SAFE_NO_PAD.encode(epoch),
                URL_SAFE_NO_PAD.encode(stream),
                self.position
            )
        } else {
            write!(f, "{}", self.position)
        }
    }
}

impl FromStr for ReplayCursor {
    type Err = ChannelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ChannelError::BadRequest("invalid replay cursor".into());
        if let Some(value) = value.strip_prefix("s1.") {
            let mut parts = value.split('.');
            let mut component = || -> Result<[u8; 16], ChannelError> {
                URL_SAFE_NO_PAD
                    .decode(parts.next().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?
                    .try_into()
                    .map_err(|_| invalid())
            };
            let epoch = component()?;
            let stream = component()?;
            let position = parts
                .next()
                .ok_or_else(invalid)?
                .parse::<i64>()
                .map_err(|_| invalid())?;
            if position < 0 || parts.next().is_some() {
                return Err(invalid());
            }
            Ok(Self {
                scope: Some((epoch, stream)),
                position,
            })
        } else {
            let position = value.parse::<i64>().map_err(|_| invalid())?;
            if position < 0 {
                return Err(invalid());
            }
            Ok(Self::postgres(position))
        }
    }
}
