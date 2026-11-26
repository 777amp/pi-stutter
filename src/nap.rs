use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Processes that can NEVER be frozen under any circumstances (terminals, shells,
/// compositors, display servers, audio, networking, core system, and ourselves).
const PROTECTED_APPS: &[&str] = &[
    // Terminals
    "lxterminal",
    "xfce4-terminal",
    "gnome-terminal",
    "gnome-terminal-server",
    "konsole",
    "kitty",
    "alacritty",
    "foot",
    "footclient",
    "wezterm",
    "wezterm-gui",
    "terminator",
    "tilix",
    "xterm",
    "uxterm",
    "urxvt",
    "rxvt",
    "st",
    "sakura",
    "qterminal",
    "mate-terminal",
    "yakuake",
    "guake",
    "ghostty",
    "tilix",
    "eterm",
    // Shells & multiplexers & remote access
    "bash",
    "sh",
    "dash",
    "zsh",
    "fish",
    "tcsh",
    "csh",
    "ksh",
    "tmux",
    "screen",
    "login",
    "su",
    "sudo",
    "sshd",
    "ssh",
    "sftp-server",
    // Display servers, compositors, desktop panels
    "xorg",
    "xwayland",
    "labwc",
    "wayfire",
    "openbox",
    "sway",
    "hyprland",
    "kwin_wayland",
    "kwin_x11",
    "mutter",
    "gnome-shell",
    "wf-panel-pi",
    "lxpanel",
    "waybar",
    "swaybar",
    "dunst",
    "mako",
    "lightdm",
    "gdm",
    "sddm",
    "xinit",
    "startx",
    "lxsession",
    "pcmanfm",
    // Audio, IPC, networking, core daemons
    "systemd",
    "init",
    "dbus-daemon",
    "dbus-broker",
    "pipewire",
    "pipewire-pulse",
    "wireplumber",
    "pulseaudio",
    "networkmanager",
    "wpa_supplicant",
    "dhcpcd",
    "polkitd",
    "lxpolkit",
    "bluetoothd",
    "avahi-daemon",
    "udevd",
    "systemd-udevd",
    "systemd-journald",
    "systemd-logind",
    // Ourselves
    "pi-stutter",
    "pi-nap",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NapConfig {
    /// Opt-in list of app names allowed to be frozen when unfocused.
    /// Empty by default: NO apps are frozen until the user toggles them on.
    #[serde(default)]
    pub enabled_apps: BTreeSet<String>,

    /// Seconds an app must stay unfocused before being frozen.
    #[serde(default = "default_grace_secs")]
    pub grace_secs: u64,

    /// Periodic brief wakeup interval in seconds so frozen apps don't break
    /// (keepalives, timers, watchdogs). Set to 0 to disable pulsing.
    #[serde(default = "default_pulse_interval_secs")]
    pub pulse_interval_secs: u64,

    /// How long (ms) a frozen app is briefly thawed during each pulse.
    #[serde(default = "default_pulse_duration_ms")]
    pub pulse_duration_ms: u64,

    /// Daemon polling interval in milliseconds.
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

fn default_grace_secs() -> u64 {
    15
}
fn default_pulse_interval_secs() -> u64 {
    20
}
fn default_pulse_duration_ms() -> u64 {
    250
}
fn default_poll_interval_ms() -> u64 {
    1000
}

impl Default for NapConfig {
    fn default() -> Self {
        Self {
            enabled_apps: BTreeSet::new(),
            grace_secs: default_grace_secs(),
            pulse_interval_secs: default_pulse_interval_secs(),
            pulse_duration_ms: default_pulse_duration_ms(),
            poll_interval_ms: default_poll_interval_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NapLiveStatus {
    pub daemon_pid: u32,
    pub updated_unix: u64,
    pub focus_backend: String,
    pub focused_pid: Option<i32>,
    pub focused_app: Option<String>,
    pub frozen_apps: BTreeMap<String, Vec<i32>>,
    pub total_frozen_pids: usize,
}

#[derive(Debug, Clone)]
pub struct RunningApp {
    pub name: String,
    pub pids: Vec<i32>,
    pub any_stopped: bool,
    pub protected: bool,
    pub enabled: bool,
    pub focused: bool,
}

#[derive(Debug, Clone)]
struct ProcEntry {
    pid: i32,
    ppid: i32,
    uid: u32,
    comm: String,
    exe_name: Option<String>,
    state: char,
    has_cmdline: bool,
}

impl ProcEntry {
    fn canonical_name(&self) -> String {
        if let Some(exe) = &self.exe_name {
            if !exe.is_empty() {
                return normalize_app_name(exe);
            }
        }
        normalize_app_name(&self.comm)
    }
}

pub fn normalize_app_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(trimmed);
    // Strip common Linux wrapper suffixes like " (deleted)"
    base.split_whitespace().next().unwrap_or(base).to_string()
}

pub fn is_protected_app(name: &str) -> bool {
    let lower = normalize_app_name(name).to_lowercase();
    if lower.is_empty() {
        return true;
    }
    if PROTECTED_APPS.iter().any(|&p| lower == p) {
        return true;
    }
    // Also protect common terminal/shell/system prefixes
    if lower.starts_with("systemd")
        || lower.starts_with("kworker")
        || lower.starts_with("ksoftirqd")
        || lower.starts_with("kthread")
        || lower.starts_with("migration")
        || lower.starts_with("rcu_")
        || lower.contains("terminal")
    {
        return true;
    }
    false
}

pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("pi-stutter").join("nap.json");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".config")
        .join("pi-stutter")
        .join("nap.json")
}

pub fn status_path() -> PathBuf {
    let uid = current_uid();
    PathBuf::from(format!("/tmp/pi-nap-status-{uid}.json"))
}

pub fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

impl NapConfig {
    pub fn load() -> Self {
        let path = config_path();
        let Ok(content) = fs::read_to_string(&path) else {
            return Self::default();
        };
        let mut cfg: NapConfig = serde_json::from_str(&content).unwrap_or_default();
        // Always enforce safety denylist even if someone hand-edited the JSON
        cfg.enabled_apps.retain(|a| !is_protected_app(a));
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut clean = self.clone();
        clean.enabled_apps.retain(|a| !is_protected_app(a));
        let json = serde_json::to_string_pretty(&clean)?;
        fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn toggle_app(&mut self, app: &str) -> Result<bool> {
        let norm = normalize_app_name(app);
        if is_protected_app(&norm) {
            bail!("'{norm}' is a protected system/terminal app and can never be frozen.");
        }
        let now_enabled = if self.is_enabled(&norm) {
            self.enabled_apps
                .retain(|a| !a.eq_ignore_ascii_case(&norm));
            false
        } else {
            self.enabled_apps.insert(norm);
            true
        };
        self.save()?;
        Ok(now_enabled)
    }

    pub fn set_enabled(&mut self, app: &str, enable: bool) -> Result<()> {
        let norm = normalize_app_name(app);
        if enable && is_protected_app(&norm) {
            bail!("'{norm}' is a protected system/terminal app and can never be frozen.");
        }
        if enable {
            self.enabled_apps.insert(norm);
        } else {
            self.enabled_apps
                .retain(|a| !a.eq_ignore_ascii_case(&norm));
        }
        self.save()?;
        Ok(())
    }

    pub fn is_enabled(&self, app: &str) -> bool {
        let norm = normalize_app_name(app);
        self.enabled_apps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&norm))
    }
}

pub fn read_live_status() -> Option<NapLiveStatus> {
    let content = fs::read_to_string(status_path()).ok()?;
    let st: NapLiveStatus = serde_json::from_str(&content).ok()?;
    // Check if daemon PID is still alive
    let alive = unsafe { libc::kill(st.daemon_pid as i32, 0) == 0 };
    if alive {
        Some(st)
    } else {
        None
    }
}

fn write_live_status(st: &NapLiveStatus) {
    if let Ok(json) = serde_json::to_string_pretty(st) {
        let _ = fs::write(status_path(), json);
    }
}

/// Lists user-owned running applications grouped by canonical name.
pub fn list_running_apps(cfg: &NapConfig, focused_pid: Option<i32>) -> Vec<RunningApp> {
    let uid = current_uid();
    let procs = scan_processes();
    let focused_chain = focused_pid
        .map(|pid| ancestor_and_self_set(pid, &procs))
        .unwrap_or_default();

    let mut groups: BTreeMap<String, Vec<&ProcEntry>> = BTreeMap::new();
    for p in &procs {
        if p.uid != uid || !p.has_cmdline || p.pid <= 2 {
            continue;
        }
        let name = p.canonical_name();
        if name.is_empty() {
            continue;
        }
        groups.entry(name).or_default().push(p);
    }

    let mut apps: Vec<RunningApp> = groups
        .into_iter()
        .map(|(name, list)| {
            let pids: Vec<i32> = list.iter().map(|p| p.pid).collect();
            let any_stopped = list.iter().any(|p| p.state == 'T' || p.state == 't');
            let focused = list.iter().any(|p| focused_chain.contains(&p.pid));
            let protected = is_protected_app(&name);
            let enabled = !protected && cfg.is_enabled(&name);
            RunningApp {
                name,
                pids,
                any_stopped,
                protected,
                enabled,
                focused,
            }
        })
        .collect();

    // Sort: enabled first, then non-protected, then alphabetical
    apps.sort_by(|a, b| {
        b.enabled
            .cmp(&a.enabled)
            .then_with(|| a.protected.cmp(&b.protected))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    apps
}

/// Immediately sends SIGCONT to all stopped processes of the current user (or matching `app_filter`).
pub fn thaw_user_apps(app_filter: Option<&str>) -> usize {
    let uid = current_uid();
    let procs = scan_processes();
    let mut count = 0;

    for p in &procs {
        if p.uid != uid || p.pid <= 2 {
            continue;
        }
        let matches_filter = match app_filter {
            Some(f) => {
                let norm = normalize_app_name(f);
                p.canonical_name().eq_ignore_ascii_case(&norm)
                    || p.comm.eq_ignore_ascii_case(&norm)
            }
            None => p.state == 'T' || p.state == 't',
        };
        if matches_filter {
            if send_signal(p.pid, libc::SIGCONT) {
                count += 1;
            }
        }
    }
    count
}

fn send_signal(pid: i32, sig: i32) -> bool {
    if pid <= 2 {
        return false;
    }
    unsafe { libc::kill(pid, sig) == 0 }
}

/// Detects the currently focused GUI window's PID across X11 and common Wayland compositors.
pub fn detect_focused_pid() -> (Option<i32>, &'static str) {
    // 1. X11 via xdotool (fastest & most reliable on Raspberry Pi OS X11 / XWayland)
    if let Some(pid) = run_cmd_parse_pid("xdotool", &["getwindowfocus", "getwindowpid"]) {
        return (Some(pid), "xdotool");
    }

    // 2. X11 via xprop (preinstalled on almost all X11 desktops)
    if let Some(pid) = detect_xprop_focused_pid() {
        return (Some(pid), "xprop");
    }

    // 3. Wayland: Sway / i3-compatible IPC
    if let Some(pid) = detect_sway_focused_pid() {
        return (Some(pid), "swaymsg");
    }

    // 4. Wayland: Hyprland
    if let Some(pid) = detect_hyprctl_focused_pid() {
        return (Some(pid), "hyprctl");
    }

    // 5. KDE Plasma / KWin Wayland
    if let Some(pid) = run_cmd_parse_pid("kdotool", &["getactivewindow", "getwindowpid"]) {
        return (Some(pid), "kdotool");
    }

    (None, "none")
}

fn run_cmd_parse_pid(bin: &str, args: &[&str]) -> Option<i32> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<i32>().ok().filter(|&p| p > 1)
}

fn detect_xprop_focused_pid() -> Option<i32> {
    let out = Command::new("xprop")
        .args(["-root", "_NET_ACTIVE_WINDOW"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let win_id = s.split('#').nth(1)?.trim().split(',').next()?.trim();
    if win_id.is_empty() || win_id == "0x0" {
        return None;
    }
    let out2 = Command::new("xprop")
        .args(["-id", win_id, "_NET_WM_PID"])
        .output()
        .ok()?;
    if !out2.status.success() {
        return None;
    }
    let s2 = String::from_utf8_lossy(&out2.stdout);
    s2.split('=')
        .nth(1)?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|&p| p > 1)
}

fn detect_sway_focused_pid() -> Option<i32> {
    let out = Command::new("swaymsg")
        .args(["-t", "get_tree"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    find_focused_pid_in_json(&v)
}

fn find_focused_pid_in_json(v: &serde_json::Value) -> Option<i32> {
    if v.get("focused").and_then(|b| b.as_bool()) == Some(true) {
        if let Some(pid) = v.get("pid").and_then(|p| p.as_i64()) {
            if pid > 1 {
                return Some(pid as i32);
            }
        }
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
            for child in arr {
                if let Some(pid) = find_focused_pid_in_json(child) {
                    return Some(pid);
                }
            }
        }
    }
    None
}

fn detect_hyprctl_focused_pid() -> Option<i32> {
    let out = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("pid")
        .and_then(|p| p.as_i64())
        .map(|p| p as i32)
        .filter(|&p| p > 1)
}

fn scan_processes() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };

    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(pid_str) = fname.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };

        let stat_path = format!("/proc/{pid}/stat");
        let Ok(stat_str) = fs::read_to_string(&stat_path) else {
            continue;
        };
        let Some(open_p) = stat_str.find('(') else {
            continue;
        };
        let Some(close_p) = stat_str.rfind(')') else {
            continue;
        };
        if close_p <= open_p || close_p + 2 >= stat_str.len() {
            continue;
        }
        let comm = stat_str[open_p + 1..close_p].to_string();
        let rest: Vec<&str> = stat_str[close_p + 2..].split_whitespace().collect();
        if rest.len() < 2 {
            continue;
        }
        let state = rest[0].chars().next().unwrap_or('?');
        let ppid: i32 = rest[1].parse().unwrap_or(0);

        let uid = read_proc_uid(pid).unwrap_or(u32::MAX);
        let exe_name = fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()));
        let has_cmdline = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| !b.is_empty())
            .unwrap_or(false);

        out.push(ProcEntry {
            pid,
            ppid,
            uid,
            comm,
            exe_name,
            state,
            has_cmdline,
        });
    }
    out
}

fn read_proc_uid(pid: i32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse::<u32>().ok();
        }
    }
    None
}

fn ancestor_and_self_set(start_pid: i32, procs: &[ProcEntry]) -> HashSet<i32> {
    let parent_map: HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut set = HashSet::new();
    let mut cur = start_pid;
    for _ in 0..32 {
        if cur <= 1 || !set.insert(cur) {
            break;
        }
        match parent_map.get(&cur) {
            Some(&ppid) => cur = ppid,
            None => break,
        }
    }
    set
}

fn collect_descendants(roots: &HashSet<i32>, procs: &[ProcEntry]) -> HashSet<i32> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    let mut result = roots.clone();
    let mut stack: Vec<i32> = roots.iter().copied().collect();
    while let Some(pid) = stack.pop() {
        if let Some(kids) = children.get(&pid) {
            for &k in kids {
                if result.insert(k) {
                    stack.push(k);
                }
            }
        }
    }
    result
}

/// Stateful App Nap controller used by the `pi-nap` daemon.
pub struct NapDaemon {
    cfg: NapConfig,
    cfg_mtime: Option<SystemTime>,
    my_uid: u32,
    my_ancestors: HashSet<i32>,
    /// Tracks when each enabled app was last seen focused (or first seen unfocused).
    unfocused_since: HashMap<String, Instant>,
    /// Currently frozen PIDs per app name.
    frozen_apps: BTreeMap<String, Vec<i32>>,
    /// Last pulse wakeup time per frozen app.
    last_pulse: HashMap<String, Instant>,
}

impl NapDaemon {
    pub fn new() -> Self {
        let procs = scan_processes();
        let my_pid = std::process::id() as i32;
        let my_ancestors = ancestor_and_self_set(my_pid, &procs);
        let cfg = NapConfig::load();
        let cfg_mtime = fs::metadata(config_path()).and_then(|m| m.modified()).ok();

        Self {
            cfg,
            cfg_mtime,
            my_uid: current_uid(),
            my_ancestors,
            unfocused_since: HashMap::new(),
            frozen_apps: BTreeMap::new(),
            last_pulse: HashMap::new(),
        }
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.cfg.poll_interval_ms.max(250))
    }

    fn maybe_reload_config(&mut self) {
        let mtime = fs::metadata(config_path()).and_then(|m| m.modified()).ok();
        if mtime != self.cfg_mtime {
            self.cfg = NapConfig::load();
            self.cfg_mtime = mtime;
        }
    }

    pub fn tick(&mut self, verbose: bool) {
        self.maybe_reload_config();

        let procs = scan_processes();
        let (focused_pid, backend) = detect_focused_pid();
        let focused_ancestors = focused_pid
            .map(|pid| ancestor_and_self_set(pid, &procs))
            .unwrap_or_default();

        let focused_app = focused_pid.and_then(|fpid| {
            procs
                .iter()
                .find(|p| p.pid == fpid)
                .map(|p| p.canonical_name())
        });

        // 1. Thaw any previously frozen app that is no longer enabled in config
        let disabled_apps: Vec<String> = self
            .frozen_apps
            .keys()
            .filter(|app| !self.cfg.is_enabled(app))
            .cloned()
            .collect();
        for app in disabled_apps {
            self.thaw_app(&app, verbose, "disabled in config");
        }

        // 2. Evaluate every opted-in app
        let enabled_list: Vec<String> = self.cfg.enabled_apps.iter().cloned().collect();
        let now = Instant::now();
        let grace = Duration::from_secs(self.cfg.grace_secs);

        for app in enabled_list {
            if is_protected_app(&app) {
                continue;
            }

            // Find all user-owned processes matching this app name + their descendants
            let direct_roots: HashSet<i32> = procs
                .iter()
                .filter(|p| {
                    p.uid == self.my_uid
                        && p.has_cmdline
                        && p.pid > 2
                        && !self.my_ancestors.contains(&p.pid)
                        && !is_protected_app(&p.canonical_name())
                        && (p.canonical_name().eq_ignore_ascii_case(&app)
                            || p.comm.eq_ignore_ascii_case(&app))
                })
                .map(|p| p.pid)
                .collect();

            if direct_roots.is_empty() {
                self.unfocused_since.remove(&app);
                self.frozen_apps.remove(&app);
                self.last_pulse.remove(&app);
                continue;
            }

            let all_pids_set = collect_descendants(&direct_roots, &procs);
            let safe_pids: Vec<i32> = procs
                .iter()
                .filter(|p| {
                    all_pids_set.contains(&p.pid)
                        && p.uid == self.my_uid
                        && !self.my_ancestors.contains(&p.pid)
                        && !is_protected_app(&p.canonical_name())
                })
                .map(|p| p.pid)
                .collect();

            // Is this app currently focused?
            let is_focused = focused_pid.map(|f| all_pids_set.contains(&f)).unwrap_or(false)
                || safe_pids.iter().any(|p| focused_ancestors.contains(p))
                || focused_app
                    .as_deref()
                    .map(|fa| fa.eq_ignore_ascii_case(&app))
                    .unwrap_or(false);

            if is_focused {
                self.unfocused_since.remove(&app);
                if self.frozen_apps.contains_key(&app) {
                    self.thaw_app(&app, verbose, "window focused");
                }
                continue;
            }

            // If we couldn't detect ANY focused window (e.g., unsupported compositor or headless),
            // do NOT blindly freeze apps unless focus detection is actually working.
            if focused_pid.is_none() {
                continue;
            }

            // App is unfocused: check grace period
            let since = *self.unfocused_since.entry(app.clone()).or_insert(now);
            if now.duration_since(since) < grace {
                continue;
            }

            // Already frozen? Maybe perform a brief non-breaking pulse wakeup
            if let Some(existing_pids) = self.frozen_apps.get(&app).cloned() {
                if self.cfg.pulse_interval_secs > 0 {
                    let last = *self.last_pulse.entry(app.clone()).or_insert(now);
                    if now.duration_since(last) >= Duration::from_secs(self.cfg.pulse_interval_secs)
                    {
                        if verbose {
                            eprintln!(
                                "[pi-nap] pulsing '{}' ({} pids) for {}ms",
                                app,
                                existing_pids.len(),
                                self.cfg.pulse_duration_ms
                            );
                        }
                        for &pid in &existing_pids {
                            send_signal(pid, libc::SIGCONT);
                        }
                        std::thread::sleep(Duration::from_millis(
                            self.cfg.pulse_duration_ms.min(2000),
                        ));
                        for &pid in &safe_pids {
                            send_signal(pid, libc::SIGSTOP);
                        }
                        self.last_pulse.insert(app.clone(), Instant::now());
                        self.frozen_apps.insert(app.clone(), safe_pids);
                    }
                }
            } else {
                // Freeze the app!
                let mut frozen = Vec::new();
                for &pid in &safe_pids {
                    if send_signal(pid, libc::SIGSTOP) {
                        frozen.push(pid);
                    }
                }
                if !frozen.is_empty() {
                    if verbose {
                        eprintln!(
                            "[pi-nap] FROZE '{}' ({} pids) after {}s unfocused",
                            app,
                            frozen.len(),
                            self.cfg.grace_secs
                        );
                    }
                    self.frozen_apps.insert(app.clone(), frozen);
                    self.last_pulse.insert(app.clone(), Instant::now());
                }
            }
        }

        // Publish live status for `pi-nap status` and `pi-stutter` TUI
        let total_frozen_pids = self.frozen_apps.values().map(|v| v.len()).sum();
        let status = NapLiveStatus {
            daemon_pid: std::process::id(),
            updated_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            focus_backend: backend.to_string(),
            focused_pid,
            focused_app,
            frozen_apps: self.frozen_apps.clone(),
            total_frozen_pids,
        };
        write_live_status(&status);
    }

    fn thaw_app(&mut self, app: &str, verbose: bool, reason: &str) {
        if let Some(pids) = self.frozen_apps.remove(app) {
            for pid in &pids {
                send_signal(*pid, libc::SIGCONT);
            }
            if verbose {
                eprintln!("[pi-nap] THAWED '{}' ({} pids) — {}", app, pids.len(), reason);
            }
        }
        self.last_pulse.remove(app);
    }

    /// Always called on shutdown/signal/drop so no app is ever left frozen.
    pub fn thaw_all(&mut self, verbose: bool) {
        let apps: Vec<String> = self.frozen_apps.keys().cloned().collect();
        for app in apps {
            self.thaw_app(&app, verbose, "daemon stopping");
        }
        let _ = fs::remove_file(status_path());
    }
}

impl Drop for NapDaemon {
    fn drop(&mut self) {
        self.thaw_all(false);
    }
}

pub fn install_systemd_user_service() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate pi-nap binary path")?;
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".config/systemd/user");
    fs::create_dir_all(&dir)?;
    let unit_path = dir.join("pi-nap.service");

    let unit = format!(
        "[Unit]\n\
         Description=pi-nap — macOS App Nap-style background app freezer for Raspberry Pi\n\
         After=graphical-session.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} run\n\
         Restart=on-failure\n\
         RestartSec=3\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    );

    fs::write(&unit_path, unit)?;
    Ok(unit_path)
}
