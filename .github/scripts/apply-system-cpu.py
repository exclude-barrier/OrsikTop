from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"expected block not found in {path}")
    p.write_text(text.replace(old, new, 1))


replace_once(
    "src/app.rs",
    "use std::{\n    io::Stdout,",
    "use std::{\n    fs,\n    io::Stdout,",
)

replace_once(
    "src/ui.rs",
    '''#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_usage: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
}
''',
    '''#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_usage: f64,
    pub per_cpu_usage: Vec<f64>,
    pub cpu_frequency_mhz: Option<f64>,
    pub cpu_temperature_c: Option<f64>,
    pub io_wait_pct: Option<f64>,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
}
''',
)

replace_once(
    "src/app.rs",
    '''        let mut system = System::new();
        let mut system_stats = SystemStats::default();
        let mut last_system_refresh: Option<Instant> = None;
''',
    '''        let mut system = System::new();
        let mut system_stats = SystemStats::default();
        let mut last_system_refresh: Option<Instant> = None;
        let mut previous_cpu_times = read_cpu_times();
''',
)

replace_once(
    "src/app.rs",
    '''                system.refresh_cpu_usage();
                system.refresh_memory();
                system_stats = SystemStats {
                    cpu_usage: system.global_cpu_usage() as f64,
                    memory_used_bytes: system.used_memory(),
                    memory_total_bytes: system.total_memory(),
                };
                last_system_refresh = Some(Instant::now());
''',
    '''                system.refresh_cpu_usage();
                system.refresh_memory();

                let current_cpu_times = read_cpu_times();
                let io_wait_pct = previous_cpu_times
                    .zip(current_cpu_times)
                    .and_then(|(previous, current)| io_wait_percent(previous, current));
                if current_cpu_times.is_some() {
                    previous_cpu_times = current_cpu_times;
                }

                let (load_one, load_five, load_fifteen) =
                    read_load_average().unwrap_or((0.0, 0.0, 0.0));
                system_stats = SystemStats {
                    cpu_usage: system.global_cpu_usage() as f64,
                    per_cpu_usage: system
                        .cpus()
                        .iter()
                        .map(|cpu| cpu.cpu_usage() as f64)
                        .collect(),
                    cpu_frequency_mhz: read_cpu_frequency_mhz(),
                    cpu_temperature_c: read_cpu_temperature_c(),
                    io_wait_pct,
                    load_one,
                    load_five,
                    load_fifteen,
                    memory_used_bytes: system.used_memory(),
                    memory_total_bytes: system.total_memory(),
                    swap_used_bytes: system.used_swap(),
                    swap_total_bytes: system.total_swap(),
                };
                last_system_refresh = Some(Instant::now());
''',
)

helpers = r'''
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct CpuTimes {
    total: u64,
    io_wait: u64,
}

fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_times(&text)
}

fn parse_cpu_times(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() < 5 {
        return None;
    }

    Some(CpuTimes {
        total: values.iter().copied().sum(),
        io_wait: values[4],
    })
}

fn io_wait_percent(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total_delta = current.total.saturating_sub(previous.total);
    if total_delta == 0 {
        return None;
    }
    let io_wait_delta = current.io_wait.saturating_sub(previous.io_wait);
    Some(io_wait_delta as f64 / total_delta as f64 * 100.0)
}

fn read_load_average() -> Option<(f64, f64, f64)> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    Some((
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ))
}

fn read_cpu_frequency_mhz() -> Option<f64> {
    let text = fs::read_to_string("/proc/cpuinfo").ok()?;
    let mut total = 0.0;
    let mut count = 0u64;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "cpu MHz" {
            continue;
        }
        let Ok(mhz) = value.trim().parse::<f64>() else {
            continue;
        };
        if mhz.is_finite() && mhz > 0.0 {
            total += mhz;
            count += 1;
        }
    }

    (count > 0).then_some(total / count as f64)
}

fn read_cpu_temperature_c() -> Option<f64> {
    let mut preferred = Vec::new();
    let mut fallback = Vec::new();
    let hwmons = fs::read_dir("/sys/class/hwmon").ok()?;

    for entry in hwmons.flatten() {
        let path = entry.path();
        let name = fs::read_to_string(path.join("name"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let cpu_sensor = name.contains("coretemp")
            || name.contains("k10temp")
            || name.contains("zenpower")
            || name.contains("cpu")
            || name.contains("x86_pkg");
        if !cpu_sensor {
            continue;
        }

        let Ok(sensors) = fs::read_dir(&path) else {
            continue;
        };
        for sensor in sensors.flatten() {
            let filename = sensor.file_name();
            let filename = filename.to_string_lossy();
            if !filename.starts_with("temp") || !filename.ends_with("_input") {
                continue;
            }

            let Ok(raw) = fs::read_to_string(sensor.path()) else {
                continue;
            };
            let Ok(millidegrees) = raw.trim().parse::<f64>() else {
                continue;
            };
            let celsius = millidegrees / 1000.0;
            if !(-20.0..=150.0).contains(&celsius) {
                continue;
            }

            let stem = filename.trim_end_matches("_input");
            let label = fs::read_to_string(path.join(format!("{stem}_label")))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if label.contains("package") || label.contains("tctl") || label.contains("cpu") {
                preferred.push(celsius);
            } else {
                fallback.push(celsius);
            }
        }
    }

    preferred
        .into_iter()
        .reduce(f64::max)
        .or_else(|| fallback.into_iter().reduce(f64::max))
}

'''
replace_once(
    "src/app.rs",
    "fn spawn_llm_worker(\n",
    helpers + "fn spawn_llm_worker(\n",
)

old_draw_system = r'''fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
    let block = Block::default()
        .title(" SYSTEM ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 16 || inner.height == 0 {
        return;
    }

    let ram_pct = percent(
        system.memory_used_bytes as f64,
        system.memory_total_bytes as f64,
    );
    let bar_width = inner.width.saturating_sub(16).max(8) as usize;

    let mut lines = vec![
        meter_line(
            "CPU",
            system.cpu_usage,
            bar_width,
            ORK_GREEN,
            format!("{:>4.1}%", clamp_percent(system.cpu_usage)),
            vec![],
        ),
        meter_line(
            "RAM",
            ram_pct,
            bar_width,
            CYAN,
            format!("{:>4.1}%", ram_pct),
            vec![],
        ),
        Line::from(vec![
            label_span(" USED     "),
            value_span(
                &format!("{:.1} GiB", bytes_to_gib(system.memory_used_bytes)),
                CYAN,
            ),
        ]),
        Line::from(vec![
            label_span(" TOTAL    "),
            value_span(
                &format!("{:.1} GiB", bytes_to_gib(system.memory_total_bytes)),
                WHITE,
            ),
        ]),
    ];

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}
'''

new_draw_system = r'''fn draw_system(frame: &mut Frame, area: Rect, system: &SystemStats) {
    let block = Block::default()
        .title(" SYSTEM ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 16 || inner.height == 0 {
        return;
    }

    let ram_pct = percent(
        system.memory_used_bytes as f64,
        system.memory_total_bytes as f64,
    );
    let bar_width = inner.width.saturating_sub(16).max(8) as usize;

    if inner.width < 34 || inner.height < 8 {
        let mut lines = vec![
            meter_line(
                "CPU",
                system.cpu_usage,
                bar_width,
                ORK_GREEN,
                format!("{:>4.1}%", clamp_percent(system.cpu_usage)),
                vec![],
            ),
            meter_line(
                "RAM",
                ram_pct,
                bar_width,
                CYAN,
                format!("{:>4.1}%", ram_pct),
                vec![],
            ),
            Line::from(vec![
                label_span(" USED     "),
                value_span(
                    &format!("{:.1} GiB", bytes_to_gib(system.memory_used_bytes)),
                    CYAN,
                ),
            ]),
            Line::from(vec![
                label_span(" TOTAL    "),
                value_span(
                    &format!("{:.1} GiB", bytes_to_gib(system.memory_total_bytes)),
                    WHITE,
                ),
            ]),
        ];
        lines.truncate(inner.height as usize);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let frequency = system
        .cpu_frequency_mhz
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| format!("{:.2}G", value / 1000.0))
        .unwrap_or_else(|| "—".to_string());
    let temperature = system
        .cpu_temperature_c
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.0}°C"))
        .unwrap_or_else(|| "—".to_string());
    let temperature_tint = system
        .cpu_temperature_c
        .map(temperature_color)
        .unwrap_or(MUTED);
    let io_wait = system
        .io_wait_pct
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "—".to_string());

    let max_per_row = inner.width.saturating_sub(11).max(1) as usize;
    let displayed_cores = system.per_cpu_usage.len().min(max_per_row * 2);
    let first_end = (displayed_cores + 1) / 2;

    let lines = vec![
        meter_line(
            "CPU",
            system.cpu_usage,
            bar_width,
            ORK_GREEN,
            format!("{:>4.1}%", clamp_percent(system.cpu_usage)),
            vec![],
        ),
        Line::from(vec![
            label_span(" FREQ "),
            value_span(&frequency, CYAN),
            label_span("  TEMP "),
            value_span(&temperature, temperature_tint),
            label_span("  IOW "),
            value_span(&io_wait, if system.io_wait_pct.unwrap_or(0.0) >= 10.0 { YELLOW } else { MUTED }),
        ]),
        Line::from(vec![
            label_span(" LOAD "),
            value_span(
                &format!(
                    "{:.2} / {:.2} / {:.2}",
                    system.load_one, system.load_five, system.load_fifteen
                ),
                WHITE,
            ),
        ]),
        core_usage_line(&system.per_cpu_usage, 0, first_end),
        core_usage_line(&system.per_cpu_usage, first_end, displayed_cores),
        meter_line(
            "RAM",
            ram_pct,
            bar_width,
            CYAN,
            format!("{:>4.1}%", ram_pct),
            vec![],
        ),
        Line::from(vec![
            label_span(" USED "),
            value_span(
                &format!(
                    "{:.1} / {:.1} GiB",
                    bytes_to_gib(system.memory_used_bytes),
                    bytes_to_gib(system.memory_total_bytes)
                ),
                CYAN,
            ),
        ]),
        Line::from(vec![
            label_span(" SWAP "),
            value_span(
                &format!(
                    "{:.1} / {:.1} GiB",
                    bytes_to_gib(system.swap_used_bytes),
                    bytes_to_gib(system.swap_total_bytes)
                ),
                if system.swap_used_bytes > 0 { YELLOW } else { MUTED },
            ),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

fn core_usage_line(usages: &[f64], start: usize, end: usize) -> Line<'static> {
    if start >= end || start >= usages.len() {
        return Line::from(vec![
            label_span(" CORES     "),
            value_span("—", MUTED),
        ]);
    }

    let end = end.min(usages.len());
    let mut spans = vec![Span::styled(
        format!(" C{start:02}-{:02}  ", end - 1),
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )];
    for usage in &usages[start..end] {
        spans.push(Span::styled(
            core_usage_glyph(*usage).to_string(),
            Style::default().fg(bar_gradient_color("CPU", clamp_percent(*usage), ORK_GREEN)),
        ));
    }
    Line::from(spans)
}

fn core_usage_glyph(usage: f64) -> char {
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
'''
replace_once("src/ui.rs", old_draw_system, new_draw_system)

replace_once(
    "src/app.rs",
    '''    #[test]
    fn refresh_change_updates_shared_value() {
        let shared = AtomicU64::new(1000);
        let mut value = 1000;
        change_refresh(&mut value, false, &shared);
        assert_eq!(value, 900);
        assert_eq!(shared.load(Ordering::Relaxed), 900);
    }
''',
    '''    #[test]
    fn refresh_change_updates_shared_value() {
        let shared = AtomicU64::new(1000);
        let mut value = 1000;
        change_refresh(&mut value, false, &shared);
        assert_eq!(value, 900);
        assert_eq!(shared.load(Ordering::Relaxed), 900);
    }

    #[test]
    fn parses_linux_cpu_times_and_iowait_delta() {
        let previous = parse_cpu_times("cpu  100 0 50 800 20 0 10 0 0 0\n").unwrap();
        let current = parse_cpu_times("cpu  120 0 60 850 25 0 15 0 0 0\n").unwrap();
        assert_eq!(previous.io_wait, 20);
        assert_eq!(current.io_wait, 25);
        let io_wait = io_wait_percent(previous, current).unwrap();
        assert!((io_wait - 5.882352941).abs() < 0.001);
    }
''',
)

replace_once(
    "src/ui.rs",
    '''    #[test]
    fn refresh_buttons_match_rendered_positions() {
''',
    '''    #[test]
    fn core_usage_glyph_scales_with_utilization() {
        assert_eq!(core_usage_glyph(0.0), '⣀');
        assert_eq!(core_usage_glyph(50.0), '⣦');
        assert_eq!(core_usage_glyph(100.0), '⣿');
    }

    #[test]
    fn refresh_buttons_match_rendered_positions() {
''',
)
