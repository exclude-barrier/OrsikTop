//! Process-table rendering and interaction tests (sort, groups, pins, search, clicks, keyboard).
use super::super::*;
use super::render_dashboard;

#[test]
fn process_header_marks_only_the_active_sort_without_brackets() {
    let line = process_header_wide(100, ProcessSortKey::Cpu, true);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("CPU↓"), "{text}");
    assert!(!text.contains('['), "{text}");
}

#[test]
fn process_group_count_moves_out_of_the_pid_cell() {
    let line = process_group_line_wide(
        "llama-server",
        30,
        12.0,
        1024,
        0,
        Some(3),
        false,
        false,
        100,
    );
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("×30"), "{text}");
    assert_eq!(line.spans[0].content.trim(), "—", "{text}");
}

#[test]
fn same_named_processes_are_grouped_with_aggregate_metrics() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "qemu-system-x86".to_string(),
            cpu_pct: 120.0,
            memory_bytes: 2_000,
            threads: Some(8),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "qemu-system-x86".to_string(),
            cpu_pct: 80.0,
            memory_bytes: 3_000,
            threads: Some(7),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 30,
            program: "llama".to_string(),
            cpu_pct: 50.0,
            ..ProcessStats::default()
        },
    ];
    let expanded = HashSet::new();
    let (_, rows) = grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);

    assert_eq!(rows.len(), 2);
    match &rows[0] {
        ProcessDisplayRow::Group {
            program,
            count,
            cpu_pct,
            memory_bytes,
            gpu_bytes,
            threads,
            expanded,
        } => {
            assert_eq!(*program, "qemu-system-x86");
            assert_eq!(*count, 2);
            assert_eq!(*cpu_pct, 200.0);
            assert_eq!(*memory_bytes, 5_000);
            assert_eq!(*gpu_bytes, 0);
            assert_eq!(*threads, Some(15));
            assert!(!expanded);
        }
        _ => panic!("expected grouped qemu row"),
    }
}

#[test]
fn group_thread_total_is_unknown_when_any_member_is_unknown() {
    // A partial sum would present an unknown member as a verified total,
    // so the aggregate stays unknown until every member is readable.
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "qemu-system-x86".to_string(),
            threads: Some(4),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "qemu-system-x86".to_string(),
            threads: None,
            ..ProcessStats::default()
        },
    ];
    let expanded = HashSet::new();
    let (_, rows) = grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);
    match &rows[0] {
        ProcessDisplayRow::Group { threads, .. } => assert_eq!(*threads, None),
        _ => panic!("expected grouped row"),
    }
}

#[test]
fn thread_cell_renders_unknown_as_dash() {
    assert_eq!(thread_cell(Some(12)), "12");
    assert_eq!(thread_cell(None), "—");
}

#[test]
fn expanded_group_exposes_individual_process_rows() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            cpu_pct: 20.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "chromium".to_string(),
            cpu_pct: 10.0,
            ..ProcessStats::default()
        },
    ];
    let expanded = HashSet::from(["chromium".to_string()]);
    let (_, rows) = grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);

    assert_eq!(rows.len(), 3);
    assert!(matches!(
        rows[0],
        ProcessDisplayRow::Group { expanded: true, .. }
    ));
    assert!(matches!(
        rows[1],
        ProcessDisplayRow::Process { child: true, .. }
    ));
    assert!(matches!(
        rows[2],
        ProcessDisplayRow::Process { child: true, .. }
    ));
}

#[test]
fn process_search_matches_program_command_and_pid() {
    let process = ProcessStats {
        pid: 4242,
        program: "llama".to_string(),
        command: "/usr/local/bin/llama-server --model Qwen".to_string(),
        ..ProcessStats::default()
    };
    assert!(process_matches_search(&process, "LLAMA"));
    assert!(process_matches_search(&process, "qwen"));
    assert!(process_matches_search(&process, "424"));
    assert!(!process_matches_search(&process, "chromium"));
}

#[test]
fn filtered_grouping_only_keeps_matching_processes() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "chromium".to_string(),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 30,
            program: "llama".to_string(),
            command: "/usr/bin/llama-server".to_string(),
            ..ProcessStats::default()
        },
    ];
    let (_, rows) = grouped_process_rows_filtered(
        &processes,
        ProcessSortKey::Cpu,
        true,
        None,
        &HashSet::new(),
        Some("llama"),
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0],
        ProcessDisplayRow::Process { process, .. } if process.pid == 30
    ));
}

#[test]
fn keyboard_navigation_respects_process_search() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "llama".to_string(),
            ..ProcessStats::default()
        },
    ];
    let mut state = UiState {
        process_search_query: "llama".to_string(),
        ..UiState::default()
    };
    state.process_home(&processes);
    assert_eq!(
        state.process_selected_pid,
        Some(ProcessIdentity::new(20, 0))
    );
}

#[test]
fn right_click_toggles_group_expansion() {
    let mut state = UiState {
        process_rows: Some(ProcessRowsHit {
            rect: Rect::new(10, 10, 40, 1),
            targets: vec![ProcessRowTarget::Group("chromium".to_string())],
        }),
        ..UiState::default()
    };

    assert!(state.right_click_process_group(12, 10));
    assert!(state.expanded_process_groups.contains("chromium"));
    assert!(state.right_click_process_group(12, 10));
    assert!(!state.expanded_process_groups.contains("chromium"));
}

#[test]
fn pinned_process_is_removed_from_sorted_stream() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            cpu_pct: 90.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            cpu_pct: 40.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 30,
            cpu_pct: 70.0,
            ..ProcessStats::default()
        },
    ];

    let (pinned, rest) = sorted_processes_with_pin(
        &processes,
        ProcessSortKey::Cpu,
        true,
        Some(ProcessIdentity::new(30, 0)),
    );
    assert_eq!(pinned.map(|process| process.pid), Some(30));
    assert_eq!(
        rest.iter().map(|process| process.pid).collect::<Vec<_>>(),
        vec![10, 20]
    );
}

#[test]
fn single_click_selects_group_without_pinning() {
    let mut state = group_click_test_state();

    assert!(state.click_process_row(12, 10));
    assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
    assert_eq!(state.process_pinned_group, None);
}

#[test]
fn double_click_pins_group() {
    let mut state = group_click_test_state();

    assert!(state.click_process_row(12, 10));
    assert!(state.click_process_row(12, 10));
    assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
    assert_eq!(state.process_pinned_group.as_deref(), Some("chromium"));
}

#[test]
fn pinned_group_is_taken_with_expanded_children() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            cpu_pct: 20.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "chromium".to_string(),
            cpu_pct: 10.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 30,
            program: "llama".to_string(),
            cpu_pct: 5.0,
            ..ProcessStats::default()
        },
    ];
    let expanded = HashSet::from(["chromium".to_string()]);
    let (_, mut rows) =
        grouped_process_rows(&processes, ProcessSortKey::Cpu, true, None, &expanded);
    let pinned = take_pinned_group_rows(&mut rows, Some("chromium"));

    assert_eq!(pinned.len(), 3);
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        pinned[0],
        ProcessDisplayRow::Group {
            program: "chromium",
            ..
        }
    ));
}

#[test]
fn single_click_selects_process_without_pinning() {
    let mut state = process_click_test_state();

    assert!(state.click_process_row(12, 10));
    assert_eq!(
        state.process_selected_pid,
        Some(ProcessIdentity::new(10, 0))
    );
    assert_eq!(state.process_pinned_pid, None);
}

#[test]
fn double_click_pins_process() {
    let mut state = process_click_test_state();

    assert!(state.click_process_row(12, 10));
    assert!(state.click_process_row(12, 10));
    assert_eq!(
        state.process_selected_pid,
        Some(ProcessIdentity::new(10, 0))
    );
    assert_eq!(state.process_pinned_pid, Some(ProcessIdentity::new(10, 0)));
}

#[test]
fn clicking_another_process_releases_pin_and_selects_new_process() {
    let mut state = process_click_test_state();

    assert!(state.click_process_row(12, 10));
    assert!(state.click_process_row(12, 10));
    assert_eq!(state.process_pinned_pid, Some(ProcessIdentity::new(10, 0)));

    assert!(state.click_process_row(12, 11));
    assert_eq!(
        state.process_selected_pid,
        Some(ProcessIdentity::new(20, 0))
    );
    assert_eq!(state.process_pinned_pid, None);
}

#[test]
fn keyboard_navigation_treats_collapsed_group_as_one_row() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            cpu_pct: 30.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "chromium".to_string(),
            cpu_pct: 20.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 30,
            program: "llama".to_string(),
            cpu_pct: 10.0,
            ..ProcessStats::default()
        },
    ];
    let mut state = UiState::default();

    state.process_home(&processes);
    assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
    assert_eq!(state.process_selected_pid, None);

    state.move_process_selection(1, &processes);
    assert_eq!(
        state.process_selected_pid,
        Some(ProcessIdentity::new(30, 0))
    );
    assert_eq!(state.process_selected_group, None);
}

#[test]
fn keyboard_navigation_enters_children_only_when_group_is_expanded() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            cpu_pct: 30.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "chromium".to_string(),
            cpu_pct: 20.0,
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 30,
            program: "llama".to_string(),
            cpu_pct: 10.0,
            ..ProcessStats::default()
        },
    ];
    let mut state = UiState {
        expanded_process_groups: HashSet::from(["chromium".to_string()]),
        ..UiState::default()
    };

    state.process_home(&processes);
    assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));

    state.move_process_selection(1, &processes);
    assert!(matches!(
        state.process_selected_pid,
        Some(ProcessIdentity { pid: 10 | 20, .. })
    ));
    assert_eq!(state.process_selected_group, None);
}

#[test]
fn keyboard_navigation_scrolls_selected_row_into_view() {
    let processes = (1..=8)
        .map(|pid| ProcessStats {
            pid,
            program: format!("proc-{pid}"),
            ..ProcessStats::default()
        })
        .collect::<Vec<_>>();
    let mut state = UiState {
        process_sort_key: ProcessSortKey::Pid,
        process_sort_desc: false,
        process_scroll_visible: 3,
        ..UiState::default()
    };

    state.process_home(&processes);
    for _ in 0..4 {
        state.move_process_selection(1, &processes);
    }

    assert_eq!(state.process_selected_pid, Some(ProcessIdentity::new(5, 0)));
    assert_eq!(state.process_scroll, 2);
}

#[test]
fn hidden_selected_child_falls_back_to_collapsed_group() {
    let processes = vec![
        ProcessStats {
            pid: 10,
            program: "chromium".to_string(),
            ..ProcessStats::default()
        },
        ProcessStats {
            pid: 20,
            program: "chromium".to_string(),
            ..ProcessStats::default()
        },
    ];
    let mut state = UiState {
        process_selected_pid: Some(ProcessIdentity::new(10, 0)),
        process_scroll_visible: 10,
        ..UiState::default()
    };

    state.clamp_process_selection(&processes);

    assert_eq!(state.process_selected_pid, None);
    assert_eq!(state.process_selected_group.as_deref(), Some("chromium"));
}

#[test]
fn selected_group_falls_back_to_single_process_when_group_disappears() {
    let processes = vec![ProcessStats {
        pid: 10,
        program: "chromium".to_string(),
        ..ProcessStats::default()
    }];
    let mut state = UiState {
        process_selected_group: Some("chromium".to_string()),
        process_scroll_visible: 10,
        ..UiState::default()
    };

    state.clamp_process_selection(&processes);

    assert_eq!(state.process_selected_group, None);
    assert_eq!(
        state.process_selected_pid,
        Some(ProcessIdentity::new(10, 0))
    );
}

#[test]
fn compact_layout_shows_an_open_process_search() {
    // The compact layout has no process table or footer, so an open search
    // must still be visible; otherwise typing vanishes into a hidden mode.
    let llm = LlmStats {
        connected: true,
        ..Default::default()
    };
    let mut state = UiState::default();
    state.open_process_search();
    state.process_search_insert_char('l');
    state.process_search_insert_char('l');
    state.process_search_insert_char('a');
    let text = render_dashboard(60, 18, 1000, &llm, &mut state);
    assert!(
        text.contains("SEARCH"),
        "search mode must be visible:\n{text}"
    );
    assert!(text.contains("/lla"), "query must be visible:\n{text}");
    assert!(text.contains("Esc cancel"), "cancel hint missing:\n{text}");
}

fn process_click_test_state() -> UiState {
    UiState {
        process_rows: Some(ProcessRowsHit {
            rect: Rect::new(10, 10, 40, 2),
            targets: vec![
                ProcessRowTarget::Process(ProcessIdentity::new(10, 0)),
                ProcessRowTarget::Process(ProcessIdentity::new(20, 0)),
            ],
        }),
        ..Default::default()
    }
}

fn group_click_test_state() -> UiState {
    UiState {
        process_rows: Some(ProcessRowsHit {
            rect: Rect::new(10, 10, 40, 1),
            targets: vec![ProcessRowTarget::Group("chromium".to_string())],
        }),
        ..Default::default()
    }
}
