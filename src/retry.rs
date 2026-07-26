use std::time::Duration;

use reqwest::{header::RETRY_AFTER, StatusCode};

pub const MAX_ATTEMPTS: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    Retryable,
    Forbidden,
    Terminal,
    RequiresReinspect,
}

pub fn classify_status(status: StatusCode, ranged_worker: bool, ephemeral: bool) -> RetryClass {
    if ranged_worker && (status == StatusCode::OK || status == StatusCode::RANGE_NOT_SATISFIABLE) {
        return RetryClass::RequiresReinspect;
    }
    if status == StatusCode::FORBIDDEN && ephemeral {
        return RetryClass::Forbidden;
    }
    if status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        RetryClass::Retryable
    } else {
        RetryClass::Terminal
    }
}

/// The stable component is exponential, capped at 30 seconds.  The small
/// deterministic jitter avoids lock-step retries while keeping tests exact.
pub fn delay(attempt: u8, retry_after: Option<&str>) -> Duration {
    let exponent = attempt.saturating_sub(1).min(6);
    let base_ms = (500_u64.saturating_mul(1_u64 << exponent)).min(30_000);
    let jitter_ms = (attempt as u64 * 73) % 251;
    let calculated = Duration::from_millis(base_ms + jitter_ms);
    retry_after_duration(retry_after).map_or(calculated, |server| server.max(calculated))
}

pub fn retry_after_duration(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let time = httpdate::parse_http_date(value).ok()?;
    Some(
        time.duration_since(std::time::SystemTime::now())
            .unwrap_or_default(),
    )
}

pub fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_a_floor_and_attempt_budget_is_five() {
        assert_eq!(MAX_ATTEMPTS, 5);
        assert!(delay(1, Some("2")).as_secs() >= 2);
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, false, false),
            RetryClass::Retryable
        );
    }
}
