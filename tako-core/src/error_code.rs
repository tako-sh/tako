//! Machine-readable codes on internal-socket error replies.
//!
//! The variants are generated from the `socket` scope of `errkit.json`
//! (`error_codes.rs`). The wire carries the readable [`ErrorCode::name`], the
//! errkit entry name, so SDKs and app code branch on `run_not_owned`, not on
//! message text. A reply without a code is unclassified.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub use crate::error_codes::Err as ErrorCode;

impl ErrorCode {
    /// The code as it appears in an error reply's `code` field.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChannelClosed => "channel_closed",
            Self::ChannelParamsInvalid => "channel_params_invalid",
            Self::ChannelUnknown => "channel_unknown",
            Self::InvalidRequest => "invalid_request",
            Self::RunIdConflict => "run_id_conflict",
            Self::RunNotOwned => "run_not_owned",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|code| code.name() == name)
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown error code '{name}'")))
    }
}

/// A code this build does not know reads as unclassified, so a reply stays
/// readable across a code being added.
pub(crate) fn lenient_code<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ErrorCode>, D::Error> {
    let name = Option::<String>::deserialize(deserializer)?;
    Ok(name.as_deref().and_then(ErrorCode::from_name))
}

/// An internal-socket failure before it becomes a [`crate::Response`]: the
/// free-text message and, when an SDK must be able to branch on it, a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketError {
    pub code: Option<ErrorCode>,
    pub message: String,
}

impl SocketError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
        }
    }

    pub fn unclassified(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    /// The same code with context in front of the message.
    pub fn context(self, prefix: impl fmt::Display) -> Self {
        Self {
            code: self.code,
            message: format!("{prefix}: {}", self.message),
        }
    }
}

impl From<String> for SocketError {
    fn from(message: String) -> Self {
        Self::unclassified(message)
    }
}

impl From<&str> for SocketError {
    fn from(message: &str) -> Self {
        Self::unclassified(message)
    }
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SocketError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_codes_round_trip() {
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(code.as_str()), Some(*code));
            assert_eq!(ErrorCode::from_name(code.name()), Some(*code));
        }
    }

    #[test]
    fn generated_codes_and_names_are_unique() {
        let mut codes = HashSet::new();
        let mut names = HashSet::new();
        for code in ErrorCode::ALL {
            assert!(codes.insert(code.as_str()), "duplicate code: {code}");
            assert!(names.insert(code.name()), "duplicate name: {}", code.name());
        }
    }

    #[test]
    fn names_are_snake_case_errkit_entry_names() {
        for code in ErrorCode::ALL {
            let name = code.name();
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{name}"
            );
            assert_eq!(serde_json::to_value(code).unwrap(), name);
        }
    }

    #[test]
    fn names_match_the_errkit_socket_scope() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../../errkit.json")).unwrap();
        let scope = config["scopes"]["socket"].as_object().unwrap();
        let mut registered: Vec<_> = scope
            .iter()
            .map(|(name, entry)| (name.as_str(), entry["code"].as_str().unwrap()))
            .collect();
        registered.sort();
        let generated: Vec<_> = ErrorCode::ALL
            .iter()
            .map(|code| (code.name(), code.as_str()))
            .collect();
        assert_eq!(generated, registered);
    }
}
