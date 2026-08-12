//! AWS SDK error wrappers.
//!
//! The AWS SDK's `SdkError<E>` does not implement `Display` or
//! `Error` uniformly across service crates, and its default rendering
//! is verbose and awkward. `AwsSdkError<E>` is a thin newtype over
//! `SdkError<E>` that formats as either `"code: message"` (when the
//! service error carries both), one of them, or the SDK's full
//! `DisplayErrorContext` as a fallback.
//!
//! Ported verbatim from `old-twoliter/tools/error-utils/src/lib.rs`.

#![allow(clippy::disallowed_types)]

use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use std::error::Error as StdError;
use std::fmt;

/// Shadow the AWS SDK error type.
pub type SdkError<E, R = ::aws_smithy_runtime_api::client::orchestrator::HttpResponse> =
    ::aws_smithy_runtime_api::client::result::SdkError<E, R>;

/// Wrapper around AWS SDK errors that ensures consistent formatting.
pub struct AwsSdkError<E, R = ::aws_smithy_runtime_api::client::orchestrator::HttpResponse>(
    pub SdkError<E, R>,
)
where
    E: ProvideErrorMetadata + StdError + 'static;

impl<E> fmt::Display for AwsSdkError<E>
where
    E: ProvideErrorMetadata + StdError + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", format_aws_sdk_error(&self.0))
    }
}

impl<E> fmt::Debug for AwsSdkError<E>
where
    E: ProvideErrorMetadata + StdError + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<E> StdError for AwsSdkError<E>
where
    E: ProvideErrorMetadata + StdError + 'static,
{
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.0)
    }
}

impl<E> From<SdkError<E>> for AwsSdkError<E>
where
    E: ProvideErrorMetadata + StdError + 'static,
{
    fn from(error: SdkError<E>) -> Self {
        AwsSdkError(error)
    }
}

impl<E> AwsSdkError<E>
where
    E: ProvideErrorMetadata + StdError + 'static,
{
    /// Get the service error from the SDK error, if available.
    pub fn as_service_error(&self) -> Option<&E> {
        self.0.as_service_error()
    }
}

/// Helper function to format AWS SDK errors as "code: message" or
/// fall back to full context.
///
/// # Examples
///
/// - With code and message: `"AccessDenied: You do not have permission"`
/// - With code only: `"AccessDenied"`
/// - With message only: `"Request timeout occurred"`
/// - With neither: Falls back to `DisplayErrorContext` for full error details.
pub fn format_aws_sdk_error<E>(sdk_error: &SdkError<E>) -> String
where
    E: ProvideErrorMetadata + StdError + 'static,
{
    match (sdk_error.code(), sdk_error.message()) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_string(),
        (None, Some(message)) => message.to_string(),
        (None, None) => format!("{}", DisplayErrorContext(sdk_error)),
    }
}
