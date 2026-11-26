mod analyzer;
mod attach;
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

use pi_stutter::nap::{self, NapConfig};

use crate::analyzer::Analyzer;
use crate::attach::{AttachSnapshot, AttachTarget, ProcessAttacher};
use crate::jitter::JitterProbe;
use crate::metrics::{MetricsCollector, ProcessMetric};
use crate::ui::{ProcSort, Tab, UiState};

const SPARK_HISTORY: usize = 120;

#[derive(Parser, Debug)]
#[command(
    name = "pi-stutter",
    version,
    about = "Lightweight Rust TUI profiler, process lag debugger, and App Nap controller for Raspberry Pi"
)]
struct Args {
    /// UI & metrics sampling interval in milliseconds
    #[arg(short, long, default_value_t = 500)]
    interval: u64,

    /// Wakeup jitter threshold (ms) to log a stutter event
    #[arg(short = 't', long, default_value_t = 10.0)]
    jitter_threshold: f64,

    /// Attach directly to a process by PID or name (e.g. --attach chromium or --attach EasyCLIProxyAPI)
    #[arg(short = 'a', long)]
    attach: Option<String>,

    /// Print a one-shot diagnostic report to stdout and exit (useful for SSH/scripts)
    #[arg(long)]
    snapshot: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let probe = JitterProbe::start();
    let mut collector = MetricsCollector::new();
    let mut analyzer = Analyzer::new(args.jitter_threshold);
    let mut attacher = ProcessAttacher::new();

    if let Some(target_str) = &args.attach {
        attacher.attach(AttachTarget::parse(target_str));
    }

    // Prime deltas with an initial read
    let _ = collector.collect();
    let _ = attacher.collect();
    std::thread::sleep(Duration::from_millis(250));

    if args.snapshot {
        let snap = collector.collect();
        let jit = probe.take_snapshot();
        let attach_snap = attacher.collect();
        analyzer.analyze(&snap, &jit);
        println!(
            "{}",
            format_report(&snap, &jit, &analyzer, attach_snap.as_ref())
        );
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(
        &mut terminal,
        &probe,
        &mut collector,
        &mut analyzer,
        &mut attacher,
        &args,
    );

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

fn sort_processes(procs: &[ProcessMetric], sort: ProcSort) -> Vec<ProcessMetric> {
    let mut out = procs.to_vec();
    match sort {
        ProcSort::Cpu => out.sort_by(|a, b| {
            b.cpu_pct
                .partial_cmp(&a.cpu_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::Memory => out.sort_by(|a, b| {
            b.rss_mb
                .partial_cmp(&a.rss_mb)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::MajFaults => out.sort_by(|a, b| {
            b.maj_flt_per_sec
                .partial_cmp(&a.maj_flt_per_sec)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::State => out.sort_by(|a, b| {
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
    out
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    probe: &JitterProbe,
    collector: &mut MetricsCollector,
    analyzer: &mut Analyzer,
    attacher: &mut ProcessAttacher,
    args: &Args,
) -> Result<()> {
    let tick_rate = Duration::from_millis(args.interval.max(100));
    let mut last_tick = Instant::now() - tick_rate;

    let mut tab = if attacher.is_attached() {
        Tab::Attach
    } else {
        Tab::Overview
    };
    let mut paused = false;
    let mut proc_sort = ProcSort::Cpu;
    let mut proc_selected: usize = 0;
    let mut nap_selected: usize = 0;
    let mut status_msg: Option<(String, Instant)> = None;

    let mut snap = collector.collect();
    let mut jit = probe.take_snapshot();
    let mut attach_snap = attacher.collect();
    let mut nap_cfg = NapConfig::load();
    let mut nap_status = nap::read_live_status();
    let mut nap_apps = nap::list_running_apps(
        &nap_cfg,
        nap_status.as_ref().and_then(|s| s.focused_pid),
    );

    let mut jitter_hist: VecDeque<u64> = VecDeque::from(vec![0; SPARK_HISTORY]);
    let mut cpu_hist: VecDeque<u64> = VecDeque::from(vec![0; SPARK_HISTORY]);

    loop {
        if !paused && last_tick.elapsed() >= tick_rate {
            snap = collector.collect();
            jit = probe.take_snapshot();
            analyzer.analyze(&snap, &jit);
            attach_snap = attacher.collect();

            if tab == Tab::AppNap {
                nap_cfg = NapConfig::load();
                nap_status = nap::read_live_status();
                nap_apps = nap::list_running_apps(
                    &nap_cfg,
                    nap_status.as_ref().and_then(|s| s.focused_pid),
                );
            }

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

        let sorted_procs = sort_processes(&snap.processes, proc_sort);
        if !sorted_procs.is_empty() && proc_selected >= sorted_procs.len() {
            proc_selected = sorted_procs.len() - 1;
        }
        if !nap_apps.is_empty() && nap_selected >= nap_apps.len() {
            nap_selected = nap_apps.len() - 1;
        }

        let j_vec: Vec<u64> = jitter_hist.iter().copied().collect();
        let c_vec: Vec<u64> = cpu_hist.iter().copied().collect();

        terminal.draw(|f| {
            let ui_state = UiState {
                tab,
                paused,
                proc_sort,
                proc_selected,
                sorted_procs: &sorted_procs,
                snap: &snap,
                jitter: &jit,
                analyzer,
                jitter_history_us: &j_vec,
                cpu_history: &c_vec,
                attach_snap: attach_snap.as_ref(),
                nap_cfg: &nap_cfg,
                nap_status: nap_status.as_ref(),
                nap_apps: &nap_apps,
                nap_selected,
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
                    KeyCode::Tab => {
                        tab = tab.next();
                        if tab == Tab::AppNap {
                            nap_cfg = NapConfig::load();
                            nap_status = nap::read_live_status();
                            nap_apps = nap::list_running_apps(
                                &nap_cfg,
                                nap_status.as_ref().and_then(|s| s.focused_pid),
                            );
                        }
                    }
                    KeyCode::Char('1') => tab = Tab::Overview,
                    KeyCode::Char('2') => tab = Tab::Processes,
                    KeyCode::Char('3') => tab = Tab::IoIrq,
                    KeyCode::Char('4') => tab = Tab::Diagnostics,
                    KeyCode::Char('5') => tab = Tab::Attach,
                    KeyCode::Char('6') => {
                        tab = Tab::AppNap;
                        nap_cfg = NapConfig::load();
                        nap_status = nap::read_live_status();
                        nap_apps = nap::list_running_apps(
                            &nap_cfg,
                            nap_status.as_ref().and_then(|s| s.focused_pid),
                        );
                    }
                    KeyCode::Char('p') => paused = !paused,
                    KeyCode::Char('r') => {
                        probe.reset();
                        analyzer.reset();
                        attacher.reset_peaks();
                        jitter_hist.iter_mut().for_each(|v| *v = 0);
                        cpu_hist.iter_mut().for_each(|v| *v = 0);
                        status_msg = Some(("Reset peaks and stutter log.".into(), Instant::now()));
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        analyzer.jitter_threshold_ms =
                            (analyzer.jitter_threshold_ms + 2.0).min(200.0);
                    }
                    KeyCode::Char('-') => {
                        analyzer.jitter_threshold_ms =
                            (analyzer.jitter_threshold_ms - 2.0).max(1.0);
                    }

                    // Processes tab controls
                    KeyCode::Down | KeyCode::Char('j') if tab == Tab::Processes => {
                        if proc_selected + 1 < sorted_procs.len() {
                            proc_selected += 1;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') if tab == Tab::Processes => {
                        proc_selected = proc_selected.saturating_sub(1);
                    }
                    KeyCode::Enter | KeyCode::Char('a') if tab == Tab::Processes => {
                        if let Some(p) = sorted_procs.get(proc_selected) {
                            let target = AttachTarget::Name(p.name.clone());
                            attacher.attach(target);
                            attach_snap = attacher.collect();
                            tab = Tab::Attach;
                            status_msg = Some((
                                format!("Attached to '{}' (tracking process tree & threads)", p.name),
                                Instant::now(),
                            ));
                        }
                    }
                    KeyCode::Char('A') if tab == Tab::Processes => {
                        if let Some(p) = sorted_procs.get(proc_selected) {
                            attacher.attach(AttachTarget::Pid(p.pid));
                            attach_snap = attacher.collect();
                            tab = Tab::Attach;
                            status_msg = Some((
                                format!("Attached to PID {} ({})", p.pid, p.name),
                                Instant::now(),
                            ));
                        }
                    }
                    KeyCode::Char('n') if tab == Tab::Processes => {
                        if let Some(p) = sorted_procs.get(proc_selected) {
                            let mut cfg = NapConfig::load();
                            match cfg.toggle_app(&p.name) {
                                Ok(true) => {
                                    status_msg = Some((
                                        format!("App Nap ENABLED for '{}' (freezes when unfocused)", p.name),
                                        Instant::now(),
                                    ));
                                }
                                Ok(false) => {
                                    let _ = nap::thaw_user_apps(Some(&p.name));
                                    status_msg = Some((
                                        format!("App Nap DISABLED for '{}' (thawed)", p.name),
                                        Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    status_msg = Some((format!("{e}"), Instant::now()));
                                }
                            }
                        }
                    }
                    KeyCode::Char('c') if tab == Tab::Processes => proc_sort = ProcSort::Cpu,
                    KeyCode::Char('m') if tab == Tab::Processes => proc_sort = ProcSort::Memory,
                    KeyCode::Char('f') if tab == Tab::Processes => proc_sort = ProcSort::MajFaults,
                    KeyCode::Char('d') if tab == Tab::Processes => proc_sort = ProcSort::State,

                    // Attach tab controls
                    KeyCode::Char('x') if tab == Tab::Attach => {
                        attacher.detach();
                        attach_snap = None;
                        status_msg = Some(("Detached from process.".into(), Instant::now()));
                    }

                    // App Nap tab controls
                    KeyCode::Down | KeyCode::Char('j') if tab == Tab::AppNap => {
                        if nap_selected + 1 < nap_apps.len() {
                            nap_selected += 1;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') if tab == Tab::AppNap => {
                        nap_selected = nap_selected.saturating_sub(1);
                    }
                    KeyCode::Char(' ') | KeyCode::Enter if tab == Tab::AppNap => {
                        if let Some(app) = nap_apps.get(nap_selected) {
                            let app_name = app.name.clone();
                            match nap_cfg.toggle_app(&app_name) {
                                Ok(true) => {
                                    status_msg = Some((
                                        format!("App Nap ENABLED for '{app_name}'"),
                                        Instant::now(),
                                    ));
                                }
                                Ok(false) => {
                                    let _ = nap::thaw_user_apps(Some(&app_name));
                                    status_msg = Some((
                                        format!("App Nap DISABLED for '{app_name}' (thawed)"),
                                        Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    status_msg = Some((format!("{e}"), Instant::now()));
                                }
                            }
                            nap_apps = nap::list_running_apps(
                                &nap_cfg,
                                nap_status.as_ref().and_then(|s| s.focused_pid),
                            );
                        }
                    }
                    KeyCode::Char('u') if tab == Tab::AppNap => {
                        let n = nap::thaw_user_apps(None);
                        status_msg = Some((
                            format!("Sent SIGCONT to {n} stopped process(es)."),
                            Instant::now(),
                        ));
                    }
                    KeyCode::Char('a') if tab == Tab::AppNap => {
                        if let Some(app) = nap_apps.get(nap_selected) {
                            let app_name = app.name.clone();
                            attacher.attach(AttachTarget::Name(app_name.clone()));
                            attach_snap = attacher.collect();
                            tab = Tab::Attach;
                            status_msg = Some((
                                format!("Attached to '{app_name}'"),
                                Instant::now(),
                            ));
                        }
                    }

                    KeyCode::Char('s') => {
                        let filename = format!(
                            "pi-stutter-report-{}.txt",
                            Local::now().format("%Y%m%d-%H%M%S")
                        );
                        let report =
                            format_report(&snap, &jit, analyzer, attach_snap.as_ref());
                        match fs::write(&filename, report) {
                            Ok(_) => {
                                status_msg = Some((
                                    format!("Saved diagnostic report to {filename}"),
                                    Instant::now(),
                                ));
                            }
                            Err(e) => {
                                status_msg = Some((
                                    format!("Failed to save report: {e}"),
                                    Instant::now(),
                                ));
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
    attach: Option<&AttachSnapshot>,
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

    if let Some(a) = attach {
        out.push_str(&format!("\n--- Attached Process: {} ---\n", a.target_label));
        if a.alive {
            out.push_str(&format!(
                "PIDs: {} | Threads: {} | Total CPU: {:.1}% | Hottest Thread: '{}' ({:.1}%)\n",
                a.matched_pids.len(),
                a.total_threads,
                a.total_cpu_pct,
                a.hottest_thread_name,
                a.hottest_thread_cpu_pct
            ));
            out.push_str(&format!(
                "Sched: run={:.1}ms wait={:.1}ms (peak {:.1}ms) | CtxSw: vol={:.0}/s invol={:.0}/s\n",
                a.sched_run_ms, a.sched_wait_ms, a.sched_wait_peak_ms, a.vol_ctxt_per_sec, a.nonvol_ctxt_per_sec
            ));
            out.push_str(&format!(
                "RAM: {:.0}MB Swap: {:.0}MB FDs: {} | Disk R={:.0}KB/s W={:.0}KB/s | Faults min={:.0}/s maj={:.1}/s\n",
                a.rss_mb, a.swap_mb, a.open_fds, a.io_read_kb_s, a.io_write_kb_s, a.minflt_per_sec, a.majflt_per_sec
            ));
            for f in &a.findings {
                out.push_str(&format!("  [{}] {}: {}\n", f.severity, f.title, f.detail));
            }
        } else {
            out.push_str("  (Process not running)\n");
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
