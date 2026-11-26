use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, Wrap};
use ratatui::{Frame, Terminal};

use pi_stutter::attach::{AttachSnapshot, AttachTarget, ProcessAttacher};

#[derive(Parser, Debug)]
#[command(
    name = "pi-attach",
    version,
    about = "Attach to a laggy Raspberry Pi process (e.g. chromium, EasyCLIProxyAPI) to diagnose single-thread CPU, scheduler wait, D-state I/O, and wchan stalls"
)]
struct Args {
    /// Target process name or PID (e.g. `chromium`, `EasyCLIProxyAPI`, or `1234`)
    target: String,

    /// Sampling interval in milliseconds
    #[arg(short, long, default_value_t = 500)]
    interval: u64,

    /// Print a one-shot diagnostic report to stdout and exit
    #[arg(long)]
    snapshot: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut attacher = ProcessAttacher::new();
    attacher.attach(AttachTarget::parse(&args.target));

    // Prime deltas
    let _ = attacher.collect();
    std::thread::sleep(Duration::from_millis(300));

    if args.snapshot {
        if let Some(snap) = attacher.collect() {
            print_snapshot(&snap);
        }
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_tui(&mut terminal, &mut attacher, args.interval);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    attacher: &mut ProcessAttacher,
    interval_ms: u64,
) -> Result<()> {
    let tick = Duration::from_millis(interval_ms.max(100));
    let mut last_tick = Instant::now() - tick;
    let mut paused = false;
    let mut snap = attacher.collect().unwrap_or_default();

    loop {
        if !paused && last_tick.elapsed() >= tick {
            if let Some(s) = attacher.collect() {
                snap = s;
            }
            last_tick = Instant::now();
        }

        terminal.draw(|f| draw_ui(f, &snap, paused))?;

        let timeout = tick
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(100));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(());
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('p') => paused = !paused,
                    KeyCode::Char('r') => attacher.reset_peaks(),
                    _ => {}
                }
            }
        }
    }
}

fn draw_ui(f: &mut Frame, a: &AttachSnapshot, paused: bool) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(f.area());

    let status_span = if !a.alive {
        Span::styled(
            " WAITING FOR PROCESS ",
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
    } else if a.d_state_threads > 0 || a.hottest_thread_cpu_pct >= 80.0 || a.sched_wait_ms >= 20.0 {
        Span::styled(
            " LAG DETECTED ",
            Style::default()
                .bg(Color::Red)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            " ATTACHED ",
            Style::default()
                .bg(Color::Green)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " pi-attach ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ Target: "),
        Span::styled(
            &a.target_label,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        status_span,
        Span::raw(format!(
            " │ PIDs: {} │ Threads: {} │ CPU: {:.1}%{}",
            a.matched_pids.len(),
            a.total_threads,
            a.total_cpu_pct,
            if paused { " [PAUSED]" } else { "" }
        )),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(header, chunks[0]);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    let wait_color = if a.sched_wait_ms >= 20.0 {
        Color::Red
    } else if a.sched_wait_ms >= 5.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    f.render_widget(
        Sparkline::default()
            .block(
                Block::default()
                    .title(format!(
                        " Runqueue Wait: {:.1}ms (peak {:.1}ms) ",
                        a.sched_wait_ms, a.sched_wait_peak_ms
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(wait_color)),
            )
            .data(&a.wait_history_us)
            .style(Style::default().fg(wait_color)),
        top_cols[0],
    );

    let thr_color = if a.hottest_thread_cpu_pct >= 80.0 {
        Color::Red
    } else if a.hottest_thread_cpu_pct >= 50.0 {
        Color::Yellow
    } else {
        Color::Cyan
    };
    f.render_widget(
        Sparkline::default()
            .block(
                Block::default()
                    .title(format!(
                        " Hottest Thread: '{}' {:.0}% ",
                        a.hottest_thread_name, a.hottest_thread_cpu_pct
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(thr_color)),
            )
            .data(&a.cpu_history)
            .max(100)
            .style(Style::default().fg(thr_color)),
        top_cols[1],
    );

    let mid_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(chunks[2]);

    let stats_lines = vec![
        Line::from(format!(
            "RAM: {:.0}MB  Swap: {:.0}MB  FDs: {}  D-State: {}",
            a.rss_mb, a.swap_mb, a.open_fds, a.d_state_threads
        )),
        Line::from(format!(
            "Sched: run={:.1}ms  wait={:.1}ms  (peak {:.1}ms)",
            a.sched_run_ms, a.sched_wait_ms, a.sched_wait_peak_ms
        )),
        Line::from(format!(
            "CtxSw/s: vol={:.0}  invol={:.0}",
            a.vol_ctxt_per_sec, a.nonvol_ctxt_per_sec
        )),
        Line::from(format!(
            "Faults/s: min={:.0}  maj={:.1}",
            a.minflt_per_sec, a.majflt_per_sec
        )),
        Line::from(format!(
            "Disk I/O: R={:.0} KB/s  W={:.0} KB/s",
            a.io_read_kb_s, a.io_write_kb_s
        )),
        Line::from(format!(
            "Syscalls: R={:.0}/s ({:.0}KB)  W={:.0}/s ({:.0}KB)",
            a.io_syscr_s, a.io_rchar_kb_s, a.io_syscw_s, a.io_wchar_kb_s
        )),
    ];
    f.render_widget(
        Paragraph::new(stats_lines).block(
            Block::default()
                .title(" Process Internals ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        mid_cols[0],
    );

    let finding_lines: Vec<Line> = a
        .findings
        .iter()
        .flat_map(|fnd| {
            let color = match fnd.severity {
                "CRIT" => Color::Red,
                "WARN" => Color::Yellow,
                _ => Color::Cyan,
            };
            vec![
                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", fnd.severity),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        fnd.title.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("  {}", fnd.detail),
                    Style::default().fg(Color::Gray),
                )),
            ]
        })
        .collect();
    f.render_widget(
        Paragraph::new(finding_lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Why Is This App Laggy? ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
        mid_cols[1],
    );

    let thr_header = Row::new(vec![
        "TID",
        "PID",
        "THREAD NAME",
        "STATE",
        "CPU %",
        "KERNEL WAIT (WCHAN)",
    ])
    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let thr_rows: Vec<Row> = a
        .threads
        .iter()
        .take(chunks[3].height.saturating_sub(3) as usize)
        .map(|t| {
            let st_style = match t.state {
                'D' => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                'R' => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::DarkGray),
            };
            Row::new(vec![
                Cell::from(t.tid.to_string()),
                Cell::from(t.pid.to_string()),
                Cell::from(t.name.clone()),
                Cell::from(t.state.to_string()).style(st_style),
                Cell::from(format!("{:5.1}", t.cpu_pct)),
                Cell::from(t.wchan.clone()),
            ])
        })
        .collect();

    let thr_table = Table::new(
        thr_rows,
        [
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Min(20),
        ],
    )
    .header(thr_header)
    .block(
        Block::default()
            .title(" Per-Thread Breakdown (D-state & hottest threads first) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(thr_table, chunks[3]);

    f.render_widget(
        Paragraph::new(" [q] Quit  [p] Pause  [r] Reset Peaks")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[4],
    );
}

fn print_snapshot(a: &AttachSnapshot) {
    println!("=== pi-attach Snapshot: {} ===", a.target_label);
    if !a.alive {
        println!("Process not running.");
        return;
    }
    println!(
        "PIDs: {} | Threads: {} | Total CPU: {:.1}% | Hottest Thread: '{}' ({:.1}%)",
        a.matched_pids.len(),
        a.total_threads,
        a.total_cpu_pct,
        a.hottest_thread_name,
        a.hottest_thread_cpu_pct
    );
    println!(
        "Sched: run={:.1}ms wait={:.1}ms (peak {:.1}ms) | CtxSw: vol={:.0}/s invol={:.0}/s",
        a.sched_run_ms,
        a.sched_wait_ms,
        a.sched_wait_peak_ms,
        a.vol_ctxt_per_sec,
        a.nonvol_ctxt_per_sec
    );
    println!(
        "RAM: {:.0}MB Swap: {:.0}MB FDs: {} | Disk R={:.0}KB/s W={:.0}KB/s | Faults min={:.0}/s maj={:.1}/s",
        a.rss_mb,
        a.swap_mb,
        a.open_fds,
        a.io_read_kb_s,
        a.io_write_kb_s,
        a.minflt_per_sec,
        a.majflt_per_sec
    );
    println!("\n--- Diagnosis ---");
    for f in &a.findings {
        println!("[{}] {}: {}", f.severity, f.title, f.detail);
    }
}
