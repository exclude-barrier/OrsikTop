//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
use super::*;

/// Join a scoped fetch handle, converting a thread panic into the caller's
/// error outcome instead of propagating it. A panic in one endpoint's fetch
/// must not kill the whole LLM worker and stop all telemetry.
pub(super) fn join_or<T>(handle: std::thread::ScopedJoinHandle<'_, T>, on_panic: T) -> T {
    handle.join().unwrap_or(on_panic)
}

/// Build an API endpoint URL from the configured base, appending `segment`
/// (`metrics`/`props`/`slots`) to the base *path* and preserving any query
/// (e.g. a token). A plain `format!("{base}/{segment}")` would fold the segment
/// into a base query (`?token=x/metrics`) or a path segment (`/v1/metrics`)
/// instead, so `/metrics` was never actually requested.
pub(super) fn endpoint_url(base: &str, segment: &str) -> String {
    match reqwest::Url::parse(base) {
        Ok(mut url) => {
            let path = url.path().trim_end_matches('/');
            url.set_path(&format!("{path}/{segment}"));
            url.to_string()
        }
        Err(_) => format!("{}/{segment}", base.trim_end_matches('/')),
    }
}

/// Fetches a raw text endpoint (currently only /metrics).
pub(super) fn fetch_raw(client: &Client, url: String) -> MetricsOutcome {
    let response = match client.get(url).send() {
        Ok(response) => response,
        // Structured redaction: strip the request URL (which may carry
        // credentials/query secrets) before it becomes a telemetry string.
        Err(err) => return MetricsOutcome::Unreachable(crate::redact::describe_reqwest_error(err)),
    };
    let status = response.status();
    if !status.is_success() {
        return MetricsOutcome::Http(status);
    }
    match read_body_limited(response, MAX_RESPONSE_BYTES) {
        Ok(body) => MetricsOutcome::Ok(body),
        Err(err) => MetricsOutcome::Body(err),
    }
}

/// Read a response body up to `limit` bytes, refusing an oversized one instead
/// of allocating without bound. The error carries no request URL, so it is safe
/// to surface.
pub(super) fn read_body_limited(
    response: reqwest::blocking::Response,
    limit: u64,
) -> Result<String, String> {
    use std::io::Read as _;
    let mut buffer = Vec::new();
    response
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .map_err(|err| err.to_string())?;
    if buffer.len() as u64 > limit {
        return Err(format!("response body exceeded {limit} bytes"));
    }
    String::from_utf8(buffer).map_err(|err| err.to_string())
}

/// Fetches a JSON endpoint (/props, /slots).
pub(super) fn fetch_json(client: &Client, url: String) -> JsonOutcome {
    let response = match client.get(url).send() {
        Ok(response) => response,
        Err(err) => return JsonOutcome::Unreachable(crate::redact::describe_reqwest_error(err)),
    };
    let status = response.status();
    if !status.is_success() {
        return JsonOutcome::Http(status);
    }
    match read_body_limited(response, MAX_RESPONSE_BYTES) {
        Ok(body) => match serde_json::from_str::<Value>(&body) {
            Ok(value) => JsonOutcome::Ok(value),
            Err(err) => JsonOutcome::Invalid(err.to_string()),
        },
        Err(err) => JsonOutcome::Invalid(err),
    }
}
