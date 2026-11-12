//! Why the Tako server rejected a request on the internal socket.

use std::{fmt, io};

/// The machine-readable reason a rejection carries. The server sends one only
/// where app code may need to branch; other rejections have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// A workflow worker write named a run this attempt no longer owns.
    RunNotOwned,
    /// An enqueue ID belongs to a different workflow or payload.
    RunIdConflict,
    /// The request or its arguments are malformed; retrying cannot succeed.
    InvalidRequest,
    /// The channel is not defined by this build or by the recipient app.
    ChannelUnknown,
    /// Channel params are malformed or fail the channel's params schema.
    ChannelParamsInvalid,
    /// The channel stream was closed and accepts no more messages.
    ChannelClosed,
    /// The server is draining, overloaded, or its storage is unreachable; retry later.
    Unavailable,
}

impl ErrorCode {
    const ALL: [Self; 7] = [
        Self::RunNotOwned,
        Self::RunIdConflict,
        Self::InvalidRequest,
        Self::ChannelUnknown,
        Self::ChannelParamsInvalid,
        Self::ChannelClosed,
        Self::Unavailable,
    ];

    /// The code as an error reply's `code` field carries it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunNotOwned => "run_not_owned",
            Self::RunIdConflict => "run_id_conflict",
            Self::InvalidRequest => "invalid_request",
            Self::ChannelUnknown => "channel_unknown",
            Self::ChannelParamsInvalid => "channel_params_invalid",
            Self::ChannelClosed => "channel_closed",
            Self::Unavailable => "unavailable",
        }
    }

    /// The code a reply's `code` field names, or `None` for a code this SDK
    /// version does not know.
    pub fn from_wire(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.as_str() == code)
    }

    /// The code of a rejected network call's error, when the server sent one.
    pub fn of(error: &io::Error) -> Option<Self> {
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Rejected>())
            .and_then(|rejected| rejected.code)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A network call the server rejected. The server's diagnostic is not kept:
/// it may describe the recipient's internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejected {
    pub code: Option<ErrorCode>,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("network primitive call failed")
    }
}

impl std::error::Error for Rejected {}

/// The `code` of an error reply, read leniently.
pub(crate) fn reply_code(response: &serde_json::Value) -> Option<ErrorCode> {
    response
        .get("code")
        .and_then(serde_json::Value::as_str)
        .and_then(ErrorCode::from_wire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_unknown_codes_are_unclassified() {
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_wire(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::from_wire("from_a_newer_server"), None);
        let rejected = io::Error::other(Rejected {
            code: Some(ErrorCode::ChannelClosed),
        });
        assert_eq!(ErrorCode::of(&rejected), Some(ErrorCode::ChannelClosed));
        assert_eq!(ErrorCode::of(&io::Error::other("other")), None);
    }
}
