use crate::JsonSchema;
use crate::TS;
use codex_protocol::protocol::CodexErrorInfo as CoreCodexErrorInfo;
use codex_protocol::protocol::NonSteerableTurnKind as CoreNonSteerableTurnKind;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

// Macro to declare a camelCased API v2 enum mirroring a core enum which
// tends to use either snake_case or kebab-case.
macro_rules! v2_enum_from_core {
    (
        $(#[$enum_meta:meta])*
        pub enum $Name:ident from $Src:path {
            $( $(#[$variant_meta:meta])* $Variant:ident ),+ $(,)?
        }
    ) => {
        #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
        $(#[$enum_meta])*
        #[serde(rename_all = "camelCase")]
        #[ts(export_to = "v2/")]
        pub enum $Name {
            $( $(#[$variant_meta])* $Variant ),+
        }

        impl $Name {
            pub fn to_core(self) -> $Src {
                match self { $( $Name::$Variant => <$Src>::$Variant ),+ }
            }
        }

        impl From<$Src> for $Name {
            fn from(value: $Src) -> Self {
                match value { $( <$Src>::$Variant => $Name::$Variant ),+ }
            }
        }
    };
}

pub(super) use v2_enum_from_core;

pub(super) const fn default_enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum NonSteerableTurnKind {
    Review,
    Compact,
}

/// This translation layer make sure that we expose codex error code in camel case.
///
/// When an upstream HTTP status is available (for example, from the Responses API or a provider),
/// it is forwarded in `httpStatusCode` on the relevant `codexErrorInfo` variant.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CodexErrorInfo {
    ContextWindowExceeded,
    SessionBudgetExceeded,
    UsageLimitExceeded,
    RateLimitExceeded,
    FlexUnavailable,
    ServerOverloaded,
    CyberPolicy,
    MisalignmentPolicyViolation,
    HttpConnectionFailed {
        #[serde(rename = "httpStatusCode")]
        #[ts(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    /// Failed to connect to the response SSE stream.
    ResponseStreamConnectionFailed {
        #[serde(rename = "httpStatusCode")]
        #[ts(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    InternalServerError,
    Unauthorized,
    BadRequest,
    ThreadRollbackFailed,
    /// The response SSE stream disconnected in the middle of a turn before completion.
    ResponseStreamDisconnected {
        #[serde(rename = "httpStatusCode")]
        #[ts(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    /// Reached the retry limit for responses.
    ResponseTooManyFailedAttempts {
        #[serde(rename = "httpStatusCode")]
        #[ts(rename = "httpStatusCode")]
        http_status_code: Option<u16>,
    },
    /// Returned when `turn/start` or `turn/steer` is submitted while the current active turn
    /// cannot accept same-turn steering, for example `/review` or manual `/compact`.
    ActiveTurnNotSteerable {
        #[serde(rename = "turnKind")]
        #[ts(rename = "turnKind")]
        turn_kind: NonSteerableTurnKind,
    },
    #[serde(
        untagged,
        serialize_with = "serialize_other_codex_error_info",
        deserialize_with = "deserialize_other_codex_error_info"
    )]
    #[ts(type = "\"other\" | string | { [key: string]: unknown }")]
    Other,
}

fn serialize_other_codex_error_info<S>(serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str("other")
}

fn deserialize_other_codex_error_info<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: serde::Deserializer<'de>,
{
    match JsonValue::deserialize(deserializer)? {
        JsonValue::String(_) | JsonValue::Object(_) => Ok(()),
        _ => Err(serde::de::Error::custom(
            "expected an error string or object",
        )),
    }
}

impl From<CoreCodexErrorInfo> for CodexErrorInfo {
    fn from(value: CoreCodexErrorInfo) -> Self {
        match value {
            CoreCodexErrorInfo::ContextWindowExceeded => CodexErrorInfo::ContextWindowExceeded,
            CoreCodexErrorInfo::SessionBudgetExceeded => CodexErrorInfo::SessionBudgetExceeded,
            CoreCodexErrorInfo::UsageLimitExceeded => CodexErrorInfo::UsageLimitExceeded,
            CoreCodexErrorInfo::RateLimitExceeded => CodexErrorInfo::RateLimitExceeded,
            CoreCodexErrorInfo::FlexUnavailable => CodexErrorInfo::FlexUnavailable,
            CoreCodexErrorInfo::ServerOverloaded => CodexErrorInfo::ServerOverloaded,
            CoreCodexErrorInfo::CyberPolicy => CodexErrorInfo::CyberPolicy,
            CoreCodexErrorInfo::BioPolicy => CodexErrorInfo::Other,
            CoreCodexErrorInfo::MisalignmentPolicyViolation => {
                CodexErrorInfo::MisalignmentPolicyViolation
            }
            CoreCodexErrorInfo::HttpConnectionFailed { http_status_code } => {
                CodexErrorInfo::HttpConnectionFailed { http_status_code }
            }
            CoreCodexErrorInfo::ResponseStreamConnectionFailed { http_status_code } => {
                CodexErrorInfo::ResponseStreamConnectionFailed { http_status_code }
            }
            CoreCodexErrorInfo::InternalServerError => CodexErrorInfo::InternalServerError,
            CoreCodexErrorInfo::Unauthorized => CodexErrorInfo::Unauthorized,
            CoreCodexErrorInfo::BadRequest => CodexErrorInfo::BadRequest,
            CoreCodexErrorInfo::InvalidPrompt => CodexErrorInfo::Other,
            CoreCodexErrorInfo::ThreadRollbackFailed => CodexErrorInfo::ThreadRollbackFailed,
            CoreCodexErrorInfo::ResponseStreamDisconnected { http_status_code } => {
                CodexErrorInfo::ResponseStreamDisconnected { http_status_code }
            }
            CoreCodexErrorInfo::ResponseTooManyFailedAttempts { http_status_code } => {
                CodexErrorInfo::ResponseTooManyFailedAttempts { http_status_code }
            }
            CoreCodexErrorInfo::ActiveTurnNotSteerable { turn_kind } => {
                CodexErrorInfo::ActiveTurnNotSteerable {
                    turn_kind: turn_kind.into(),
                }
            }
            CoreCodexErrorInfo::Other => CodexErrorInfo::Other,
        }
    }
}

impl From<CoreNonSteerableTurnKind> for NonSteerableTurnKind {
    fn from(value: CoreNonSteerableTurnKind) -> Self {
        match value {
            CoreNonSteerableTurnKind::Review => Self::Review,
            CoreNonSteerableTurnKind::Compact => Self::Compact,
        }
    }
}
