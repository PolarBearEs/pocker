use crate::error::DockerPullError;
use std::future::Future;
use std::time::{Duration, Instant};

// Keep retry timing predictable for interactive pulls on unreliable links,
// while adding a small spread so concurrent layers do not retry in lockstep.
const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const RETRY_JITTER: Duration = Duration::from_millis(250);
pub(crate) const RETRY_COUNTDOWN_INTERVAL: Duration = Duration::from_secs(1);

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
    let base = exponential_delay(attempt);
    let jitter_ms = RETRY_JITTER.as_millis() as i128;
    let offset_ms = fastrand::u64(0..=(jitter_ms as u64 * 2)) as i128 - jitter_ms;
    let delay_ms = (base.as_millis() as i128 + offset_ms).clamp(
        MIN_RETRY_DELAY.as_millis() as i128,
        MAX_RETRY_DELAY.as_millis() as i128,
    );
    Duration::from_millis(delay_ms as u64)
}

fn exponential_delay(attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(6)).unwrap_or(u32::MAX);
    INITIAL_RETRY_DELAY
        .saturating_mul(multiplier)
        .min(MAX_RETRY_DELAY)
}

pub(crate) fn format_retry_delay(delay: Duration) -> String {
    let seconds = delay.as_secs();
    format!("{seconds}s")
}

pub(crate) async fn countdown_sleep<SleepFn, SleepFuture, OnTick>(
    delay: Duration,
    mut sleep_for: SleepFn,
    mut on_tick: OnTick,
) -> Result<(), DockerPullError>
where
    SleepFn: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = Result<(), DockerPullError>>,
    OnTick: FnMut(Duration),
{
    let started = Instant::now();
    let mut last_status = None;
    loop {
        let elapsed = started.elapsed();
        if elapsed >= delay {
            return Ok(());
        }
        let remaining = delay.saturating_sub(elapsed);
        sleep_for(remaining.min(RETRY_COUNTDOWN_INTERVAL)).await?;

        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        let status = format_retry_delay(remaining);
        if last_status.as_deref() != Some(status.as_str()) {
            on_tick(remaining);
            last_status = Some(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        MAX_RETRY_DELAY, MIN_RETRY_DELAY, RETRY_JITTER, countdown_sleep, format_retry_delay,
        jittered_backoff_delay, record_retry_attempt,
    };
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
    fn jittered_backoff_uses_small_additive_jitter() {
        let cases = [
            (0, Duration::from_secs(1), Duration::from_millis(1_250)),
            (
                1,
                Duration::from_millis(1_750),
                Duration::from_millis(2_250),
            ),
            (
                2,
                Duration::from_millis(3_750),
                Duration::from_millis(4_250),
            ),
            (5, MAX_RETRY_DELAY - RETRY_JITTER, MAX_RETRY_DELAY),
            (6, MAX_RETRY_DELAY - RETRY_JITTER, MAX_RETRY_DELAY),
        ];

        for (attempt, min, max) in cases {
            for _ in 0..128 {
                let delay = jittered_backoff_delay(attempt);
                assert!(
                    (min..=max).contains(&delay),
                    "attempt {attempt} delay {delay:?} outside {min:?}..={max:?}"
                );
            }
        }
    }

    #[test]
    fn retry_delay_format_uses_whole_seconds() {
        assert_eq!(format_retry_delay(Duration::ZERO), "0s");
        assert_eq!(format_retry_delay(Duration::from_millis(750)), "0s");
        assert_eq!(format_retry_delay(Duration::from_secs(3)), "3s");
        assert_eq!(format_retry_delay(Duration::from_millis(3200)), "3s");
    }

    #[tokio::test]
    async fn countdown_sleep_does_not_emit_zero_remaining_tick() {
        let mut ticks = Vec::new();

        countdown_sleep(
            Duration::from_millis(1),
            |delay| async move {
                tokio::time::sleep(delay).await;
                Ok(())
            },
            |remaining| ticks.push(format_retry_delay(remaining)),
        )
        .await
        .expect("countdown sleep should complete");

        assert!(ticks.is_empty());
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
