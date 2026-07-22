use std::time::Duration;

use reqwest::{RequestBuilder, Response, StatusCode};
use tracing::warn;

use crate::error::AppError;

const MAX_ATTEMPTS: usize = 4;

pub async fn send_with_retry(
    request: RequestBuilder,
    service: &str,
    endpoint: &str,
) -> Result<Response, AppError> {
    for attempt in 1..=MAX_ATTEMPTS {
        let attempt_request = request.try_clone().ok_or_else(|| {
            AppError::Remote(format!(
                "cannot clone retryable {service} request: {endpoint}"
            ))
        })?;
        match attempt_request.send().await {
            Ok(response) if retryable_status(response.status()) && attempt < MAX_ATTEMPTS => {
                let delay = retry_delay(&response, attempt);
                warn!(
                    service,
                    endpoint,
                    status = %response.status(),
                    attempt,
                    delay_ms = delay.as_millis(),
                    "transient HTTP response; retrying"
                );
                tokio::time::sleep(delay).await;
            }
            Ok(response) => return Ok(response),
            Err(error) if retryable_error(&error) && attempt < MAX_ATTEMPTS => {
                let delay = exponential_delay(attempt);
                warn!(
                    service,
                    endpoint,
                    attempt,
                    delay_ms = delay.as_millis(),
                    error = %error,
                    "transient HTTP error; retrying"
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                return Err(AppError::Remote(format!(
                    "{service} GET {endpoint}: {error}"
                )));
            }
        }
    }
    unreachable!("retry loop always returns")
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retryable_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_body()
}

fn retry_delay(response: &Response, attempt: usize) -> Duration {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(60)))
        .unwrap_or_else(|| exponential_delay(attempt))
}

fn exponential_delay(attempt: usize) -> Duration {
    Duration::from_millis(250 * 2_u64.pow((attempt.saturating_sub(1)) as u32))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    struct TransientResponder(Arc<AtomicUsize>);

    impl Respond for TransientResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_string("ok")
            }
        }
    }

    #[tokio::test]
    async fn retries_transient_statuses() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/retry"))
            .respond_with(TransientResponder(calls.clone()))
            .mount(&server)
            .await;
        let endpoint = format!("{}/retry", server.uri());
        let response = send_with_retry(reqwest::Client::new().get(&endpoint), "test", &endpoint)
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
