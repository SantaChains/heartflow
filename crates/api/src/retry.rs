use std::future::Future;
use std::time::Duration;

use crate::error::ApiError;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-read timeout: bounds silent stalls without capping total stream time.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(300);

const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(2);
const DEFAULT_MAX_RETRIES: u32 = 2;

/// Shared HTTP client: bounded connect/read timeouts so a dead endpoint or a
/// stalled stream can never hang the agent loop forever.
#[must_use]
pub(crate) fn build_http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        // panic-ok: only static timeouts are configured, so build() cannot fail
        .expect("static reqwest configuration must build")
}

/// Status codes worth retrying: transient rate limits, timeouts, and
/// server-side failures. Everything else (auth, bad request) is terminal.
#[must_use]
pub(crate) const fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504)
}

/// Longest server-named wait we will actually sit through. Beyond this the
/// correct move is to stop retrying (the caller surfaces the error) rather than
/// block a turn for minutes or ignore the server and retry early.
pub(crate) const MAX_HONOURED_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Parse a `Retry-After` header into a delay.
///
/// Only the delta-seconds form (`Retry-After: 30`) is honoured — that is what
/// LLM gateways and rate limiters emit, and the HTTP-date form would drag in a
/// date parser for a case that does not occur here. An absent, non-UTF-8, or
/// malformed value yields `None`, so the caller falls back to its own backoff
/// instead of mistaking a parse failure for "retry immediately".
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Capped exponential backoff shared by every provider transport, so retry
/// semantics stay identical regardless of the wire protocol.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    max_retries: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

impl RetryPolicy {
    #[must_use]
    pub fn new(max_retries: u32, initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_retries,
            initial_backoff,
            max_backoff,
        }
    }

    fn backoff_for_attempt(&self, attempt: u32) -> Result<Duration, ApiError> {
        let Some(multiplier) = 1_u32.checked_shl(attempt.saturating_sub(1)) else {
            return Err(ApiError::BackoffOverflow {
                attempt,
                base_delay: self.initial_backoff,
            });
        };
        Ok(self
            .initial_backoff
            .checked_mul(multiplier)
            .map_or(self.max_backoff, |delay| delay.min(self.max_backoff)))
    }

    /// Drive `send` under the policy, classifying each response with `check`.
    /// `send` issues the raw request; `check` turns a response into either a
    /// success or a (possibly retryable) error. Retries only on retryable
    /// errors, sleeping `backoff_for_attempt` between tries.
    pub(crate) async fn run<F, Fut, C, Cfut>(
        &self,
        mut send: F,
        mut check: C,
    ) -> Result<reqwest::Response, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<reqwest::Response, ApiError>>,
        C: FnMut(reqwest::Response) -> Cfut,
        Cfut: Future<Output = Result<reqwest::Response, ApiError>>,
    {
        let mut attempts = 0_u32;
        let mut last_error: Option<ApiError>;

        loop {
            attempts += 1;
            match send().await {
                Ok(response) => match check(response).await {
                    Ok(response) => return Ok(response),
                    Err(error) if error.is_retryable() && attempts <= self.max_retries + 1 => {
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                },
                Err(error) if error.is_retryable() && attempts <= self.max_retries + 1 => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }

            if attempts > self.max_retries {
                break;
            }

            // A server-named delay wins over the local schedule: retrying
            // before it asked earns another rejection, and waiting blindly
            // wastes the turn. If it named a wait longer than we are willing to
            // sit out, stop and surface the error instead of blocking for
            // minutes.
            let delay = match last_error.as_ref().and_then(ApiError::retry_after) {
                Some(server) if server > MAX_HONOURED_RETRY_AFTER => break,
                Some(server) => server,
                None => self.backoff_for_attempt(attempts)?,
            };
            tokio::time::sleep(delay).await;
        }

        Err(ApiError::RetriesExhausted {
            attempts,
            // panic-ok: every path that leaves the loop above records an error first
            last_error: Box::new(last_error.expect("retry loop must capture an error")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::RetryPolicy;
    use std::time::Duration;

    #[test]
    fn backoff_doubles_until_maximum() {
        let policy = RetryPolicy::new(3, Duration::from_millis(10), Duration::from_millis(25));
        assert_eq!(
            policy.backoff_for_attempt(1).expect("attempt 1"),
            Duration::from_millis(10)
        );
        assert_eq!(
            policy.backoff_for_attempt(2).expect("attempt 2"),
            Duration::from_millis(20)
        );
        assert_eq!(
            policy.backoff_for_attempt(3).expect("attempt 3"),
            Duration::from_millis(25)
        );
    }

    #[test]
    fn retry_after_delta_seconds_is_parsed() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("30"),
        );
        assert_eq!(
            super::parse_retry_after(&headers),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn absent_or_malformed_retry_after_falls_back_to_local_backoff() {
        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(super::parse_retry_after(&empty), None);

        let mut http_date = reqwest::header::HeaderMap::new();
        http_date.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        // The HTTP-date form is deliberately unsupported. A parse failure must
        // never read as "retry immediately", so it degrades to `None` and the
        // caller keeps its own exponential backoff.
        assert_eq!(super::parse_retry_after(&http_date), None);
    }

    #[test]
    fn retry_after_is_surfaced_through_the_error_chain() {
        let error = super::ApiError::Api {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            error_type: None,
            message: None,
            body: String::new(),
            retryable: true,
            retry_after: Some(Duration::from_secs(5)),
        };
        assert!(error.is_retryable());
        assert_eq!(error.retry_after(), Some(Duration::from_secs(5)));

        // `RetriesExhausted` wraps the last failure, so the delay must survive
        // the wrap — that is what the caller reports after giving up.
        let exhausted = super::ApiError::RetriesExhausted {
            attempts: 3,
            last_error: Box::new(super::ApiError::Api {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                error_type: None,
                message: None,
                body: String::new(),
                retryable: true,
                retry_after: Some(Duration::from_secs(2)),
            }),
        };
        assert_eq!(exhausted.retry_after(), Some(Duration::from_secs(2)));

        // Errors that never carry a server hint stay `None`.
        assert_eq!(super::ApiError::MissingApiKey.retry_after(), None);
    }

    #[test]
    fn retryable_statuses_are_detected() {
        assert!(super::is_retryable_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(super::is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!super::is_retryable_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
    }
}
