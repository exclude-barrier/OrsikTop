//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
use reqwest::blocking::Client;
use serde_json::Value;

use super::{JsonOutcome, MetricsOutcome, MAX_RESPONSE_BYTES};

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

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    /// Serve one canned HTTP response on a loopback port (no real inference
    /// server) and return the port. The accept happens on a helper thread so
    /// the caller can issue the request against a live listener.
    fn serve_once(response: String) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let port = listener.local_addr().expect("listener local addr").port();
        thread::spawn(move || {
            if let Ok((mut stream, _addr)) = listener.accept() {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request); // drain the request headers
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        port
    }

    /// A 200 response with the given body.
    fn serve_200(body: &str) -> u16 {
        serve_once(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    /// A port that was bound once and released: connecting to it is refused,
    /// which is the deterministic stand-in for an unreachable endpoint.
    fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        listener.local_addr().expect("listener local addr").port()
        // listener dropped here -> the port is closed again
    }

    /// `MetricsOutcome` has no `Debug` impl (production enum, untouched by
    /// these tests), so mismatch diagnostics name the variant by hand.
    fn outcome_variant(outcome: &MetricsOutcome) -> &'static str {
        match outcome {
            MetricsOutcome::Ok(_) => "Ok",
            MetricsOutcome::Http(_) => "Http",
            MetricsOutcome::Body(_) => "Body",
            MetricsOutcome::Unreachable(_) => "Unreachable",
        }
    }

    fn test_client() -> Client {
        Client::builder()
            .timeout(std::time::Duration::from_millis(2000))
            .connect_timeout(std::time::Duration::from_millis(750))
            .build()
            .expect("test client")
    }

    #[test]
    fn endpoint_url_appends_segment_to_path_and_keeps_query() {
        // Regression of the old `format!("{base}/{segment}")` bug: the segment
        // must land in the path, never folded into the query (`?token=x/metrics`).
        assert_eq!(
            endpoint_url("http://127.0.0.1:8080/?token=secret", "metrics"),
            "http://127.0.0.1:8080/metrics?token=secret"
        );
        // Path + query with a trailing slash on the base path.
        assert_eq!(
            endpoint_url("http://127.0.0.1:8080/v1/", "props"),
            "http://127.0.0.1:8080/v1/props"
        );
        // Bare base (empty path) and a non-empty path without trailing slash.
        assert_eq!(
            endpoint_url("http://127.0.0.1:8080", "slots"),
            "http://127.0.0.1:8080/slots"
        );
        assert_eq!(
            endpoint_url("http://127.0.0.1:8080/admin/", "metrics"),
            "http://127.0.0.1:8080/admin/metrics"
        );
    }

    #[test]
    fn endpoint_url_falls_back_for_invalid_base() {
        assert_eq!(endpoint_url("not a url", "metrics"), "not a url/metrics");
        assert_eq!(endpoint_url("not a url/", "metrics"), "not a url/metrics");
    }

    #[test]
    fn fetch_raw_ok_returns_body() {
        let body = "llama_http_requests_total 42\n";
        let port = serve_200(body);
        let outcome = fetch_raw(&test_client(), format!("http://127.0.0.1:{port}/metrics"));
        match outcome {
            MetricsOutcome::Ok(text) => assert_eq!(text, body),
            other => panic!("expected Ok, got {}", outcome_variant(&other)),
        }
    }

    #[test]
    fn fetch_raw_non_success_maps_to_http_status() {
        let port = serve_once(
            "HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        );
        let outcome = fetch_raw(&test_client(), format!("http://127.0.0.1:{port}/metrics"));
        let variant = outcome_variant(&outcome);
        assert!(
            matches!(outcome, MetricsOutcome::Http(status) if status == StatusCode::NOT_IMPLEMENTED),
            "expected Http(501), got {variant}"
        );
    }

    #[test]
    fn fetch_raw_connection_refusal_maps_to_redacted_unreachable() {
        // No listener at all: connect is refused. The base carries a query
        // token so the test also proves the error text stays URL-free.
        let port = closed_port();
        let url = endpoint_url(
            &format!("http://127.0.0.1:{port}/?token=supersecret"),
            "metrics",
        );
        let outcome = fetch_raw(&test_client(), url);
        match outcome {
            MetricsOutcome::Unreachable(message) => {
                assert!(!message.is_empty());
                assert!(
                    !message.contains("supersecret"),
                    "token leaked into error text: {message}"
                );
                assert!(
                    !message.contains("token="),
                    "query leaked into error text: {message}"
                );
                assert!(
                    !message.contains(&format!("http://127.0.0.1:{port}")),
                    "request URL leaked into error text: {message}"
                );
            }
            other => panic!("expected Unreachable, got {}", outcome_variant(&other)),
        }
    }

    #[test]
    fn read_body_limited_accepts_body_within_limit() {
        let body = "hello metrics";
        let port = serve_200(body);
        let response = test_client()
            .get(format!("http://127.0.0.1:{port}/metrics"))
            .send()
            .expect("send request");
        let text = read_body_limited(response, 64).expect("body within limit");
        assert_eq!(text, body);
    }

    #[test]
    fn read_body_limited_rejects_oversized_body() {
        let body = "x".repeat(200);
        let port = serve_200(&body);
        let response = test_client()
            .get(format!("http://127.0.0.1:{port}/metrics"))
            .send()
            .expect("send request");
        let err = read_body_limited(response, 64).expect_err("oversized body must fail");
        assert!(
            err.contains("exceeded 64 bytes"),
            "unexpected limit error: {err}"
        );
    }
}
