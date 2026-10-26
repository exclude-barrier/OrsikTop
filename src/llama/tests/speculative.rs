//! Spec-decoding tests: slot-shape status and the local process `/proc` fallback.
use super::super::*;
use serde_json::json;

#[test]
fn reads_speculative_status_and_n_max_from_slot_shape() {
    let slot = json!({
        "speculative": true,
        "params": {"speculative.n_max": 3}
    });
    assert_eq!(
        speculative_config(&slot),
        SpeculativeConfig {
            present: true,
            enabled: Some(true),
            is_mtp: false,
            n_max: Some(3),
            types_known: false,
            named_type: false,
        }
    );
}

#[test]
fn slot_telemetry_marks_speculative_enabled() {
    let mut stats = LlmStats::default();
    let slots = json!([{
        "id": 0,
        "id_task": 11,
        "n_ctx": 4096,
        "speculative": true,
        "is_processing": true,
        "params": {"speculative.n_max": 3},
        "n_prompt_tokens": 128,
        "n_prompt_tokens_processed": 128,
        "next_token": {"n_decoded": 9}
    }]);
    apply_slots_json(&mut stats, &slots).unwrap();
    assert!(stats.spec_enabled);
    assert_eq!(stats.spec_n_max, Some(3));
}

#[test]
fn loopback_host_matches_ipv4_ipv6_and_localhost_equally() {
    // IPv6 hosts come back from Url::host_str() bracketed; the local
    // SPEC-CLI fallback must treat `[::1]` exactly like `127.0.0.1`.
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("localhost"));
    assert!(is_loopback_host("::1"));
    assert!(is_loopback_host("[::1]"));
    // Any loopback address in the reserved ranges counts.
    assert!(is_loopback_host("127.0.0.2"));
    // A remote or wildcard host must not trigger a local /proc scan.
    assert!(!is_loopback_host("192.168.1.20"));
    assert!(!is_loopback_host("0.0.0.0"));
    assert!(!is_loopback_host("::"));
    assert!(!is_loopback_host("[::]"));
    assert!(!is_loopback_host("example.com"));
    assert!(!is_loopback_host(""));
}

#[test]
fn reads_draft_mtp_type_from_custom_slot_shape() {
    let value = serde_json::json!({
        "speculative": true,
        "params": {"speculative.types": "none,draft-mtp"}
    });
    let spec = speculative_config(&value);
    assert_eq!(spec.enabled, Some(true));
    assert!(spec.is_mtp);
    assert_eq!(spec.n_max, None);
}

#[test]
fn recognizes_llama_serve_subcommand_and_reads_n_max() {
    let args = vec![
        "/home/user/.local/bin/llama".to_string(),
        "serve".to_string(),
        "-hf".to_string(),
        "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".to_string(),
        "--port".to_string(),
        "8081".to_string(),
        "--spec-type".to_string(),
        "draft-mtp".to_string(),
        "--spec-draft-n-max".to_string(),
        "3".to_string(),
    ];

    assert!(is_llama_server_process(&args));
    assert!(command_targets_port(&args, 8081));
    let spec = speculative_from_process(&args, None);
    assert!(spec.enabled);
    assert!(spec.is_mtp);
    assert_eq!(spec.n_max, Some(3));
}

#[test]
fn reads_n_max_from_cli() {
    let args = vec![
        "llama-server".to_string(),
        "--port".to_string(),
        "8081".to_string(),
        "--spec-type".to_string(),
        "draft-mtp".to_string(),
        "--spec-draft-n-max".to_string(),
        "3".to_string(),
    ];
    let spec = speculative_from_process(&args, None);
    assert!(spec.enabled);
    assert!(spec.is_mtp);
    assert_eq!(spec.n_max, Some(3));
    assert!(command_targets_port(&args, 8081));
}

#[test]
fn reads_n_max_from_environment() {
    let args = vec!["llama-server".to_string(), "--port=8081".to_string()];
    let env = b"LLAMA_ARG_SPEC_TYPE=draft-mtp\0LLAMA_ARG_SPEC_DRAFT_N_MAX=3\0";
    let spec = speculative_from_process(&args, Some(env));
    assert!(spec.enabled);
    assert!(spec.is_mtp);
    assert_eq!(spec.n_max, Some(3));
}

#[test]
fn sanitize_error_text_neutralizes_injected_control_characters() {
    // The display boundary `sample()` applies to every error field. A
    // synthetic server-influenced error carries ANSI/CR/LF/tab/NUL; after
    // the boundary it must be terminal-safe (no injection, no forged report
    // line) while the useful diagnostic context survives.
    let mut stats = LlmStats {
        error: "transport\u{1b}[31mboom\r\nforged line\ttail".to_string(),
        slots_error: "slots\u{0}error".to_string(),
        ..Default::default()
    };
    sanitize_error_text(&mut stats);
    for text in [&stats.error, &stats.slots_error] {
        assert!(
            !text.chars().any(char::is_control),
            "control character survived the boundary: {text:?}"
        );
    }
    assert!(stats.error.contains("transport"), "{:?}", stats.error);
    assert!(stats.error.contains("boom"), "{:?}", stats.error);
    assert!(stats.error.contains("forged line"), "{:?}", stats.error);
    assert!(
        stats.slots_error.contains("slots"),
        "{:?}",
        stats.slots_error
    );
    assert!(
        stats.slots_error.contains("error"),
        "{:?}",
        stats.slots_error
    );

    // The length is bounded so a hostile response cannot bloat the output.
    let mut long = LlmStats {
        error: "x".repeat(1000),
        ..Default::default()
    };
    sanitize_error_text(&mut long);
    assert_eq!(
        long.error.chars().count(),
        crate::redact::MAX_UNTRUSTED_DISPLAY_CHARS + 1
    );
}
