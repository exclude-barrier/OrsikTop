from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"expected block not found in {path}")
    p.write_text(text.replace(old, new, 1))


replace_once(
    "src/ui.rs",
    '''fn core_usage_glyph(usage: f64) -> char {
    match clamp_percent(usage) {
        value if value >= 92.0 => '⣿',
        value if value >= 75.0 => '⣷',
        value if value >= 58.0 => '⣶',
        value if value >= 42.0 => '⣦',
        value if value >= 25.0 => '⣤',
        value if value >= 8.0 => '⣄',
        _ => '⣀',
    }
}
''',
    '''fn core_usage_glyph(usage: f64) -> char {
    match clamp_percent(usage) {
        value if value >= 87.5 => '█',
        value if value >= 75.0 => '▇',
        value if value >= 62.5 => '▆',
        value if value >= 50.0 => '▅',
        value if value >= 37.5 => '▄',
        value if value >= 25.0 => '▃',
        value if value >= 12.5 => '▂',
        _ => '▁',
    }
}
''',
)

replace_once(
    "src/ui.rs",
    '''    fn core_usage_glyph_scales_with_utilization() {
        assert_eq!(core_usage_glyph(0.0), '⣀');
        assert_eq!(core_usage_glyph(50.0), '⣦');
        assert_eq!(core_usage_glyph(100.0), '⣿');
    }
''',
    '''    fn core_usage_glyph_scales_with_utilization() {
        assert_eq!(core_usage_glyph(0.0), '▁');
        assert_eq!(core_usage_glyph(12.5), '▂');
        assert_eq!(core_usage_glyph(50.0), '▅');
        assert_eq!(core_usage_glyph(87.5), '█');
        assert_eq!(core_usage_glyph(100.0), '█');
    }
''',
)

replace_once(
    "LICENSE",
    "   Copyright 2026 exclude-barrier\n",
    "   Copyright [yyyy] [name of copyright owner]\n",
)
