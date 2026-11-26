use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, State as ToplevelState, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

#[derive(Debug, Clone, Default)]
pub struct WaylandFocusState {
    pub connected: bool,
    pub socket: String,
    pub focused_app_id: Option<String>,
    pub focused_title: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ToplevelInfo {
    app_id: String,
    title: String,
    activated: bool,
}

struct WlrApp {
    manager_bound: bool,
    toplevels: HashMap<ObjectId, ToplevelInfo>,
    last_activated: Option<ObjectId>,
    shared: Arc<Mutex<WaylandFocusState>>,
    focus_changed: Arc<AtomicBool>,
    socket: String,
}

impl WlrApp {
    fn publish(&self) {
        // Prefer the most recently activated toplevel to avoid HashMap iteration order races
        let active = self
            .last_activated
            .as_ref()
            .and_then(|id| self.toplevels.get(id))
            .filter(|t| t.activated)
            .or_else(|| self.toplevels.values().find(|t| t.activated));

        let new_app_id = active
            .map(|t| t.app_id.clone())
            .filter(|s| !s.is_empty());
        let new_title = active.map(|t| t.title.clone()).filter(|s| !s.is_empty());

        if let Ok(mut st) = self.shared.lock() {
            let changed = st.focused_app_id != new_app_id || st.focused_title != new_title;
            st.connected = self.manager_bound;
            st.socket = self.socket.clone();
            st.focused_app_id = new_app_id;
            st.focused_title = new_title;
            if changed {
                self.focus_changed.store(true, Ordering::Release);
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WlrApp {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ZwlrForeignToplevelManagerV1::interface().name {
                let ver = version.min(3);
                registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(name, ver, qh, ());
                state.manager_bound = true;
                state.publish();
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WlrApp {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state
                .toplevels
                .insert(toplevel.id(), ToplevelInfo::default());
        }
    }

    wayland_client::event_created_child!(WlrApp, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WlrApp {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.toplevels.entry(id).or_default().app_id = app_id;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.toplevels.entry(id).or_default().title = title;
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_bytes } => {
                let mut activated = false;
                for chunk in raw_bytes.chunks_exact(4) {
                    let val = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    if val == ToplevelState::Activated as u32 {
                        activated = true;
                    }
                }
                let entry = state.toplevels.entry(id.clone()).or_default();
                entry.activated = activated;
                if activated {
                    state.last_activated = Some(id);
                } else if state.last_activated.as_ref() == Some(&id) {
                    state.last_activated = None;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.publish();
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
                if state.last_activated.as_ref() == Some(&id) {
                    state.last_activated = None;
                }
                state.publish();
            }
            _ => {}
        }
    }
}

/// Finds the active Wayland socket path, even when WAYLAND_DISPLAY is unset (e.g. systemd / SSH).
pub fn find_wayland_socket() -> Option<(PathBuf, String)> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::geteuid() };
            PathBuf::from(format!("/run/user/{uid}"))
        });

    if let Ok(wd) = std::env::var("WAYLAND_DISPLAY") {
        if !wd.is_empty() {
            let p = if wd.starts_with('/') {
                PathBuf::from(&wd)
            } else {
                runtime_dir.join(&wd)
            };
            if p.exists() {
                return Some((p, wd));
            }
        }
    }

    // Fallback: scan /run/user/<uid>/wayland-*
    let entries = fs::read_dir(&runtime_dir).ok()?;
    let mut candidates: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("wayland-") && !name.ends_with(".lock") {
                Some((e.path(), name))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by(|a, b| a.1.cmp(&b.1));
    candidates.into_iter().next()
}

/// One-shot probe for `pi-nap setup` / `pi-nap list`.
pub fn probe_wayland_focus() -> Option<WaylandFocusState> {
    let (sock_path, sock_name) = find_wayland_socket()?;
    let stream = UnixStream::connect(&sock_path).ok()?;
    let conn = Connection::from_socket(stream).ok()?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<WlrApp>();
    let qh = queue.handle();

    let shared = Arc::new(Mutex::new(WaylandFocusState::default()));
    let mut app = WlrApp {
        manager_bound: false,
        toplevels: HashMap::new(),
        last_activated: None,
        shared: Arc::clone(&shared),
        focus_changed: Arc::new(AtomicBool::new(false)),
        socket: sock_name,
    };

    let _registry = display.get_registry(&qh, ());
    queue.roundtrip(&mut app).ok()?;
    if !app.manager_bound {
        return None;
    }
    queue.roundtrip(&mut app).ok()?;
    app.publish();

    let st = shared.lock().ok()?.clone();
    Some(st)
}

/// Long-running background Wayland focus watcher for `pi-nap run`.
pub struct WaylandFocusWatcher {
    state: Arc<Mutex<WaylandFocusState>>,
    focus_changed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl WaylandFocusWatcher {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(WaylandFocusState::default()));
        let focus_changed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let st_bg = Arc::clone(&state);
        let fc_bg = Arc::clone(&focus_changed);
        let stop_bg = Arc::clone(&stop);

        let _ = thread::Builder::new()
            .name("wayland-focus".into())
            .spawn(move || {
                while !stop_bg.load(Ordering::Relaxed) {
                    if run_watcher_session(&st_bg, &fc_bg, &stop_bg).is_err() {
                        if let Ok(mut s) = st_bg.lock() {
                            s.connected = false;
                        }
                    }
                    for _ in 0..20 {
                        if stop_bg.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            });

        Self {
            state,
            focus_changed,
            stop,
        }
    }

    pub fn snapshot(&self) -> WaylandFocusState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Returns true if focus changed since last check (clears flag).
    pub fn take_focus_changed(&self) -> bool {
        self.focus_changed.swap(false, Ordering::AcqRel)
    }
}

impl Drop for WaylandFocusWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_watcher_session(
    shared: &Arc<Mutex<WaylandFocusState>>,
    focus_changed: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
) -> Result<(), ()> {
    let (sock_path, sock_name) = find_wayland_socket().ok_or(())?;
    let stream = UnixStream::connect(&sock_path).map_err(|_| ())?;
    let conn = Connection::from_socket(stream).map_err(|_| ())?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<WlrApp>();
    let qh = queue.handle();

    let mut app = WlrApp {
        manager_bound: false,
        toplevels: HashMap::new(),
        last_activated: None,
        shared: Arc::clone(shared),
        focus_changed: Arc::clone(focus_changed),
        socket: sock_name,
    };

    let _registry = display.get_registry(&qh, ());
    queue.roundtrip(&mut app).map_err(|_| ())?;
    if !app.manager_bound {
        return Err(());
    }
    queue.roundtrip(&mut app).map_err(|_| ())?;
    app.publish();

    while !stop.load(Ordering::Relaxed) {
        queue.dispatch_pending(&mut app).map_err(|_| ())?;
        let _ = queue.flush();

        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd, 1, 100) };
            if ret > 0 {
                if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                    return Err(());
                }
                match guard.read() {
                    Ok(_) => {
                        queue.dispatch_pending(&mut app).map_err(|_| ())?;
                    }
                    Err(wayland_client::backend::WaylandError::Io(ref e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return Err(()),
                }
            } else if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(());
                }
            }
            // If ret == 0 (timeout), guard drops and cancels read cleanly.
        }
    }

    Ok(())
}
