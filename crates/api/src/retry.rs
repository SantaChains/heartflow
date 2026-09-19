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
        .expect("static reqwest configuration must build")
}

/// Status codes worth retrying: transient rate limits, timeouts, and
/// server-side failures. Everything else (auth, bad request) is terminal.
#[must_use]
pub(crate) const fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504)
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

            tokio::time::sleep(self.backoff_for_attempt(attempts)?).await;
        }

        Err(ApiError::RetriesExhausted {
            attempts,
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
