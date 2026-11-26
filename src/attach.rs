use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::time::Instant;

const ATTACH_HISTORY: usize = 90;

#[derive(Debug, Clone)]
pub enum AttachTarget {
    Pid(i32),
    Name(String),
}

impl AttachTarget {
    pub fn parse(s: &str) -> Self {
        if let Ok(pid) = s.trim().parse::<i32>() {
            AttachTarget::Pid(pid)
        } else {
            AttachTarget::Name(s.trim().to_string())
        }
    }

    pub fn label(&self) -> String {
        match self {
            AttachTarget::Pid(p) => format!("PID {p}"),
            AttachTarget::Name(n) => format!("'{n}'"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadMetric {
    pub pid: i32,
    pub tid: i32,
    pub name: String,
    pub state: char,
    pub cpu_pct: f64,
    pub wchan: String,
}

#[derive(Debug, Clone)]
pub struct AttachFinding {
    pub severity: &'static str, // "CRIT", "WARN", "INFO"
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct AttachSnapshot {
    pub target_label: String,
    pub alive: bool,
    pub matched_pids: Vec<i32>,
    pub total_threads: usize,
    pub total_cpu_pct: f64,
    pub hottest_thread_cpu_pct: f64,
    pub hottest_thread_name: String,
    pub d_state_threads: usize,
    pub rss_mb: f64,
    pub swap_mb: f64,
    pub open_fds: usize,
    pub sched_run_ms: f64,
    pub sched_wait_ms: f64,
    pub sched_wait_peak_ms: f64,
    pub vol_ctxt_per_sec: f64,
    pub nonvol_ctxt_per_sec: f64,
    pub minflt_per_sec: f64,
    pub majflt_per_sec: f64,
    pub io_read_kb_s: f64,
    pub io_write_kb_s: f64,
    pub io_rchar_kb_s: f64,
    pub io_wchar_kb_s: f64,
    pub io_syscr_s: f64,
    pub io_syscw_s: f64,
    pub threads: Vec<ThreadMetric>,
    pub findings: Vec<AttachFinding>,
    pub wait_history_us: Vec<u64>,
    pub cpu_history: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
struct PrevPidData {
    run_ns: u64,
    wait_ns: u64,
    vol_ctxt: u64,
    nonvol_ctxt: u64,
    minflt: u64,
    majflt: u64,
    read_bytes: u64,
    write_bytes: u64,
    rchar: u64,
    wchar: u64,
    syscr: u64,
    syscw: u64,
}

pub struct ProcessAttacher {
    target: Option<AttachTarget>,
    clk_tck: f64,
    last_time: Instant,
    prev_pids: HashMap<i32, PrevPidData>,
    prev_threads: HashMap<i32, u64>, // tid -> utime+stime
    wait_peak_ms: f64,
    wait_hist_us: VecDeque<u64>,
    cpu_hist: VecDeque<u64>,
}

impl ProcessAttacher {
    pub fn new() -> Self {
        Self {
            target: None,
            clk_tck: 100.0,
            last_time: Instant::now(),
            prev_pids: HashMap::new(),
            prev_threads: HashMap::new(),
            wait_peak_ms: 0.0,
            wait_hist_us: VecDeque::from(vec![0; ATTACH_HISTORY]),
            cpu_hist: VecDeque::from(vec![0; ATTACH_HISTORY]),
        }
    }

    pub fn attach(&mut self, target: AttachTarget) {
        self.target = Some(target);
        self.prev_pids.clear();
        self.prev_threads.clear();
        self.wait_peak_ms = 0.0;
        self.wait_hist_us = VecDeque::from(vec![0; ATTACH_HISTORY]);
        self.cpu_hist = VecDeque::from(vec![0; ATTACH_HISTORY]);
        self.last_time = Instant::now();
    }

    pub fn detach(&mut self) {
        self.target = None;
        self.prev_pids.clear();
        self.prev_threads.clear();
    }

    pub fn is_attached(&self) -> bool {
        self.target.is_some()
    }

    pub fn reset_peaks(&mut self) {
        self.wait_peak_ms = 0.0;
        self.wait_hist_us.iter_mut().for_each(|v| *v = 0);
        self.cpu_hist.iter_mut().for_each(|v| *v = 0);
    }

    pub fn collect(&mut self) -> Option<AttachSnapshot> {
        let target = self.target.as_ref()?.clone();
        let now = Instant::now();
        let dt = now.duration_since(self.last_time).as_secs_f64().max(0.001);
        self.last_time = now;

        let pids = resolve_target_pids(&target);
        if pids.is_empty() {
            return Some(AttachSnapshot {
                target_label: target.label(),
                alive: false,
                wait_history_us: self.wait_hist_us.iter().copied().collect(),
                cpu_history: self.cpu_hist.iter().copied().collect(),
                ..Default::default()
            });
        }

        let mut snap = AttachSnapshot {
            target_label: target.label(),
            alive: true,
            matched_pids: pids.clone(),
            ..Default::default()
        };

        let mut new_prev_pids = HashMap::new();
        let mut new_prev_threads = HashMap::new();
        let mut all_threads = Vec::new();

        for &pid in &pids {
            // 1. /proc/<pid>/stat for faults
            let (minflt, majflt) = read_pid_faults(pid);
            // 2. /proc/<pid>/schedstat for run/wait ns
            let (run_ns, wait_ns) = read_schedstat(pid);
            // 3. /proc/<pid>/status for RSS, Swap, ctx switches
            let (rss_kb, swap_kb, vol_ctxt, nonvol_ctxt) = read_pid_status(pid);
            // 4. /proc/<pid>/io for disk & syscall I/O
            let (rchar, wchar, syscr, syscw, read_bytes, write_bytes) = read_pid_io(pid);
            // 5. Open FDs
            let fds = fs::read_dir(format!("/proc/{pid}/fd"))
                .map(|it| it.count())
                .unwrap_or(0);

            snap.rss_mb += rss_kb as f64 / 1024.0;
            snap.swap_mb += swap_kb as f64 / 1024.0;
            snap.open_fds += fds;

            if let Some(prev) = self.prev_pids.get(&pid) {
                snap.sched_run_ms += run_ns.saturating_sub(prev.run_ns) as f64 / 1_000_000.0;
                snap.sched_wait_ms += wait_ns.saturating_sub(prev.wait_ns) as f64 / 1_000_000.0;
                snap.vol_ctxt_per_sec += vol_ctxt.saturating_sub(prev.vol_ctxt) as f64 / dt;
                snap.nonvol_ctxt_per_sec +=
                    nonvol_ctxt.saturating_sub(prev.nonvol_ctxt) as f64 / dt;
                snap.minflt_per_sec += minflt.saturating_sub(prev.minflt) as f64 / dt;
                snap.majflt_per_sec += majflt.saturating_sub(prev.majflt) as f64 / dt;
                snap.io_read_kb_s +=
                    (read_bytes.saturating_sub(prev.read_bytes) as f64 / 1024.0) / dt;
                snap.io_write_kb_s +=
                    (write_bytes.saturating_sub(prev.write_bytes) as f64 / 1024.0) / dt;
                snap.io_rchar_kb_s += (rchar.saturating_sub(prev.rchar) as f64 / 1024.0) / dt;
                snap.io_wchar_kb_s += (wchar.saturating_sub(prev.wchar) as f64 / 1024.0) / dt;
                snap.io_syscr_s += syscr.saturating_sub(prev.syscr) as f64 / dt;
                snap.io_syscw_s += syscw.saturating_sub(prev.syscw) as f64 / dt;
            }

            new_prev_pids.insert(
                pid,
                PrevPidData {
                    run_ns,
                    wait_ns,
                    vol_ctxt,
                    nonvol_ctxt,
                    minflt,
                    majflt,
                    read_bytes,
                    write_bytes,
                    rchar,
                    wchar,
                    syscr,
                    syscw,
                },
            );

            // 6. Per-thread scan: /proc/<pid>/task/*
            if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
                for entry in tasks.flatten() {
                    let fname = entry.file_name();
                    let Some(tid_str) = fname.to_str() else {
                        continue;
                    };
                    let Ok(tid) = tid_str.parse::<i32>() else {
                        continue;
                    };
                    if let Some((comm, state, ticks)) = read_thread_stat(pid, tid) {
                        let cpu_pct = if let Some(&prev_ticks) = self.prev_threads.get(&tid) {
                            let d = ticks.saturating_sub(prev_ticks) as f64;
                            (d / self.clk_tck / dt) * 100.0
                        } else {
                            0.0
                        };
                        new_prev_threads.insert(tid, ticks);

                        if state == 'D' {
                            snap.d_state_threads += 1;
                        }
                        snap.total_cpu_pct += cpu_pct;
                        if cpu_pct > snap.hottest_thread_cpu_pct {
                            snap.hottest_thread_cpu_pct = cpu_pct;
                            snap.hottest_thread_name = comm.clone();
                        }

                        let wchan = read_wchan(pid, tid);
                        all_threads.push(ThreadMetric {
                            pid,
                            tid,
                            name: comm,
                            state,
                            cpu_pct,
                            wchan,
                        });
                    }
                }
            }
        }

        self.prev_pids = new_prev_pids;
        self.prev_threads = new_prev_threads;

        snap.total_threads = all_threads.len();
        if snap.sched_wait_ms > self.wait_peak_ms {
            self.wait_peak_ms = snap.sched_wait_ms;
        }
        snap.sched_wait_peak_ms = self.wait_peak_ms;

        // Sort threads: D-state first, then highest CPU%
        all_threads.sort_by(|a, b| {
            let a_d = a.state == 'D';
            let b_d = b.state == 'D';
            b_d.cmp(&a_d).then_with(|| {
                b.cpu_pct
                    .partial_cmp(&a.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        snap.threads = all_threads;

        // Update sparkline history
        if self.wait_hist_us.len() >= ATTACH_HISTORY {
            self.wait_hist_us.pop_front();
        }
        self.wait_hist_us
            .push_back((snap.sched_wait_ms * 1000.0) as u64);

        if self.cpu_hist.len() >= ATTACH_HISTORY {
            self.cpu_hist.pop_front();
        }
        self.cpu_hist
            .push_back(snap.hottest_thread_cpu_pct.round().clamp(0.0, 100.0) as u64);

        snap.wait_history_us = self.wait_hist_us.iter().copied().collect();
        snap.cpu_history = self.cpu_hist.iter().copied().collect();

        // Generate root-cause diagnosis
        snap.findings = diagnose_attached(&snap);

        Some(snap)
    }
}

fn diagnose_attached(s: &AttachSnapshot) -> Vec<AttachFinding> {
    let mut findings = Vec::new();

    if s.d_state_threads > 0 {
        let wchans: Vec<String> = s
            .threads
            .iter()
            .filter(|t| t.state == 'D')
            .map(|t| format!("{}({})", t.name, t.wchan))
            .take(3)
            .collect();
        findings.push(AttachFinding {
            severity: "CRIT",
            title: "Uninterruptible I/O Stall (D-state)".into(),
            detail: format!(
                "{} thread(s) blocked in kernel I/O: {}",
                s.d_state_threads,
                wchans.join(", ")
            ),
        });
    }

    if s.hottest_thread_cpu_pct >= 80.0 {
        findings.push(AttachFinding {
            severity: "CRIT",
            title: "Single-Thread CPU Bottleneck".into(),
            detail: format!(
                "Thread '{}' is pegged at {:.0}% of 1 core! Even if Pi total CPU looks low, this thread is maxing out its core.",
                s.hottest_thread_name, s.hottest_thread_cpu_pct
            ),
        });
    }

    if s.sched_wait_ms >= 20.0 {
        findings.push(AttachFinding {
            severity: if s.sched_wait_ms >= 80.0 { "CRIT" } else { "WARN" },
            title: "Scheduler Runqueue Delay".into(),
            detail: format!(
                "Spent {:.1}ms runnable but waiting for CPU (peak {:.1}ms) — preempted by other processes or thermal throttling.",
                s.sched_wait_ms, s.sched_wait_peak_ms
            ),
        });
    }

    if s.swap_mb >= 32.0 || s.majflt_per_sec >= 5.0 {
        findings.push(AttachFinding {
            severity: "CRIT",
            title: "Swap / Major Page Fault Stalls".into(),
            detail: format!(
                "VmSwap={:.0}MB, MajFlt={:.1}/s — paging from SD card causes severe UI freezes.",
                s.swap_mb, s.majflt_per_sec
            ),
        });
    }

    if s.io_write_kb_s >= 500.0 || s.io_read_kb_s >= 2000.0 {
        findings.push(AttachFinding {
            severity: "WARN",
            title: "Heavy Disk I/O (Cache / Log / SQLite)".into(),
            detail: format!(
                "Disk R={:.0} KB/s W={:.0} KB/s — synchronous cache/DB writes frequently stutter Chromium & proxy apps on microSD.",
                s.io_read_kb_s, s.io_write_kb_s
            ),
        });
    }

    let futex_threads = s
        .threads
        .iter()
        .filter(|t| t.wchan.contains("futex"))
        .count();
    if s.vol_ctxt_per_sec >= 3000.0 && futex_threads > 0 {
        findings.push(AttachFinding {
            severity: "WARN",
            title: "Lock / Futex / IPC Contention".into(),
            detail: format!(
                "{:.0} vol ctx/s with {} thread(s) in futex wait — threads are bouncing on locks or IPC.",
                s.vol_ctxt_per_sec, futex_threads
            ),
        });
    }

    if s.nonvol_ctxt_per_sec >= 500.0 {
        findings.push(AttachFinding {
            severity: "WARN",
            title: "Frequent Kernel Preemption".into(),
            detail: format!(
                "{:.0} involuntary ctx/s — competing for CPU time slices.",
                s.nonvol_ctxt_per_sec
            ),
        });
    }

    if s.minflt_per_sec >= 5000.0 {
        findings.push(AttachFinding {
            severity: "INFO",
            title: "High Memory Allocation / GC Churn".into(),
            detail: format!(
                "{:.0} minor faults/s — rapid heap allocations or garbage collection.",
                s.minflt_per_sec
            ),
        });
    }

    if findings.is_empty() {
        let epoll_or_net = s
            .threads
            .iter()
            .filter(|t| {
                t.wchan.contains("ep_poll")
                    || t.wchan.contains("poll")
                    || t.wchan.contains("sk_wait")
                    || t.wchan.contains("unix_stream")
            })
            .count();
        if epoll_or_net > 0 && s.total_cpu_pct < 15.0 {
            findings.push(AttachFinding {
                severity: "INFO",
                title: "Mostly Idle / Waiting on Network, IPC, or GPU".into(),
                detail: format!(
                    "{} thread(s) sleeping in poll/socket/IPC (CPU={:.1}%, wait={:.1}ms). If it feels laggy right now, check upstream network/API latency, DNS, or GPU compositor sync.",
                    epoll_or_net, s.total_cpu_pct, s.sched_wait_ms
                ),
            });
        } else {
            findings.push(AttachFinding {
                severity: "INFO",
                title: "No Active Bottleneck Detected".into(),
                detail: "Trigger the lag in the app while watching this view to catch spikes in Single-Thread CPU, Sched Wait, D-state, or Disk I/O.".into(),
            });
        }
    }

    findings
}

fn resolve_target_pids(target: &AttachTarget) -> Vec<i32> {
    match target {
        AttachTarget::Pid(pid) => {
            if fs::metadata(format!("/proc/{pid}")).is_ok() {
                vec![*pid]
            } else {
                Vec::new()
            }
        }
        AttachTarget::Name(query) => {
            let q = query.to_lowercase();
            let mut matched = HashSet::new();
            let mut parent_map: HashMap<i32, i32> = HashMap::new();
            let Ok(entries) = fs::read_dir("/proc") else {
                return Vec::new();
            };

            for entry in entries.flatten() {
                let fname = entry.file_name();
                let Some(s) = fname.to_str() else { continue };
                let Ok(pid) = s.parse::<i32>() else { continue };
                if pid <= 2 {
                    continue;
                }

                if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                    if let (Some(o), Some(c)) = (stat.find('('), stat.rfind(')')) {
                        if c > o && c + 2 < stat.len() {
                            let comm = &stat[o + 1..c];
                            let ppid: i32 = stat[c + 2..]
                                .split_whitespace()
                                .nth(1)
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                            parent_map.insert(pid, ppid);

                            let exe_base = fs::read_link(format!("/proc/{pid}/exe"))
                                .ok()
                                .and_then(|p| p.file_name().map(|x| x.to_string_lossy().to_string()))
                                .unwrap_or_default();

                            if comm.to_lowercase().contains(&q)
                                || exe_base.to_lowercase().contains(&q)
                            {
                                matched.insert(pid);
                            }
                        }
                    }
                }
            }

            // Include direct children of matched PIDs (captures Chromium helper/renderer processes)
            let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
            for (pid, ppid) in parent_map {
                children.entry(ppid).or_default().push(pid);
            }
            let mut stack: Vec<i32> = matched.iter().copied().collect();
            while let Some(p) = stack.pop() {
                if let Some(kids) = children.get(&p) {
                    for &k in kids {
                        if matched.insert(k) {
                            stack.push(k);
                        }
                    }
                }
            }

            let mut res: Vec<i32> = matched.into_iter().collect();
            res.sort_unstable();
            res
        }
    }
}

fn read_pid_faults(pid: i32) -> (u64, u64) {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return (0, 0);
    };
    let Some(c) = stat.rfind(')') else {
        return (0, 0);
    };
    if c + 2 >= stat.len() {
        return (0, 0);
    }
    let rest: Vec<&str> = stat[c + 2..].split_whitespace().collect();
    if rest.len() < 10 {
        return (0, 0);
    }
    let minflt = rest[7].parse().unwrap_or(0);
    let majflt = rest[9].parse().unwrap_or(0);
    (minflt, majflt)
}

fn read_schedstat(pid: i32) -> (u64, u64) {
    let Ok(s) = fs::read_to_string(format!("/proc/{pid}/schedstat")) else {
        return (0, 0);
    };
    let mut parts = s.split_whitespace();
    let run_ns = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let wait_ns = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (run_ns, wait_ns)
}

fn read_pid_status(pid: i32) -> (u64, u64, u64, u64) {
    let Ok(s) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return (0, 0, 0, 0);
    };
    let (mut rss, mut swap, mut vol, mut nonvol) = (0, 0, 0, 0);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmSwap:") {
            swap = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:") {
            vol = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            nonvol = v.trim().parse().unwrap_or(0);
        }
    }
    (rss, swap, vol, nonvol)
}

fn read_pid_io(pid: i32) -> (u64, u64, u64, u64, u64, u64) {
    let Ok(s) = fs::read_to_string(format!("/proc/{pid}/io")) else {
        return (0, 0, 0, 0, 0, 0);
    };
    let (mut rchar, mut wchar, mut syscr, mut syscw, mut rbytes, mut wbytes) = (0, 0, 0, 0, 0, 0);
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            let val: u64 = v.parse().unwrap_or(0);
            match k {
                "rchar:" => rchar = val,
                "wchar:" => wchar = val,
                "syscr:" => syscr = val,
                "syscw:" => syscw = val,
                "read_bytes:" => rbytes = val,
                "write_bytes:" => wbytes = val,
                _ => {}
            }
        }
    }
    (rchar, wchar, syscr, syscw, rbytes, wbytes)
}

fn read_thread_stat(pid: i32, tid: i32) -> Option<(String, char, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    let o = stat.find('(')?;
    let c = stat.rfind(')')?;
    if c <= o || c + 2 >= stat.len() {
        return None;
    }
    let comm = stat[o + 1..c].to_string();
    let rest: Vec<&str> = stat[c + 2..].split_whitespace().collect();
    if rest.len() < 13 {
        return None;
    }
    let state = rest[0].chars().next().unwrap_or('?');
    let utime: u64 = rest[11].parse().unwrap_or(0);
    let stime: u64 = rest[12].parse().unwrap_or(0);
    Some((comm, state, utime + stime))
}

fn read_wchan(pid: i32, tid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/task/{tid}/wchan"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "0")
        .unwrap_or_else(|| "-".into())
}
