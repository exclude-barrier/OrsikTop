//! Layout-shell tests: compact/full floors, header/footer, widgets, CPU/system/GPU panels.
use super::super::*;
use super::{
    focus_server_list, press, render_dashboard, render_llm_panel, settings_for, type_keys,
};
use crate::domain::{DeviceId, GpuEvidence, GpuVendor, MappedGpu};

#[test]
fn physical_core_average_combines_smt_threads() {
    let core = CpuPhysicalCore {
        kind: CpuCoreKind::Performance,
        logical_cpus: vec![0, 1],
    };
    assert_eq!(
        physical_core_average(&core, &[80.0, 20.0], &[true, true]),
        50.0
    );
}

#[test]
fn core_heatmap_marks_offline_holes_instead_of_a_zero_percent_core() {
    let usages = [10.0, 0.0, 30.0, 40.0];
    let kinds = [
        CpuCoreKind::Performance,
        CpuCoreKind::Unknown,
        CpuCoreKind::Efficiency,
        CpuCoreKind::Efficiency,
    ];
    let online = [true, false, true, true];
    let rows = core_heatmap_rows(&usages, &kinds, &online, 80, Some(0), true, 4);
    let text: String = rows
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
        .collect();
    assert!(
        text.contains("01?") && text.contains('—'),
        "offline hole must render as a muted dash, got: {text:?}"
    );
}

#[test]
fn physical_core_average_skips_offline_smt_siblings() {
    // Thread 1 offline: its 0.0 placeholder must not halve the average.
    let core = CpuPhysicalCore {
        kind: CpuCoreKind::Performance,
        logical_cpus: vec![0, 1],
    };
    assert_eq!(
        physical_core_average(&core, &[80.0, 0.0], &[true, false]),
        80.0
    );
}

#[test]
fn physical_core_color_heats_up_toward_saturation() {
    assert_eq!(physical_core_color(0.0, ORK_GREEN), ORK_GREEN);
    assert_eq!(physical_core_color(55.0, CYAN), CYAN);
    assert_eq!(physical_core_color(80.0, ORK_GREEN), YELLOW);
    assert_eq!(physical_core_color(92.0, CYAN), ORANGE);
    assert_eq!(physical_core_color(100.0, ORK_GREEN), RED);

    let mid = physical_core_color(86.0, ORK_GREEN);
    assert_ne!(mid, YELLOW);
    assert_ne!(mid, ORANGE);
}

#[test]
fn core_usage_glyph_uses_heatmap_levels() {
    assert_eq!(core_usage_glyph(0.0), '·');
    assert_eq!(core_usage_glyph(2.0), '░');
    assert_eq!(core_usage_glyph(25.0), '▒');
    assert_eq!(core_usage_glyph(50.0), '▓');
    assert_eq!(core_usage_glyph(75.0), '█');
    assert_eq!(core_usage_glyph(100.0), '█');
}

#[test]
fn core_usage_color_uses_non_red_load_scale() {
    assert_eq!(core_usage_color(0.0), BAR_EMPTY);
    assert_eq!(core_usage_color(2.0), DIM_GREEN);
    assert_eq!(core_usage_color(25.0), ORK_GREEN);
    assert_eq!(core_usage_color(50.0), BRIGHT_GREEN);
    assert_eq!(core_usage_color(75.0), YELLOW);
    assert_eq!(core_usage_color(90.0), ORANGE);
    assert_eq!(core_usage_color(100.0), ORANGE);
}

#[test]
fn refresh_buttons_match_rendered_positions() {
    let area = Rect::new(0, 0, 120, 3);
    let controls = refresh_controls(area).unwrap();

    assert!(rect_contains(
        controls.minus,
        controls.minus.x + 2,
        controls.minus.y
    ));
    assert!(rect_contains(
        controls.plus,
        controls.plus.x + 2,
        controls.plus.y
    ));
    assert!(!rect_contains(
        controls.minus,
        controls.plus.x,
        controls.plus.y
    ));
}

#[test]
fn refresh_controls_hide_when_terminal_is_too_narrow() {
    assert!(refresh_controls(Rect::new(0, 0, 20, 3)).is_none());
}

#[test]
fn endpoint_is_compact() {
    assert_eq!(compact_endpoint("http://127.0.0.1:8081/"), "127.0.0.1:8081");
}

#[test]
fn compact_endpoint_never_shows_credentials_path_or_query() {
    // Regression: the header used to paint the endpoint verbatim.
    assert_eq!(
        compact_endpoint("http://demo-user:demo-password@10.0.0.7:8080/v1?token=demo_secret"),
        "10.0.0.7:8080/…?…"
    );
    assert!(!compact_endpoint("https://user:s3cret@host:8443/x").contains("s3cret"));
    // A legacy scheme-less host:port stays readable.
    assert_eq!(compact_endpoint("127.0.0.1:8080"), "127.0.0.1:8080");
    // Anything ambiguous is hidden, never echoed.
    assert_eq!(
        compact_endpoint("localhost:8080?token=demo_secret"),
        crate::redact::UNPARSEABLE_ENDPOINT
    );
}

#[test]
fn small_terminal_between_floors_draws_the_reduced_layout() {
    let llm = LlmStats {
        connected: true,
        model: "tiny-ork".to_string(),
        context_size: 4096,
        context_used: Some(1024),
        ..Default::default()
    };
    let mut state = UiState::default();

    let text = render_dashboard(60, 18, 1000, &llm, &mut state);
    assert!(
        !text.contains("needs at least"),
        "60x18 is above the hard floor and must not be a dead end:\n{text}"
    );
    assert!(
        text.contains("ONLINE"),
        "connection status missing:\n{text}"
    );
    assert!(text.contains("CTX"), "context short info missing:\n{text}");
    assert!(
        text.contains("tiny-ork"),
        "model short info missing:\n{text}"
    );

    // Exactly the hard floor is still usable, not the dead-end notice.
    let text = render_dashboard(54, 16, 1000, &llm, &mut state);
    assert!(
        !text.contains("needs at least"),
        "54x16 is the hard floor and stays usable:\n{text}"
    );
    assert!(text.contains("ONLINE"), "status lost at the floor:\n{text}");
}

#[test]
fn below_the_hard_floor_still_shows_the_notice() {
    let llm = LlmStats::default();
    let mut state = UiState::default();
    let text = render_dashboard(53, 15, 1000, &llm, &mut state);
    assert!(
        text.contains("needs at least"),
        "below the floor must show the notice:\n{text}"
    );
    assert!(
        !text.contains("ONLINE"),
        "no status below the floor:\n{text}"
    );
}

#[test]
fn full_layout_is_used_from_72x22() {
    let llm = LlmStats {
        connected: true,
        model: "tiny-ork".to_string(),
        ..Default::default()
    };
    let mut state = UiState::default();
    let text = render_dashboard(72, 22, 1000, &llm, &mut state);
    assert!(
        !text.contains("compact"),
        "72x22 uses the full layout, not the compact one:\n{text}"
    );
}

#[test]
fn compact_layout_help_is_visible_at_the_hard_floor() {
    // `[h]` is advertised by the compact hint, so help must render
    // something even where the full popup does not fit (heights 16..19).
    // Regression for the invisible, key-swallowing help mode.
    let llm = LlmStats {
        connected: true,
        ..Default::default()
    };
    let mut state = UiState::default();
    state.toggle_help();
    assert!(state.is_help_open());
    let text = render_dashboard(54, 16, 1000, &llm, &mut state);
    assert!(
        text.contains("HELP") && text.contains("close help"),
        "help must be visible at 54x16:\n{text}"
    );
    assert!(!text.contains("needs at least"), "{text}");
}

#[test]
fn help_popup_legend_explains_context_and_request_at_72x22() {
    // CFA-91: the dense panel/selector cells cannot explain themselves, so
    // the full help popup carries a field legend, above all the CTX vs.
    // REQUEST distinction. 72x22 is the full-layout floor.
    let llm = LlmStats {
        connected: true,
        ..Default::default()
    };
    let mut state = UiState::default();
    state.toggle_help();
    let text = render_dashboard(72, 22, 1000, &llm, &mut state);
    assert!(
        text.contains("READING THE PANEL"),
        "legend missing:\n{text}"
    );
    assert!(text.contains("CTX"), "CTX row missing:\n{text}");
    assert!(
        text.contains("context occupancy"),
        "CTX explanation missing:\n{text}"
    );
    assert!(text.contains("REQUEST"), "REQUEST row missing:\n{text}");
    assert!(
        text.contains("in-flight request"),
        "REQUEST explanation missing:\n{text}"
    );
}

#[test]
fn help_popup_legend_stays_readable_on_a_large_terminal() {
    let llm = LlmStats {
        connected: true,
        ..Default::default()
    };
    let mut state = UiState::default();
    state.toggle_help();
    let text = render_dashboard(120, 40, 1000, &llm, &mut state);
    assert!(text.contains("READING THE PANEL"), "{text}");
    assert!(
        text.contains("STALE"),
        "the freshness rule must be explained:\n{text}"
    );
    assert!(
        text.contains("unknown or unavailable"),
        "the unknown-value rule must be explained:\n{text}"
    );
}

#[test]
fn compact_help_names_context_and_request_at_the_hard_floor() {
    // The compact help (54x16) still has to distinguish the two central
    // fields, even though the full legend does not fit there.
    let llm = LlmStats {
        connected: true,
        ..Default::default()
    };
    let mut state = UiState::default();
    state.toggle_help();
    let text = render_dashboard(54, 16, 1000, &llm, &mut state);
    assert!(
        text.contains("CTX") && text.contains("REQUEST"),
        "compact help must name CTX and REQUEST:\n{text}"
    );
}

#[test]
fn panel_labels_freshness_as_sample_not_poll() {
    let llm = LlmStats {
        connected: true,
        metrics_available: true,
        model: "m".to_string(),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 12);
    assert!(
        text.contains("SAMPLE"),
        "freshness field must be labelled SAMPLE:\n{text}"
    );
    assert!(
        !text.contains("POLL"),
        "the ambiguous POLL label is gone:\n{text}"
    );
}

#[test]
fn offline_panel_states_the_concrete_backend_failure() {
    // CFA-91: name the actual failure from the sanitized sample instead of
    // a generic "connection lost".
    let llm = LlmStats {
        connected: false,
        error: "cannot reach llama.cpp: connection refused".to_string(),
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 8);
    assert!(text.contains("SERVER OFFLINE"), "{text}");
    assert!(
        text.contains("connection refused"),
        "reason must name the failure:\n{text}"
    );
}

#[test]
fn offline_panel_without_an_error_suggests_checking_the_endpoint() {
    let llm = LlmStats {
        connected: false,
        ..Default::default()
    };
    let text = render_llm_panel(&llm, 90, 8);
    assert!(
        text.contains("check host and port"),
        "an empty sample must give an actionable hint:\n{text}"
    );
    assert!(!text.contains("connection lost"), "{text}");
}

#[test]
fn unknown_metrics_and_slots_stay_dash_at_all_target_layouts() {
    // A reachable server with neither /metrics nor /slots has unknown
    // values: they must render `—`, never a fabricated 0, at every target
    // size (54x16, 72x22, large).
    for (width, height) in [(54u16, 16u16), (72, 22), (120, 40)] {
        let llm = LlmStats {
            connected: true,
            metrics_available: false,
            model: "m".to_string(),
            ..Default::default()
        };
        let text = render_llm_panel(&llm, width, height);
        assert!(
            text.contains("UNAVAILABLE"),
            "metrics-unavailable panel missing at {width}x{height}:\n{text}"
        );
        assert!(
            !text.contains("0/0"),
            "no fabricated slot counts at {width}x{height}:\n{text}"
        );
        assert!(
            text.contains('—'),
            "unknown placeholder missing at {width}x{height}:\n{text}"
        );
    }
}

#[test]
fn compact_hint_fits_at_the_hard_floor_with_a_five_digit_refresh() {
    let llm = LlmStats {
        connected: true,
        ..Default::default()
    };
    let mut state = UiState::default();
    let text = render_dashboard(54, 16, 10_000, &llm, &mut state);
    assert!(
        text.contains("10000ms"),
        "refresh interval missing:\n{text}"
    );
    assert!(
        text.contains("esc quit"),
        "quit hint must still fit at 54 wide:\n{text}"
    );
}

#[test]
fn uses_full_layout_matches_the_compact_thresholds() {
    assert!(!uses_full_layout(54, 16));
    assert!(!uses_full_layout(71, 21));
    assert!(!uses_full_layout(100, 21));
    assert!(!uses_full_layout(71, 40));
    assert!(uses_full_layout(72, 22));
    assert!(uses_full_layout(120, 40));
}

#[test]
fn settings_popup_lists_servers_redacted_with_monitoring_state() {
    let mut settings = settings_for("http://admin:primarypw@127.0.0.1:8080/?key=primarykey");
    settings.servers = (0..16)
        .map(|i| format!("http://user:pw{i}secret@10.0.0.{i}:9090/v1?token=tok{i}"))
        .collect();
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    focus_server_list(&mut state);
    // Highlight the last entry: the window must follow it.
    for _ in 0..15 {
        press(&mut state, &[KeyCode::Down]);
    }
    let screen = render_settings(&state, 100, 40).join("\n");
    assert!(screen.contains("SERVER LIST"), "{screen}");
    assert!(
        screen.contains("16 additional · monitored 16/16"),
        "{screen}"
    );
    assert!(screen.contains("1 beyond the 16-server limit"), "{screen}");
    assert!(screen.contains("10.0.0.15:9090"), "{screen}");
    assert!(screen.contains("not monitored"), "{screen}");
    for (index, secret) in [
        "secret",
        "primarypw",
        "primarykey",
        "tok1",
        "admin",
        "user:",
    ]
    .into_iter()
    .enumerate()
    {
        // Name the probe by index only so no secret value reaches the output.
        assert!(!screen.contains(secret), "synthetic secret #{index} leaked");
    }
}

#[test]
fn settings_popup_masks_secrets_in_the_server_draft() {
    let mut settings = settings_for("http://127.0.0.1:8080");
    settings.servers = vec!["https://bob:hunter2@llm.invalid:8443/v1?api_key=sk-123".to_string()];
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    focus_server_list(&mut state);
    press(&mut state, &[KeyCode::Char('e')]);
    let screen = render_settings(&state, 80, 24).join("\n");
    assert!(screen.contains("llm.invalid:8443"), "{screen}");
    for (index, secret) in ["bob", "hunter2", "sk-123"].into_iter().enumerate() {
        // Name the probe by index only so no secret value reaches the output.
        assert!(!screen.contains(secret), "synthetic secret #{index} leaked");
    }
    // A rejected entry reports a redacted reason.
    press(&mut state, &[KeyCode::Esc, KeyCode::Char('a')]);
    // Same target, other credentials: a duplicate (userinfo is not identity).
    type_keys(
        &mut state,
        "https://eve:pw@llm.invalid:8443/v1?api_key=sk-123",
    );
    press(&mut state, &[KeyCode::Enter]);
    let screen = render_settings(&state, 80, 24).join("\n");
    assert!(screen.contains("already configured"), "{screen}");
    for (index, secret) in ["eve", "hunter2", "sk-123"].into_iter().enumerate() {
        // Name the probe by index only so no secret value reaches the output.
        assert!(!screen.contains(secret), "synthetic secret #{index} leaked");
    }
}

#[test]
fn settings_popup_fits_small_terminals_with_a_long_server_list() {
    let mut settings = settings_for("http://127.0.0.1:8080");
    settings.servers = (0..20).map(|i| format!("http://10.0.0.{i}:9090")).collect();
    let mut state = UiState::default();
    state.open_settings("http://127.0.0.1:8080", &settings);
    focus_server_list(&mut state);
    for _ in 0..19 {
        press(&mut state, &[KeyCode::Down]);
    }
    // Smallest supported size (60x19) up to a roomy one: the fields, the
    // highlighted entry and the list hint must always be visible, and the
    // popup must stay inside the terminal.
    for (width, height) in [(60, 19), (60, 22), (80, 24), (120, 50)] {
        let rows = render_settings(&state, width, height);
        let screen = rows.join("\n");
        assert!(screen.contains("SETTINGS"), "{width}x{height}:\n{screen}");
        assert!(
            screen.contains("Auto discovery"),
            "{width}x{height}:\n{screen}"
        );
        assert!(
            screen.contains("10.0.0.19:9090"),
            "{width}x{height}:\n{screen}"
        );
        assert!(screen.contains("a add"), "{width}x{height}:\n{screen}");
        assert!(
            screen.contains("20 additional"),
            "{width}x{height}:\n{screen}"
        );
        assert!(
            rows.iter().filter(|row| row.contains('└')).count() == 1,
            "bottom border must be drawn once at {width}x{height}:\n{screen}"
        );
    }
    // With a draft and an error both visible at the minimum size.
    press(&mut state, &[KeyCode::Char('a')]);
    type_keys(&mut state, "http://10.0.0.3:9090/");
    press(&mut state, &[KeyCode::Enter]);
    let screen = render_settings(&state, 60, 19).join("\n");
    assert!(screen.contains("already configured"), "{screen}");
    assert!(screen.contains("10.0.0.3:9090/_"), "{screen}");
    // Below the minimum the compact notice is shown instead.
    let screen = render_settings(&state, 59, 18).join("\n");
    assert!(screen.contains("too small"), "{screen}");
}

#[test]
fn trend_plot_keeps_requested_dimensions() {
    let now = Instant::now();
    let history = VecDeque::from([
        TimedSample {
            at: now - Duration::from_secs(30),
            value: 25.0,
        },
        TimedSample {
            at: now,
            value: 100.0,
        },
    ]);
    let lines = trend_lines(&history, 5, 3, ORK_GREEN);
    let rendered = lines.iter().map(|line| line.width()).collect::<Vec<_>>();
    assert_eq!(rendered, vec![5, 5, 5]);
}

#[test]
fn history_resampling_holds_the_latest_value_between_samples() {
    let now = Instant::now();
    let history = VecDeque::from([
        TimedSample {
            at: now - Duration::from_secs(30),
            value: 25.0,
        },
        TimedSample {
            at: now - Duration::from_secs(10),
            value: 50.0,
        },
    ]);

    let columns = resample_history(&history, 7, now);
    assert_eq!(
        columns,
        vec![
            None,
            None,
            None,
            Some(25.0),
            Some(25.0),
            Some(50.0),
            Some(50.0),
        ]
    );
}

#[test]
fn braille_masks_map_to_expected_cells() {
    assert_eq!(braille_char(0), ' ');
    assert_eq!(braille_char(0xc0), '⣀');
    assert_eq!(braille_char(0xff), '⣿');
}

#[test]
fn fine_bar_has_subcell_resolution() {
    assert_eq!(fine_bar(0.0, 4), ("".to_string(), "⣀⣀⣀⣀".to_string()));
    assert_eq!(
        fine_bar(10.0, 10),
        ("⣿".to_string(), "⣀⣀⣀⣀⣀⣀⣀⣀⣀".to_string())
    );
    assert_eq!(fine_bar(25.0, 2), ("⣇".to_string(), "⣀".to_string()));
    assert_eq!(fine_bar(50.0, 2), ("⣿".to_string(), "⣀".to_string()));
    assert_eq!(fine_bar(100.0, 2), ("⣿⣿".to_string(), "".to_string()));
}

#[test]
fn bar_gradients_use_expected_endpoints() {
    assert_eq!(bar_gradient_color("GPU", 100.0, WHITE), ORK_GREEN);
    assert_eq!(bar_gradient_color("VRAM", 90.0, WHITE), CYAN);
    assert_eq!(bar_gradient_color("VRAM", 100.0, WHITE), RED);
    assert_eq!(bar_gradient_color("PWR", 100.0, WHITE), RED);
    assert_eq!(bar_gradient_color("CTX", 100.0, WHITE), RED);
    assert_eq!(bar_gradient_color("OTHER", 50.0, CYAN), CYAN);
}

#[test]
fn token_counts_are_grouped_for_readability() {
    assert_eq!(grouped_u64(999), "999");
    assert_eq!(grouped_u64(1_000), "1,000");
    assert_eq!(grouped_u64(196_608), "196,608");
    assert_eq!(grouped_f64(447_924.0), "447,924");
}

#[test]
fn gradient_bar_preserves_terminal_width() {
    let spans = fine_bar_spans("GPU", 50.0, 12, ORK_GREEN);
    assert_eq!(Line::from(spans).width(), 12);
}

#[test]
fn nan_percent_is_safely_clamped() {
    assert_eq!(clamp_percent(f64::NAN), 0.0);
}

#[test]
fn io_row_renders_meter_or_dash_placeholder_at_fixed_width() {
    let base = GpuStats {
        available: true,
        index: 0,
        name: "Test".to_string(),
        ..Default::default()
    };
    let with_link = GpuStats {
        pcie_rx_mb_s: Some(15753.6),
        pcie_tx_mb_s: Some(15753.6),
        pcie_link_speed_gts: Some(16.0),
        pcie_link_width: Some(16),
        ..base.clone()
    };
    let without_link = GpuStats { ..base };

    let row = |gpu: &GpuStats| match gpu::pcie_utilization_pct(
        gpu.pcie_rx_mb_s,
        gpu.pcie_tx_mb_s,
        gpu.pcie_link_speed_gts,
        gpu.pcie_link_width,
    ) {
        Some(pct) => meter_line(
            "I/O",
            pct,
            14,
            CYAN,
            format!("{:>3.0}%", pct),
            vec![
                fixed_data_pair(
                    "MEMCTRL",
                    optional_number(gpu.memory_utilization, 0, "%"),
                    CYAN,
                    17,
                ),
                fixed_data_pair(
                    "PCIe RX",
                    optional_number(gpu.pcie_rx_mb_s, 1, " MB/s"),
                    CYAN,
                    22,
                ),
                fixed_data_pair(
                    "TX",
                    optional_number(gpu.pcie_tx_mb_s, 1, " MB/s"),
                    CYAN,
                    18,
                ),
            ],
        ),
        None => Line::from(vec![
            label_span(" I/O    "),
            Span::styled(
                format!("{:<width$}", " \u{2014}", width = 14 + 11),
                Style::default().fg(MUTED),
            ),
            fixed_data_pair(
                "MEMCTRL",
                optional_number(gpu.memory_utilization, 0, "%"),
                CYAN,
                17,
            ),
            fixed_data_pair(
                "PCIe RX",
                optional_number(gpu.pcie_rx_mb_s, 1, " MB/s"),
                CYAN,
                22,
            ),
            fixed_data_pair(
                "TX",
                optional_number(gpu.pcie_tx_mb_s, 1, " MB/s"),
                CYAN,
                18,
            ),
        ]),
    };
    assert_eq!(row(&with_link).width(), row(&without_link).width());
}

#[test]
fn enc_dec_idle_detects_missing_or_sub_percent_utilization() {
    let gpu = |enc: Option<f64>, dec: Option<f64>| GpuStats {
        encoder_utilization: enc,
        decoder_utilization: dec,
        ..GpuStats::default()
    };

    assert!(enc_dec_idle(&gpu(None, None)));
    assert!(enc_dec_idle(&gpu(Some(0.0), Some(0.0))));
    assert!(enc_dec_idle(&gpu(Some(0.5), None)));
    assert!(!enc_dec_idle(&gpu(Some(1.0), None)));
    assert!(!enc_dec_idle(&gpu(None, Some(5.0))));
}

#[test]
fn small_hybrid_box_keeps_the_full_view_with_the_minibar() {
    // Regression: a 4P+4E Core Ultra (the 288V) allocated
    // `system_panel_height` = 14 → inner 12, which the old hard-coded
    // `inner.height < 15` gate rejected, collapsing the panel to the
    // 4-line compact view and hiding the P/E minibar entirely. The 8P+8E
    // box (12900K) survived because inner 16 >= 15.
    let small = hybrid_system(4, 4);
    assert_eq!(system_panel_height(&small), 14);
    assert!(
        system_full_view(35, 14 - 2),
        "4P+4E inner height 12 must be the full view"
    );

    let wide = hybrid_system(8, 8);
    assert_eq!(system_panel_height(&wide), 18);
    assert!(
        system_full_view(35, 18 - 2),
        "8P+8E inner height 16 must stay the full view"
    );

    // Homogeneous boxes (panel 13 → inner 11) render the full view with
    // the per-core heatmap too.
    let homo = SystemStats::default();
    assert_eq!(system_panel_height(&homo), 13);
    assert!(system_full_view(35, 13 - 2));

    // Below the full-view floors the compact fallback wins: the minibar
    // needs the 35-column floor, and 8 rows cannot hold the fixed rows
    // plus a core row.
    assert!(!system_full_view(34, 20));
    assert!(!system_full_view(60, 8));
}

#[test]
fn rendered_system_panel_shows_the_pe_minibar_on_a_small_hybrid_box() {
    // End-to-end regression for the 288V report: paint the real SYSTEM
    // panel at the size the layout allocates for a 4P+4E box (panel
    // height 14, inner 12) and assert the P/E minibar renders — it used
    // to fall through to the compact 4-line view.
    let system = hybrid_system(4, 4);
    let area = Rect::new(0, 0, 70, system_panel_height(&system));
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_system(frame, area, &system))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("P-CORES") && text.contains("E-CORES"),
        "P/E minibar header must render, got:\n{text}",
    );
}

#[test]
fn system_panel_renders_ram_unavailable_when_total_is_zero() {
    // Regression for review finding C3: with `memory_total_bytes == 0`
    // the SYSTEM panel must render the RAM meter as unavailable ("—")
    // like VRAM/POWER — never a fabricated 0.0 % / 0.0 GiB.
    let system = SystemStats {
        memory_used_bytes: 16 * 1024 * 1024 * 1024,
        memory_total_bytes: 0,
        ..Default::default()
    };

    // Full view (panel height 13, width 70).
    let rows = render_system_rows(&system, 70, 13);
    let ram_index = rows
        .iter()
        .position(|row| row.trim_start().starts_with("RAM"))
        .expect("RAM meter row must render");
    let ram_row = &rows[ram_index];
    assert!(
        ram_row.contains('—'),
        "RAM meter must render unavailable, got: {ram_row:?}"
    );
    assert!(
        !ram_row.contains("0.0%"),
        "RAM meter must not fabricate a 0.0 % value, got: {ram_row:?}"
    );
    // The used/total GiB pair row directly below the meter is
    // unavailable as well.
    let pair_row = rows
        .get(ram_index + 1)
        .expect("used/total GiB pair row must render");
    assert_eq!(
        pair_row, "—",
        "used/total GiB pair must be unavailable, got: {pair_row:?}"
    );

    // Compact fallback (below the full-view height floor) shares the
    // same validity semantics.
    let rows = render_system_rows(&system, 70, 8);
    let ram_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("RAM"))
        .expect("RAM meter row must render in compact view");
    assert!(
        ram_row.contains('—'),
        "compact RAM meter must render unavailable, got: {ram_row:?}"
    );
    assert!(
        !ram_row.contains("0.0%"),
        "compact RAM meter must not fabricate a 0.0 % value, got: {ram_row:?}"
    );
    let used_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("USED"))
        .expect("USED row must render in compact view");
    assert!(
        used_row.contains('—') && !used_row.contains("GiB"),
        "compact USED row must be unavailable, got: {used_row:?}"
    );
    let total_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("TOTAL"))
        .expect("TOTAL row must render in compact view");
    assert!(
        total_row.contains('—') && !total_row.contains("GiB"),
        "compact TOTAL row must be unavailable, got: {total_row:?}"
    );
}

#[test]
fn system_panel_renders_ram_meter_when_total_is_valid() {
    // Existing behavior must be preserved: a valid total renders the
    // real percentage and GiB pair.
    let system = SystemStats {
        memory_used_bytes: 16 * 1024 * 1024 * 1024,
        memory_total_bytes: 64 * 1024 * 1024 * 1024,
        ..Default::default()
    };

    let rows = render_system_rows(&system, 70, 13);
    let ram_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("RAM"))
        .expect("RAM meter row must render");
    assert!(
        ram_row.contains("25.0%"),
        "RAM meter must show 25.0 %, got: {ram_row:?}"
    );
    assert!(
        !ram_row.contains('—'),
        "RAM meter must not render unavailable for a valid total, got: {ram_row:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("16.0 / 64.0 GiB")),
        "full view must show the used/total GiB pair, got: {rows:?}"
    );
}

#[test]
fn system_panel_renders_load_average_unavailable_without_a_reading() {
    // Regression: an unreadable /proc/loadavg must render `—`, not three
    // fabricated zeros. A real zero stays a number.
    let unknown = SystemStats::default();
    let rows = render_system_rows(&unknown, 70, 13);
    let load_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("LOAD"))
        .expect("LOAD row must render");
    assert!(
        load_row.contains('—'),
        "unknown load must render —: {load_row:?}"
    );
    assert!(
        !load_row.contains("0.00"),
        "unknown load must not fabricate zeros: {load_row:?}"
    );

    let zero = SystemStats {
        load_one: Some(0.0),
        load_five: Some(0.0),
        load_fifteen: Some(0.0),
        ..Default::default()
    };
    let rows = render_system_rows(&zero, 70, 13);
    let load_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("LOAD"))
        .expect("LOAD row must render");
    assert!(
        load_row.contains("0.00 / 0.00 / 0.00"),
        "a real zero load must stay a number: {load_row:?}"
    );

    let known = SystemStats {
        load_one: Some(1.25),
        load_five: Some(0.75),
        load_fifteen: Some(0.5),
        ..Default::default()
    };
    let rows = render_system_rows(&known, 70, 13);
    let load_row = rows
        .iter()
        .find(|row| row.trim_start().starts_with("LOAD"))
        .expect("LOAD row must render");
    assert!(
        load_row.contains("1.25 / 0.75 / 0.50"),
        "known loads must render as read: {load_row:?}"
    );
}

#[test]
fn ram_history_skips_samples_without_total() {
    // The history sparkline must follow the same validity semantics:
    // no total, no sample — never a fabricated zero.
    let mut state = UiState::default();
    let gpu = GpuStats::default();
    let mut system = SystemStats::default();

    state.push_sample(&gpu, &system);
    assert!(state.ram_history.is_empty());

    system.memory_used_bytes = 16 * 1024 * 1024 * 1024;
    system.memory_total_bytes = 64 * 1024 * 1024 * 1024;
    state.push_sample(&gpu, &system);
    assert_eq!(state.ram_history.len(), 1);
    assert!((state.ram_history[0].value - 25.0).abs() < f64::EPSILON);
}

#[test]
fn gpu_in_mapping_matches_only_the_mapped_device() {
    let gpu = GpuStats {
        device: DeviceId::new(Some("0000:01:00.0".into()), None),
        ..Default::default()
    };

    let mapped = MappedGpu {
        device: gpu.device.clone(),
        name: "card0".to_string(),
        vendor: GpuVendor::Nvidia,
        evidence: GpuEvidence::NvmlCompute,
    };
    let other = MappedGpu {
        device: DeviceId::new(Some("0000:02:00.0".into()), None),
        ..mapped.clone()
    };

    assert!(gpu_in_mapping(&gpu, &GpuMapping::Single(mapped.clone())));
    assert!(gpu_in_mapping(
        &gpu,
        &GpuMapping::Multi(vec![other.clone(), mapped])
    ));
    assert!(!gpu_in_mapping(&gpu, &GpuMapping::Single(other)));
    assert!(!gpu_in_mapping(&gpu, &GpuMapping::None));
    assert!(!gpu_in_mapping(&gpu, &GpuMapping::Unknown));

    // A GPU without a stable key never matches — even against a mapping
    // entry that is itself unkeyed.
    let unkeyed = GpuStats::default();
    let unkeyed_map = MappedGpu {
        device: DeviceId::default(),
        name: "card1".to_string(),
        vendor: GpuVendor::Amd,
        evidence: GpuEvidence::RenderNodeFd,
    };
    assert!(!gpu_in_mapping(&unkeyed, &GpuMapping::Single(unkeyed_map)));
}

#[test]
fn gpu_in_mapping_matches_mig_parent_and_child() {
    let parent_uuid = "GPU-1a2b3c4d-5e6f-7890-abcd-ef0123456789";
    let parent = GpuStats {
        device: DeviceId::new(Some("0000:41:00.0".into()), Some(parent_uuid.into())),
        ..Default::default()
    };
    let child = GpuStats {
        device: DeviceId::new(None, Some(format!("MIG-{parent_uuid}/1/0"))),
        ..Default::default()
    };
    let parent_mapped = MappedGpu {
        device: parent.device.clone(),
        name: String::new(),
        vendor: GpuVendor::Nvidia,
        evidence: GpuEvidence::NvmlCompute,
    };
    let child_mapped = MappedGpu {
        device: child.device.clone(),
        ..parent_mapped.clone()
    };

    // Server attributed to the physical parent: chip on the child panel.
    assert!(gpu_in_mapping(
        &child,
        &GpuMapping::Single(parent_mapped.clone())
    ));
    // Server attributed to the child: chip on the parent panel.
    assert!(gpu_in_mapping(
        &parent,
        &GpuMapping::Single(child_mapped.clone())
    ));
    // The same child matches directly as well.
    assert!(gpu_in_mapping(
        &child,
        &GpuMapping::Single(child_mapped.clone())
    ));
    // A child of a different physical GPU never matches.
    let foreign = GpuStats {
        device: DeviceId::new(None, Some("MIG-GPU-other/1/0".into())),
        ..Default::default()
    };
    assert!(!gpu_in_mapping(&foreign, &GpuMapping::Single(child_mapped)));
}

#[test]
fn gpu_attribution_label_shows_single_key_and_visible_multi_count() {
    let mapped = MappedGpu {
        device: DeviceId::new(Some("0000:01:00.0".into()), None),
        name: "card0".to_string(),
        vendor: GpuVendor::Nvidia,
        evidence: GpuEvidence::NvmlCompute,
    };
    let other = MappedGpu {
        device: DeviceId::new(Some("0000:02:00.0".into()), None),
        ..mapped.clone()
    };

    assert_eq!(gpu_attribution_label(&GpuMapping::None), None);
    assert_eq!(gpu_attribution_label(&GpuMapping::Unknown), None);
    assert_eq!(
        gpu_attribution_label(&GpuMapping::Single(mapped.clone())),
        Some("0000:01:00.0".to_string())
    );

    // Multi must not silently look like a single GPU: the count is visible.
    let label = gpu_attribution_label(&GpuMapping::Multi(vec![mapped, other])).unwrap();
    assert!(
        label.contains("+1"),
        "multi attribution must show the GPU count"
    );
}

#[test]
fn gpu_panel_title_shows_the_llm_chip_when_mapped() {
    let gpu = GpuStats {
        available: true,
        name: "RTX 4090".to_string(),
        device: DeviceId::new(Some("0000:01:00.0".into()), None),
        ..Default::default()
    };
    let mapped = MappedGpu {
        device: gpu.device.clone(),
        name: "card0".to_string(),
        vendor: GpuVendor::Nvidia,
        evidence: GpuEvidence::NvmlCompute,
    };
    let area = Rect::new(0, 0, 40, 7);

    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_gpu(frame, area, &gpu, &GpuMapping::Single(mapped)))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("LLM"),
        "mapped GPU title must carry the LLM chip, got:\n{text}",
    );

    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_gpu(frame, area, &gpu, &GpuMapping::None))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !text.contains("LLM"),
        "unmapped GPU title must not carry the LLM chip, got:\n{text}",
    );
}

#[test]
fn gpu_panel_renders_pwr_unavailable_without_power_telemetry() {
    // Regression: the PWR meter was built unconditionally from
    // `power_pct.unwrap_or(0.0)`, so a driver that exposes no power
    // telemetry (e.g. the Intel xe iGPU) rendered an empty bar with
    // "0%" — indistinguishable from a real 0 W reading. It must render
    // unavailable ("—") like the GPU/VRAM rows do.
    let gpu = GpuStats {
        available: true,
        name: "Intel Arc (Core Ultra 200V iGPU)".to_string(),
        utilization: Some(12.0),
        ..Default::default()
    };
    let text = render_gpu_panel(&gpu, 80, 8);
    assert!(
        text.contains("PWR"),
        "PWR row must still render, got:\n{text}"
    );
    assert!(
        !text.contains("0%"),
        "PWR must not fabricate a 0% reading, got:\n{text}"
    );
    assert!(
        text.matches('—').count() >= 3,
        "PWR/VRAM/DRAW must all render unavailable, got:\n{text}"
    );
}

#[test]
fn gpu_panel_renders_pwr_meter_when_power_is_reported() {
    // Guard the other direction: with real power telemetry the meter
    // still renders a percentage instead of degrading to "—".
    let gpu = GpuStats {
        available: true,
        name: "RTX 4090".to_string(),
        power_w: Some(150.0),
        power_limit_w: Some(450.0),
        ..Default::default()
    };
    let text = render_gpu_panel(&gpu, 80, 8);
    assert!(
        text.contains("PWR") && text.contains("33%"),
        "PWR meter must render the real percentage, got:\n{text}"
    );
}

#[test]
fn footer_marks_a_reachable_metrics_failure_as_metrics_unreachable() {
    let llm = LlmStats {
        connected: true,
        metrics_available: false,
        error: "cannot reach llama.cpp: connection refused".to_string(),
        ..Default::default()
    };
    let status = footer_status(&llm, &GpuStats::default());
    assert!(status.contains("METRICS UNREACHABLE"), "got: {status}");
    assert!(!status.contains("SERVER OFFLINE"), "got: {status}");
}

#[test]
fn footer_renders_drop_warning_only_after_a_dropped_snapshot() {
    let area = Rect::new(0, 0, 100, 3);
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut state = UiState::default();

    terminal
        .draw(|frame| {
            draw_footer(
                frame,
                area,
                &LlmStats::default(),
                &GpuStats::default(),
                &state,
            );
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !text.contains("DROPS"),
        "healthy footer must stay clean:\n{text}"
    );

    state.set_snapshot_drops(2, 1);
    terminal
        .draw(|frame| {
            draw_footer(
                frame,
                area,
                &LlmStats::default(),
                &GpuStats::default(),
                &state,
            );
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("DROPS f2 l1"),
        "drop counters must be visible in the footer:\n{text}"
    );
}

#[test]
fn footer_hides_drop_counts_until_a_snapshot_is_dropped() {
    let mut state = UiState::default();
    assert_eq!(state.snapshot_drop_text(), None);

    state.set_snapshot_drops(2, 1);
    assert_eq!(state.snapshot_drop_text().as_deref(), Some("DROPS f2 l1"));

    state.set_snapshot_drops(0, 0);
    assert_eq!(state.snapshot_drop_text(), None);
}

#[test]
fn footer_still_reports_an_unreachable_server_as_offline() {
    let llm = LlmStats {
        connected: false,
        metrics_available: false,
        error: "cannot reach llama.cpp: connection refused".to_string(),
        ..Default::default()
    };
    let status = footer_status(&llm, &GpuStats::default());
    assert!(status.contains("SERVER OFFLINE"), "got: {status}");
}

fn render_settings(state: &UiState, width: u16, height: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_settings_popup(frame, frame.area(), state))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn hybrid_system(perf: usize, eff: usize) -> SystemStats {
    let mut core_kinds = Vec::new();
    let mut physical_core_groups = Vec::new();
    for i in 0..perf {
        core_kinds.push(CpuCoreKind::Performance);
        physical_core_groups.push(CpuPhysicalCore {
            kind: CpuCoreKind::Performance,
            logical_cpus: vec![i],
        });
    }
    for i in 0..eff {
        core_kinds.push(CpuCoreKind::Efficiency);
        physical_core_groups.push(CpuPhysicalCore {
            kind: CpuCoreKind::Efficiency,
            logical_cpus: vec![perf + i],
        });
    }
    SystemStats {
        cpu_topology: CpuTopology {
            core_kinds,
            physical_core_groups,
            ..CpuTopology::default()
        },
        ..SystemStats::default()
    }
}

fn render_system_rows(system: &SystemStats, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_system(frame, area, system))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let raw: String = (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            raw.trim_start_matches('│')
                .trim_end_matches('│')
                .trim()
                .to_string()
        })
        .collect()
}

fn render_gpu_panel(gpu: &GpuStats, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_gpu(frame, area, gpu, &GpuMapping::None))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}
