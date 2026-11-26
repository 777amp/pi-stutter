use std::collections::HashMap;
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
    config_path, detect_focused_pid, discover_installed_apps, discover_user_services,
    install_systemd_user_service, list_all_apps, read_live_status, thaw_user_apps, AppKind,
    NapConfig, NapDaemon, NapLiveStatus, RunningApp,
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
    /// Launch interactive TUI to toggle apps/services and edit settings (default)
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
    /// List installed & running applications and services
    List {
        /// Filter: all, apps, services, running
        #[arg(short, long, default_value = "all")]
        filter: String,
    },
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
        Commands::List { filter } => {
            let cfg = NapConfig::load();
            let (focused_pid, backend) = detect_focused_pid();
            let installed = discover_installed_apps();
            let services = discover_user_services();
            let mut apps = list_all_apps(&cfg, focused_pid, &installed, &services);

            let f_mode = match filter.to_lowercase().as_str() {
                "app" | "apps" | "applications" => FilterMode::AppsOnly,
                "svc" | "service" | "services" => FilterMode::ServicesOnly,
                "run" | "running" => FilterMode::RunningOnly,
                "enabled" | "on" => FilterMode::EnabledOnly,
                _ => FilterMode::All,
            };
            apps = apply_filter_and_sort(apps, f_mode, SortMode::AppsFirst, "");

            println!(
                "Focus backend: {backend} | Filter: {} | Total: {}",
                f_mode.label(),
                apps.len()
            );
            println!(
                "{:<24} {:<9} {:<11} {:<6} {}",
                "APP / BINARY", "TYPE", "STATUS", "PIDS", "STATE / NAME"
            );
            println!("{}", "-".repeat(76));
            for a in apps {
                let status = if a.protected {
                    "PROTECTED"
                } else if a.enabled {
                    "ENABLED"
                } else {
                    "off"
                };
                let state = if a.pids.is_empty() {
                    "installed"
                } else if a.any_stopped {
                    "FROZEN"
                } else if a.focused {
                    "FOCUSED"
                } else {
                    "running"
                };
                let extra = match &a.display_name {
                    Some(d) if !d.eq_ignore_ascii_case(&a.name) => format!("{state} ({d})"),
                    _ => state.to_string(),
                };
                println!(
                    "{:<24} {:<9} {:<11} {:<6} {}",
                    a.name,
                    a.kind.label(),
                    status,
                    a.pids.len(),
                    extra
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
                    cfg.enabled_apps
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
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

    let cfg = NapConfig::load();
    cfg.save()?;
    println!("[1/4] Config ready: {}", config_path().display());
    println!(
        "      Opted-in apps: {} (all apps start unfrozen by default; terminals are always protected)",
        cfg.enabled_apps.len()
    );

    if let Some(wl) = pi_stutter::wayland_focus::probe_wayland_focus() {
        println!(
            "[2/4] Window focus backend detected: wayland-wlr ({}) — focused: {:?}",
            wl.socket,
            wl.focused_app_id.or(wl.focused_title)
        );
    } else {
        let (focused_pid, backend) = detect_focused_pid();
        if backend != "none" {
            println!(
                "[2/4] Window focus backend detected: {backend} (focused pid: {:?})",
                focused_pid
            );
        } else {
            println!("[2/4] Note: no active Wayland wlroots or X11 focus backend detected yet.");
            println!("      On Wayland (labwc/wayfire/sway/hyprland), pi-nap connects natively once your desktop session is active.");
            println!("      On X11, install xdotool: sudo apt-get install -y xdotool");
        }
    }

    let unit_path = install_systemd_user_service()?;
    println!(
        "[3/4] Installed systemd user service: {}",
        unit_path.display()
    );

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", "pi-nap"])
        .status();
    let restart = Command::new("systemctl")
        .args(["--user", "restart", "pi-nap"])
        .status();

    match (reload, enable, restart) {
        (Ok(r), Ok(e), Ok(rs)) if r.success() && e.success() && rs.success() => {
            println!("[4/4] Service enabled and (re)started via systemd (`pi-nap.service`).");
        }
        _ => {
            println!("[4/4] Could not auto-start systemd user service (common over non-login SSH or containers).");
            println!("      On your Pi desktop terminal, run:");
            println!("        systemctl --user daemon-reload && systemctl --user enable --now pi-nap");
            println!("      Or run directly in the background:");
            println!("        pi-nap run -v");
        }
    }

    println!("\nAll set! Run `pi-nap` (or `pi-nap tui`) to pick which apps/services to freeze and adjust settings.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NapTab {
    Apps = 0,
    Settings = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    AppsFirst,
    ServicesFirst,
    RunningFirst,
    EnabledFirst,
    NameAsc,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::AppsFirst => SortMode::ServicesFirst,
            SortMode::ServicesFirst => SortMode::RunningFirst,
            SortMode::RunningFirst => SortMode::EnabledFirst,
            SortMode::EnabledFirst => SortMode::NameAsc,
            SortMode::NameAsc => SortMode::AppsFirst,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::AppsFirst => "Apps First",
            SortMode::ServicesFirst => "Services First",
            SortMode::RunningFirst => "Running First",
            SortMode::EnabledFirst => "Enabled First",
            SortMode::NameAsc => "Name (A-Z)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    All,
    AppsOnly,
    ServicesOnly,
    RunningOnly,
    EnabledOnly,
}

impl FilterMode {
    fn next(self) -> Self {
        match self {
            FilterMode::All => FilterMode::AppsOnly,
            FilterMode::AppsOnly => FilterMode::ServicesOnly,
            FilterMode::ServicesOnly => FilterMode::RunningOnly,
            FilterMode::RunningOnly => FilterMode::EnabledOnly,
            FilterMode::EnabledOnly => FilterMode::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            FilterMode::All => "All",
            FilterMode::AppsOnly => "Apps Only",
            FilterMode::ServicesOnly => "Services Only",
            FilterMode::RunningOnly => "Running Only",
            FilterMode::EnabledOnly => "Enabled Only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    AddApp(String),
    Search(String),
}

fn apply_filter_and_sort(
    mut apps: Vec<RunningApp>,
    filter: FilterMode,
    sort: SortMode,
    search: &str,
) -> Vec<RunningApp> {
    apps.retain(|a| match filter {
        FilterMode::All => true,
        FilterMode::AppsOnly => a.kind == AppKind::Application,
        FilterMode::ServicesOnly => a.kind == AppKind::Service,
        FilterMode::RunningOnly => !a.pids.is_empty(),
        FilterMode::EnabledOnly => a.enabled,
    });

    let q = search.trim().to_lowercase();
    if !q.is_empty() {
        apps.retain(|a| {
            a.name.to_lowercase().contains(&q)
                || a.display_name
                    .as_deref()
                    .map(|d| d.to_lowercase().contains(&q))
                    .unwrap_or(false)
        });
    }

    match sort {
        SortMode::AppsFirst => apps.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| b.enabled.cmp(&a.enabled))
                .then_with(|| a.protected.cmp(&b.protected))
                .then_with(|| b.pids.is_empty().cmp(&a.pids.is_empty()).reverse())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortMode::ServicesFirst => apps.sort_by(|a, b| {
            b.kind
                .cmp(&a.kind)
                .then_with(|| b.enabled.cmp(&a.enabled))
                .then_with(|| a.protected.cmp(&b.protected))
                .then_with(|| b.pids.is_empty().cmp(&a.pids.is_empty()).reverse())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortMode::RunningFirst => apps.sort_by(|a, b| {
            a.pids
                .is_empty()
                .cmp(&b.pids.is_empty())
                .then_with(|| b.enabled.cmp(&a.enabled))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortMode::EnabledFirst => apps.sort_by(|a, b| {
            b.enabled
                .cmp(&a.enabled)
                .then_with(|| a.protected.cmp(&b.protected))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortMode::NameAsc => apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
    }

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

    // Scan installed desktop apps & user services once at startup (refreshable with 'R')
    let mut installed_apps: HashMap<String, (String, String)> = discover_installed_apps();
    let mut user_services: HashMap<String, (String, String)> = discover_user_services();

    let mut tab = NapTab::Apps;
    let mut cfg = NapConfig::load();
    let mut status = read_live_status();
    let mut sort_mode = SortMode::AppsFirst;
    let mut filter_mode = FilterMode::All;
    let mut search_query = String::new();
    let mut input_mode = InputMode::Normal;

    let mut raw_apps = list_all_apps(
        &cfg,
        status.as_ref().and_then(|s| s.focused_pid),
        &installed_apps,
        &user_services,
    );
    let mut app_selected: usize = 0;
    let mut setting_selected: usize = 0;
    let mut toast: Option<(String, Instant)> = None;

    loop {
        if last_tick.elapsed() >= tick_rate {
            status = read_live_status();
            raw_apps = list_all_apps(
                &cfg,
                status.as_ref().and_then(|s| s.focused_pid),
                &installed_apps,
                &user_services,
            );
            last_tick = Instant::now();
        }

        if let Some((_, ts)) = &toast {
            if ts.elapsed() > Duration::from_secs(4) {
                toast = None;
            }
        }

        let active_search = match &input_mode {
            InputMode::Search(s) => s.as_str(),
            _ => search_query.as_str(),
        };
        let visible_apps =
            apply_filter_and_sort(raw_apps.clone(), filter_mode, sort_mode, active_search);

        if !visible_apps.is_empty() && app_selected >= visible_apps.len() {
            app_selected = visible_apps.len() - 1;
        }

        terminal.draw(|f| {
            draw_nap_tui(
                f,
                tab,
                &cfg,
                status.as_ref(),
                &visible_apps,
                app_selected,
                setting_selected,
                sort_mode,
                filter_mode,
                &input_mode,
                &search_query,
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

                // Handle inline text input modes (AddApp / Search)
                match &mut input_mode {
                    InputMode::AddApp(buf) => {
                        match key.code {
                            KeyCode::Esc => input_mode = InputMode::Normal,
                            KeyCode::Enter => {
                                let name = buf.trim().to_string();
                                input_mode = InputMode::Normal;
                                if !name.is_empty() {
                                    match cfg.set_enabled(&name, true) {
                                        Ok(_) => {
                                            toast = Some((
                                                format!("Enabled App Nap for '{name}' (saved)"),
                                                Instant::now(),
                                            ));
                                            raw_apps = list_all_apps(
                                                &cfg,
                                                status.as_ref().and_then(|s| s.focused_pid),
                                                &installed_apps,
                                                &user_services,
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
                    InputMode::Search(buf) => {
                        match key.code {
                            KeyCode::Esc => {
                                search_query.clear();
                                input_mode = InputMode::Normal;
                            }
                            KeyCode::Enter => {
                                search_query = buf.clone();
                                input_mode = InputMode::Normal;
                                app_selected = 0;
                            }
                            KeyCode::Backspace => {
                                buf.pop();
                                app_selected = 0;
                            }
                            KeyCode::Char(c) => {
                                if buf.len() < 48 {
                                    buf.push(c);
                                    app_selected = 0;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    InputMode::Normal => {}
                }

                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Esc => {
                        if !search_query.is_empty() {
                            search_query.clear();
                            app_selected = 0;
                        } else {
                            return Ok(());
                        }
                    }
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
                        if app_selected + 1 < visible_apps.len() {
                            app_selected += 1;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') if tab == NapTab::Apps => {
                        app_selected = app_selected.saturating_sub(1);
                    }
                    KeyCode::Char(' ') | KeyCode::Enter if tab == NapTab::Apps => {
                        if let Some(app) = visible_apps.get(app_selected) {
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
                            raw_apps = list_all_apps(
                                &cfg,
                                status.as_ref().and_then(|s| s.focused_pid),
                                &installed_apps,
                                &user_services,
                            );
                        }
                    }
                    KeyCode::Char('s') if tab == NapTab::Apps => {
                        sort_mode = sort_mode.next();
                        app_selected = 0;
                        toast = Some((format!("Sort: {}", sort_mode.label()), Instant::now()));
                    }
                    KeyCode::Char('f') if tab == NapTab::Apps => {
                        filter_mode = filter_mode.next();
                        app_selected = 0;
                        toast = Some((format!("Filter: {}", filter_mode.label()), Instant::now()));
                    }
                    KeyCode::Char('/') if tab == NapTab::Apps => {
                        input_mode = InputMode::Search(search_query.clone());
                    }
                    KeyCode::Char('a') if tab == NapTab::Apps => {
                        input_mode = InputMode::AddApp(String::new());
                    }
                    KeyCode::Char('R') if tab == NapTab::Apps => {
                        installed_apps = discover_installed_apps();
                        user_services = discover_user_services();
                        raw_apps = list_all_apps(
                            &cfg,
                            status.as_ref().and_then(|s| s.focused_pid),
                            &installed_apps,
                            &user_services,
                        );
                        toast = Some((
                            format!(
                                "Rescanned {} installed apps & {} user services.",
                                installed_apps.len(),
                                user_services.len()
                            ),
                            Instant::now(),
                        ));
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
                            toast = Some((
                                "Saved settings (daemon hot-reloads automatically).".into(),
                                Instant::now(),
                            ));
                        }
                    }
                    KeyCode::Left | KeyCode::Char('-') if tab == NapTab::Settings => {
                        adjust_setting(&mut cfg, setting_selected, false);
                        if let Err(e) = cfg.save() {
                            toast = Some((format!("Save failed: {e}"), Instant::now()));
                        } else {
                            toast = Some((
                                "Saved settings (daemon hot-reloads automatically).".into(),
                                Instant::now(),
                            ));
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
            if inc {
                cfg.pulse_interval_secs = (cfg.pulse_interval_secs + 5).min(600);
            } else {
                cfg.pulse_interval_secs = cfg.pulse_interval_secs.saturating_sub(5);
            }
        }
        2 => {
            if inc {
                cfg.pulse_duration_ms = (cfg.pulse_duration_ms + 50).min(2000);
            } else {
                cfg.pulse_duration_ms = cfg.pulse_duration_ms.saturating_sub(50).max(50);
            }
        }
        3 => {
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
    sort_mode: SortMode,
    filter_mode: FilterMode,
    input_mode: &InputMode,
    search_query: &str,
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
        Line::from(Span::styled(
            "1: Apps & Services",
            Style::default().fg(Color::White),
        )),
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
        NapTab::Apps => draw_apps_tab(
            f,
            chunks[2],
            apps,
            app_selected,
            sort_mode,
            filter_mode,
            input_mode,
            search_query,
        ),
        NapTab::Settings => draw_settings_tab(f, chunks[2], cfg, setting_selected),
    }

    let footer_text = if let Some(msg) = toast {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        match input_mode {
            InputMode::AddApp(_) => Line::from(Span::styled(
                " Type app/service name (e.g. chromium) and press Enter to enable | Esc to cancel",
                Style::default().fg(Color::Yellow),
            )),
            InputMode::Search(_) => Line::from(Span::styled(
                " Type to search apps & services | Enter to apply | Esc to clear",
                Style::default().fg(Color::Yellow),
            )),
            InputMode::Normal => match tab {
                NapTab::Apps => Line::from(Span::styled(
                    " [Space/Enter] Toggle  [s] Sort (Apps/Services)  [f] Filter  [/] Search  [a] Add  [u] Thaw  [Tab] Settings  [q] Quit",
                    Style::default().fg(Color::DarkGray),
                )),
                NapTab::Settings => Line::from(Span::styled(
                    " [↑/↓] Select  [←/→ or -/+] Change Value  [r] Reset Defaults  [Tab/1] Apps  [q] Quit",
                    Style::default().fg(Color::DarkGray),
                )),
            },
        }
    };
    f.render_widget(Paragraph::new(footer_text), chunks[3]);
}

#[allow(clippy::too_many_arguments)]
fn draw_apps_tab(
    f: &mut Frame,
    area: Rect,
    apps: &[RunningApp],
    selected: usize,
    sort_mode: SortMode,
    filter_mode: FilterMode,
    input_mode: &InputMode,
    search_query: &str,
) {
    let has_prompt = !matches!(input_mode, InputMode::Normal);
    let chunks = if has_prompt {
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

    let header = Row::new(vec![
        "",
        "NAME",
        "TYPE",
        "NAP STATUS",
        "PIDS",
        "LIVE STATE",
    ])
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

            let type_style = match app.kind {
                AppKind::Application => Style::default().fg(Color::Cyan),
                AppKind::Service => Style::default().fg(Color::Yellow),
            };

            let live = if app.pids.is_empty() {
                Span::styled("installed", Style::default().fg(Color::DarkGray))
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
                Span::styled("running", Style::default().fg(Color::Green))
            };

            let display_col = match &app.display_name {
                Some(d) if !d.eq_ignore_ascii_case(&app.name) => format!("{} ({})", app.name, d),
                _ => app.name.clone(),
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
                Cell::from(display_col),
                Cell::from(app.kind.label()).style(type_style),
                Cell::from(opt_text).style(opt_style),
                Cell::from(app.pids.len().to_string()),
                Cell::from(Line::from(live)),
            ])
            .style(row_style)
        })
        .collect();

    let search_badge = if !search_query.is_empty() {
        format!(" | Search: \"{search_query}\" (Esc clears)")
    } else {
        String::new()
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Min(28),
            Constraint::Length(9),
            Constraint::Length(15),
            Constraint::Length(6),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(format!(
                " {} items | Sort: {} [s] | Filter: {} [f]{} ",
                apps.len(),
                sort_mode.label(),
                filter_mode.label(),
                search_badge
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(table, chunks[0]);

    match input_mode {
        InputMode::AddApp(buf) => {
            let prompt = Paragraph::new(format!(" App/service name to enable: {buf}█"))
                .style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .block(
                    Block::default()
                        .title(" Add App Manually ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                );
            f.render_widget(prompt, chunks[1]);
        }
        InputMode::Search(buf) => {
            let prompt = Paragraph::new(format!(" Search: {buf}█"))
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .block(
                    Block::default()
                        .title(" Filter by Name ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                );
            f.render_widget(prompt, chunks[1]);
        }
        InputMode::Normal => {}
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
