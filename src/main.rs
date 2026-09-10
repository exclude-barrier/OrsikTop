use std::{
    collections::HashMap,
    io::{self, Stdout},
    process::Command,
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame, Terminal,
};
use serde_json::Value;
use sysinfo::System;

const ORK_GREEN: Color = Color::Rgb(105, 210, 70);
const DIM_GREEN: Color = Color::Rgb(70, 135, 60);

#[derive(Parser, Debug)]
#[command(name = "orsiktop", version, about = "btop for local LLM Orks")]
struct Args {
    /// llama.cpp server base URL
    #[arg(long, env = "ORSIKTOP_SERVER", default_value = "http://127.0.0.1:8080")]
    server: String,

    /// Refresh interval in milliseconds
    #[arg(long, default_value_t = 1000)]
    interval: u64,
}

#[derive(Clone, Default)]
struct GpuStats {
    name: String,
    utilization: f64,
    memory_used_mib: f64,
    memory_total_mib: f64,
    temperature_c: f64,
    power_w: f64,
}

#[derive(Clone, Default)]
struct LlmStats {
    connected: bool,
    model: String,
    context_size: u64,
    context_used: u64,
    prompt_total: f64,
    generated_total: f64,
    prompt_tps: f64,
    generation_tps: f64,
    active_requests: f64,
    deferred_requests: f64,
    error: String,
}

#[derive(Default)]
struct PreviousCounters {
    at: Option<Instant>,
    prompt_total: f64,
    generated_total: f64,
}

fn main() -> anyhow_free::Result<()> {
    let args = Args::parse();
    let tick = Duration::from_millis(args.interval.max(200));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &args.server, tick);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    server: &str,
    tick: Duration,
) -> anyhow_free::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(700))
        .build()?;
    let mut system = System::new_all();
    let mut previous = PreviousCounters::default();
    let mut last_refresh = Instant::now() - tick;

    let mut llm = LlmStats::default();
    let mut gpu = GpuStats::default();

    loop {
        if last_refresh.elapsed() >= tick {
            system.refresh_all();
            llm = fetch_llm_stats(&client, server, &mut previous);
            gpu = fetch_gpu_stats().unwrap_or_default();
            last_refresh = Instant::now();
        }

        terminal.draw(|frame| draw(frame, &system, &llm, &gpu, server))?;

        let wait = tick
            .checked_sub(last_refresh.elapsed())
            .unwrap_or(Duration::ZERO)
            .min(Duration::from_millis(100));

        if event::poll(wait)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn fetch_llm_stats(
    client: &reqwest::blocking::Client,
    server: &str,
    previous: &mut PreviousCounters,
) -> LlmStats {
    let mut stats = LlmStats::default();
    let base = server.trim_end_matches('/');

    let metrics_text = match client.get(format!("{base}/metrics")).send() {
        Ok(response) if response.status().is_success() => match response.text() {
            Ok(text) => text,
            Err(err) => {
                stats.error = format!("metrics response error: {err}");
                return stats;
            }
        },
        Ok(response) => {
            stats.error = format!("/metrics returned HTTP {} (start llama-server with --metrics)", response.status());
            return stats;
        }
        Err(err) => {
            stats.error = format!("cannot reach llama.cpp: {err}");
            return stats;
        }
    };

    stats.connected = true;
    let metrics = parse_prometheus(&metrics_text);

    stats.prompt_total = pick_metric(
        &metrics,
        &[
            "llamacpp:tokens_evaluated_total",
            "llamacpp_prompt_tokens_total",
            "prompt_tokens_total",
        ],
    );
    stats.generated_total = pick_metric(
        &metrics,
        &[
            "llamacpp:tokens_predicted_total",
            "llamacpp_predicted_tokens_total",
            "predicted_tokens_total",
        ],
    );
    stats.active_requests = pick_metric(
        &metrics,
        &[
            "llamacpp:requests_processing",
            "llamacpp_requests_processing",
            "requests_processing",
        ],
    );
    stats.deferred_requests = pick_metric(
        &metrics,
        &[
            "llamacpp:requests_deferred",
            "llamacpp_requests_deferred",
            "requests_deferred",
        ],
    );

    let kv_ratio = pick_metric(
        &metrics,
        &[
            "llamacpp:kv_cache_usage_ratio",
            "llamacpp_kv_cache_usage_ratio",
            "kv_cache_usage_ratio",
        ],
    );

    if let Some(previous_at) = previous.at {
        let seconds = previous_at.elapsed().as_secs_f64();
        if seconds > 0.0 {
            stats.prompt_tps = ((stats.prompt_total - previous.prompt_total) / seconds).max(0.0);
            stats.generation_tps = ((stats.generated_total - previous.generated_total) / seconds).max(0.0);
        }
    }
    previous.at = Some(Instant::now());
    previous.prompt_total = stats.prompt_total;
    previous.generated_total = stats.generated_total;

    if let Ok(response) = client.get(format!("{base}/props")).send() {
        if let Ok(props) = response.json::<Value>() {
            stats.model = json_string(&props, &["model_alias", "model_name", "model_path"])
                .unwrap_or_else(|| "llama.cpp model".to_string());
            stats.context_size = json_u64_path(&props, &["default_generation_settings", "n_ctx"])
                .or_else(|| json_u64_path(&props, &["n_ctx"]))
                .unwrap_or(0);
        }
    }

    if stats.model.is_empty() {
        stats.model = "llama.cpp model".to_string();
    }

    if stats.context_size > 0 && kv_ratio > 0.0 {
        stats.context_used = (stats.context_size as f64 * kv_ratio.clamp(0.0, 1.0)) as u64;
    }

    stats
}

fn parse_prometheus(input: &str) -> HashMap<String, f64> {
    let mut metrics = HashMap::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name_with_labels, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name_with_labels.split('{').next().unwrap_or(name_with_labels);
        if let Ok(value) = value.trim().parse::<f64>() {
            *metrics.entry(name.to_string()).or_insert(0.0) += value;
        }
    }
    metrics
}

fn pick_metric(metrics: &HashMap<String, f64>, names: &[&str]) -> f64 {
    names.iter().find_map(|name| metrics.get(*name).copied()).unwrap_or(0.0)
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(ToOwned::to_owned))
}

fn json_u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

fn fetch_gpu_stats() -> Option<GpuStats> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout.lines().next()?;
    let fields: Vec<_> = line.split(',').map(str::trim).collect();
    if fields.len() < 6 {
        return None;
    }

    Some(GpuStats {
        name: fields[0].to_string(),
        utilization: parse_num(fields[1]),
        memory_used_mib: parse_num(fields[2]),
        memory_total_mib: parse_num(fields[3]),
        temperature_c: parse_num(fields[4]),
        power_w: parse_num(fields[5]),
    })
}

fn parse_num(value: &str) -> f64 {
    value.parse::<f64>().unwrap_or(0.0)
}

fn draw(frame: &mut Frame, system: &System, llm: &LlmStats, gpu: &GpuStats, server: &str) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(3),
        ])
        .split(area);

    let status = if llm.connected { "ONLINE" } else { "OFFLINE" };
    let status_style = if llm.connected {
        Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" OrsikTop ", Style::default().fg(ORK_GREEN).add_modifier(Modifier::BOLD)),
        Span::raw("— btop for local LLM Orks  "),
        Span::styled(status, status_style),
        Span::raw(format!("  {server}")),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(DIM_GREEN)));
    frame.render_widget(header, rows[0]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(rows[1]);

    draw_llm(frame, columns[0], llm);
    draw_machine(frame, columns[1], system, gpu);

    let footer_text = if llm.error.is_empty() {
        " q/Esc quit  |  LOCAL LLMS  |  MORE POWER  |  HAPPIER ORKS ".to_string()
    } else {
        format!(" q/Esc quit  |  {} ", llm.error)
    };
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(Color::Gray))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(DIM_GREEN)));
    frame.render_widget(footer, rows[2]);
}

fn draw_llm(frame: &mut Frame, area: ratatui::layout::Rect, llm: &LlmStats) {
    let block = Block::default()
        .title(" LLM ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!("Model       {}", llm.model)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Throughput  prompt {:>8.1} tok/s   generate {:>8.1} tok/s",
            llm.prompt_tps, llm.generation_tps
        )),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Tokens      prompt {:>10.0}   generated {:>10.0}",
            llm.prompt_total, llm.generated_total
        )),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Requests    {:.0} active / {:.0} deferred",
            llm.active_requests, llm.deferred_requests
        )),
        rows[3],
    );

    let ratio = if llm.context_size > 0 {
        llm.context_used as f64 / llm.context_size as f64
    } else {
        0.0
    };
    frame.render_widget(
        Gauge::default()
            .block(Block::default().title(" Context / KV ").borders(Borders::TOP))
            .gauge_style(Style::default().fg(ORK_GREEN))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(if llm.context_size > 0 {
                format!("{} / {} ({:.0}%)", llm.context_used, llm.context_size, ratio * 100.0)
            } else {
                "waiting for context metric".to_string()
            }),
        rows[4],
    );
}

fn draw_machine(frame: &mut Frame, area: ratatui::layout::Rect, system: &System, gpu: &GpuStats) {
    let block = Block::default()
        .title(" Machine ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORK_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let total_mem = system.total_memory() as f64;
    let used_mem = system.used_memory() as f64;
    let ram_ratio = if total_mem > 0.0 { used_mem / total_mem } else { 0.0 };
    let gpu_mem_ratio = if gpu.memory_total_mib > 0.0 {
        gpu.memory_used_mib / gpu.memory_total_mib
    } else {
        0.0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(format!("GPU         {}", fallback(&gpu.name, "nvidia-smi unavailable"))), rows[0]);
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ORK_GREEN))
            .ratio((gpu.utilization / 100.0).clamp(0.0, 1.0))
            .label(format!("GPU {:.0}%", gpu.utilization)),
        rows[1],
    );
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ORK_GREEN))
            .ratio(gpu_mem_ratio.clamp(0.0, 1.0))
            .label(format!("VRAM {:.0} / {:.0} MiB", gpu.memory_used_mib, gpu.memory_total_mib)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(format!("Temp        {:.0} °C        Power {:.0} W", gpu.temperature_c, gpu.power_w)),
        rows[3],
    );
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ORK_GREEN))
            .ratio(ram_ratio.clamp(0.0, 1.0))
            .label(format!("RAM {:.1} / {:.1} GiB", bytes_to_gib(used_mem), bytes_to_gib(total_mem))),
        rows[4],
    );
    frame.render_widget(
        Paragraph::new(format!("CPU         {:>5.1}%", system.global_cpu_usage())),
        rows[5],
    );
}

fn bytes_to_gib(bytes: f64) -> f64 {
    bytes / 1024.0 / 1024.0 / 1024.0
}

fn fallback<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

mod anyhow_free {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
}
