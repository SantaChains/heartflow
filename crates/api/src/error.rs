use std::env::VarError;
use std::fmt::{Display, Formatter};
use std::time::Duration;

#[derive(Debug)]
pub enum ApiError {
    MissingApiKey,
    InvalidApiKeyEnv(VarError),
    Http(reqwest::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    Api {
        status: reqwest::StatusCode,
        error_type: Option<String>,
        message: Option<String>,
        body: String,
        retryable: bool,
        /// Server-requested wait (`Retry-After`) parsed from the response, when
        /// present. `None` means the retry policy should use its own backoff.
        retry_after: Option<Duration>,
    },
    RetriesExhausted {
        attempts: u32,
        last_error: Box<ApiError>,
    },
    InvalidSseFrame(&'static str),
    BackoffOverflow {
        attempt: u32,
        base_delay: Duration,
    },
}

impl ApiError {
    /// Short machine-readable category of the failure, shown alongside the
    /// raw error in `RetriesExhausted` so users can tell at a glance whether
    /// the problem is DNS, TLS, connectivity, rate limiting, etc.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Http(error) => {
                if error.is_connect() {
                    "connection failed"
                } else if error.is_timeout() {
                    "timeout"
                } else if error.is_decode() {
                    "decode error"
                } else if error.is_body() {
                    "body error"
                } else if error.is_request() {
                    "request error"
                } else if error.is_redirect() {
                    "redirect error"
                } else {
                    "http error"
                }
            }
            Self::Api { status, .. } => {
                let code = status.as_u16();
                match code {
                    400 => "bad request",
                    401 | 403 => "auth error",
                    404 => "not found",
                    408 => "request timeout",
                    409 => "conflict",
                    413 => "payload too large",
                    429 => "rate limited",
                    500 => "server error",
                    502 => "bad gateway",
                    503 => "service unavailable",
                    504 => "gateway timeout",
                    _ if (400..500).contains(&code) => "client error",
                    _ if (500..600).contains(&code) => "server error",
                    _ => "api error",
                }
            }
            Self::RetriesExhausted { last_error, .. } => last_error.kind(),
            Self::MissingApiKey => "missing api key",
            Self::InvalidApiKeyEnv(_) => "invalid api key env",
            Self::Io(_) => "io error",
            Self::Json(_) => "json decode error",
            Self::InvalidSseFrame(_) => "sse frame error",
            Self::BackoffOverflow { .. } => "backoff overflow",
        }
    }

    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(error) => error.is_connect() || error.is_timeout() || error.is_request(),
            Self::Api { retryable, .. } => *retryable,
            Self::RetriesExhausted { last_error, .. } => last_error.is_retryable(),
            Self::MissingApiKey
            | Self::InvalidApiKeyEnv(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::InvalidSseFrame(_)
            | Self::BackoffOverflow { .. } => false,
        }
    }

    /// The delay the server asked us to wait before retrying (`Retry-After`),
    /// if the failing response carried one. Rate limiters and gateways emit
    /// this, and honouring it beats guessing with a local exponential backoff:
    /// retrying earlier burns quota and earns another 429, waiting blindly
    /// wastes a turn. `None` falls back to the policy's own schedule.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            Self::RetriesExhausted { last_error, .. } => last_error.retry_after(),
            _ => None,
        }
    }

    /// A short, human-readable hint for what to try next. Returns `None` when
    /// the error is self-explanatory or no concrete advice applies. Used by
    /// the CLI to surface next-steps alongside the raw error (error
    /// humanization), so users don't have to guess from a raw status code.
    #[must_use]
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::RetriesExhausted { last_error, .. } => last_error.suggestion(),
            Self::Http(error) => {
                if error.is_connect() {
                    Some("check your network connection and base_url, or try a different provider")
                } else if error.is_timeout() {
                    Some("the request timed out — check network latency or try again")
                } else if error.is_decode() {
                    Some("response decode error — the server may have returned an unexpected format")
                } else {
                    None
                }
            }
            Self::Api { status, .. } => {
                let code = status.as_u16();
                match code {
                    401 | 403 => Some("check your API key and ensure it has the required permissions"),
                    404 => Some("model or endpoint not found — check the model name and base_url"),
                    429 => Some("rate limited — wait a moment and try again, or reduce request frequency"),
                    500 | 502 | 503 => Some("provider-side error — this is usually temporary, try again shortly"),
                    504 => Some("gateway timeout — the provider is slow, try again in a moment"),
                    413 => Some("payload too large — try a shorter prompt or fewer attached files"),
                    _ => None,
                }
            }
            Self::MissingApiKey => Some(
                "set ANTHROPIC_AUTH_TOKEN or DEEPSEEK_API_KEY (or the relevant env var for your provider)",
            ),
            Self::InvalidApiKeyEnv(_) => {
                Some("check that the API key environment variable is set correctly")
            }
            Self::Json(_) => Some("unexpected response format — the provider API may have changed"),
            Self::Io(_) | Self::InvalidSseFrame(_) | Self::BackoffOverflow { .. } => None,
        }
    }
}

impl Display for ApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey => {
                write!(
                    f,
                    "ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY is not set; export one before calling the Anthropic API"
                )
            }
            Self::InvalidApiKeyEnv(error) => {
                write!(
                    f,
                    "failed to read ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY: {error}"
                )
            }
            Self::Http(error) => write!(f, "http error: {error}"),
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Json(error) => write!(f, "json error: {error}"),
            Self::Api {
                status,
                error_type,
                message,
                body,
                ..
            } => match (error_type, message) {
                // Protocol-neutral wording: these arms fire for every transport
                // (Anthropic, OpenAI, DeepSeek, ...), so naming one vendor here
                // mislabels the others and misleads debugging.
                (Some(error_type), Some(message)) => {
                    write!(f, "api returned {status} ({error_type}): {message}")
                }
                _ => write!(f, "api returned {status}: {body}"),
            },
            Self::RetriesExhausted {
                attempts,
                last_error,
            } => write!(
                f,
                "api request failed after {attempts} attempts ({}): {last_error}",
                last_error.kind()
            ),
            Self::InvalidSseFrame(message) => write!(f, "invalid sse frame: {message}"),
            Self::BackoffOverflow {
                attempt,
                base_delay,
            } => write!(
                f,
                "retry backoff overflowed on attempt {attempt} with base delay {base_delay:?}"
            ),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<reqwest::Error> for ApiError {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(value)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<VarError> for ApiError {
    fn from(value: VarError) -> Self {
        Self::InvalidApiKeyEnv(value)
    }
}
