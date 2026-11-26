use chrono::Local;
use std::collections::VecDeque;

use crate::jitter::JitterSnapshot;
use crate::metrics::SystemSnapshot;

const MAX_EVENTS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Crit,
}

#[derive(Debug, Clone)]
pub struct StutterEvent {
    pub timestamp: String,
    pub severity: Severity,
    pub jitter_ms: f64,
    pub causes: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub name: &'static str,
    pub ok: bool,
    pub status: String,
    pub advice: &'static str,
}

pub struct Analyzer {
    pub jitter_threshold_ms: f64,
    pub events: VecDeque<StutterEvent>,
    pub total_stutters: u64,
}

impl Analyzer {
    pub fn new(jitter_threshold_ms: f64) -> Self {
        Self {
            jitter_threshold_ms,
            events: VecDeque::with_capacity(MAX_EVENTS),
            total_stutters: 0,
        }
    }

    pub fn reset(&mut self) {
        self.events.clear();
        self.total_stutters = 0;
    }

    pub fn analyze(&mut self, snap: &SystemSnapshot, jitter: &JitterSnapshot) {
        let mut causes: Vec<String> = Vec::new();
        let mut details: Vec<String> = Vec::new();
        let mut severe = false;

        if snap.throttle.under_voltage_now {
            causes.push("UNDER-VOLTAGE".into());
            details.push("PSU voltage drop detected".into());
            severe = true;
        }
        if snap.throttle.throttled_now {
            causes.push("THERMAL-THROTTLE".into());
            if let Some(t) = snap.temp_c {
                details.push(format!("CPU throttled at {t:.1}°C"));
            } else {
                details.push("CPU actively throttled".into());
            }
            severe = true;
        } else if snap.throttle.freq_capped_now || snap.throttle.soft_temp_limit_now {
            causes.push("FREQ-CAPPED".into());
            details.push("ARM clock capped".into());
        }

        let max_disk_busy = snap
            .disks
            .iter()
            .map(|d| d.io_busy_pct)
            .fold(0.0_f64, f64::max);
        let max_disk_await = snap
            .disks
            .iter()
            .map(|d| d.avg_await_ms)
            .fold(0.0_f64, f64::max);

        if snap.cpu.iowait_pct > 20.0
            || max_disk_busy > 80.0
            || max_disk_await > 30.0
            || snap.psi.io.some_avg10 > 10.0
        {
            causes.push("SD/IO-STALL".into());
            details.push(format!(
                "iowait={:.0}% disk_busy={:.0}% await={:.0}ms",
                snap.cpu.iowait_pct, max_disk_busy, max_disk_await
            ));
            if max_disk_await > 80.0 || snap.cpu.iowait_pct > 45.0 {
                severe = true;
            }
        }

        if snap.mem.swap_in_per_sec > 5.0
            || snap.mem.swap_out_per_sec > 5.0
            || snap.mem.alloc_stall_per_sec > 1.0
        {
            causes.push("SWAP-THRASH".into());
            details.push(format!(
                "swap_in={:.0}/s swap_out={:.0}/s alloc_stall={:.0}/s",
                snap.mem.swap_in_per_sec, snap.mem.swap_out_per_sec, snap.mem.alloc_stall_per_sec
            ));
            severe = true;
        } else if snap.mem.maj_faults_per_sec > 20.0 {
            causes.push("MAJOR-FAULTS".into());
            details.push(format!("majflt={:.0}/s", snap.mem.maj_faults_per_sec));
        }

        if snap.cpu.total_pct > 90.0 || snap.psi.cpu.some_avg10 > 25.0 {
            causes.push("CPU-SATURATED".into());
            details.push(format!(
                "cpu={:.0}% runq={}",
                snap.cpu.total_pct, snap.cpu.procs_running
            ));
        }

        if snap.cpu.irq_pct + snap.cpu.softirq_pct > 20.0 {
            causes.push("IRQ-STORM".into());
            details.push(format!(
                "irq+softirq={:.0}% ({:.0}/s)",
                snap.cpu.irq_pct + snap.cpu.softirq_pct,
                snap.cpu.intr_per_sec
            ));
        }

        if snap.cpu.procs_blocked >= 2 {
            causes.push("BLOCKED-TASKS".into());
            details.push(format!("{} tasks in D-state", snap.cpu.procs_blocked));
        }

        let jitter_spike = jitter.window_max_ms >= self.jitter_threshold_ms;
        if jitter_spike || severe {
            if causes.is_empty() {
                causes.push("SCHED-JITTER".into());
                if let Some(top) = snap.processes.iter().max_by(|a, b| {
                    a.cpu_pct
                        .partial_cmp(&b.cpu_pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    details.push(format!(
                        "wakeup overshoot {:.1}ms (top: {} {:.0}%)",
                        jitter.window_max_ms, top.name, top.cpu_pct
                    ));
                } else {
                    details.push(format!("wakeup overshoot {:.1}ms", jitter.window_max_ms));
                }
            }

            let severity = if severe || jitter.window_max_ms >= self.jitter_threshold_ms * 3.0 {
                Severity::Crit
            } else {
                Severity::Warn
            };

            let event = StutterEvent {
                timestamp: Local::now().format("%H:%M:%S").to_string(),
                severity,
                jitter_ms: jitter.window_max_ms,
                causes,
                detail: details.join(" | "),
            };

            if self.events.len() >= MAX_EVENTS {
                self.events.pop_back();
            }
            self.events.push_front(event);
            self.total_stutters += 1;
        }
    }

    pub fn health_checks(&self, snap: &SystemSnapshot, jitter: &JitterSnapshot) -> Vec<HealthCheck> {
        let mut checks = Vec::new();

        // 1. Power / Under-voltage
        let uv_ok = !snap.throttle.under_voltage_now && !snap.throttle.under_voltage_occurred;
        checks.push(HealthCheck {
            name: "Power Supply (Under-voltage)",
            ok: uv_ok,
            status: if snap.throttle.under_voltage_now {
                "UNDER-VOLTAGE NOW!".into()
            } else if snap.throttle.under_voltage_occurred {
                "Under-voltage occurred since boot".into()
            } else if snap.throttle.raw.is_some() {
                "OK (5V stable)".into()
            } else {
                "N/A (non-Pi or no vcgencmd)".into()
            },
            advice: "Use an official 5V/3A (Pi 4) or 5V/5A PD (Pi 5) PSU and short thick USB-C cable.",
        });

        // 2. Thermals
        let temp_ok = snap.temp_c.map(|t| t < 75.0).unwrap_or(true)
            && !snap.throttle.throttled_now
            && !snap.throttle.soft_temp_limit_now;
        checks.push(HealthCheck {
            name: "Thermals & Throttling",
            ok: temp_ok,
            status: match snap.temp_c {
                Some(t) if snap.throttle.throttled_now => format!("{t:.1}°C — THROTTLED!"),
                Some(t) if t >= 75.0 => format!("{t:.1}°C — Running hot"),
                Some(t) => format!("{t:.1}°C — OK"),
                None => "N/A".into(),
            },
            advice: "Add heatsink/active cooler; Pi 4/5 soft-throttles at 80°C and hard-throttles at 85°C.",
        });

        // 3. Scheduler Jitter
        let jit_ok = jitter.p95_ms < self.jitter_threshold_ms;
        checks.push(HealthCheck {
            name: "Scheduler Wakeup Jitter",
            ok: jit_ok,
            status: format!(
                "p95={:.2}ms  p99={:.2}ms  peak={:.2}ms",
                jitter.p95_ms, jitter.p99_ms, jitter.peak_ms
            ),
            advice: "High jitter (>10ms) means tasks are stalled by I/O, swap, IRQ storms, or CPU contention.",
        });

        // 4. Storage / SD Card I/O
        let worst_disk = snap.disks.first();
        let io_ok = snap.cpu.iowait_pct < 15.0
            && worst_disk.map(|d| d.avg_await_ms < 25.0).unwrap_or(true);
        checks.push(HealthCheck {
            name: "SD Card / Storage I/O",
            ok: io_ok,
            status: if let Some(d) = worst_disk {
                format!(
                    "{}: busy={:.0}% await={:.1}ms dirty={:.1}MB",
                    d.name, d.io_busy_pct, d.avg_await_ms, snap.mem.dirty_mb
                )
            } else {
                format!("iowait={:.1}% dirty={:.1}MB", snap.cpu.iowait_pct, snap.mem.dirty_mb)
            },
            advice: "Use an A2-rated microSD or USB/NVMe SSD; mount with noatime; tune vm.dirty_background_ratio.",
        });

        // 5. Memory & Swap
        let mem_pct = if snap.mem.total_mb > 0.0 {
            (snap.mem.used_mb / snap.mem.total_mb) * 100.0
        } else {
            0.0
        };
        let mem_ok = mem_pct < 88.0 && snap.mem.swap_in_per_sec < 2.0 && snap.mem.alloc_stall_per_sec < 1.0;
        checks.push(HealthCheck {
            name: "Memory & Swap Pressure",
            ok: mem_ok,
            status: format!(
                "RAM {:.0}% ({:.0}/{:.0}MB) | Swap {:.0}MB | MajFlt {:.0}/s",
                mem_pct, snap.mem.used_mb, snap.mem.total_mb, snap.mem.swap_used_mb, snap.mem.maj_faults_per_sec
            ),
            advice: "Swap on microSD causes severe micro-stutters. Use zram-swap instead of dphys-swapfile.",
        });

        // 6. CPU Governor
        let gov_ok = snap.governor != "powersave";
        checks.push(HealthCheck {
            name: "CPU Frequency Governor",
            ok: gov_ok,
            status: format!(
                "{} ({:.0}/{:.0} MHz)",
                snap.governor,
                snap.cpu_freq_mhz.unwrap_or(0.0),
                snap.cpu_max_mhz.unwrap_or(0.0)
            ),
            advice: "If using 'powersave' or laggy 'ondemand', try 'performance' or 'schedutil'.",
        });

        checks
    }
}
