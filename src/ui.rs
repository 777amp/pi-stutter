use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Sparkline, Table, Tabs, Wrap,
};
use ratatui::Frame;

use crate::analyzer::{Analyzer, Severity};
use crate::jitter::JitterSnapshot;
use crate::metrics::SystemSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview = 0,
    Processes = 1,
    IoIrq = 2,
    Diagnostics = 3,
}

impl Tab {
    pub fn titles() -> Vec<&'static str> {
        vec![
            "1: Overview",
            "2: Processes",
            "3: I/O & IRQs",
            "4: Diagnostics",
        ]
    }

    pub fn next(self) -> Self {
        match self {
            Tab::Overview => Tab::Processes,
            Tab::Processes => Tab::IoIrq,
            Tab::IoIrq => Tab::Diagnostics,
            Tab::Diagnostics => Tab::Overview,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcSort {
    Cpu,
    Memory,
    MajFaults,
    State,
}

impl ProcSort {
    pub fn label(self) -> &'static str {
        match self {
            ProcSort::Cpu => "CPU%",
            ProcSort::Memory => "RSS",
            ProcSort::MajFaults => "MajFlt/s",
            ProcSort::State => "State(D)",
        }
    }
}

pub struct UiState<'a> {
    pub tab: Tab,
    pub paused: bool,
    pub proc_sort: ProcSort,
    pub snap: &'a SystemSnapshot,
    pub jitter: &'a JitterSnapshot,
    pub analyzer: &'a Analyzer,
    pub jitter_history_us: &'a [u64],
    pub cpu_history: &'a [u64],
    pub status_msg: Option<&'a str>,
}

pub fn draw(f: &mut Frame, ui: &UiState<'_>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header + badges
            Constraint::Length(3), // Tabs
            Constraint::Min(10),   // Main content
            Constraint::Length(1), // Footer keybindings
        ])
        .split(f.area());

    draw_header(f, chunks[0], ui);
    draw_tabs(f, chunks[1], ui);

    match ui.tab {
        Tab::Overview => draw_overview(f, chunks[2], ui),
        Tab::Processes => draw_processes(f, chunks[2], ui),
        Tab::IoIrq => draw_io_irq(f, chunks[2], ui),
        Tab::Diagnostics => draw_diagnostics(f, chunks[2], ui),
    }

    draw_footer(f, chunks[3], ui);
}

fn draw_header(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let snap = ui.snap;
    let temp_span = match snap.temp_c {
        Some(t) if t >= 80.0 => Span::styled(
            format!(" {t:.1}°C "),
            Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Some(t) if t >= 70.0 => Span::styled(
            format!(" {t:.1}°C "),
            Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD),
        ),
        Some(t) => Span::styled(
            format!(" {t:.1}°C "),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        None => Span::styled(" Temp:N/A ", Style::default().fg(Color::DarkGray)),
    };

    let freq_str = match (snap.cpu_freq_mhz, snap.cpu_max_mhz) {
        (Some(cur), Some(max)) => format!(" {cur:.0}/{max:.0}MHz "),
        (Some(cur), None) => format!(" {cur:.0}MHz "),
        _ => " Freq:N/A ".into(),
    };

    let throttle_span = if snap.throttle.under_voltage_now {
        Span::styled(
            " ⚡ UNDER-VOLTAGE! ",
            Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD),
        )
    } else if snap.throttle.throttled_now {
        Span::styled(
            " 🔥 THROTTLED! ",
            Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD),
        )
    } else if snap.throttle.freq_capped_now || snap.throttle.soft_temp_limit_now {
        Span::styled(
            " ⚠ FREQ CAPPED ",
            Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD),
        )
    } else if snap.throttle.any_historical() {
        Span::styled(
            " ⚠ PAST THROTTLE ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else if snap.throttle.raw.is_some() {
        Span::styled(" ✓ PWR/THRM OK ", Style::default().fg(Color::Green))
    } else {
        Span::styled(" (Generic Linux) ", Style::default().fg(Color::DarkGray))
    };

    let pause_span = if ui.paused {
        Span::styled(
            " [PAUSED] ",
            Style::default().bg(Color::Magenta).fg(Color::White).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    let line = Line::from(vec![
        Span::styled(
            " pi-stutter ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ "),
        Span::styled(&snap.model, Style::default().fg(Color::White)),
        Span::raw(" │"),
        temp_span,
        Span::raw("│"),
        Span::styled(freq_str, Style::default().fg(Color::Cyan)),
        Span::raw("│"),
        throttle_span,
        Span::raw(format!(
            " │ Load: {:.2} {:.2} {:.2}",
            snap.load_avg.0, snap.load_avg.1, snap.load_avg.2
        )),
        pause_span,
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_tabs(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let titles: Vec<Line> = Tab::titles()
        .into_iter()
        .map(|t| Line::from(Span::styled(t, Style::default().fg(Color::White))))
        .collect();

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .select(ui.tab as usize)
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
    f.render_widget(tabs, area);
}

fn draw_overview(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Jitter + CPU sparklines
            Constraint::Length(6), // PSI + Memory
            Constraint::Min(6),    // Stutter event log
        ])
        .split(area);

    // Row 0: Jitter Sparkline + CPU Sparkline
    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(rows[0]);

    let jit_color = if ui.jitter.window_max_ms >= ui.analyzer.jitter_threshold_ms {
        Color::Red
    } else if ui.jitter.p95_ms >= ui.analyzer.jitter_threshold_ms * 0.5 {
        Color::Yellow
    } else {
        Color::Green
    };

    let jitter_title = format!(
        " Scheduler Jitter (thr={:.0}ms) | now:{:.2}ms p95:{:.2}ms peak:{:.2}ms ",
        ui.analyzer.jitter_threshold_ms,
        ui.jitter.window_max_ms,
        ui.jitter.p95_ms,
        ui.jitter.peak_ms
    );
    let jitter_spark = Sparkline::default()
        .block(
            Block::default()
                .title(jitter_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(jit_color)),
        )
        .data(ui.jitter_history_us)
        .style(Style::default().fg(jit_color));
    f.render_widget(jitter_spark, top_cols[0]);

    let c = &ui.snap.cpu;
    let cpu_title = format!(
        " CPU {:.0}% (usr:{:.0} sys:{:.0} iow:{:.0} irq:{:.0}) | blk:{} ",
        c.total_pct,
        c.user_pct,
        c.sys_pct,
        c.iowait_pct,
        c.irq_pct + c.softirq_pct,
        c.procs_blocked
    );
    let cpu_color = if c.iowait_pct > 20.0 || c.total_pct > 90.0 {
        Color::Red
    } else if c.total_pct > 70.0 {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let cpu_spark = Sparkline::default()
        .block(
            Block::default()
                .title(cpu_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(cpu_color)),
        )
        .data(ui.cpu_history)
        .max(100)
        .style(Style::default().fg(cpu_color));
    f.render_widget(cpu_spark, top_cols[1]);

    // Row 1: PSI Gauges + Memory/Swap
    let mid_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    draw_psi_box(f, mid_cols[0], ui);
    draw_mem_box(f, mid_cols[1], ui);

    // Row 2: Recent Stutter Events
    draw_events_list(f, rows[2], ui);
}

fn draw_psi_box(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let block = Block::default()
        .title(" Linux PSI Stall Pressure (avg10) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if !ui.snap.psi.available {
        f.render_widget(
            Paragraph::new("PSI (/proc/pressure/*) not enabled in this kernel.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let items = [
        ("CPU", ui.snap.psi.cpu.some_avg10, ui.snap.psi.cpu.some_delta_ms),
        ("I/O", ui.snap.psi.io.some_avg10, ui.snap.psi.io.some_delta_ms),
        ("MEM", ui.snap.psi.memory.some_avg10, ui.snap.psi.memory.some_delta_ms),
    ];

    for (i, (label, avg10, delta_ms)) in items.iter().enumerate() {
        let color = if *avg10 > 20.0 {
            Color::Red
        } else if *avg10 > 5.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        let ratio = (*avg10 / 100.0).clamp(0.0, 1.0);
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(color).bg(Color::DarkGray))
            .ratio(ratio)
            .label(format!("{label}: {avg10:5.1}% (stall +{delta_ms:.1}ms)"));
        f.render_widget(gauge, rows[i]);
    }
}

fn draw_mem_box(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let m = &ui.snap.mem;
    let block = Block::default()
        .title(" Memory, Swap & Page Faults ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let ram_ratio = if m.total_mb > 0.0 {
        (m.used_mb / m.total_mb).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ram_color = if ram_ratio > 0.90 {
        Color::Red
    } else if ram_ratio > 0.75 {
        Color::Yellow
    } else {
        Color::Cyan
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ram_color).bg(Color::DarkGray))
            .ratio(ram_ratio)
            .label(format!("RAM: {:.0}/{:.0} MB ({:.0}%)", m.used_mb, m.total_mb, ram_ratio * 100.0)),
        rows[0],
    );

    let swap_ratio = if m.swap_total_mb > 0.0 {
        (m.swap_used_mb / m.swap_total_mb).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let swap_color = if m.swap_in_per_sec > 2.0 || swap_ratio > 0.5 {
        Color::Red
    } else {
        Color::Green
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(swap_color).bg(Color::DarkGray))
            .ratio(swap_ratio)
            .label(format!(
                "Swap: {:.0}/{:.0} MB (in:{:.0}/s out:{:.0}/s)",
                m.swap_used_mb, m.swap_total_mb, m.swap_in_per_sec, m.swap_out_per_sec
            )),
        rows[1],
    );

    let extra = Paragraph::new(Line::from(vec![
        Span::raw(format!("Dirty: {:.1}MB  Writeback: {:.1}MB  ", m.dirty_mb, m.writeback_mb)),
        Span::styled(
            format!("MajFlt: {:.0}/s", m.maj_faults_per_sec),
            Style::default().fg(if m.maj_faults_per_sec > 15.0 {
                Color::Red
            } else {
                Color::White
            }),
        ),
    ]));
    f.render_widget(extra, rows[2]);
}

fn draw_events_list(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let title = format!(
        " Stutter Events ({} total) — triggers when jitter >= {:.0}ms or critical stall ",
        ui.analyzer.total_stutters, ui.analyzer.jitter_threshold_ms
    );

    if ui.analyzer.events.is_empty() {
        let p = Paragraph::new("No stutter events detected yet. System looks smooth! (Use +/- to lower jitter threshold)")
            .style(Style::default().fg(Color::Green))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = ui
        .analyzer
        .events
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|ev| {
            let (sev_str, sev_color) = match ev.severity {
                Severity::Crit => ("CRIT", Color::Red),
                Severity::Warn => ("WARN", Color::Yellow),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", ev.timestamp), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("[{sev_str}] "),
                    Style::default().fg(sev_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:5.1}ms ", ev.jitter_ms),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("{} ", ev.causes.join(",")),
                    Style::default().fg(sev_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw("— "),
                Span::raw(&ev.detail),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(list, area);
}

fn draw_processes(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let mut procs = ui.snap.processes.clone();
    match ui.proc_sort {
        ProcSort::Cpu => procs.sort_by(|a, b| {
            b.cpu_pct
                .partial_cmp(&a.cpu_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::Memory => procs.sort_by(|a, b| {
            b.rss_mb
                .partial_cmp(&a.rss_mb)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::MajFaults => procs.sort_by(|a, b| {
            b.maj_flt_per_sec
                .partial_cmp(&a.maj_flt_per_sec)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::State => procs.sort_by(|a, b| {
            let rank = |s: char| match s {
                'D' => 0,
                'R' => 1,
                _ => 2,
            };
            rank(a.state).cmp(&rank(b.state)).then_with(|| {
                b.cpu_pct
                    .partial_cmp(&a.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        }),
    }

    let header = Row::new(vec!["PID", "NAME", "STATE", "CPU %", "MAJFLT/s", "RSS (MB)"])
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = procs
        .into_iter()
        .take(area.height.saturating_sub(3) as usize)
        .map(|p| {
            let state_style = match p.state {
                'D' => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                'R' => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::DarkGray),
            };
            Row::new(vec![
                Cell::from(p.pid.to_string()),
                Cell::from(p.name),
                Cell::from(p.state.to_string()).style(state_style),
                Cell::from(format!("{:5.1}", p.cpu_pct)),
                Cell::from(format!("{:6.1}", p.maj_flt_per_sec)),
                Cell::from(format!("{:7.1}", p.rss_mb)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Min(20),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(
                " Top Processes (sort: {} — keys: c=CPU m=Mem f=MajFlt d=State/D) ",
                ui.proc_sort.label()
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(table, area);
}

fn draw_io_irq(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let cols = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    // Disks
    let disk_header = Row::new(vec![
        "DEVICE",
        "READ KB/s",
        "WRITE KB/s",
        "BUSY %",
        "IN-FLIGHT",
        "AVG AWAIT (ms)",
    ])
    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let disk_rows: Vec<Row> = ui
        .snap
        .disks
        .iter()
        .map(|d| {
            let style = if d.avg_await_ms > 30.0 || d.io_busy_pct > 80.0 {
                Style::default().fg(Color::Red)
            } else if d.avg_await_ms > 10.0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            Row::new(vec![
                Cell::from(d.name.clone()),
                Cell::from(format!("{:8.1}", d.read_kb_s)),
                Cell::from(format!("{:8.1}", d.write_kb_s)),
                Cell::from(format!("{:5.1}%", d.io_busy_pct)),
                Cell::from(d.in_flight.to_string()),
                Cell::from(format!("{:6.1}", d.avg_await_ms)),
            ])
            .style(style)
        })
        .collect();

    let disk_table = Table::new(
        disk_rows,
        [
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Min(14),
        ],
    )
    .header(disk_header)
    .block(
        Block::default()
            .title(" Block Devices (/proc/diskstats) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(disk_table, cols[0]);

    // IRQs
    let irq_header = Row::new(vec!["IRQ", "RATE / sec", "DEVICE / HANDLER"])
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let irq_rows: Vec<Row> = ui
        .snap
        .irqs
        .iter()
        .map(|irq| {
            let style = if irq.rate_per_sec > 5000.0 {
                Style::default().fg(Color::Red)
            } else if irq.rate_per_sec > 1000.0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            Row::new(vec![
                Cell::from(irq.irq.clone()),
                Cell::from(format!("{:9.0}", irq.rate_per_sec)),
                Cell::from(irq.label.clone()),
            ])
            .style(style)
        })
        .collect();

    let irq_table = Table::new(
        irq_rows,
        [
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(irq_header)
    .block(
        Block::default()
            .title(format!(
                " Top Active Interrupts (Total: {:.0} intr/s | {:.0} ctxt/s) ",
                ui.snap.cpu.intr_per_sec, ui.snap.cpu.ctxt_per_sec
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(irq_table, cols[1]);
}

fn draw_diagnostics(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let checks = ui.analyzer.health_checks(ui.snap, ui.jitter);
    let mut lines: Vec<Line> = Vec::new();

    for hc in checks {
        let badge = if hc.ok {
            Span::styled(
                " [OK]   ",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [WARN] ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        };
        lines.push(Line::from(vec![
            badge,
            Span::styled(hc.name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::raw(": "),
            Span::styled(
                hc.status,
                Style::default().fg(if hc.ok { Color::Cyan } else { Color::Yellow }),
            ),
        ]));
        if !hc.ok {
            lines.push(Line::from(vec![
                Span::raw("        ↳ "),
                Span::styled(hc.advice, Style::default().fg(Color::Gray)),
            ]));
        }
        lines.push(Line::raw(""));
    }

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Raspberry Pi Stutter Health Checklist (press 's' to save report) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, ui: &UiState<'_>) {
    let text = if let Some(msg) = ui.status_msg {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            " [q] Quit  [Tab/1-4] Views  [p] Pause  [r] Reset  [+/-] Jitter Thr  [s] Save Report",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(text), area);
}
