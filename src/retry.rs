use crate::error::DockerPullError;
use std::time::Duration;

const MIN_RETRY_DELAY: Duration = Duration::from_millis(100);
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

pub(crate) fn retry_limit_exhausted(retries: u32, retry_limit: Option<u32>) -> bool {
    retry_limit.is_some_and(|limit| retries >= limit)
}

pub(crate) fn retry_budget(next_retry: u32, retry_limit: Option<u32>) -> String {
    match retry_limit {
        Some(limit) => format!("{next_retry}/{limit}"),
        None => format!("{next_retry}/unlimited"),
    }
}

pub(crate) fn retry_limit_exceeded(
    operation: impl Into<String>,
    retries: u32,
    detail: impl Into<String>,
) -> DockerPullError {
    DockerPullError::RetryLimitExceeded {
        operation: operation.into(),
        retries,
        detail: detail.into(),
    }
}

pub(crate) fn jittered_backoff_delay(attempt: u32) -> Duration {
    let max = exponential_delay(attempt);
    let min_ms = MIN_RETRY_DELAY.as_millis() as u64;
    let max_ms = max.as_millis() as u64;
    let jitter_ms = fastrand::u64(min_ms..=max_ms);
    Duration::from_millis(jitter_ms)
}

fn exponential_delay(attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(6)).unwrap_or(u32::MAX);
    INITIAL_RETRY_DELAY
        .saturating_mul(multiplier)
        .min(MAX_RETRY_DELAY)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RETRY_DELAY, MIN_RETRY_DELAY, jittered_backoff_delay};

    #[test]
    fn jittered_backoff_stays_within_bounds() {
        for attempt in 0..16 {
            for _ in 0..128 {
                let delay = jittered_backoff_delay(attempt);
                assert!(delay >= MIN_RETRY_DELAY);
                assert!(delay <= MAX_RETRY_DELAY);
            }
        }
    }
}
