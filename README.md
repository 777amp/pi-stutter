# pi-stutter & pi-nap

A lightweight Rust toolkit for diagnosing and fixing **Raspberry Pi micro-stutter, lag, throttling, I/O stalls, and background app bloat**:

1. **`pi-nap`** — A **macOS App Nap-style background freezer daemon** for Raspberry Pi. Opt-in by default (nothing is frozen until you toggle it on), protects terminals and core system/desktop apps so they can **never** be frozen, uses a grace period + periodic non-breaking wake pulses so frozen apps don't crash or drop connections, and automatically thaws everything on exit.
2. **`pi-stutter`** — A **TUI system & per-process lag profiler** that catches power/thermal throttling, scheduler wakeup jitter, PSI stalls, SD card I/O stalls, AND lets you **attach to a specific laggy app** (like `chromium` or `EasyCLIProxyAPI`) to see *why* it lags even when the Pi looks idle.

---

## Quick Download (Prebuilt Binaries)

Prebuilt, stripped release binaries are included in [`bin/`](https://github.com/777amp/pi-stutter/tree/main/bin):

### Raspberry Pi 64-bit OS (`aarch64` — Pi 3 / 4 / 5 / Zero 2 W)
```bash
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-stutter-aarch64-unknown-linux-gnu -o pi-stutter
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-nap-aarch64-unknown-linux-gnu -o pi-nap
chmod +x pi-stutter pi-nap
```

### Raspberry Pi 32-bit OS (`armv7` — Pi 2 / 3 / 4 32-bit)
```bash
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-stutter-armv7-unknown-linux-gnueabihf -o pi-stutter
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-nap-armv7-unknown-linux-gnueabihf -o pi-nap
chmod +x pi-stutter pi-nap
```

### Linux x86_64
```bash
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-stutter-x86_64-unknown-linux-gnu -o pi-stutter
curl -fsSL https://raw.githubusercontent.com/777amp/pi-stutter/main/bin/pi-nap-x86_64-unknown-linux-gnu -o pi-nap
chmod +x pi-stutter pi-nap
```

---

## 1. `pi-nap` — App Nap Background Freezer

- **Opt-in by default**: No app is frozen until you explicitly enable it.
- **Hardcoded safety protection**: Terminals (`lxterminal`, `xfce4-terminal`, `kitty`, `alacritty`, `foot`, `xterm`, etc.), shells (`bash`, `zsh`, `tmux`, `sshd`), compositors/display servers (`Xorg`, `labwc`, `wayfire`, `openbox`), audio (`pipewire`, `pulseaudio`), and `pi-stutter`/`pi-nap` can **never** be frozen.
- **Non-breaking freeze**: Waits for a grace period (`15s` default) after an app loses window focus before sending `SIGSTOP` to its process tree, briefly pulses it awake (`250ms` every `20s`) so sockets/watchdogs don't time out, immediately thaws (`SIGCONT`) when you click back to it, and **always thaws all frozen apps** on Ctrl+C / shutdown.

```bash
# See running apps and what is protected vs opted-in
./pi-nap list

# Toggle freeze-when-unfocused for specific heavy apps
./pi-nap toggle chromium
./pi-nap toggle EasyCLIProxyAPI

# Run in foreground (with verbose logging)
./pi-nap run -v

# Or install as a systemd user service so it runs automatically
./pi-nap install-service
systemctl --user daemon-reload
systemctl --user enable --now pi-nap

# Check live status or emergency-thaw all apps
./pi-nap status
./pi-nap thaw
```

> **Tip:** You can also toggle App Nap opt-in interactively inside `pi-stutter` on **Tab 6 (`App Nap`)** or by pressing `n` on any process in **Tab 2 (`Processes`)**. Changes hot-reload into `pi-nap` immediately.

---

## 2. `pi-stutter` — System & Process Lag Profiler

### Attach to a Laggy Process
```bash
# Attach directly to an app by name (tracks entire process tree & all threads!)
./pi-stutter --attach chromium
./pi-stutter --attach EasyCLIProxyAPI

# Or attach to a specific PID
./pi-stutter --attach 1234

# One-shot CLI diagnostic snapshot (great over SSH)
./pi-stutter --attach chromium --snapshot
```

When attached, `pi-stutter` inspects:
- **Per-thread CPU & kernel wait (`wchan`)** — catches single-thread bottlenecks (1 core pegged at 100% while Pi total CPU looks low) and exact kernel functions (`futex_wait_queue`, `ext4_sync_file`, `blk_mq_get_tag`, `ep_poll`, `sk_wait_data`).
- **Scheduler Runqueue Wait (`/proc/<pid>/schedstat`)** — exact milliseconds the app spent runnable but waiting for CPU.
- **Voluntary vs Involuntary Context Switches** — separates lock/IPC contention from CPU preemption.
- **Per-process Disk & Syscall I/O (`/proc/<pid>/io`)** — catches SQLite/cache/log write storms on microSD.
- **Swap, Minor/Major Page Faults & Open FDs** — catches GC churn and SD card swap stalls.

### Keybindings

| Key | Action |
|---|---|
| `Tab` / `1`–`6` | Switch views (`Overview`, `Processes`, `I/O & IRQs`, `Diagnostics`, `Attach`, `App Nap`) |
| `↑` / `↓` (or `j`/`k`) | Select process (Tab 2) or app (Tab 6) |
| `Enter` / `a` | Attach deep profiler to selected process/app (`A` in Tab 2 for single PID only) |
| `n` (Tab 2) / `Space` (Tab 6) | Toggle App Nap freeze opt-in for selected app |
| `u` (Tab 6) | Immediately thaw all frozen apps |
| `x` (Tab 5) | Detach from current process |
| `c` / `m` / `f` / `d` (Tab 2) | Sort processes by CPU / Memory / MajFaults / State (`D` first) |
| `p` / `r` / `+/-` / `s` / `q` | Pause, reset peaks, adjust jitter threshold, save report, quit |

---

## Building from Source

```bash
cargo build --release --bins
```
