use crate::error::DockerPullError;

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
