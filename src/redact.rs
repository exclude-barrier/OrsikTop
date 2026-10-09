//! No-secrets URL rendering shared by diagnostics and telemetry error paths.
//!
//! The diagnostics report and the error strings that feed it must never expose
//! URL credentials or other confidential URL parts. Two layers live here:
//! a structured reqwest-error renderer (preferred, because it still has the
//! error object) and a defensive free-text URL redactor.

use std::error::Error as _;

/// Placeholder for an endpoint that cannot be parsed. The input is never
/// echoed: an unparseable value can still contain a secret.
pub(crate) const UNPARSEABLE_ENDPOINT: &str = "<unparseable endpoint>";

/// Safe display form of an endpoint.
///
/// Keeps the connection-relevant, non-secret parts — scheme, host and port —
/// and discards everything that could carry a credential: userinfo, path,
/// query and fragment. Query/fragment/path contents are replaced by `…`
/// markers so the reader still sees that they existed. A URL that cannot be
/// parsed becomes a placeholder rather than the original string, so no
/// fallback path can leak a confidential value. This is display-only; the
/// endpoint used for the actual connection is never modified.
pub(crate) fn safe_endpoint(raw: &str) -> String {
    match reqwest::Url::parse(raw.trim()) {
        Ok(url) => safe_endpoint_of(&url),
        Err(_) => UNPARSEABLE_ENDPOINT.to_string(),
    }
}

fn safe_endpoint_of(url: &reqwest::Url) -> String {
    let Some(host) = url.host_str() else {
        return UNPARSEABLE_ENDPOINT.to_string();
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut out = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    if !matches!(url.path(), "" | "/") {
        out.push_str("/…");
    }
    if url.query().is_some() {
        out.push_str("?…");
    }
    if url.fragment().is_some() {
        out.push_str("#…");
    }
    out
}

/// A URL-free, category-preserving description of a reqwest error.
///
/// Preferred over free-text scanning: reqwest attaches the request URL to its
/// errors, so the URL is stripped structurally (`without_url`) before the
/// message is assembled. The source chain is appended for the useful
/// transport detail (e.g. `Connection refused`), and the result is passed
/// through [`redact_urls`] so no leftover URL can slip through. HTTP status and
/// error category are preserved; the request URL is not.
pub(crate) fn describe_reqwest_error(err: reqwest::Error) -> String {
    // Collect the source-chain detail first (a borrow), then strip the
    // reqwest-attached request URL (which consumes the error).
    let mut detail = String::new();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() {
            detail.push_str(": ");
            detail.push_str(&text);
        }
        source = cause.source();
    }
    let mut message = err.without_url().to_string();
    message.push_str(&detail);
    // `without_url` clears the reqwest-attached URL; the source chain is not
    // guaranteed to be URL-free, so redact as a defensive second layer.
    redact_urls(&message)
}

/// Redact every URL embedded in free text (e.g. a pre-formatted error string)
/// through [`safe_endpoint`], so error paths cannot bypass the no-secrets rule.
///
/// A URL token is everything up to the next whitespace: characters that are
/// legal inside a URL — `,`, `)`, `]` (an IPv6 host ends in `]`), `}` — must not
/// terminate it, otherwise the tail (which can carry a token) would be copied
/// out unredacted.
pub(crate) fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = find_url_start(rest) {
        out.push_str(&rest[..index]);
        let candidate = &rest[index..];
        let end = candidate
            .find(char::is_whitespace)
            .unwrap_or(candidate.len());
        out.push_str(&redact_url_token(&candidate[..end]));
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

/// Redact one whitespace-delimited token that starts with a URL scheme.
///
/// Wrapper punctuation appended by the surrounding text (e.g. the closing `)`
/// in `… (http://host/path)`) is preserved as inert text. A token that does not
/// parse as a URL is replaced entirely — never echoed — so a truncated or
/// malformed URL cannot leak its secret.
fn redact_url_token(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    // Try the token with trailing wrapper punctuation split off first, so the
    // punctuation stays in the output instead of being folded into the URL.
    let trimmed = token.trim_end_matches(is_wrapper_delimiter);
    let suffix = &token[trimmed.len()..];
    if let Some(redacted) = parse_safe(trimmed) {
        return format!("{redacted}{suffix}");
    }
    // A legitimate URL may itself end in one of those characters (e.g. an IPv6
    // host `[::1]`); fall back to the whole token before giving up.
    if let Some(redacted) = parse_safe(token) {
        return redacted;
    }
    format!("{UNPARSEABLE_ENDPOINT}{suffix}")
}

fn is_wrapper_delimiter(ch: char) -> bool {
    matches!(ch, ')' | ',' | ']' | '}' | '>' | '"' | '\'')
}

/// `Some(safe)` when `candidate` parses as an absolute URL with a host.
fn parse_safe(candidate: &str) -> Option<String> {
    let url = reqwest::Url::parse(candidate).ok()?;
    url.host_str()?;
    Some(safe_endpoint_of(&url))
}

/// Longest display string for externally controlled free text (server-supplied
/// model names and error details), in `char`s. A hostile or buggy server cannot
/// bloat the TUI or `orsiktop diag` output past this.
pub(crate) const MAX_UNTRUSTED_DISPLAY_CHARS: usize = 240;

/// Make externally controlled text safe to display.
///
/// Terminal control characters (ANSI/ESC, CR, LF, TAB, NUL, ...) are removed so
/// a server cannot inject escape sequences or forge lines in a structured
/// report; runs of whitespace collapse to a single space. Ordinary Unicode is
/// preserved. The result is bounded to `max_chars` with an ellipsis so a server
/// cannot bloat the output. This is a presentation boundary only: it never
/// changes the endpoint used for networking or any parsed value.
pub(crate) fn sanitize_display_text(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if max_chars == 0 {
        return String::new();
    }
    if out.chars().count() > max_chars {
        let mut truncated: String = out.chars().take(max_chars).collect();
        truncated.push('…');
        truncated
    } else {
        out
    }
}

/// Byte index of the first `http://`/`https://` scheme in `text`, if any.
/// `to_ascii_lowercase` keeps byte lengths, so the index is valid for `text`.
///
/// Deliberately scoped to http(s): those are the only schemes `reqwest`
/// actually requests in production, and [`describe_reqwest_error`] strips the
/// reqwest-attached URL structurally (`without_url`) before this free-text pass
/// ever runs, so a non-http(s) URL cannot reach an error string from a request.
/// Endpoint *display* does not depend on this function — it goes through
/// [`safe_endpoint`], which parses any scheme. Broadening this scan to unused
/// protocols would be speculative and is intentionally not done.
fn find_url_start(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    match (lower.find("http://"), lower.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn safe_endpoint_keeps_only_scheme_host_and_port() {
        assert_eq!(
            safe_endpoint("http://demo-user:demo-password@localhost:8080"),
            "http://localhost:8080"
        );
        assert_eq!(
            safe_endpoint("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            safe_endpoint("http://127.0.0.1:8080/v1?token=abc#frag"),
            "http://127.0.0.1:8080/…?…#…"
        );
        assert_eq!(safe_endpoint("http://[::1]:8080"), "http://[::1]:8080");
        assert_eq!(
            safe_endpoint("http://user%40name:p%40ss@localhost:8080"),
            "http://localhost:8080"
        );
        assert_eq!(safe_endpoint("https://example.com/"), "https://example.com");
    }

    #[test]
    fn safe_endpoint_never_echoes_invalid_input() {
        for raw in [
            "not a url with secret=abc",
            "localhost:8080?token=abc",
            "http://",
            "",
        ] {
            let safe = safe_endpoint(raw);
            assert_eq!(safe, UNPARSEABLE_ENDPOINT, "input {raw:?} leaked");
            assert!(!safe.contains("secret"));
        }
    }

    #[test]
    fn redact_urls_handles_delimiters_inside_urls() {
        // Regression: `]`, `,` and `)` are legal inside a URL (IPv6 host, path,
        // query) and must not terminate the token and leave its token tail
        // behind. All three reproduced error texts must come out clean.
        let cases = [
            "error sending request for url (http://[::1]:9/?token=demo_secret/metrics)",
            "error sending request for url (http://127.0.0.1:9/a,b?token=demo_secret/metrics)",
            "error sending request for url (http://127.0.0.1:9/a)b?token=demo_secret/metrics)",
        ];
        for text in cases {
            let redacted = redact_urls(text);
            assert!(
                !redacted.contains("demo_secret"),
                "secret leaked for {text:?}: {redacted}"
            );
            assert!(
                redacted.contains("http://"),
                "connection info lost for {text:?}: {redacted}"
            );
        }
    }

    #[test]
    fn redact_urls_strips_credentials_from_error_text() {
        let text = "error sending request for url \
                    (http://demo-user:demo-password@localhost:8080/metrics): connection refused";
        let redacted = redact_urls(text);
        assert!(!redacted.contains("demo-user"), "{redacted}");
        assert!(!redacted.contains("demo-password"), "{redacted}");
        assert!(redacted.contains("http://localhost:8080"), "{redacted}");
        assert!(redacted.contains("connection refused"), "{redacted}");
    }

    #[test]
    fn redact_urls_never_echoes_a_truncated_or_unparseable_url() {
        let truncated = "error sending request for url (http://[::1";
        let redacted = redact_urls(truncated);
        assert!(
            !redacted.contains("[::1"),
            "truncated URL leaked: {redacted}"
        );
        assert!(redacted.contains(UNPARSEABLE_ENDPOINT), "{redacted}");

        // Text without a URL is returned unchanged.
        assert_eq!(
            redact_urls("cannot reach llama.cpp: timeout"),
            "cannot reach llama.cpp: timeout"
        );
    }

    /// Integration test for the diagnostics error path: generate a *real*
    /// reqwest error (a loopback request to a closed port) whose URL carries a
    /// synthetic secret, then assert the structured renderer removes it while
    /// keeping the error understandable.
    #[test]
    fn describe_reqwest_error_removes_a_real_request_url() {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("client");
        let err = client
            .get("http://127.0.0.1:9/metrics?token=demo_secret")
            .send()
            .expect_err("port 9 must not accept connections");
        let described = describe_reqwest_error(err);
        assert!(
            !described.contains("demo_secret"),
            "request URL secret leaked: {described}"
        );
        assert!(
            !described.contains("127.0.0.1:9"),
            "request URL should be removed: {described}"
        );
        // The category is preserved and the message stays non-empty.
        assert!(described.contains("error sending request"), "{described}");
    }

    #[test]
    fn sanitize_display_text_neutralizes_control_characters() {
        // ANSI escape sequence: the ESC and CSI are stripped; the bracket text
        // is inert (it cannot start a sequence without ESC).
        let ansi = sanitize_display_text("evil\u{1b}[31mred\u{1b}[0m", 100);
        assert!(!ansi.chars().any(char::is_control), "{ansi:?}");
        assert!(!ansi.contains('\u{1b}'), "{ansi:?}");

        // CR/LF must not forge lines in a structured report; they collapse to
        // single spaces.
        assert_eq!(
            sanitize_display_text("line1\r\nline2\rline3", 100),
            "line1 line2 line3"
        );

        // Embedded tabs and runs of whitespace collapse to a single space.
        assert_eq!(sanitize_display_text("a\t\tb   c", 100), "a b c");

        // NUL and other C0/C1 controls are removed.
        assert_eq!(sanitize_display_text("a\u{0}b\u{7}c", 100), "a b c");

        // Ordinary Unicode, including non-ASCII names, is preserved.
        assert_eq!(
            sanitize_display_text("Qwen3.8-27B – 模型", 100),
            "Qwen3.8-27B – 模型"
        );
    }

    #[test]
    fn sanitize_display_text_bounds_length_without_losing_context() {
        let long = "e".repeat(1000);
        let bounded = sanitize_display_text(&long, 240);
        // 240 kept + one ellipsis.
        assert_eq!(bounded.chars().count(), 241);
        assert!(bounded.ends_with('…'));

        // A short string is returned unchanged, and a zero bound is empty.
        assert_eq!(
            sanitize_display_text("connection refused", 240),
            "connection refused"
        );
        assert_eq!(sanitize_display_text("anything", 0), "");
    }

    #[test]
    fn sanitize_display_text_keeps_a_malformed_url_fragment_inert() {
        // A malformed URL fragment is not a control-char vector once the text
        // boundary is applied; the useful text survives.
        let text = sanitize_display_text("cannot reach http://[::1?t=se\u{1b}[2Jcret", 240);
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(text.contains("cannot reach"), "{text:?}");
    }
}
