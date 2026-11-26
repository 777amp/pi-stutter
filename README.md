# pi-stutter

A lightweight Rust TUI profiler purpose-built to diagnose **Raspberry Pi micro-stutter, lag, throttling, SD card I/O stalls, swap thrashing, and scheduler jitter**.

## Quick Download (Prebuilt Binaries)

Prebuilt, stripped release binaries are included in [`bin/`](https://github.com/777amp/pi-stutter/tree/main/bin):

### Raspberry Pi 64-bit OS (`aarch64` — Pi 3/4/5/Zero 2 W)
```bash
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-stutter-aarch64-unknown-linux-gnu -o pi-stutter
chmod +x pi-stutter
./pi-stutter
```

### Raspberry Pi 32-bit OS (`armv7` — Pi 2/3/4 32-bit)
```bash
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-stutter-armv7-unknown-linux-gnueabihf -o pi-stutter
chmod +x pi-stutter
./pi-stutter
```

### Linux x86_64
```bash
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-stutter-x86_64-unknown-linux-gnu -o pi-stutter
chmod +x pi-stutter
./pi-stutter
```

## What It Detects

- **Power & Thermal Throttling** — reads Pi firmware throttle flags (`get_throttled`) to catch under-voltage (`⚡ UNDER-VOLTAGE!`), frequency capping, and thermal throttling (`🔥 THROTTLED!`).
- **Scheduler Wakeup Jitter** — runs a lightweight 10ms background probe to measure actual kernel wakeup overshoot in milliseconds (p95, p99, peak) so brief micro-stutters are never missed.
- **Linux PSI (Pressure Stall Information)** — tracks `/proc/pressure/{cpu,io,memory}` stall percentages and millisecond deltas.
- **SD Card / Storage I/O Stalls** — monitors `/proc/diskstats` (busy %, in-flight I/O, average await ms), `iowait%`, and dirty/writeback memory.
- **Memory & Swap Thrashing** — tracks major page faults/sec (`pgmajfault`), swap-in/out rates, and kernel `allocstall` events.
- **Top Offending Processes & IRQs** — highlights `D`-state (uninterruptible I/O sleep) processes, high major-fault processes, and IRQ storms.
- **Automated Stutter Event Log & Checklist** — correlates jitter spikes with probable root causes and gives actionable fixes.

## Keybindings

| Key | Action |
|---|---|
| `Tab` / `1`-`4` | Switch views (`Overview`, `Processes`, `I/O & IRQs`, `Diagnostics`) |
| `p` | Pause / resume updates |
| `r` | Reset peaks and stutter event log |
| `+` / `-` | Adjust jitter threshold (ms) |
| `c` / `m` / `f` / `d` | Sort processes by CPU / Memory / MajFaults / State (`D` first) |
| `s` | Save a diagnostic report (`pi-stutter-report-*.txt`) |
| `q` / `Esc` | Quit |

## CLI Options

```bash
pi-stutter --interval 500 --jitter-threshold 10
pi-stutter --snapshot   # Print a one-shot diagnostic report and exit
```

## Building from Source

```bash
cargo build --release
```
