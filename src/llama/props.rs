//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
use super::*;

/// Apply the display-safe text boundary to the free-text error fields of a
/// sample. This is the single place server-influenced error text enters
/// [`LlmStats`], so stripping terminal control characters and bounding the
/// length here covers every consumer (TUI and `orsiktop diag`). URLs were
/// already redacted when the error strings were built.
pub(super) fn sanitize_error_text(stats: &mut LlmStats) {
    stats.error = crate::redact::sanitize_display_text(
        &stats.error,
        crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS,
    );
    stats.slots_error = crate::redact::sanitize_display_text(
        &stats.slots_error,
        crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS,
    );
}

pub(super) fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

pub(super) fn json_u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

pub(super) fn model_display_name(value: &str) -> String {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .trim_end_matches(".gguf");
    crate::redact::sanitize_display_text(name, MAX_MODEL_DISPLAY_CHARS)
}
