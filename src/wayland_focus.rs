use std::collections::HashMap;
use std::fs;
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
    shared: Arc<Mutex<WaylandFocusState>>,
    socket: String,
}

impl WlrApp {
    fn publish(&self) {
        let active = self.toplevels.values().find(|t| t.activated);
        if let Ok(mut st) = self.shared.lock() {
            st.connected = self.manager_bound;
            st.socket = self.socket.clone();
            st.focused_app_id = active
                .map(|t| t.app_id.clone())
                .filter(|s| !s.is_empty());
            st.focused_title = active.map(|t| t.title.clone()).filter(|s| !s.is_empty());
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
            state.toplevels.insert(toplevel.id(), ToplevelInfo::default());
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
                // Wayland array of u32 in native endianness
                let mut activated = false;
                for chunk in raw_bytes.chunks_exact(4) {
                    let val = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    if val == ToplevelState::Activated as u32 {
                        activated = true;
                    }
                }
                state.toplevels.entry(id).or_default().activated = activated;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.publish();
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
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
        shared: Arc::clone(&shared),
        socket: sock_name,
    };

    let _registry = display.get_registry(&qh, ());
    // Roundtrip 1: discover globals & bind zwlr_foreign_toplevel_manager_v1
    queue.roundtrip(&mut app).ok()?;
    if !app.manager_bound {
        return None;
    }
    // Roundtrip 2: receive initial toplevel list + app_id + state + done
    queue.roundtrip(&mut app).ok()?;
    app.publish();

    let st = shared.lock().ok()?.clone();
    Some(st)
}

/// Long-running background Wayland focus watcher for `pi-nap run`.
pub struct WaylandFocusWatcher {
    state: Arc<Mutex<WaylandFocusState>>,
    stop: Arc<AtomicBool>,
}

impl WaylandFocusWatcher {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(WaylandFocusState::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let st_bg = Arc::clone(&state);
        let stop_bg = Arc::clone(&stop);

        let _ = thread::Builder::new()
            .name("wayland-focus".into())
            .spawn(move || {
                while !stop_bg.load(Ordering::Relaxed) {
                    if let Err(_) = run_watcher_session(&st_bg, &stop_bg) {
                        if let Ok(mut s) = st_bg.lock() {
                            s.connected = false;
                        }
                    }
                    // Wait before retrying connection
                    for _ in 0..20 {
                        if stop_bg.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            });

        Self { state, stop }
    }

    pub fn snapshot(&self) -> WaylandFocusState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

impl Drop for WaylandFocusWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_watcher_session(
    shared: &Arc<Mutex<WaylandFocusState>>,
    stop: &Arc<AtomicBool>,
) -> Result<(), ()> {
    let (sock_path, sock_name) = find_wayland_socket().ok_or(())?;
    let stream = UnixStream::connect(&sock_path).map_err(|_| ())?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let conn = Connection::from_socket(stream).map_err(|_| ())?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<WlrApp>();
    let qh = queue.handle();

    let mut app = WlrApp {
        manager_bound: false,
        toplevels: HashMap::new(),
        shared: Arc::clone(shared),
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
        let _ = queue.flush();
        if let Some(guard) = queue.prepare_read() {
            match guard.read() {
                Ok(_) => {
                    queue.dispatch_pending(&mut app).map_err(|_| ())?;
                }
                Err(wayland_client::backend::WaylandError::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Normal read timeout so we can check `stop` flag
                }
                Err(_) => return Err(()),
            }
        } else {
            queue.dispatch_pending(&mut app).map_err(|_| ())?;
        }
    }

    Ok(())
}
