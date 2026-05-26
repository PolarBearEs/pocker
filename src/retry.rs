use crate::error::DockerPullError;
use std::time::Duration;

// Keep the first retry responsive for brief network blips, then cap backoff so
// long-running pulls on poor links continue to make periodic progress.
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

pub(crate) fn record_retry_attempt(
    retries: &mut u32,
    retry_limit: Option<u32>,
    operation: impl Into<String>,
    detail: impl Into<String>,
) -> Result<String, DockerPullError> {
    let detail = detail.into();
    if retry_limit_exhausted(*retries, retry_limit) {
        return Err(retry_limit_exceeded(operation, *retries, detail));
    }

    *retries += 1;
    Ok(retry_budget(*retries, retry_limit))
}

pub(crate) fn jittered_backoff_delay(attempt: u32) -> Duration {
    let max = exponential_delay(attempt);
    let min_ms = MIN_RETRY_DELAY.as_millis() as u64;
    let max_ms = max.as_millis() as u64;
    debug_assert!(
        max_ms >= min_ms,
        "exponential_delay must never return less than MIN_RETRY_DELAY"
    );
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
    use super::{MAX_RETRY_DELAY, MIN_RETRY_DELAY, jittered_backoff_delay, record_retry_attempt};
    use crate::error::DockerPullError;

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

    #[test]
    fn record_retry_attempt_formats_budget_and_enforces_limit() {
        let mut retries = 0;
        let budget = record_retry_attempt(&mut retries, Some(1), "operation", "detail")
            .expect("first retry should be allowed");
        assert_eq!(budget, "1/1");
        assert_eq!(retries, 1);

        let error = record_retry_attempt(&mut retries, Some(1), "operation", "detail")
            .expect_err("second retry should be rejected");
        assert!(matches!(
            error,
            DockerPullError::RetryLimitExceeded {
                operation,
                retries: 1,
                detail,
            } if operation == "operation" && detail == "detail"
        ));
    }
}
