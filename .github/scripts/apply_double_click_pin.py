from pathlib import Path

path = Path('src/ui.rs')
text = path.read_text()

text = text.replace(
    'const REFRESH_CONTROL_WIDTH: u16 = 22;\n',
    'const REFRESH_CONTROL_WIDTH: u16 = 22;\nconst PROCESS_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);\n',
    1,
)

text = text.replace(
    '    process_selected_pid: Option<u32>,\n    process_scroll: usize,\n',
    '    process_selected_pid: Option<u32>,\n    process_pinned_pid: Option<u32>,\n    last_process_click: Option<(u32, Instant)>,\n    process_scroll: usize,\n',
    1,
)

text = text.replace(
    '            process_selected_pid: None,\n            process_scroll: 0,\n',
    '            process_selected_pid: None,\n            process_pinned_pid: None,\n            last_process_click: None,\n            process_scroll: 0,\n',
    1,
)

old_clamp = '''    pub fn clamp_process_selection(&mut self, processes: &[ProcessStats]) {
        if self
            .process_selected_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_selected_pid = None;
        }
        self.process_scroll = self.process_scroll.min(processes.len().saturating_sub(1));
    }
'''
new_clamp = '''    pub fn clamp_process_selection(&mut self, processes: &[ProcessStats]) {
        if self
            .process_selected_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_selected_pid = None;
        }
        if self
            .process_pinned_pid
            .is_some_and(|pid| !processes.iter().any(|process| process.pid == pid))
        {
            self.process_pinned_pid = None;
        }
        self.process_scroll = self.process_scroll.min(processes.len().saturating_sub(1));
    }
'''
assert old_clamp in text
text = text.replace(old_clamp, new_clamp, 1)

text = text.replace(
    '''    pub fn clear_process_selection(&mut self) {
        self.process_selected_pid = None;
    }
''',
    '''    pub fn clear_process_selection(&mut self) {
        self.process_selected_pid = None;
        self.process_pinned_pid = None;
        self.last_process_click = None;
    }
''',
    1,
)

old_click = '''    pub fn click_process_row(&mut self, x: u16, y: u16) -> bool {
        let Some(rows) = self.process_rows.as_ref() else {
            return false;
        };
        if !rect_contains(rows.rect, x, y) {
            return false;
        }
        let row = y.saturating_sub(rows.rect.y) as usize;
        let Some(pid) = rows.pids.get(row).copied() else {
            return false;
        };
        self.process_selected_pid = Some(pid);
        true
    }
'''
new_click = '''    pub fn click_process_row(&mut self, x: u16, y: u16) -> bool {
        let Some(rows) = self.process_rows.as_ref() else {
            return false;
        };
        if !rect_contains(rows.rect, x, y) {
            return false;
        }
        let row = y.saturating_sub(rows.rect.y) as usize;
        let Some(pid) = rows.pids.get(row).copied() else {
            return false;
        };

        let now = Instant::now();
        let is_double_click = self.last_process_click.is_some_and(|(last_pid, at)| {
            last_pid == pid && now.saturating_duration_since(at) <= PROCESS_DOUBLE_CLICK_WINDOW
        });

        self.process_selected_pid = Some(pid);
        if is_double_click {
            self.process_pinned_pid = Some(pid);
            self.last_process_click = None;
        } else {
            if self.process_pinned_pid.is_some() && self.process_pinned_pid != Some(pid) {
                self.process_pinned_pid = None;
            }
            self.last_process_click = Some((pid, now));
        }
        true
    }
'''
assert old_click in text
text = text.replace(old_click, new_click, 1)

text = text.replace(
    '''        state.process_sort_desc,
        state.process_selected_pid,
    );
    if state.process_selected_pid.is_some() && pinned.is_none() {
        state.process_selected_pid = None;
    }
''',
    '''        state.process_sort_desc,
        state.process_pinned_pid,
    );
    if state.process_pinned_pid.is_some() && pinned.is_none() {
        state.process_pinned_pid = None;
    }
''',
    1,
)

text = text.replace(
    '        let is_selected = state.process_selected_pid == Some(process.pid);\n',
    '''        let is_selected = state.process_selected_pid == Some(process.pid)
            || state.process_pinned_pid == Some(process.pid);
''',
    1,
)

text = text.replace(
    '        " [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  [click header] sort  {refresh_ms} ms  "\n',
    '        " [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  [click] select  [2x] pin  [header] sort  {refresh_ms} ms  "\n',
    1,
)

insert_after = '''    fn pinned_process_is_removed_from_sorted_stream() {
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

        let (pinned, rest) =
            sorted_processes_with_pin(&processes, ProcessSortKey::Cpu, true, Some(30));
        assert_eq!(pinned.map(|process| process.pid), Some(30));
        assert_eq!(
            rest.iter().map(|process| process.pid).collect::<Vec<_>>(),
            vec![10, 20]
        );
    }
'''
new_tests = insert_after + '''
    #[test]
    fn single_click_selects_process_without_pinning() {
        let mut state = UiState::default();
        state.process_rows = Some(ProcessRowsHit {
            rect: Rect::new(10, 10, 40, 2),
            pids: vec![10, 20],
        });

        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_selected_pid, Some(10));
        assert_eq!(state.process_pinned_pid, None);
    }

    #[test]
    fn double_click_pins_process() {
        let mut state = UiState::default();
        state.process_rows = Some(ProcessRowsHit {
            rect: Rect::new(10, 10, 40, 2),
            pids: vec![10, 20],
        });

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_selected_pid, Some(10));
        assert_eq!(state.process_pinned_pid, Some(10));
    }

    #[test]
    fn clicking_another_process_releases_pin_and_selects_new_process() {
        let mut state = UiState::default();
        state.process_rows = Some(ProcessRowsHit {
            rect: Rect::new(10, 10, 40, 2),
            pids: vec![10, 20],
        });

        assert!(state.click_process_row(12, 10));
        assert!(state.click_process_row(12, 10));
        assert_eq!(state.process_pinned_pid, Some(10));

        assert!(state.click_process_row(12, 11));
        assert_eq!(state.process_selected_pid, Some(20));
        assert_eq!(state.process_pinned_pid, None);
    }
'''
assert insert_after in text
text = text.replace(insert_after, new_tests, 1)

path.write_text(text)
