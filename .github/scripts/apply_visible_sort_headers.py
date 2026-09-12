from pathlib import Path

path = Path('src/ui.rs')
text = path.read_text()

old = '''    frame.render_widget(\n        Paragraph::new(header).style(Style::default().fg(WHITE).add_modifier(Modifier::BOLD)),\n        Rect::new(inner.x, inner.y, table_width, 1),\n    );\n'''
new = '''    frame.render_widget(\n        Paragraph::new(header),\n        Rect::new(inner.x, inner.y, table_width, 1),\n    );\n'''
assert old in text
text = text.replace(old, new, 1)

old = '''fn process_header_label(\n    label: &str,\n    key: ProcessSortKey,\n    active: ProcessSortKey,\n    descending: bool,\n) -> String {\n    if key == active {\n        format!(\"{label}{}\", if descending { \"↓\" } else { \"↑\" })\n    } else {\n        label.to_string()\n    }\n}\n'''
new = '''fn process_header_button(\n    label: &str,\n    key: ProcessSortKey,\n    active: ProcessSortKey,\n    descending: bool,\n    width: usize,\n    right_align: bool,\n) -> Span<'static> {\n    let is_active = key == active;\n    let arrow = if is_active {\n        if descending { \"↓\" } else { \"↑\" }\n    } else {\n        \"↕\"\n    };\n    let button = format!(\"[{label}{arrow}]\");\n    let cell = if right_align {\n        format!(\"{button:>width$}\")\n    } else {\n        format!(\"{button:<width$}\")\n    };\n    let mut style = Style::default()\n        .fg(if is_active { ORK_GREEN } else { CYAN })\n        .add_modifier(Modifier::BOLD);\n    if is_active {\n        style = style\n            .bg(PROCESS_SELECTED_BG)\n            .add_modifier(Modifier::UNDERLINED);\n    }\n    Span::styled(cell, style)\n}\n'''
assert old in text
text = text.replace(old, new, 1)

old = '''fn process_header_compact(width: usize, active: ProcessSortKey, descending: bool) -> String {\n    let fixed = 7 + 7 + 9 + 6;\n    let program_width = width.saturating_sub(fixed).max(8);\n    let pid = process_header_label(\"PID\", ProcessSortKey::Pid, active, descending);\n    let program = process_header_label(\"PROGRAM\", ProcessSortKey::Program, active, descending);\n    let cpu = process_header_label(\"CPU%\", ProcessSortKey::Cpu, active, descending);\n    let memory = process_header_label(\"MEM\", ProcessSortKey::Memory, active, descending);\n    let threads = process_header_label(\"THR\", ProcessSortKey::Threads, active, descending);\n    format!(\" {pid:<6}{program:<program_width$}{cpu:>6} {memory:>8} {threads:>5}\")\n}\n'''
new = '''fn process_header_compact(\n    width: usize,\n    active: ProcessSortKey,\n    descending: bool,\n) -> Line<'static> {\n    let fixed = 7 + 7 + 9 + 6;\n    let program_width = width.saturating_sub(fixed).max(10);\n    Line::from(vec![\n        Span::raw(\" \"),\n        process_header_button(\"PID\", ProcessSortKey::Pid, active, descending, 6, false),\n        process_header_button(\n            \"PROGRAM\",\n            ProcessSortKey::Program,\n            active,\n            descending,\n            program_width,\n            false,\n        ),\n        process_header_button(\"CPU\", ProcessSortKey::Cpu, active, descending, 6, true),\n        Span::raw(\" \"),\n        process_header_button(\"MEM\", ProcessSortKey::Memory, active, descending, 8, true),\n        Span::raw(\" \"),\n        process_header_button(\"THR\", ProcessSortKey::Threads, active, descending, 5, true),\n    ])\n}\n'''
assert old in text
text = text.replace(old, new, 1)

old = '''fn process_header_wide(width: usize, active: ProcessSortKey, descending: bool) -> String {\n    let fixed = 7 + 17 + 7 + 9 + 6;\n    let command_width = width.saturating_sub(fixed).max(12);\n    let pid = process_header_label(\"PID\", ProcessSortKey::Pid, active, descending);\n    let program = process_header_label(\"PROGRAM\", ProcessSortKey::Program, active, descending);\n    let cpu = process_header_label(\"CPU%\", ProcessSortKey::Cpu, active, descending);\n    let memory = process_header_label(\"MEM\", ProcessSortKey::Memory, active, descending);\n    let threads = process_header_label(\"THR\", ProcessSortKey::Threads, active, descending);\n    format!(\n        \" {pid:<6}{program:<16}{:<command_width$}{cpu:>6} {memory:>8} {threads:>5}\",\n        \"COMMAND\"\n    )\n}\n'''
new = '''fn process_header_wide(\n    width: usize,\n    active: ProcessSortKey,\n    descending: bool,\n) -> Line<'static> {\n    let fixed = 7 + 17 + 7 + 9 + 6;\n    let command_width = width.saturating_sub(fixed).max(12);\n    Line::from(vec![\n        Span::raw(\" \"),\n        process_header_button(\"PID\", ProcessSortKey::Pid, active, descending, 6, false),\n        process_header_button(\n            \"PROGRAM\",\n            ProcessSortKey::Program,\n            active,\n            descending,\n            16,\n            false,\n        ),\n        Span::styled(\n            format!(\"{:<command_width$}\", \"COMMAND\"),\n            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),\n        ),\n        process_header_button(\"CPU\", ProcessSortKey::Cpu, active, descending, 6, true),\n        Span::raw(\" \"),\n        process_header_button(\"MEM\", ProcessSortKey::Memory, active, descending, 8, true),\n        Span::raw(\" \"),\n        process_header_button(\"THR\", ProcessSortKey::Threads, active, descending, 5, true),\n    ])\n}\n'''
assert old in text
text = text.replace(old, new, 1)

old = '''    let help =\n        format!(\" [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  [click] sort  {refresh_ms} ms  \");\n'''
new = '''    let help = format!(\n        \" [q] quit  [-]/[+] refresh  [↑/↓ Pg] proc  [click header] sort  {refresh_ms} ms  \"\n    );\n'''
assert old in text
text = text.replace(old, new, 1)

text = text.replace(
    'format!(\" PROCESSES · {sort_name} {sort_arrow} · 0 \")',
    'format!(\" PROCESSES · SORT {sort_name} {sort_arrow} · 0 \")',
    1,
)
text = text.replace(
    '\" PROCESSES · {sort_name} {sort_arrow} · {}–{}/{} \",',
    '\" PROCESSES · SORT {sort_name} {sort_arrow} · {}–{}/{} \",',
    1,
)

path.write_text(text)
