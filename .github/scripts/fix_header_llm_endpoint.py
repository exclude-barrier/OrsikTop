from pathlib import Path

path = Path('src/ui.rs')
text = path.read_text()
old = '''        spans.extend([\n            Span::styled(status, status_style),\n            Span::styled("   ", Style::default()),\n            Span::styled(endpoint, Style::default().fg(MUTED)),\n        ]);\n'''
new = '''        spans.extend([\n            Span::styled(status, status_style),\n            Span::styled("   ", Style::default()),\n            Span::styled("LLM ENDPOINT ", Style::default().fg(WHITE)),\n            Span::styled(endpoint, Style::default().fg(MUTED)),\n        ]);\n'''
if old not in text:
    raise SystemExit('header endpoint block not found')
path.write_text(text.replace(old, new, 1))
