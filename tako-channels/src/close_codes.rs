pub use crate::error_codes::Err as ChannelCloseCode;

impl ChannelCloseCode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::AuthFrameMalformed => "auth-frame-malformed",
            Self::AuthFrameMissing => "auth-frame-missing",
            Self::ChannelUnknown => "channel-unknown",
            Self::ParamsInvalid => "params-invalid",
            Self::ReplayTooOld => "replay-too-old",
            Self::VerifyRejected => "verify-rejected",
        }
    }

    /// WebSocket close code in the 4xxx app-defined range.
    pub const fn ws_close_code(self) -> u16 {
        match self {
            Self::AuthFrameMalformed => 4400,
            Self::AuthFrameMissing => 4401,
            Self::VerifyRejected => 4403,
            Self::ChannelUnknown => 4404,
            Self::ReplayTooOld => 4410,
            Self::ParamsInvalid => 4422,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_codes_round_trip() {
        for code in ChannelCloseCode::ALL {
            assert_eq!(ChannelCloseCode::from_code(code.as_str()), Some(*code));
        }
    }

    #[test]
    fn generated_codes_are_unique() {
        let mut seen = HashSet::new();
        for code in ChannelCloseCode::ALL {
            assert!(seen.insert(code.as_str()), "duplicate code: {code}");
        }
    }

    #[test]
    fn close_code_names_are_kebab_case() {
        assert_eq!(
            ChannelCloseCode::AuthFrameMissing.name(),
            "auth-frame-missing"
        );
        assert_eq!(ChannelCloseCode::ParamsInvalid.name(), "params-invalid");
    }

    #[test]
    fn ws_close_codes_are_in_app_range() {
        for code in ChannelCloseCode::ALL {
            assert!(
                (4000..5000).contains(&code.ws_close_code()),
                "{code:?} mapped outside app-defined range"
            );
        }
    }
}
