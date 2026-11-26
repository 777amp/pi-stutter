mod analyzer;
mod jitter;
mod metrics;
mod ui;

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::analyzer::Analyzer;
use crate::jitter::JitterProbe;
use crate::metrics::MetricsCollector;
use crate::ui::{ProcSort, Tab, UiState};

const SPARK_HISTORY: usize = 120;

#[derive(Parser, Debug)]
#[command(
    name = "pi-stutter",
    version,
    about = "Lightweight Rust TUI profiler for diagnosing Raspberry Pi micro-stutter & lag"
)]
struct Args {
    /// UI & metrics sampling interval in milliseconds
    #[arg(short, long, default_value_t = 500)]
    interval: u64,

    /// Wakeup jitter threshold (ms) to log a stutter event
    #[arg(short = 't', long, default_value_t = 10.0)]
    jitter_threshold: f64,

    /// Print a one-shot diagnostic report to stdout and exit (useful for SSH/scripts)
    #[arg(long)]
    snapshot: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let probe = JitterProbe::start();
    let mut collector = MetricsCollector::new();
    let mut analyzer = Analyzer::new(args.jitter_threshold);

    // Prime deltas with an initial read
    let _ = collector.collect();
    std::thread::sleep(Duration::from_millis(250));

    if args.snapshot {
        let snap = collector.collect();
        let jit = probe.take_snapshot();
        analyzer.analyze(&snap, &jit);
        println!("{}", format_report(&snap, &jit, &analyzer));
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, &probe, &mut collector, &mut analyzer, &args);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    probe: &JitterProbe,
    collector: &mut MetricsCollector,
    analyzer: &mut Analyzer,
    args: &Args,
) -> Result<()> {
    let tick_rate = Duration::from_millis(args.interval.max(100));
    let mut last_tick = Instant::now() - tick_rate;

    let mut tab = Tab::Overview;
    let mut paused = false;
    let mut proc_sort = ProcSort::Cpu;
    let mut status_msg: Option<(String, Instant)> = None;

    let mut snap = collector.collect();
    let mut jit = probe.take_snapshot();
    let mut jitter_hist: VecDeque<u64> = VecDeque::from(vec![0; SPARK_HISTORY]);
    let mut cpu_hist: VecDeque<u64> = VecDeque::from(vec![0; SPARK_HISTORY]);

    loop {
        if !paused && last_tick.elapsed() >= tick_rate {
            snap = collector.collect();
            jit = probe.take_snapshot();
            analyzer.analyze(&snap, &jit);

            if jitter_hist.len() >= SPARK_HISTORY {
                jitter_hist.pop_front();
            }
            jitter_hist.push_back((jit.window_max_ms * 1000.0) as u64);

            if cpu_hist.len() >= SPARK_HISTORY {
                cpu_hist.pop_front();
            }
            cpu_hist.push_back(snap.cpu.total_pct.round().clamp(0.0, 100.0) as u64);

            last_tick = Instant::now();
        }

        if let Some((_, ts)) = &status_msg {
            if ts.elapsed() > Duration::from_secs(4) {
                status_msg = None;
            }
        }

        let j_vec: Vec<u64> = jitter_hist.iter().copied().collect();
        let c_vec: Vec<u64> = cpu_hist.iter().copied().collect();

        terminal.draw(|f| {
            let ui_state = UiState {
                tab,
                paused,
                proc_sort,
                snap: &snap,
                jitter: &jit,
                analyzer,
                jitter_history_us: &j_vec,
                cpu_history: &c_vec,
                status_msg: status_msg.as_ref().map(|(s, _)| s.as_str()),
            };
            ui::draw(f, &ui_state);
        })?;

        let timeout = tick_rate
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
                    KeyCode::Tab => tab = tab.next(),
                    KeyCode::Char('1') => tab = Tab::Overview,
                    KeyCode::Char('2') => tab = Tab::Processes,
                    KeyCode::Char('3') => tab = Tab::IoIrq,
                    KeyCode::Char('4') => tab = Tab::Diagnostics,
                    KeyCode::Char('p') => paused = !paused,
                    KeyCode::Char('r') => {
                        probe.reset();
                        analyzer.reset();
                        jitter_hist.iter_mut().for_each(|v| *v = 0);
                        cpu_hist.iter_mut().for_each(|v| *v = 0);
                        status_msg = Some(("Reset peaks and stutter log.".into(), Instant::now()));
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        analyzer.jitter_threshold_ms = (analyzer.jitter_threshold_ms + 2.0).min(200.0);
                    }
                    KeyCode::Char('-') => {
                        analyzer.jitter_threshold_ms = (analyzer.jitter_threshold_ms - 2.0).max(1.0);
                    }
                    KeyCode::Char('c') if tab == Tab::Processes => proc_sort = ProcSort::Cpu,
                    KeyCode::Char('m') if tab == Tab::Processes => proc_sort = ProcSort::Memory,
                    KeyCode::Char('f') if tab == Tab::Processes => proc_sort = ProcSort::MajFaults,
                    KeyCode::Char('d') if tab == Tab::Processes => proc_sort = ProcSort::State,
                    KeyCode::Char('s') => {
                        let filename = format!(
                            "pi-stutter-report-{}.txt",
                            Local::now().format("%Y%m%d-%H%M%S")
                        );
                        let report = format_report(&snap, &jit, analyzer);
                        match fs::write(&filename, report) {
                            Ok(_) => {
                                status_msg = Some((
                                    format!("Saved diagnostic report to {filename}"),
                                    Instant::now(),
                                ));
                            }
                            Err(e) => {
                                status_msg =
                                    Some((format!("Failed to save report: {e}"), Instant::now()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn format_report(
    snap: &metrics::SystemSnapshot,
    jit: &jitter::JitterSnapshot,
    analyzer: &Analyzer,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "=== pi-stutter Diagnostic Report ({}) ===\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    out.push_str(&format!("Model: {}\n", snap.model));
    out.push_str(&format!(
        "CPUs: {} | Load: {:.2} {:.2} {:.2} | Governor: {}\n",
        snap.num_cpus, snap.load_avg.0, snap.load_avg.1, snap.load_avg.2, snap.governor
    ));
    if let Some(t) = snap.temp_c {
        out.push_str(&format!("Temperature: {t:.1}°C\n"));
    }
    if let Some(raw) = snap.throttle.raw {
        out.push_str(&format!("Throttle flags: 0x{raw:x}\n"));
    }
    out.push_str(&format!(
        "Scheduler Jitter: last={:.2}ms p95={:.2}ms p99={:.2}ms peak={:.2}ms\n\n",
        jit.last_ms, jit.p95_ms, jit.p99_ms, jit.peak_ms
    ));

    out.push_str("--- Health Checks ---\n");
    for hc in analyzer.health_checks(snap, jit) {
        let tag = if hc.ok { "OK" } else { "WARN" };
        out.push_str(&format!("[{tag}] {}: {}\n", hc.name, hc.status));
        if !hc.ok {
            out.push_str(&format!("      Advice: {}\n", hc.advice));
        }
    }

    if !analyzer.events.is_empty() {
        out.push_str("\n--- Recent Stutter Events ---\n");
        for ev in analyzer.events.iter().take(25) {
            out.push_str(&format!(
                "{} [{:?}] {:.1}ms {} — {}\n",
                ev.timestamp,
                ev.severity,
                ev.jitter_ms,
                ev.causes.join(","),
                ev.detail
            ));
        }
    }

    out
}
