//! Stream idle watchdog (S1, stability.md): a live SSE connection that stops
//! producing bytes is indistinguishable from a hung tunnel — the consumer
//! waits forever and the turn never returns. Every transport wraps its chunk
//! reads here so the silence budget is enforced at the one place all streams
//! share.
//!
//! Budget resolution: a client can pin it via its builder (tests do this to
//! avoid `env::set_var` races); otherwise `HEARTFLOW_STREAM_IDLE_TIMEOUT_MS`
//! decides, falling back to [`DEFAULT_STREAM_IDLE_TIMEOUT`]. `0` disables the
//! watchdog entirely (opt back into the old wait-forever behavior).

use std::env;
use std::time::Duration;

use tokio::time::timeout;

use crate::error::ApiError;

/// Environment variable selecting the stream idle budget, in milliseconds.
pub const STREAM_IDLE_ENV: &str = "HEARTFLOW_STREAM_IDLE_TIMEOUT_MS";

/// Default silence budget for a live SSE stream. Long enough that a model
/// thinking before its first token does not trip it (first-byte latency is
/// bounded by the request timeout, not this watchdog — this governs only the
/// gap *between* chunks), short enough that a dead tunnel is noticed within
/// a coffee break, not a workday.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Resolve the idle budget from the environment. `None` disables the watchdog.
#[must_use]
pub fn stream_idle_timeout_from_env() -> Option<Duration> {
    env::var(STREAM_IDLE_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|ms| {
            if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms))
            }
        })
        .unwrap_or(Some(DEFAULT_STREAM_IDLE_TIMEOUT))
}

/// Read the next body chunk under the idle budget. `None` budget = wait
/// forever (the pre-S1 behavior, kept reachable for users who need it).
///
/// # Errors
/// Propagates transport errors from the underlying read; a budget expiry
/// surfaces as [`ApiError::StreamIdle`] — the stream is consumed up to the
/// last delivered byte by the caller (blocks land in the transcript there).
pub async fn read_chunk_with_idle(
    response: &mut reqwest::Response,
    idle: Option<Duration>,
) -> Result<Option<bytes::Bytes>, ApiError> {
    match idle {
        None => Ok(response.chunk().await?),
        Some(budget) => match timeout(budget, response.chunk()).await {
            Ok(result) => Ok(result?),
            Err(_elapsed) => Err(ApiError::StreamIdle { idle: budget }),
        },
    }
}
