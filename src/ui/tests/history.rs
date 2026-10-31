//! Rendered history headers must distinguish absent/stale values from zero.
use super::super::history::{
    draw_history_column, draw_history_rate_column, draw_llm_rate_history_column,
};
use super::super::*;

fn render_column(kind: u8, samples: &VecDeque<TimedSample>, observed: Option<Instant>) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 5)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            match kind {
                0 => draw_history_column(frame, area, "GPU", samples, observed, CYAN, false, false),
                1 => draw_history_rate_column(
                    frame, area, "DECODE", samples, observed, CYAN, false, false,
                ),
                _ => draw_llm_rate_history_column(
                    frame, area, "DECODE", samples, observed, CYAN, false,
                ),
            }
        })
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .take(60)
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

#[test]
fn empty_expired_and_nonfinite_history_headers_are_unavailable_not_zero_or_idle() {
    let now = Instant::now();
    for samples in [
        VecDeque::new(),
        VecDeque::from([TimedSample {
            at: now - HISTORY_WINDOW - Duration::from_secs(1),
            value: 42.0,
        }]),
        VecDeque::from([TimedSample {
            at: now,
            value: f64::NAN,
        }]),
        VecDeque::from([TimedSample {
            at: now,
            value: f64::INFINITY,
        }]),
    ] {
        for kind in 0..3 {
            let text = render_column(kind, &samples, None);
            assert!(text.contains('—'), "{text}");
            assert!(
                !text.contains("0%") && !text.contains("0.0 tok/s") && !text.contains("idle"),
                "{text}"
            );
        }
        assert!(!llm_rate_history_has_activity(&samples));
    }
}

#[test]
fn current_zero_history_is_a_real_measurement_but_a_later_gap_is_not() {
    let now = Instant::now();
    let samples = VecDeque::from([TimedSample {
        at: now,
        value: 0.0,
    }]);
    for kind in 0..3 {
        let text = render_column(kind, &samples, Some(now));
        // A measured zero stays a measured zero (`compact_rate(0.0)` -> "0").
        assert!(
            text.contains(if kind == 0 { "0%" } else { "0 tok/s" }),
            "{text}"
        );
        assert!(!text.contains('—'), "{text}");
        let missing = render_column(kind, &samples, Some(now + Duration::from_nanos(1)));
        assert!(
            missing.contains('—') && !missing.contains("idle"),
            "{missing}"
        );
    }
}

#[test]
fn missing_host_sensor_hides_current_header_without_erasing_valid_history() {
    let mut state = UiState::default();
    state.push_sample(
        &GpuStats {
            available: true,
            utilization: Some(42.0),
            ..Default::default()
        },
        &SystemStats::default(),
    );
    assert!(render_column(0, &state.gpu_history, state.last_host_sample_at).contains("42%"));
    state.push_sample(&GpuStats::default(), &SystemStats::default());
    assert_eq!(state.gpu_history.len(), 1);
    assert!(render_column(0, &state.gpu_history, state.last_host_sample_at).contains('—'));
}

#[test]
fn missing_or_held_llm_rates_are_not_presented_as_current_history_values() {
    let mut state = UiState::default();
    state.record_llm_sample(
        "a",
        "a",
        &LlmStats {
            connected: true,
            metrics_available: true,
            generation_tps: Some(42.0),
            ..Default::default()
        },
    );
    assert!(render_column(
        1,
        &state.llm_history.decode,
        state.llm_history.last_observed_at
    )
    .contains("42.0 tok/s"));
    for llm in [
        LlmStats {
            connected: true,
            metrics_available: true,
            ..Default::default()
        },
        LlmStats {
            connected: true,
            metrics_available: true,
            reconnecting: true,
            generation_tps: Some(999.0),
            ..Default::default()
        },
        LlmStats::default(),
    ] {
        state.record_llm_sample("a", "a", &llm);
        assert_eq!(state.llm_history.decode.len(), 1);
        for kind in 1..3 {
            let text = render_column(
                kind,
                &state.llm_history.decode,
                state.llm_history.last_observed_at,
            );
            assert!(
                text.contains('—') && !text.contains("42.0 tok/s") && !text.contains("idle"),
                "{text}"
            );
        }
    }
}
