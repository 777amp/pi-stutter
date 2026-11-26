use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use pi_stutter::nap::{
    self, config_path, detect_focused_pid, install_systemd_user_service, list_running_apps,
    read_live_status, thaw_user_apps, NapConfig, NapDaemon,
};

#[derive(Parser, Debug)]
#[command(
    name = "pi-nap",
    version,
    about = "macOS App Nap-style background app freezer for Raspberry Pi (opt-in per app, terminals protected)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
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
    },
    /// Install a systemd user service (~/.config/systemd/user/pi-nap.service)
    InstallService,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
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
                eprintln!("[pi-nap] No apps enabled yet (opt-in by default). Run `pi-nap toggle <app>` or use pi-stutter's App Nap tab.");
            }

            while !stop.load(Ordering::Relaxed) {
                daemon.tick(verbose);
                let step = daemon.poll_interval();
                let mut slept = std::time::Duration::ZERO;
                while slept < step && !stop.load(Ordering::Relaxed) {
                    let chunk = std::time::Duration::from_millis(100).min(step - slept);
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
            println!("Focus backend: {backend} | Config: {}", config_path().display());
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
                    "(none — opt-in with `pi-nap toggle <app>`)".into()
                } else {
                    cfg.enabled_apps.iter().cloned().collect::<Vec<_>>().join(", ")
                }
            );
            println!(
                "Grace: {}s | Pulse: every {}s for {}ms",
                cfg.grace_secs, cfg.pulse_interval_secs, cfg.pulse_duration_ms
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
                    println!("Daemon: NOT RUNNING (start with `pi-nap run` or `pi-nap install-service`)");
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
        } => {
            let mut cfg = NapConfig::load();
            if let Some(g) = grace_secs {
                cfg.grace_secs = g;
            }
            if let Some(pi) = pulse_interval_secs {
                cfg.pulse_interval_secs = pi;
            }
            if let Some(pd) = pulse_duration_ms {
                cfg.pulse_duration_ms = pd;
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
