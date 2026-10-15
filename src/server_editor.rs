//! Pure, I/O-free helpers for editing the additional LLM server list.
//!
//! The settings dialog edits `settings.servers` (the additional endpoints kept
//! alongside the primary one). Every decision the dialog needs — identity for
//! duplicate detection, validation of a typed candidate, the monitored/overflow
//! split and the redacted display label — is computed here from strings only:
//! no network, no filesystem, no telemetry, no new dependency. Secrets stay
//! intact in the stored value and are never rendered: display goes exclusively
//! through [`crate::redact::safe_endpoint`].

use crate::domain::{server_key, MAX_MONITORED_SERVERS};
use crate::redact::safe_endpoint;

/// Stable identity of an endpoint (delegates to domain::server_key).
///
/// Identity ignores userinfo and normalizes scheme/host case, default ports and
/// trailing slashes, so the same target URL written differently shares one key;
/// distinct hosts never collapse.
pub(crate) fn endpoint_key(endpoint: &str) -> String {
    server_key(endpoint)
}

/// First existing entry in `list` sharing its identity with `candidate`, if any.
///
/// Compares via [`crate::domain::server_key`], so a candidate that differs only
/// in credentials, case or trailing slash still counts as a duplicate of an
/// existing entry.
pub(crate) fn find_duplicate<'a>(list: &'a [String], candidate: &str) -> Option<&'a String> {
    let key = server_key(candidate);
    list.iter().find(|entry| server_key(entry) == key)
}

/// Validate and trim a candidate additional endpoint. On success returns the
/// trimmed original string (credentials/query preserved verbatim); on failure a
/// human-readable, redacted error message.
///
/// Accepts only absolute URLs with a host (parsed via `reqwest::Url`). The
/// stored value is the trimmed input untouched — scheme, userinfo, path, query
/// and fragment survive byte-for-byte, so credentials are never mangled. The
/// error message never echoes the raw input: it names the problem and shows at
/// most the [`safe_endpoint`] form (scheme/host/port only, or the redact
/// module's placeholder for anything unparseable).
pub(crate) fn normalize_endpoint(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Server URL must not be empty".to_string());
    }
    let Ok(parsed) = reqwest::Url::parse(trimmed) else {
        return Err(format!(
            "Not a valid server URL: {}",
            safe_endpoint(trimmed)
        ));
    };
    if parsed.host_str().is_none() {
        return Err(format!(
            "Server URL must include a host: {}",
            safe_endpoint(trimmed)
        ));
    }
    Ok(trimmed.to_string())
}

/// Split a combined endpoint list into the monitored head (at most
/// MAX_MONITORED_SERVERS entries) and the preserved-but-unmonitored overflow.
/// Both parts preserve input order.
pub(crate) fn split_monitored(list: &[String]) -> (Vec<String>, Vec<String>) {
    let monitored = list.iter().take(MAX_MONITORED_SERVERS).cloned().collect();
    let overflow = list.iter().skip(MAX_MONITORED_SERVERS).cloned().collect();
    (monitored, overflow)
}

/// Safe, redacted display label for an endpoint (scheme+host+port only; no
/// userinfo/path/query/fragment).
///
/// Delegates to [`crate::redact::safe_endpoint`]: path/query/fragment appear
/// only as inert `…` markers, and an unparseable endpoint becomes the redact
/// module's placeholder instead of the original string.
pub(crate) fn display_label(endpoint: &str) -> String {
    safe_endpoint(endpoint)
}

/// How the runtime will treat the configured endpoints shown in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Monitoring {
    /// One flag per additional entry (`true` = monitored).
    pub flags: Vec<bool>,
    /// Distinct configured endpoints that will be polled (primary included).
    pub monitored: usize,
    /// Distinct configured endpoints beyond the cap: kept, but not polled.
    pub overflow: usize,
}

/// Monitoring status of each additional entry as the runtime will see it.
///
/// The runtime assembles the primary endpoint first, then the additional
/// entries, de-duplicated by identity, and caps the result at
/// [`MAX_MONITORED_SERVERS`]. This mirrors that order so the dialog can flag
/// exactly the entries that will be kept but not polled.
pub(crate) fn monitored_flags(primary: Option<&str>, extras: &[String]) -> Monitoring {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = primary
        .map(str::to_string)
        .into_iter()
        .chain(extras.iter().cloned())
        .filter(|endpoint| seen.insert(endpoint_key(endpoint)))
        .collect();
    let (monitored, overflow) = split_monitored(&unique);
    let monitored_keys: std::collections::HashSet<String> = monitored
        .iter()
        .map(|endpoint| endpoint_key(endpoint))
        .collect();
    let flags = extras
        .iter()
        .map(|endpoint| monitored_keys.contains(&endpoint_key(endpoint)))
        .collect();
    Monitoring {
        flags,
        monitored: monitored.len(),
        overflow: overflow.len(),
    }
}

/// Mask character used by [`mask_draft`].
const MASK: char = '•';

/// Render a (possibly incomplete) endpoint being typed without revealing its
/// secrets.
///
/// The draft must stay editable character by character, so it cannot go
/// through [`safe_endpoint`] (which drops whole components). Instead every
/// character of the userinfo (`user:password@`), of each query value and of
/// the fragment is replaced by `•`, keeping the length so the cursor position
/// stays meaningful. Scheme, host, port, path and query keys stay readable.
/// Userinfo is recognised once its terminating `@` has been typed.
pub(crate) fn mask_draft(draft: &str) -> String {
    let chars: Vec<char> = draft.chars().collect();
    let authority_start = draft
        .find("://")
        .map(|byte| draft[..byte].chars().count() + 3)
        .unwrap_or(0);
    let authority_end = chars[authority_start..]
        .iter()
        .position(|ch| matches!(ch, '/' | '?' | '#'))
        .map_or(chars.len(), |offset| authority_start + offset);
    let userinfo_end = chars[authority_start..authority_end]
        .iter()
        .rposition(|ch| *ch == '@')
        .map(|offset| authority_start + offset);

    let mut out = String::with_capacity(draft.len());
    let mut in_query = false;
    let mut in_value = false;
    let mut in_fragment = false;
    for (index, ch) in chars.iter().copied().enumerate() {
        if userinfo_end.is_some_and(|end| index >= authority_start && index < end) {
            out.push(MASK);
            continue;
        }
        if index < authority_end {
            out.push(ch);
            continue;
        }
        if in_fragment {
            out.push(MASK);
            continue;
        }
        match ch {
            '#' => {
                in_fragment = true;
                out.push(ch);
            }
            '?' if !in_query => {
                in_query = true;
                out.push(ch);
            }
            '&' if in_query => {
                in_value = false;
                out.push(ch);
            }
            '=' if in_query && !in_value => {
                in_value = true;
                out.push(ch);
            }
            _ if in_value => out.push(MASK),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::UNPARSEABLE_ENDPOINT;

    #[test]
    fn endpoint_key_matches_domain_identity_across_surface_forms() {
        // Same target written differently -> same identity.
        assert_eq!(
            endpoint_key("http://127.0.0.1:8081"),
            endpoint_key("http://127.0.0.1:8081/")
        );
        assert_eq!(
            endpoint_key("HTTP://LocalHost:8081"),
            endpoint_key("http://localhost:8081")
        );
        assert_eq!(endpoint_key("http://h:80"), endpoint_key("http://h"));
        assert_eq!(
            endpoint_key("http://user:pass@h:8081/v1/"),
            endpoint_key("http://h:8081/v1")
        );
        // Distinct hosts and distinct queries never collapse.
        assert_ne!(
            endpoint_key("http://127.0.0.1:8081"),
            endpoint_key("http://localhost:8081")
        );
        assert_ne!(
            endpoint_key("http://h:8081/?token=a"),
            endpoint_key("http://h:8081/?token=b")
        );
    }

    #[test]
    fn find_duplicate_detects_rewrites_and_rejects_distinct_hosts() {
        let list = vec![
            "http://localhost:8081/v1".to_string(),
            "https://other.example:8443".to_string(),
        ];
        // Same target, different spelling (case, trailing slash, credentials).
        let dup = find_duplicate(&list, "HTTP://LocalHost:8081/v1/").expect("duplicate");
        assert_eq!(dup, "http://localhost:8081/v1");
        let dup = find_duplicate(&list, "http://user:secret@localhost:8081/v1")
            .expect("credentials must not hide a duplicate");
        assert_eq!(dup, "http://localhost:8081/v1");
        // A distinct host is not a duplicate even with the same port/path.
        assert!(find_duplicate(&list, "http://127.0.0.1:8081/v1").is_none());
        assert!(find_duplicate(&list, "https://other.example:8443/v1").is_none());
        // Empty list never reports a duplicate.
        assert!(find_duplicate(&[], "http://localhost:8081").is_none());
    }

    #[test]
    fn normalize_endpoint_rejects_empty_and_hostless_input() {
        assert_eq!(
            normalize_endpoint(""),
            Err("Server URL must not be empty".to_string())
        );
        assert_eq!(
            normalize_endpoint("   \t "),
            Err("Server URL must not be empty".to_string())
        );
        // Not a URL at all.
        let err = normalize_endpoint("not a url with secret=hunter2").unwrap_err();
        assert!(err.contains(UNPARSEABLE_ENDPOINT), "{err}");
        assert!(!err.contains("hunter2"), "secret leaked: {err}");
        // Parses as a scheme without a host ("localhost:" looks like a scheme).
        let err = normalize_endpoint("localhost:8080?token=hunter2").unwrap_err();
        assert!(err.contains(UNPARSEABLE_ENDPOINT), "{err}");
        assert!(!err.contains("hunter2"), "secret leaked: {err}");
    }

    #[test]
    fn normalize_endpoint_keeps_credentials_and_routing_verbatim() {
        let raw = "  http://user:p%40ss@example.com:8081/v1?token=abc#frag  ";
        let ok = normalize_endpoint(raw).expect("valid endpoint");
        // Trimmed, but everything else byte-identical: no credential loss.
        assert_eq!(ok, "http://user:p%40ss@example.com:8081/v1?token=abc#frag");
        // A plain endpoint round-trips unchanged.
        assert_eq!(
            normalize_endpoint("https://example.com").unwrap(),
            "https://example.com"
        );
        // IPv6 hosts stay valid.
        assert_eq!(
            normalize_endpoint("http://[::1]:8081/v1").unwrap(),
            "http://[::1]:8081/v1"
        );
    }

    #[test]
    fn split_monitored_cuts_exactly_at_the_cap_and_keeps_order() {
        let cap = MAX_MONITORED_SERVERS; // 16, from domain — not hard-coded here.
        assert_eq!(cap, 16);

        // One below the cap: everything monitored, no overflow.
        let list: Vec<String> = (0..cap - 1).map(|i| format!("http://h{i}:8081")).collect();
        let (monitored, overflow) = split_monitored(&list);
        assert_eq!(monitored, list);
        assert!(overflow.is_empty());

        // Exactly at the cap: still no overflow.
        let list: Vec<String> = (0..cap).map(|i| format!("http://h{i}:8081")).collect();
        let (monitored, overflow) = split_monitored(&list);
        assert_eq!(monitored, list);
        assert!(overflow.is_empty());

        // One past the cap: exactly one entry overflows, order preserved in both parts.
        let list: Vec<String> = (0..cap + 1).map(|i| format!("http://h{i}:8081")).collect();
        let (monitored, overflow) = split_monitored(&list);
        assert_eq!(monitored.len(), cap);
        assert_eq!(monitored, &list[..cap]);
        assert_eq!(overflow, vec![format!("http://h{cap}:8081")]);
    }

    #[test]
    fn display_label_redacts_everything_but_scheme_host_and_port() {
        assert_eq!(
            display_label("http://user:pass@example.com:8081/v1?token=abc#frag"),
            "http://example.com:8081/…?…#…"
        );
        assert_eq!(
            display_label("https://127.0.0.1:8081"),
            "https://127.0.0.1:8081"
        );
        assert_eq!(display_label("http://[::1]:8080"), "http://[::1]:8080");
        // Unparseable input becomes the placeholder, never the raw string.
        let label = display_label("garbage secret=hunter2");
        assert_eq!(label, UNPARSEABLE_ENDPOINT);
        assert!(!label.contains("hunter2"));
    }

    #[test]
    fn monitored_flags_follow_runtime_order_dedup_and_cap() {
        let cap = MAX_MONITORED_SERVERS;
        // Primary + cap extras: exactly the last extra is beyond the cap.
        let extras: Vec<String> = (0..cap).map(|i| format!("http://h{i}:8081")).collect();
        let status = monitored_flags(Some("http://primary:8080"), &extras);
        assert_eq!((status.monitored, status.overflow), (cap, 1));
        assert!(status.flags[..cap - 1].iter().all(|flag| *flag));
        assert!(!status.flags[cap - 1]);

        // Without a primary every extra fits.
        let status = monitored_flags(None, &extras);
        assert_eq!((status.monitored, status.overflow), (cap, 0));
        assert!(status.flags.iter().all(|flag| *flag));

        // An extra that duplicates the primary does not consume a slot and is
        // reported as monitored (it is the primary).
        let mut extras = extras;
        extras.insert(0, "http://primary:8080/".to_string());
        let status = monitored_flags(Some("http://primary:8080"), &extras);
        assert_eq!((status.monitored, status.overflow), (cap, 1));
        assert!(status.flags[0]);
        assert!(!status.flags[cap]);
    }

    #[test]
    fn mask_draft_hides_userinfo_query_values_and_fragment() {
        assert_eq!(
            mask_draft("http://user:pw@host:8081/v1?token=abc&x=1#frag"),
            "http://•••••••@host:8081/v1?token=•••&x=•#••••"
        );
        // Plain endpoints and partial input stay readable.
        assert_eq!(mask_draft("http://host:8081/v1"), "http://host:8081/v1");
        assert_eq!(mask_draft("http://ho"), "http://ho");
        assert_eq!(mask_draft(""), "");
        // A token-only userinfo is masked, an `@` in the path is not userinfo.
        assert_eq!(mask_draft("https://tok@h"), "https://•••@h");
        assert_eq!(mask_draft("http://h/a@b"), "http://h/a@b");
        // Length is preserved so the cursor position stays meaningful.
        let raw = "http://ü:sécret@h?k=välue";
        assert_eq!(mask_draft(raw).chars().count(), raw.chars().count());
        assert!(!mask_draft(raw).contains("sécret"));
        assert!(!mask_draft(raw).contains("välue"));
    }
}
