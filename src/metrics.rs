use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct ThrottleStatus {
    pub raw: Option<u32>,
    pub under_voltage_now: bool,
    pub freq_capped_now: bool,
    pub throttled_now: bool,
    pub soft_temp_limit_now: bool,
    pub under_voltage_occurred: bool,
    pub freq_capped_occurred: bool,
    pub throttled_occurred: bool,
    pub soft_temp_limit_occurred: bool,
}

impl ThrottleStatus {
    pub fn from_raw(v: u32) -> Self {
        Self {
            raw: Some(v),
            under_voltage_now: (v & (1 << 0)) != 0,
            freq_capped_now: (v & (1 << 1)) != 0,
            throttled_now: (v & (1 << 2)) != 0,
            soft_temp_limit_now: (v & (1 << 3)) != 0,
            under_voltage_occurred: (v & (1 << 16)) != 0,
            freq_capped_occurred: (v & (1 << 17)) != 0,
            throttled_occurred: (v & (1 << 18)) != 0,
            soft_temp_limit_occurred: (v & (1 << 19)) != 0,
        }
    }

    pub fn any_active(&self) -> bool {
        self.under_voltage_now
            || self.freq_capped_now
            || self.throttled_now
            || self.soft_temp_limit_now
    }

    pub fn any_historical(&self) -> bool {
        self.under_voltage_occurred
            || self.freq_capped_occurred
            || self.throttled_occurred
            || self.soft_temp_limit_occurred
    }
}

#[derive(Debug, Clone, Default)]
pub struct CpuMetrics {
    pub total_pct: f64,
    pub user_pct: f64,
    pub sys_pct: f64,
    pub iowait_pct: f64,
    pub irq_pct: f64,
    pub softirq_pct: f64,
    pub ctxt_per_sec: f64,
    pub intr_per_sec: f64,
    pub procs_running: u64,
    pub procs_blocked: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PsiMetric {
    pub some_avg10: f64,
    pub full_avg10: f64,
    pub some_delta_ms: f64,
    pub full_delta_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct PsiMetrics {
    pub available: bool,
    pub cpu: PsiMetric,
    pub io: PsiMetric,
    pub memory: PsiMetric,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryMetrics {
    pub total_mb: f64,
    pub used_mb: f64,
    pub available_mb: f64,
    pub dirty_mb: f64,
    pub writeback_mb: f64,
    pub swap_total_mb: f64,
    pub swap_used_mb: f64,
    pub maj_faults_per_sec: f64,
    pub swap_in_per_sec: f64,
    pub swap_out_per_sec: f64,
    pub alloc_stall_per_sec: f64,
}

#[derive(Debug, Clone, Default)]
pub struct DiskMetric {
    pub name: String,
    pub read_kb_s: f64,
    pub write_kb_s: f64,
    pub io_busy_pct: f64,
    pub in_flight: u64,
    pub avg_await_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct IrqMetric {
    pub irq: String,
    pub label: String,
    pub rate_per_sec: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ProcessMetric {
    pub pid: i32,
    pub name: String,
    pub state: char,
    pub cpu_pct: f64,
    pub maj_flt_per_sec: f64,
    pub rss_mb: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    pub model: String,
    pub temp_c: Option<f64>,
    pub cpu_freq_mhz: Option<f64>,
    pub cpu_max_mhz: Option<f64>,
    pub governor: String,
    pub throttle: ThrottleStatus,
    pub cpu: CpuMetrics,
    pub psi: PsiMetrics,
    pub mem: MemoryMetrics,
    pub disks: Vec<DiskMetric>,
    pub irqs: Vec<IrqMetric>,
    pub processes: Vec<ProcessMetric>,
    pub load_avg: (f64, f64, f64),
    pub num_cpus: usize,
}

#[derive(Debug, Clone, Default)]
struct RawCpuStat {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
    ctxt: u64,
    intr: u64,
    procs_running: u64,
    procs_blocked: u64,
}

impl RawCpuStat {
    fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }
}

#[derive(Debug, Clone, Default)]
struct RawVmStat {
    pgmajfault: u64,
    pswpin: u64,
    pswpout: u64,
    allocstall: u64,
}

#[derive(Debug, Clone, Default)]
struct RawDiskStat {
    reads_completed: u64,
    sectors_read: u64,
    writes_completed: u64,
    sectors_written: u64,
    in_flight: u64,
    io_ms: u64,
    weighted_io_ms: u64,
}

#[derive(Debug, Clone, Default)]
struct RawProcStat {
    utime: u64,
    stime: u64,
    majflt: u64,
}

pub struct MetricsCollector {
    model: String,
    num_cpus: usize,
    clk_tck: f64,
    page_size_kb: f64,
    last_time: Instant,
    prev_cpu: Option<RawCpuStat>,
    prev_vm: Option<RawVmStat>,
    prev_psi: HashMap<&'static str, (u64, u64)>,
    prev_disks: HashMap<String, RawDiskStat>,
    prev_irqs: HashMap<String, u64>,
    prev_procs: HashMap<i32, RawProcStat>,
    vcgencmd_available: bool,
}

impl MetricsCollector {
    pub fn new() -> Self {
        let model = read_model();
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let vcgencmd_available = Command::new("vcgencmd")
            .arg("get_throttled")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        Self {
            model,
            num_cpus,
            clk_tck: 100.0, // Standard Linux USER_HZ
            page_size_kb: 4.0,
            last_time: Instant::now(),
            prev_cpu: None,
            prev_vm: None,
            prev_psi: HashMap::new(),
            prev_disks: HashMap::new(),
            prev_irqs: HashMap::new(),
            prev_procs: HashMap::new(),
            vcgencmd_available,
        }
    }

    pub fn collect(&mut self) -> SystemSnapshot {
        let now = Instant::now();
        let dt = now.duration_since(self.last_time).as_secs_f64().max(0.001);
        self.last_time = now;

        let cpu = self.collect_cpu(dt);
        let psi = self.collect_psi();
        let mem = self.collect_memory(dt);
        let disks = self.collect_disks(dt);
        let irqs = self.collect_irqs(dt);
        let processes = self.collect_processes(dt);

        SystemSnapshot {
            model: self.model.clone(),
            temp_c: read_temp_c(),
            cpu_freq_mhz: read_cpufreq_mhz("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"),
            cpu_max_mhz: read_cpufreq_mhz("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"),
            governor: read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
                .unwrap_or_else(|| "n/a".into()),
            throttle: read_throttle_status(self.vcgencmd_available),
            cpu,
            psi,
            mem,
            disks,
            irqs,
            processes,
            load_avg: read_load_avg(),
            num_cpus: self.num_cpus,
        }
    }

    fn collect_cpu(&mut self, dt: f64) -> CpuMetrics {
        let raw = parse_proc_stat().unwrap_or_default();
        let mut metrics = CpuMetrics {
            procs_running: raw.procs_running,
            procs_blocked: raw.procs_blocked,
            ..Default::default()
        };

        if let Some(prev) = &self.prev_cpu {
            let total_d = raw.total().saturating_sub(prev.total()) as f64;
            if total_d > 0.0 {
                let user_d = (raw.user + raw.nice).saturating_sub(prev.user + prev.nice) as f64;
                let sys_d = raw.system.saturating_sub(prev.system) as f64;
                let iowait_d = raw.iowait.saturating_sub(prev.iowait) as f64;
                let irq_d = raw.irq.saturating_sub(prev.irq) as f64;
                let softirq_d = raw.softirq.saturating_sub(prev.softirq) as f64;
                let idle_d = raw.idle.saturating_sub(prev.idle) as f64;

                metrics.user_pct = (user_d / total_d) * 100.0;
                metrics.sys_pct = (sys_d / total_d) * 100.0;
                metrics.iowait_pct = (iowait_d / total_d) * 100.0;
                metrics.irq_pct = (irq_d / total_d) * 100.0;
                metrics.softirq_pct = (softirq_d / total_d) * 100.0;
                metrics.total_pct = ((total_d - idle_d - iowait_d).max(0.0) / total_d) * 100.0;
            }
            metrics.ctxt_per_sec = raw.ctxt.saturating_sub(prev.ctxt) as f64 / dt;
            metrics.intr_per_sec = raw.intr.saturating_sub(prev.intr) as f64 / dt;
        }

        self.prev_cpu = Some(raw);
        metrics
    }

    fn collect_psi(&mut self) -> PsiMetrics {
        let mut psi = PsiMetrics::default();
        let mut any = false;

        for (key, path) in [
            ("cpu", "/proc/pressure/cpu"),
            ("io", "/proc/pressure/io"),
            ("memory", "/proc/pressure/memory"),
        ] {
            if let Some((some_avg10, some_total, full_avg10, full_total)) = parse_psi(path) {
                any = true;
                let (prev_some, prev_full) = self
                    .prev_psi
                    .insert(key, (some_total, full_total))
                    .unwrap_or((some_total, full_total));

                let metric = PsiMetric {
                    some_avg10,
                    full_avg10,
                    some_delta_ms: some_total.saturating_sub(prev_some) as f64 / 1000.0,
                    full_delta_ms: full_total.saturating_sub(prev_full) as f64 / 1000.0,
                };
                match key {
                    "cpu" => psi.cpu = metric,
                    "io" => psi.io = metric,
                    "memory" => psi.memory = metric,
                    _ => {}
                }
            }
        }

        psi.available = any;
        psi
    }

    fn collect_memory(&mut self, dt: f64) -> MemoryMetrics {
        let mut mem = MemoryMetrics::default();
        if let Ok(content) = fs::read_to_string("/proc/meminfo") {
            let mut map: HashMap<&str, f64> = HashMap::new();
            for line in content.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                    if let Ok(kb) = v.parse::<f64>() {
                        map.insert(k.trim_end_matches(':'), kb / 1024.0);
                    }
                }
            }
            mem.total_mb = *map.get("MemTotal").unwrap_or(&0.0);
            mem.available_mb = *map.get("MemAvailable").unwrap_or(&0.0);
            mem.used_mb = (mem.total_mb - mem.available_mb).max(0.0);
            mem.dirty_mb = *map.get("Dirty").unwrap_or(&0.0);
            mem.writeback_mb = *map.get("Writeback").unwrap_or(&0.0);
            mem.swap_total_mb = *map.get("SwapTotal").unwrap_or(&0.0);
            let swap_free = *map.get("SwapFree").unwrap_or(&0.0);
            mem.swap_used_mb = (mem.swap_total_mb - swap_free).max(0.0);
        }

        let raw_vm = parse_vmstat().unwrap_or_default();
        if let Some(prev) = &self.prev_vm {
            mem.maj_faults_per_sec = raw_vm.pgmajfault.saturating_sub(prev.pgmajfault) as f64 / dt;
            mem.swap_in_per_sec = raw_vm.pswpin.saturating_sub(prev.pswpin) as f64 / dt;
            mem.swap_out_per_sec = raw_vm.pswpout.saturating_sub(prev.pswpout) as f64 / dt;
            mem.alloc_stall_per_sec = raw_vm.allocstall.saturating_sub(prev.allocstall) as f64 / dt;
        }
        self.prev_vm = Some(raw_vm);

        mem
    }

    fn collect_disks(&mut self, dt: f64) -> Vec<DiskMetric> {
        let mut result = Vec::new();
        let Ok(content) = fs::read_to_string("/proc/diskstats") else {
            return result;
        };

        let mut current = HashMap::new();
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 14 {
                continue;
            }
            let name = parts[2];
            if !is_interesting_disk(name) {
                continue;
            }
            let stat = RawDiskStat {
                reads_completed: parts[3].parse().unwrap_or(0),
                sectors_read: parts[5].parse().unwrap_or(0),
                writes_completed: parts[7].parse().unwrap_or(0),
                sectors_written: parts[9].parse().unwrap_or(0),
                in_flight: parts[11].parse().unwrap_or(0),
                io_ms: parts[12].parse().unwrap_or(0),
                weighted_io_ms: parts[13].parse().unwrap_or(0),
            };

            if let Some(prev) = self.prev_disks.get(name) {
                let d_sec_r = stat.sectors_read.saturating_sub(prev.sectors_read) as f64;
                let d_sec_w = stat.sectors_written.saturating_sub(prev.sectors_written) as f64;
                let d_io_ms = stat.io_ms.saturating_sub(prev.io_ms) as f64;
                let d_weighted = stat.weighted_io_ms.saturating_sub(prev.weighted_io_ms) as f64;
                let d_ops = stat.reads_completed.saturating_sub(prev.reads_completed)
                    + stat.writes_completed.saturating_sub(prev.writes_completed);

                let busy_pct = ((d_io_ms / (dt * 1000.0)) * 100.0).min(100.0);
                let avg_await = if d_ops > 0 {
                    d_weighted / d_ops as f64
                } else {
                    0.0
                };

                result.push(DiskMetric {
                    name: name.to_string(),
                    read_kb_s: (d_sec_r * 0.5) / dt,
                    write_kb_s: (d_sec_w * 0.5) / dt,
                    io_busy_pct: busy_pct,
                    in_flight: stat.in_flight,
                    avg_await_ms: avg_await,
                });
            }
            current.insert(name.to_string(), stat);
        }

        self.prev_disks = current;
        result.sort_by(|a, b| {
            b.io_busy_pct
                .partial_cmp(&a.io_busy_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }

    fn collect_irqs(&mut self, dt: f64) -> Vec<IrqMetric> {
        let mut result = Vec::new();
        let Ok(content) = fs::read_to_string("/proc/interrupts") else {
            return result;
        };

        let mut lines = content.lines();
        let Some(header) = lines.next() else {
            return result;
        };
        let cpu_cols = header.split_whitespace().count();
        let mut current = HashMap::new();

        for line in lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() <= cpu_cols + 1 {
                continue;
            }
            let irq = parts[0].trim_end_matches(':').to_string();
            let mut sum: u64 = 0;
            let mut valid = true;
            for col in &parts[1..=cpu_cols] {
                if let Ok(v) = col.parse::<u64>() {
                    sum = sum.saturating_add(v);
                } else {
                    valid = false;
                    break;
                }
            }
            if !valid {
                continue;
            }
            let label = parts[cpu_cols + 1..].join(" ");
            if let Some(&prev) = self.prev_irqs.get(&irq) {
                let rate = sum.saturating_sub(prev) as f64 / dt;
                if rate > 0.5 {
                    result.push(IrqMetric {
                        irq: irq.clone(),
                        label,
                        rate_per_sec: rate,
                    });
                }
            }
            current.insert(irq, sum);
        }

        self.prev_irqs = current;
        result.sort_by(|a, b| {
            b.rate_per_sec
                .partial_cmp(&a.rate_per_sec)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result.truncate(15);
        result
    }

    fn collect_processes(&mut self, dt: f64) -> Vec<ProcessMetric> {
        let mut procs = Vec::new();
        let mut current = HashMap::new();

        let Ok(entries) = fs::read_dir("/proc") else {
            return procs;
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            let Ok(pid) = name_str.parse::<i32>() else {
                continue;
            };

            let stat_path = format!("/proc/{pid}/stat");
            let Ok(stat_str) = fs::read_to_string(&stat_path) else {
                continue;
            };

            let Some(open_paren) = stat_str.find('(') else {
                continue;
            };
            let Some(close_paren) = stat_str.rfind(')') else {
                continue;
            };
            if close_paren <= open_paren || close_paren + 2 >= stat_str.len() {
                continue;
            }

            let comm = stat_str[open_paren + 1..close_paren].to_string();
            let rest: Vec<&str> = stat_str[close_paren + 2..].split_whitespace().collect();
            if rest.len() < 22 {
                continue;
            }

            let state = rest[0].chars().next().unwrap_or('?');
            let majflt: u64 = rest[9].parse().unwrap_or(0);
            let utime: u64 = rest[11].parse().unwrap_or(0);
            let stime: u64 = rest[12].parse().unwrap_or(0);
            let rss_pages: u64 = rest[21].parse().unwrap_or(0);

            let (cpu_pct, maj_flt_per_sec) = if let Some(prev) = self.prev_procs.get(&pid) {
                let d_ticks = (utime + stime).saturating_sub(prev.utime + prev.stime) as f64;
                let cpu = (d_ticks / self.clk_tck / dt) * 100.0;
                let mf = majflt.saturating_sub(prev.majflt) as f64 / dt;
                (cpu, mf)
            } else {
                (0.0, 0.0)
            };

            current.insert(
                pid,
                RawProcStat {
                    utime,
                    stime,
                    majflt,
                },
            );

            procs.push(ProcessMetric {
                pid,
                name: comm,
                state,
                cpu_pct,
                maj_flt_per_sec,
                rss_mb: (rss_pages as f64 * self.page_size_kb) / 1024.0,
            });
        }

        self.prev_procs = current;
        procs
    }
}

fn read_model() -> String {
    if let Ok(raw) = fs::read_to_string("/proc/device-tree/model") {
        let cleaned = raw.trim_matches('\0').trim().to_string();
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    if let Ok(raw) = fs::read_to_string("/sys/devices/virtual/dmi/id/product_name") {
        let cleaned = raw.trim().to_string();
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    "Linux System".to_string()
}

fn read_temp_c() -> Option<f64> {
    for path in [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/hwmon/hwmon0/temp1_input",
    ] {
        if let Some(s) = read_trimmed(path) {
            if let Ok(millideg) = s.parse::<f64>() {
                return Some(millideg / 1000.0);
            }
        }
    }
    None
}

fn read_cpufreq_mhz(path: &str) -> Option<f64> {
    read_trimmed(path)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|khz| khz / 1000.0)
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_throttle_status(vcgencmd_available: bool) -> ThrottleStatus {
    // 1. Try sysfs (faster, no fork!)
    let sysfs_path = "/sys/devices/platform/soc/soc:firmware/get_throttled";
    if Path::new(sysfs_path).exists() {
        if let Some(s) = read_trimmed(sysfs_path) {
            let hex = s.trim_start_matches("0x").trim_start_matches("0X");
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return ThrottleStatus::from_raw(v);
            }
        }
    }
    // 2. Fallback to vcgencmd
    if vcgencmd_available {
        if let Ok(out) = Command::new("vcgencmd").arg("get_throttled").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(hex_part) = s.trim().split('=').nth(1) {
                    let hex = hex_part.trim_start_matches("0x").trim_start_matches("0X");
                    if let Ok(v) = u32::from_str_radix(hex, 16) {
                        return ThrottleStatus::from_raw(v);
                    }
                }
            }
        }
    }
    ThrottleStatus::default()
}

fn parse_proc_stat() -> Option<RawCpuStat> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    let mut raw = RawCpuStat::default();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("cpu") => {
                raw.user = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.nice = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.system = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.idle = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.iowait = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.irq = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.softirq = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                raw.steal = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            Some("ctxt") => {
                raw.ctxt = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            Some("intr") => {
                raw.intr = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            Some("procs_running") => {
                raw.procs_running = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            Some("procs_blocked") => {
                raw.procs_blocked = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            _ => {}
        }
    }
    Some(raw)
}

fn parse_psi(path: &str) -> Option<(f64, u64, f64, u64)> {
    let content = fs::read_to_string(path).ok()?;
    let mut some_avg10 = 0.0;
    let mut some_total = 0;
    let mut full_avg10 = 0.0;
    let mut full_total = 0;

    for line in content.lines() {
        let is_some = line.starts_with("some ");
        let is_full = line.starts_with("full ");
        if !is_some && !is_full {
            continue;
        }
        for tok in line.split_whitespace().skip(1) {
            if let Some(v) = tok.strip_prefix("avg10=") {
                if let Ok(f) = v.parse::<f64>() {
                    if is_some {
                        some_avg10 = f;
                    } else {
                        full_avg10 = f;
                    }
                }
            } else if let Some(v) = tok.strip_prefix("total=") {
                if let Ok(t) = v.parse::<u64>() {
                    if is_some {
                        some_total = t;
                    } else {
                        full_total = t;
                    }
                }
            }
        }
    }
    Some((some_avg10, some_total, full_avg10, full_total))
}

fn parse_vmstat() -> Option<RawVmStat> {
    let content = fs::read_to_string("/proc/vmstat").ok()?;
    let mut raw = RawVmStat::default();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            let val: u64 = v.parse().unwrap_or(0);
            match k {
                "pgmajfault" => raw.pgmajfault = val,
                "pswpin" => raw.pswpin = val,
                "pswpout" => raw.pswpout = val,
                _ if k.starts_with("allocstall") => {
                    raw.allocstall = raw.allocstall.saturating_add(val)
                }
                _ => {}
            }
        }
    }
    Some(raw)
}

fn is_interesting_disk(name: &str) -> bool {
    // Include whole disks (mmcblk0, sda, nvme0n1, vda, zram0) and skip partitions & loop/ram
    if name.starts_with("mmcblk") {
        !name.contains('p')
    } else if name.starts_with("nvme") {
        !name.contains('p')
    } else if name.starts_with("sd") || name.starts_with("vd") {
        !name.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false)
    } else {
        name.starts_with("zram")
    }
}

fn read_load_avg() -> (f64, f64, f64) {
    let Ok(s) = fs::read_to_string("/proc/loadavg") else {
        return (0.0, 0.0, 0.0);
    };
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 3 {
        (
            parts[0].parse().unwrap_or(0.0),
            parts[1].parse().unwrap_or(0.0),
            parts[2].parse().unwrap_or(0.0),
        )
    } else {
        (0.0, 0.0, 0.0)
    }
}
