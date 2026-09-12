from pathlib import Path

cpu = Path('src/cpu.rs')
text = cpu.read_text()

text = text.replace(
'''#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]\npub enum CpuCoreKind {\n    Performance,\n    Efficiency,\n    #[default]\n    Unknown,\n}\n\n#[derive(Clone, Debug, Default)]\npub struct CpuTopology {''',
'''#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]\npub enum CpuCoreKind {\n    Performance,\n    Efficiency,\n    #[default]\n    Unknown,\n}\n\n#[derive(Clone, Debug, Default, PartialEq, Eq)]\npub struct CpuPhysicalCore {\n    pub kind: CpuCoreKind,\n    pub logical_cpus: Vec<usize>,\n}\n\n#[derive(Clone, Debug, Default)]\npub struct CpuTopology {''',
1,
)

text = text.replace(
'''    pub efficiency_cores: Option<usize>,\n    pub core_kinds: Vec<CpuCoreKind>,\n}''',
'''    pub efficiency_cores: Option<usize>,\n    pub core_kinds: Vec<CpuCoreKind>,\n    pub physical_core_groups: Vec<CpuPhysicalCore>,\n}''',
1,
)

text = text.replace(
'''    let performance_cores = count_kind_groups(&core_groups, &core_kinds, CpuCoreKind::Performance);\n    let efficiency_cores = count_kind_groups(&core_groups, &core_kinds, CpuCoreKind::Efficiency);\n\n    CpuTopology {''',
'''    let performance_cores = count_kind_groups(&core_groups, &core_kinds, CpuCoreKind::Performance);\n    let efficiency_cores = count_kind_groups(&core_groups, &core_kinds, CpuCoreKind::Efficiency);\n    let physical_core_groups = build_physical_core_groups(&core_groups, &core_kinds);\n\n    CpuTopology {''',
1,
)

text = text.replace(
'''        efficiency_cores,\n        core_kinds,\n    }''',
'''        efficiency_cores,\n        core_kinds,\n        physical_core_groups,\n    }''',
1,
)

insert_at = text.index('fn count_unique_groups(')
helper = '''fn build_physical_core_groups(\n    groups: &[Option<String>],\n    kinds: &[CpuCoreKind],\n) -> Vec<CpuPhysicalCore> {\n    if groups.len() != kinds.len() || groups.is_empty() || groups.iter().any(Option::is_none) {\n        return Vec::new();\n    }\n\n    let mut keyed = Vec::<(String, CpuPhysicalCore)>::new();\n    for (logical_cpu, (group, kind)) in groups.iter().zip(kinds).enumerate() {\n        let Some(key) = group.as_ref() else {\n            return Vec::new();\n        };\n\n        if let Some((_, core)) = keyed.iter_mut().find(|(existing, _)| existing == key) {\n            if core.kind != *kind {\n                core.kind = CpuCoreKind::Unknown;\n            }\n            core.logical_cpus.push(logical_cpu);\n        } else {\n            keyed.push((\n                key.clone(),\n                CpuPhysicalCore {\n                    kind: *kind,\n                    logical_cpus: vec![logical_cpu],\n                },\n            ));\n        }\n    }\n\n    keyed\n        .into_iter()\n        .map(|(_, mut core)| {\n            core.logical_cpus.sort_unstable();\n            core\n        })\n        .collect()\n}\n\n'''
text = text[:insert_at] + helper + text[insert_at:]

marker = '''    #[test]\n    fn capacity_classes_require_meaningful_difference() {'''
test = '''    #[test]\n    fn groups_logical_threads_into_physical_cores() {\n        let groups = vec![\n            Some("0,4".to_string()),\n            Some("1,5".to_string()),\n            Some("2".to_string()),\n            Some("3".to_string()),\n            Some("0,4".to_string()),\n            Some("1,5".to_string()),\n        ];\n        let kinds = vec![\n            CpuCoreKind::Performance,\n            CpuCoreKind::Performance,\n            CpuCoreKind::Efficiency,\n            CpuCoreKind::Efficiency,\n            CpuCoreKind::Performance,\n            CpuCoreKind::Performance,\n        ];\n        assert_eq!(\n            build_physical_core_groups(&groups, &kinds),\n            vec![\n                CpuPhysicalCore {\n                    kind: CpuCoreKind::Performance,\n                    logical_cpus: vec![0, 4],\n                },\n                CpuPhysicalCore {\n                    kind: CpuCoreKind::Performance,\n                    logical_cpus: vec![1, 5],\n                },\n                CpuPhysicalCore {\n                    kind: CpuCoreKind::Efficiency,\n                    logical_cpus: vec![2],\n                },\n                CpuPhysicalCore {\n                    kind: CpuCoreKind::Efficiency,\n                    logical_cpus: vec![3],\n                },\n            ]\n        );\n    }\n\n'''
if marker not in text:
    raise SystemExit('cpu test insertion marker not found')
text = text.replace(marker, test + marker, 1)
cpu.write_text(text)

ui = Path('src/ui.rs')
text = ui.read_text()
text = text.replace(
'''    cpu::{CpuCoreKind, CpuTopology, CpuVendor},''',
'''    cpu::{CpuCoreKind, CpuPhysicalCore, CpuTopology, CpuVendor},''',
1,
)

old = '''    lines.extend(core_heatmap_rows(\n        &system.per_cpu_usage,\n        &system.cpu_topology.core_kinds,\n        inner.width,\n        busiest,\n        system.cpu_topology.is_hybrid(),\n        4,\n    ));'''
new = '''    if system.cpu_topology.is_hybrid() && !system.cpu_topology.physical_core_groups.is_empty() {\n        lines.extend(physical_core_rows(\n            &system.per_cpu_usage,\n            &system.cpu_topology.physical_core_groups,\n            inner.width,\n            busiest,\n            4,\n        ));\n    } else {\n        lines.extend(core_heatmap_rows(\n            &system.per_cpu_usage,\n            &system.cpu_topology.core_kinds,\n            inner.width,\n            busiest,\n            system.cpu_topology.is_hybrid(),\n            4,\n        ));\n    }'''
if old not in text:
    raise SystemExit('ui heatmap call marker not found')
text = text.replace(old, new, 1)

insert_at = text.index('fn core_heatmap_rows(')
helpers = '''fn physical_core_rows(\n    usages: &[f64],\n    cores: &[CpuPhysicalCore],\n    width: u16,\n    busiest: Option<usize>,\n    max_rows: usize,\n) -> Vec<Line<'static>> {\n    if max_rows < 2 {\n        return Vec::new();\n    }\n\n    let performance = cores\n        .iter()\n        .filter(|core| core.kind == CpuCoreKind::Performance)\n        .collect::<Vec<_>>();\n    let efficiency = cores\n        .iter()\n        .filter(|core| core.kind == CpuCoreKind::Efficiency)\n        .collect::<Vec<_>>();\n\n    if performance.is_empty() || efficiency.is_empty() {\n        return Vec::new();\n    }\n\n    let p_rows = (max_rows / 2).max(1);\n    let e_rows = max_rows.saturating_sub(p_rows).max(1);\n    let mut lines = physical_kind_rows(\n        usages,\n        &performance,\n        "P",\n        BRIGHT_GREEN,\n        width,\n        busiest,\n        p_rows,\n    );\n    lines.extend(physical_kind_rows(\n        usages,\n        &efficiency,\n        "E",\n        CYAN,\n        width,\n        busiest,\n        e_rows,\n    ));\n    lines.truncate(max_rows);\n    lines\n}\n\nfn physical_kind_rows(\n    usages: &[f64],\n    cores: &[&CpuPhysicalCore],\n    prefix: &'static str,\n    label_color: Color,\n    width: u16,\n    busiest: Option<usize>,\n    max_rows: usize,\n) -> Vec<Line<'static>> {\n    if cores.is_empty() || max_rows == 0 {\n        return Vec::new();\n    }\n\n    // Typical hybrid cores fit in ~7 terminal cells (\"P0 ░·  \"), so size\n    // rows from the actual panel width while preferring two rows per core class.\n    let max_cols = (width.saturating_sub(2) as usize / 7).max(1);\n    let cols = cores.len().div_ceil(max_rows).max(1).min(max_cols);\n    let visible = (cols * max_rows).min(cores.len());\n    let mut lines = Vec::new();\n\n    for row in 0..max_rows {\n        let start = row * cols;\n        let end = ((row + 1) * cols).min(visible);\n        if start >= end {\n            break;\n        }\n\n        let mut spans = vec![Span::raw(" ")];\n        for (offset, core) in cores[start..end].iter().enumerate() {\n            let core_index = start + offset;\n            if offset > 0 {\n                spans.push(Span::raw("  "));\n            }\n\n            let core_is_busiest = busiest\n                .map(|cpu| core.logical_cpus.contains(&cpu))\n                .unwrap_or(false);\n            let mut label_style = Style::default().fg(label_color);\n            if core_is_busiest {\n                label_style = label_style.add_modifier(Modifier::BOLD);\n            }\n            spans.push(Span::styled(format!("{prefix}{core_index} "), label_style));\n\n            let mut rendered_threads = 0usize;\n            for &logical_cpu in core.logical_cpus.iter().take(2) {\n                let usage = usages.get(logical_cpu).copied().unwrap_or(0.0);\n                let mut style = Style::default().fg(core_usage_color(usage));\n                if busiest == Some(logical_cpu) {\n                    style = style.add_modifier(Modifier::BOLD);\n                }\n                spans.push(Span::styled(core_usage_glyph(usage).to_string(), style));\n                rendered_threads += 1;\n            }\n\n            // Align single-thread E-cores with the two-thread P-core cells.\n            if rendered_threads < 2 {\n                spans.push(Span::raw(" "));\n            }\n        }\n        lines.push(Line::from(spans));\n    }\n\n    lines\n}\n\n'''
text = text[:insert_at] + helpers + text[insert_at:]
ui.write_text(text)
