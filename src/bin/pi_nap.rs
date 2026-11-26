use std::collections::BTreeSet;
use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::{Frame, Terminal};

use pi_stutter::nap::{
    config_path, detect_focused_pid, install_systemd_user_service, list_running_apps,
    read_live_status, thaw_user_apps, NapConfig, NapDaemon, NapLiveStatus, RunningApp,
};

#[derive(Parser, Debug)]
#[command(
    name = "pi-nap",
    version,
    about = "macOS App Nap-style background app freezer for Raspberry Pi (opt-in per app, terminals protected)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Launch interactive TUI to toggle apps and edit settings (default when no subcommand is given)
    Tui,
    /// One-shot setup: initialize config, check window-focus tools, install & start systemd user service
    Setup,
    /// Run the background App Nap daemon (automatically thaws all apps on exit/Ctrl+C)
    Run {
        /// Print freeze/thaw events to stderr
        #[arg(short, long)]
        verbose: bool,
    },
    /// Toggle an app's freeze-when-unfocused opt-in (e.g. `pi-nap toggle chromium`)
    Toggle { app: String },
    /// Enable freeze-when-unfocused for an app
    Enable { app: String },
    /// Disable freeze-when-unfocused for an app (and immediately thaw it)
    Disable { app: String },
    /// List running user apps and their freeze opt-in / protected status
    List,
    /// Show daemon status, focused window, and currently frozen apps
    Status,
    /// Immediately unfreeze one app or all stopped user apps
    Thaw {
        /// Optional app name to thaw (omit to thaw all)
        app: Option<String>,
    },
    /// View or update grace period and pulse settings
    Config {
        /// Seconds unfocused before freezing (default: 15)
        #[arg(long)]
        grace_secs: Option<u64>,
        /// Brief wakeup pulse interval in seconds so apps don't break (0 = disabled, default: 20)
        #[arg(long)]
        pulse_interval_secs: Option<u64>,
        /// Pulse wakeup duration in ms (default: 250)
        #[arg(long)]
        pulse_duration_ms: Option<u64>,
        /// Daemon polling interval in ms (default: 1000)
        #[arg(long)]
        poll_interval_ms: Option<u64>,
    },
    /// Install a systemd user service (~/.config/systemd/user/pi-nap.service)
    InstallService,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => run_tui(),
        Commands::Setup => run_setup(),
        Commands::Run { verbose } => {
            let stop = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
            signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
            signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&stop))?;

            let mut daemon = NapDaemon::new();
            let cfg = NapConfig::load();
            eprintln!(
                "[pi-nap] started (config: {}, opted-in apps: {}, grace: {}s, pulse: {}s/{}ms)",
                config_path().display(),
                cfg.enabled_apps.len(),
                cfg.grace_secs,
                cfg.pulse_interval_secs,
                cfg.pulse_duration_ms
            );
            if cfg.enabled_apps.is_empty() {
                eprintln!("[pi-nap] No apps enabled yet (opt-in by default). Run `pi-nap` to open the TUI and select apps.");
            }

            while !stop.load(Ordering::Relaxed) {
                daemon.tick(verbose);
                let step = daemon.poll_interval();
                let mut slept = Duration::ZERO;
                while slept < step && !stop.load(Ordering::Relaxed) {
                    let chunk = Duration::from_millis(100).min(step - slept);
                    std::thread::sleep(chunk);
                    slept += chunk;
                }
            }

            eprintln!("[pi-nap] shutting down — thawing all frozen apps...");
            daemon.thaw_all(true);
            Ok(())
        }
        Commands::Toggle { app } => {
            let mut cfg = NapConfig::load();
            let enabled = cfg.toggle_app(&app)?;
            if enabled {
                println!("Enabled App Nap freezing for '{app}' (will freeze when unfocused).");
            } else {
                let thawed = thaw_user_apps(Some(&app));
                println!("Disabled App Nap freezing for '{app}' (thawed {thawed} process(es)).");
            }
            Ok(())
        }
        Commands::Enable { app } => {
            let mut cfg = NapConfig::load();
            cfg.set_enabled(&app, true)?;
            println!("Enabled App Nap freezing for '{app}'.");
            Ok(())
        }
        Commands::Disable { app } => {
            let mut cfg = NapConfig::load();
            cfg.set_enabled(&app, false)?;
            let thawed = thaw_user_apps(Some(&app));
            println!("Disabled App Nap freezing for '{app}' (thawed {thawed} process(es)).");
            Ok(())
        }
        Commands::List => {
            let cfg = NapConfig::load();
            let (focused_pid, backend) = detect_focused_pid();
            let apps = list_running_apps(&cfg, focused_pid);
            println!(
                "Focus backend: {backend} | Config: {}",
                config_path().display()
            );
            println!("{:<24} {:<12} {:<8} {}", "APP", "STATUS", "PIDS", "NOTES");
            println!("{}", "-".repeat(64));
            for a in apps {
                let status = if a.protected {
                    "PROTECTED"
                } else if a.enabled {
                    "ENABLED"
                } else {
                    "off"
                };
                let mut notes = Vec::new();
                if a.focused {
                    notes.push("FOCUSED");
                }
                if a.any_stopped {
                    notes.push("FROZEN");
                }
                println!(
                    "{:<24} {:<12} {:<8} {}",
                    a.name,
                    status,
                    a.pids.len(),
                    notes.join(" ")
                );
            }
            Ok(())
        }
        Commands::Status => {
            let cfg = NapConfig::load();
            println!("Config: {}", config_path().display());
            println!(
                "Opted-in apps ({}): {}",
                cfg.enabled_apps.len(),
                if cfg.enabled_apps.is_empty() {
                    "(none — opt-in with `pi-nap` TUI or `pi-nap toggle <app>`)".into()
                } else {
                    cfg.enabled_apps.iter().cloned().collect::<Vec<_>>().join(", ")
                }
            );
            println!(
                "Grace: {}s | Pulse: every {}s for {}ms | Poll: {}ms",
                cfg.grace_secs, cfg.pulse_interval_secs, cfg.pulse_duration_ms, cfg.poll_interval_ms
            );
            match read_live_status() {
                Some(st) => {
                    println!(
                        "Daemon: RUNNING (pid={}, focus_backend={})",
                        st.daemon_pid, st.focus_backend
                    );
                    println!(
                        "Focused: {} (pid={:?})",
                        st.focused_app.as_deref().unwrap_or("none"),
                        st.focused_pid
                    );
                    if st.frozen_apps.is_empty() {
                        println!("Currently frozen: none");
                    } else {
                        println!("Currently frozen ({} pids):", st.total_frozen_pids);
                        for (app, pids) in &st.frozen_apps {
                            println!("  - {app}: {} pid(s)", pids.len());
                        }
                    }
                }
                None => {
                    println!("Daemon: NOT RUNNING (run `pi-nap setup` or `pi-nap run`)");
                }
            }
            Ok(())
        }
        Commands::Thaw { app } => {
            let n = thaw_user_apps(app.as_deref());
            match app {
                Some(a) => println!("Sent SIGCONT to {n} process(es) matching '{a}'."),
                None => println!("Sent SIGCONT to {n} stopped process(es)."),
            }
            Ok(())
        }
        Commands::Config {
            grace_secs,
            pulse_interval_secs,
            pulse_duration_ms,
            poll_interval_ms,
        } => {
            let mut cfg = NapConfig::load();
            if let Some(g) = grace_secs {
                cfg.grace_secs = g.max(1);
            }
            if let Some(pi) = pulse_interval_secs {
                cfg.pulse_interval_secs = pi;
            }
            if let Some(pd) = pulse_duration_ms {
                cfg.pulse_duration_ms = pd.clamp(50, 2000);
            }
            if let Some(pm) = poll_interval_ms {
                cfg.poll_interval_ms = pm.clamp(250, 5000);
            }
            cfg.save()?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        Commands::InstallService => {
            let path = install_systemd_user_service()?;
            println!("Installed systemd user unit: {}", path.display());
            println!("Enable and start it with:");
            println!("  systemctl --user daemon-reload");
            println!("  systemctl --user enable --now pi-nap");
            Ok(())
        }
    }
}

fn run_setup() -> Result<()> {
    println!("=== pi-nap Setup ===");

    // 1. Initialize config file (preserving any existing settings)
    let cfg = NapConfig::load();
    cfg.save()?;
    println!("[1/4] Config ready: {}", config_path().display());
    println!(
        "      Opted-in apps: {} (all apps start unfrozen by default; terminals are always protected)",
        cfg.enabled_apps.len()
    );

    // 2. Check window focus backend
    let (focused_pid, backend) = detect_focused_pid();
    if backend != "none" {
        println!(
            "[2/4] Window focus backend detected: {backend} (focused pid: {:?})",
            focused_pid
        );
    } else {
        let has_xdotool = Command::new("which")
            .arg("xdotool")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let has_xprop = Command::new("which")
            .arg("xprop")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if has_xdotool || has_xprop {
            println!("[2/4] Window focus tools installed (xdotool/xprop), waiting for active desktop session.");
        } else {
            println!("[2/4] Note: no X11/Wayland focus tool detected yet.");
            println!("      For Raspberry Pi desktop (X11/XWayland), install xdotool:");
            println!("        sudo apt-get update && sudo apt-get install -y xdotool");
        }
    }

    // 3. Install systemd user unit
    let unit_path = install_systemd_user_service()?;
    println!("[3/4] Installed systemd user service: {}", unit_path.display());

    // 4. Reload & enable+start via systemctl --user
    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", "pi-nap"])
        .status();

    match (reload, enable) {
        (Ok(r), Ok(e)) if r.success() && e.success() => {
            println!("[4/4] Service enabled and started via systemd (`pi-nap.service`).");
        }
        _ => {
            println!("[4/4] Could not auto-start systemd user service (common over non-login SSH or containers).");
            println!("      On your Pi desktop terminal, run:");
            println!("        systemctl --user daemon-reload && systemctl --user enable --now pi-nap");
            println!("      Or run directly in the background:");
            println!("        pi-nap run -v");
        }
    }

    println!("\nAll set! Run `pi-nap` (or `pi-nap tui`) to pick which apps to freeze and adjust settings.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NapTab {
    Apps = 0,
    Settings = 1,
}

fn build_app_list(cfg: &NapConfig, focused_pid: Option<i32>) -> Vec<RunningApp> {
    let mut apps = list_running_apps(cfg, focused_pid);
    let present: BTreeSet<String> = apps.iter().map(|a| a.name.to_lowercase()).collect();

    // Also include opted-in apps that aren't currently running so the user can toggle them off
    for enabled_name in &cfg.enabled_apps {
        if !present.contains(&enabled_name.to_lowercase()) {
            apps.push(RunningApp {
                name: enabled_name.clone(),
                pids: Vec::new(),
                any_stopped: false,
                protected: false,
                enabled: true,
                focused: false,
            });
        }
    }

    apps.sort_by(|a, b| {
        b.enabled
            .cmp(&a.enabled)
            .then_with(|| a.protected.cmp(&b.protected))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    apps
}

fn run_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_tui_loop(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

fn run_tui_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let tick_rate = Duration::from_millis(800);
    let mut last_tick = Instant::now() - tick_rate;

    let mut tab = NapTab::Apps;
    let mut cfg = NapConfig::load();
    let mut status = read_live_status();
    let mut apps = build_app_list(&cfg, status.as_ref().and_then(|s| s.focused_pid));
    let mut app_selected: usize = 0;
    let mut setting_selected: usize = 0;
    let mut add_input: Option<String> = None;
    let mut toast: Option<(String, Instant)> = None;

    loop {
        if last_tick.elapsed() >= tick_rate {
            status = read_live_status();
            apps = build_app_list(&cfg, status.as_ref().and_then(|s| s.focused_pid));
            last_tick = Instant::now();
        }

        if let Some((_, ts)) = &toast {
            if ts.elapsed() > Duration::from_secs(4) {
                toast = None;
            }
        }

        if !apps.is_empty() && app_selected >= apps.len() {
            app_selected = apps.len() - 1;
        }

        terminal.draw(|f| {
            draw_nap_tui(
                f,
                tab,
                &cfg,
                status.as_ref(),
                &apps,
                app_selected,
                setting_selected,
                add_input.as_deref(),
                toast.as_ref().map(|(s, _)| s.as_str()),
            );
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

                // Handle inline "add app by name" input modal
                if let Some(buf) = add_input.as_mut() {
                    match key.code {
                        KeyCode::Esc => {
                            add_input = None;
                        }
                        KeyCode::Enter => {
                            let name = buf.trim().to_string();
                            add_input = None;
                            if !name.is_empty() {
                                match cfg.set_enabled(&name, true) {
                                    Ok(_) => {
                                        toast = Some((
                                            format!("Enabled App Nap for '{name}' (saved)"),
                                            Instant::now(),
                                        ));
                                        apps = build_app_list(
                                            &cfg,
                                            status.as_ref().and_then(|s| s.focused_pid),
                                        );
                                    }
                                    Err(e) => {
                                        toast = Some((format!("{e}"), Instant::now()));
                                    }
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        KeyCode::Char(c) => {
                            if buf.len() < 48 {
                                buf.push(c);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Tab => {
                        tab = match tab {
                            NapTab::Apps => NapTab::Settings,
                            NapTab::Settings => NapTab::Apps,
                        };
                    }
                    KeyCode::Char('1') => tab = NapTab::Apps,
                    KeyCode::Char('2') => tab = NapTab::Settings,

                    // Apps tab
                    KeyCode::Down | KeyCode::Char('j') if tab == NapTab::Apps => {
                        if app_selected + 1 < apps.len() {
                            app_selected += 1;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') if tab == NapTab::Apps => {
                        app_selected = app_selected.saturating_sub(1);
                    }
                    KeyCode::Char(' ') | KeyCode::Enter if tab == NapTab::Apps => {
                        if let Some(app) = apps.get(app_selected) {
                            let name = app.name.clone();
                            match cfg.toggle_app(&name) {
                                Ok(true) => {
                                    toast = Some((
                                        format!("Enabled freeze-when-unfocused for '{name}'"),
                                        Instant::now(),
                                    ));
                                }
                                Ok(false) => {
                                    let n = thaw_user_apps(Some(&name));
                                    toast = Some((
                                        format!("Disabled '{name}' (thawed {n} pid(s))"),
                                        Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    toast = Some((format!("{e}"), Instant::now()));
                                }
                            }
                            apps = build_app_list(
                                &cfg,
                                status.as_ref().and_then(|s| s.focused_pid),
                            );
                        }
                    }
                    KeyCode::Char('a') if tab == NapTab::Apps => {
                        add_input = Some(String::new());
                    }
                    KeyCode::Char('u') => {
                        let n = thaw_user_apps(None);
                        toast = Some((
                            format!("Sent SIGCONT to {n} stopped process(es)."),
                            Instant::now(),
                        ));
                    }

                    // Settings tab
                    KeyCode::Down | KeyCode::Char('j') if tab == NapTab::Settings => {
                        setting_selected = (setting_selected + 1).min(3);
                    }
                    KeyCode::Up | KeyCode::Char('k') if tab == NapTab::Settings => {
                        setting_selected = setting_selected.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=')
                        if tab == NapTab::Settings =>
                    {
                        adjust_setting(&mut cfg, setting_selected, true);
                        if let Err(e) = cfg.save() {
                            toast = Some((format!("Save failed: {e}"), Instant::now()));
                        } else {
                            toast = Some(("Saved settings (daemon hot-reloads automatically).".into(), Instant::now()));
                        }
                    }
                    KeyCode::Left | KeyCode::Char('-') if tab == NapTab::Settings => {
                        adjust_setting(&mut cfg, setting_selected, false);
                        if let Err(e) = cfg.save() {
                            toast = Some((format!("Save failed: {e}"), Instant::now()));
                        } else {
                            toast = Some(("Saved settings (daemon hot-reloads automatically).".into(), Instant::now()));
                        }
                    }
                    KeyCode::Char('r') if tab == NapTab::Settings => {
                        let keep_apps = cfg.enabled_apps.clone();
                        cfg = NapConfig::default();
                        cfg.enabled_apps = keep_apps;
                        let _ = cfg.save();
                        toast = Some(("Reset timing settings to defaults.".into(), Instant::now()));
                    }
                    _ => {}
                }
            }
        }
    }
}

fn adjust_setting(cfg: &mut NapConfig, idx: usize, inc: bool) {
    match idx {
        0 => {
            // grace_secs (1..=600, step 5s or 1s near bottom)
            if inc {
                cfg.grace_secs = if cfg.grace_secs < 5 {
                    cfg.grace_secs + 1
                } else {
                    (cfg.grace_secs + 5).min(600)
                };
            } else {
                cfg.grace_secs = if cfg.grace_secs <= 5 {
                    cfg.grace_secs.saturating_sub(1).max(1)
                } else {
                    cfg.grace_secs.saturating_sub(5).max(1)
                };
            }
        }
        1 => {
            // pulse_interval_secs (0..=600, step 5s)
            if inc {
                cfg.pulse_interval_secs = (cfg.pulse_interval_secs + 5).min(600);
            } else {
                cfg.pulse_interval_secs = cfg.pulse_interval_secs.saturating_sub(5);
            }
        }
        2 => {
            // pulse_duration_ms (50..=2000, step 50ms)
            if inc {
                cfg.pulse_duration_ms = (cfg.pulse_duration_ms + 50).min(2000);
            } else {
                cfg.pulse_duration_ms = cfg.pulse_duration_ms.saturating_sub(50).max(50);
            }
        }
        3 => {
            // poll_interval_ms (250..=5000, step 250ms)
            if inc {
                cfg.poll_interval_ms = (cfg.poll_interval_ms + 250).min(5000);
            } else {
                cfg.poll_interval_ms = cfg.poll_interval_ms.saturating_sub(250).max(250);
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_nap_tui(
    f: &mut Frame,
    tab: NapTab,
    cfg: &NapConfig,
    status: Option<&NapLiveStatus>,
    apps: &[RunningApp],
    app_selected: usize,
    setting_selected: usize,
    add_input: Option<&str>,
    toast: Option<&str>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(3), // Tabs
            Constraint::Min(10),   // Body
            Constraint::Length(1), // Footer
        ])
        .split(f.area());

    let daemon_span = match status {
        Some(st) => Span::styled(
            format!(" DAEMON RUNNING (pid {}) ", st.daemon_pid),
            Style::default()
                .bg(Color::Green)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        None => Span::styled(
            " DAEMON STOPPED (run `pi-nap setup`) ",
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let focused_str = status
        .and_then(|s| s.focused_app.as_deref())
        .unwrap_or("n/a");
    let frozen_cnt = status.map(|s| s.total_frozen_pids).unwrap_or(0);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " pi-nap ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ "),
        daemon_span,
        Span::raw(format!(
            " │ Opted-in: {} │ Focused: {} │ Frozen PIDs: {}",
            cfg.enabled_apps.len(),
            focused_str,
            frozen_cnt
        )),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(header, chunks[0]);

    let titles = vec![
        Line::from(Span::styled("1: Apps", Style::default().fg(Color::White))),
        Line::from(Span::styled(
            "2: Settings",
            Style::default().fg(Color::White),
        )),
    ];
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .select(tab as usize)
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
    f.render_widget(tabs, chunks[1]);

    match tab {
        NapTab::Apps => draw_apps_tab(f, chunks[2], apps, app_selected, add_input),
        NapTab::Settings => draw_settings_tab(f, chunks[2], cfg, setting_selected),
    }

    let footer_text = if let Some(msg) = toast {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else if add_input.is_some() {
        Line::from(Span::styled(
            " Type app name (e.g. chromium) and press Enter to enable | Esc to cancel",
            Style::default().fg(Color::Yellow),
        ))
    } else {
        match tab {
            NapTab::Apps => Line::from(Span::styled(
                " [↑/↓] Select  [Space/Enter] Toggle Freeze  [a] Add App Name  [u] Thaw All  [Tab/2] Settings  [q] Quit",
                Style::default().fg(Color::DarkGray),
            )),
            NapTab::Settings => Line::from(Span::styled(
                " [↑/↓] Select Setting  [←/→ or -/+] Change Value  [r] Reset Defaults  [Tab/1] Apps  [q] Quit",
                Style::default().fg(Color::DarkGray),
            )),
        }
    };
    f.render_widget(Paragraph::new(footer_text), chunks[3]);
}

fn draw_apps_tab(
    f: &mut Frame,
    area: Rect,
    apps: &[RunningApp],
    selected: usize,
    add_input: Option<&str>,
) {
    let chunks = if add_input.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(3)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6)])
            .split(area)
    };

    let header = Row::new(vec!["", "APP NAME", "NAP STATUS", "PIDS", "LIVE STATE"])
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let max_rows = chunks[0].height.saturating_sub(3) as usize;
    let scroll = if selected >= max_rows && max_rows > 0 {
        selected - max_rows + 1
    } else {
        0
    };

    let rows: Vec<Row> = apps
        .iter()
        .enumerate()
        .skip(scroll)
        .take(max_rows)
        .map(|(idx, app)| {
            let is_sel = idx == selected;
            let (opt_text, opt_style) = if app.protected {
                ("[PROTECTED]", Style::default().fg(Color::DarkGray))
            } else if app.enabled {
                (
                    "[ON — Freeze]",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("[off]", Style::default().fg(Color::Gray))
            };

            let live = if app.pids.is_empty() {
                Span::styled("(not running)", Style::default().fg(Color::DarkGray))
            } else if app.any_stopped {
                Span::styled(
                    "❄ FROZEN",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )
            } else if app.focused {
                Span::styled("● FOCUSED", Style::default().fg(Color::Cyan))
            } else {
                Span::styled("active", Style::default().fg(Color::DarkGray))
            };

            let row_style = if is_sel {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(if is_sel { "▶" } else { " " }),
                Cell::from(app.name.clone()),
                Cell::from(opt_text).style(opt_style),
                Cell::from(app.pids.len().to_string()),
                Cell::from(Line::from(live)),
            ])
            .style(row_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Min(24),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Length(16),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Select Apps to Freeze When Unfocused (Terminals & system apps are always PROTECTED) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(table, chunks[0]);

    if let Some(buf) = add_input {
        let prompt = Paragraph::new(format!(" App name to enable: {buf}█"))
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .block(
                Block::default()
                    .title(" Add App Manually (even if not running yet) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            );
        f.render_widget(prompt, chunks[1]);
    }
}

fn draw_settings_tab(f: &mut Frame, area: Rect, cfg: &NapConfig, selected: usize) {
    let items: [(&str, String, &str); 4] = [
        (
            "Grace Period (grace_secs)",
            format!("{} s", cfg.grace_secs),
            "How long an app must stay unfocused before pi-nap freezes it. Higher = fewer freezes on quick alt-tabs.",
        ),
        (
            "Wake Pulse Interval (pulse_interval_secs)",
            if cfg.pulse_interval_secs == 0 {
                "OFF (pure freeze)".into()
            } else {
                format!("every {} s", cfg.pulse_interval_secs)
            },
            "Periodically wakes frozen apps briefly so network sockets, timers, and watchdogs don't break. Set to 0 to disable.",
        ),
        (
            "Wake Pulse Duration (pulse_duration_ms)",
            format!("{} ms", cfg.pulse_duration_ms),
            "How long (in milliseconds) a frozen app stays awake during each periodic pulse.",
        ),
        (
            "Daemon Poll Interval (poll_interval_ms)",
            format!("{} ms", cfg.poll_interval_ms),
            "How often pi-nap checks which window is focused. Lower = faster thaw when clicking back; higher = less overhead.",
        ),
    ];

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("  Config file: {}", config_path().display()),
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
    ];

    for (i, (label, value, desc)) in items.iter().enumerate() {
        let is_sel = i == selected;
        let prefix = if is_sel { " ▶ " } else { "   " };
        let label_style = if is_sel {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let val_style = if is_sel {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, label_style),
            Span::styled(format!("{label:<42}"), label_style),
            Span::raw("  "),
            Span::styled(format!(" ◀ {value} ▶ "), val_style),
        ]));
        lines.push(Line::from(Span::styled(
            format!("     {desc}"),
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::raw(""));
    }

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" pi-nap Configuration (auto-saves & hot-reloads into daemon) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
    f.render_widget(p, area);
}
