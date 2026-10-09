//! Extracted from the former monolithic `ui.rs`. Behavior-preserving split.
use super::*;

/// Encoder/decoder utilization is idle when both are missing or below 1%.
/// On a pure LLM card this is always the case, so the ENC/DEC row is
/// hidden and its LIMIT pair moves to the I/O row.
pub(super) fn enc_dec_idle(gpu: &GpuStats) -> bool {
    let enc_idle = gpu.encoder_utilization.map(|v| v < 1.0).unwrap_or(true);
    let dec_idle = gpu.decoder_utilization.map(|v| v < 1.0).unwrap_or(true);
    enc_idle && dec_idle
}

/// True when the shown GPU is one the inference server is attributed to.
///
/// Identity is `DeviceId::key()` (BDF, then UUID), extended by the MIG
/// parent/child relationship: a server attributed to a MIG child belongs to
/// its physical GPU panel, and a server attributed to the physical parent
/// belongs to its MIG children's panels. `None`/`Unknown` mappings and GPUs
/// without a stable key never match — no fake "LLM".
pub(super) fn gpu_in_mapping(gpu: &GpuStats, map: &GpuMapping) -> bool {
    let key = gpu.device.key();
    if key.is_empty() {
        return false;
    }
    let matches = |mapped: &MappedGpu| {
        if mapped.key() == key {
            return true;
        }
        // MIG parent↔child relationship. Guarded with `is_some` on the
        // derived parent so two BDF-only non-MIG identities (both `None`)
        // never match.
        let gpu_parent = gpu.device.mig_parent_uuid();
        let mapped_parent = mapped.device.mig_parent_uuid();
        (gpu_parent.is_some() && gpu_parent.as_deref() == mapped.device.uuid.as_deref())
            || (mapped_parent.is_some() && mapped_parent.as_deref() == gpu.device.uuid.as_deref())
    };
    match map {
        GpuMapping::Single(m) => matches(m),
        GpuMapping::Multi(ms) => ms.iter().any(matches),
        GpuMapping::None | GpuMapping::Unknown => false,
    }
}

/// Text shown for the selected server's GPU attribution in the LLM panel.
///
/// `None` means no attribution (not computed or unknown) — the caller renders
/// `—`. A single device shows its stable key. Multiple devices show the first
/// key plus a visible `+N` count, so a tensor-parallel server is never mistaken
/// for a single-GPU one.
pub(super) fn gpu_attribution_label(map: &GpuMapping) -> Option<String> {
    match map {
        GpuMapping::Single(m) => Some(fit_cell(m.key(), 16)),
        GpuMapping::Multi(ms) => {
            let first = ms.first().map_or("<unkeyed>", MappedGpu::key);
            Some(format!(
                "{} +{}",
                fit_cell(first, 13),
                ms.len().saturating_sub(1)
            ))
        }
        GpuMapping::None | GpuMapping::Unknown => None,
    }
}

pub(super) fn draw_gpu(frame: &mut Frame, area: Rect, gpu: &GpuStats, gpu_map: &GpuMapping) {
    let idle = enc_dec_idle(gpu);
    let base_title = if gpu.available {
        format!(" GPU{} · {} ", gpu.index, gpu.name)
    } else if gpu.error.is_empty() {
        format!(" GPU{} · unavailable ", gpu.index)
    } else {
        format!(" GPU · {} ", gpu.error)
    };
    let title: Line<'static> = if gpu_in_mapping(gpu, gpu_map) {
        Line::from(vec![
            Span::raw(base_title),
            Span::styled(
                "LLM",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(Span::raw(base_title))
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM_GREEN));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 28 || inner.height == 0 {
        return;
    }

    if !gpu.available {
        frame.render_widget(
            Paragraph::new(" GPU telemetry unavailable")
                .style(Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            inner,
        );
        return;
    }

    let vram_pct = match (gpu.memory_used_mib, gpu.memory_total_mib) {
        (Some(used), Some(total)) if total > 0.0 => Some(percent(used, total)),
        _ => None,
    };
    let power_pct = match (gpu.power_w, gpu.power_limit_w) {
        (Some(power), Some(limit)) if limit > 0.0 => Some(percent(power, limit)),
        _ => None,
    };
    let bar_width = gpu_bar_width(inner.width);

    let draw_text = match (gpu.power_w, gpu.power_limit_w) {
        (Some(power), Some(limit)) => format!("{power:.0}/{limit:.0} W"),
        (Some(power), None) => format!("{power:.0} W"),
        _ => "—".to_string(),
    };
    let temp_text = optional_number(gpu.temperature_c, 0, "°C");
    let fan_text = optional_number(gpu.fan_percent, 0, "%");
    let core_text = optional_number(gpu.graphics_clock_mhz, 0, " MHz");
    let vclk_text = optional_number(gpu.memory_clock_mhz, 0, " MHz");
    let power_tint = power_pct.map(power_color).unwrap_or(MUTED);
    let io_suffix = || {
        let mut suffix = vec![
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
        ];
        if idle {
            suffix.push(fixed_data_pair(
                "LIMIT",
                gpu.limit_reason.clone(),
                limit_reason_color(&gpu.limit_reason),
                20,
            ));
        }
        suffix
    };

    let gpu_row = match gpu.utilization {
        Some(utilization) => meter_line(
            "GPU",
            utilization,
            bar_width,
            ORK_GREEN,
            format!("{:>3.0}%", utilization),
            vec![
                fixed_data_pair("CORE", core_text, ORK_GREEN, 21),
                fixed_data_pair("PSTATE", gpu.pstate.clone(), CYAN, 16),
            ],
        ),
        None => unavailable_meter_line(
            "GPU",
            bar_width,
            vec![
                fixed_data_pair("CORE", core_text, ORK_GREEN, 21),
                fixed_data_pair("PSTATE", gpu.pstate.clone(), CYAN, 16),
            ],
        ),
    };
    let used_text = match (gpu.memory_used_mib, gpu.memory_total_mib) {
        (Some(used), Some(total)) => {
            format!("{:.1}/{:.1} GiB", used / 1024.0, total / 1024.0)
        }
        _ => "—".to_string(),
    };
    let vram_tint = vram_pct.map(vram_color).unwrap_or(MUTED);
    let vram_row = match vram_pct {
        Some(pct) => meter_line(
            "VRAM",
            pct,
            bar_width,
            vram_tint,
            format!("{:>3.0}%", pct),
            vec![
                fixed_data_pair("USED", used_text, vram_tint, 25),
                fixed_data_pair("VCLK", vclk_text, CYAN, 20),
            ],
        ),
        None => unavailable_meter_line(
            "VRAM",
            bar_width,
            vec![
                fixed_data_pair("USED", used_text, vram_tint, 25),
                fixed_data_pair("VCLK", vclk_text, CYAN, 20),
            ],
        ),
    };
    let power_row = match power_pct {
        Some(pct) => meter_line(
            "PWR",
            pct,
            bar_width,
            power_tint,
            format!("{:>3.0}%", pct),
            vec![
                fixed_data_pair("DRAW", draw_text, power_tint, 21),
                fixed_data_pair(
                    "TEMP",
                    temp_text,
                    gpu.temperature_c.map(temperature_color).unwrap_or(MUTED),
                    16,
                ),
                fixed_data_pair("FAN", fan_text, ORK_GREEN, 13),
            ],
        ),
        None => unavailable_meter_line(
            "PWR",
            bar_width,
            vec![
                fixed_data_pair("DRAW", draw_text, power_tint, 21),
                fixed_data_pair(
                    "TEMP",
                    temp_text,
                    gpu.temperature_c.map(temperature_color).unwrap_or(MUTED),
                    16,
                ),
                fixed_data_pair("FAN", fan_text, ORK_GREEN, 13),
            ],
        ),
    };
    let mut lines = vec![
        gpu_row,
        vram_row,
        power_row,
        match gpu::pcie_utilization_pct(
            gpu.pcie_rx_mb_s,
            gpu.pcie_tx_mb_s,
            gpu.pcie_link_speed_gts,
            gpu.pcie_link_width,
        ) {
            Some(pct) => meter_line(
                "I/O",
                pct,
                bar_width,
                CYAN,
                format!("{:>3.0}%", pct),
                io_suffix(),
            ),
            None => {
                let mut spans = vec![
                    label_span(" I/O    "),
                    Span::styled(
                        format!("{:<width$}", " \u{2014}", width = bar_width + 11),
                        Style::default().fg(MUTED),
                    ),
                ];
                spans.extend(io_suffix());
                Line::from(spans)
            }
        },
    ];

    if !idle {
        lines.push(Line::from(vec![
            label_span("        "),
            Span::raw(" ".repeat(bar_width + 7)),
            fixed_data_pair(
                "ENC",
                optional_number(gpu.encoder_utilization, 0, "%"),
                WHITE,
                11,
            ),
            fixed_data_pair(
                "DEC",
                optional_number(gpu.decoder_utilization, 0, "%"),
                WHITE,
                11,
            ),
            fixed_data_pair(
                "LIMIT",
                gpu.limit_reason.clone(),
                limit_reason_color(&gpu.limit_reason),
                20,
            ),
        ]));
    }

    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn temperature_color(celsius: f64) -> Color {
    match celsius {
        v if v >= 85.0 => RED,
        v if v >= 80.0 => ORANGE,
        v if v >= 70.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

pub(super) fn power_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 95.0 => ORANGE,
        v if v >= 85.0 => YELLOW,
        _ => ORK_GREEN,
    }
}

pub(super) fn vram_color(percent: f64) -> Color {
    match percent {
        v if v >= 99.0 => RED,
        v if v >= 98.0 => ORANGE,
        v if v >= 95.0 => YELLOW,
        _ => CYAN,
    }
}

pub(super) fn limit_reason_color(reason: &str) -> Color {
    if reason == "none" || reason == "idle" || reason == "—" {
        MUTED
    } else if reason.contains("thermal") || reason.contains("power-brake") || reason.contains("hw")
    {
        RED
    } else if reason.contains("power") {
        ORANGE
    } else {
        YELLOW
    }
}
