//! Screen capture and input injection backends.
//!
//! * `kde`: KWin on Wayland. Capture through KWin's privileged screencast protocol into a
//!   PipeWire stream (no portal dialog), input through KWin's fake-input protocol, clipboard
//!   through wlr-data-control. Needs a `.desktop` file that grants the two protocols, which
//!   `skylight install` writes.
//! * `x11`: any X11 server. MIT-SHM capture driven by XDamage, cursor from XFixes, input via XTEST.
//! * `synthetic`: a moving test pattern, for tests and benchmarks without a display.

pub mod kde;
pub mod pw;
pub mod synthetic;
pub mod x11;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::frame::FrameStore;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MonitorInfo {
    /// Stable connector name, e.g. `HDMI-A-1`. `*` is the whole desktop when there are several.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Position and size in the compositor's global (logical) coordinate space.
    pub x: i32,
    pub y: i32,
    pub logical_width: u32,
    pub logical_height: u32,
    /// Size in physical pixels.
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub scale: f64,
}

/// Pointer buttons, as Linux evdev codes.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchKind {
    Down,
    Move,
    Up,
    Cancel,
}

/// Injects input into the desktop. Coordinates are global logical coordinates.
pub trait InputSink: Send + Sync {
    fn pointer_abs(&self, x: f64, y: f64);
    fn pointer_rel(&self, dx: f64, dy: f64);
    fn button(&self, button: u32, down: bool);
    /// Scroll in "wheel units": 1.0 is one notch of a regular mouse wheel.
    fn scroll(&self, dx: f64, dy: f64);
    /// Linux evdev keycode.
    fn key(&self, code: u32, down: bool);
    fn touch(&self, kind: TouchKind, id: u32, x: f64, y: f64);
    fn flush(&self) {}
}

/// Text clipboard of the remote desktop.
pub trait Clipboard: Send + Sync {
    fn set_text(&self, text: String);
    /// The latest clipboard text, updated whenever something on the desktop copies text.
    fn watch(&self) -> tokio::sync::watch::Receiver<Option<Arc<str>>>;
}

/// A running capture. Dropping the last reference stops it.
pub struct Capture {
    pub store: Arc<FrameStore>,
    pub monitor: MonitorInfo,
    pub _guard: Box<dyn Send + Sync>,
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    fn monitors(&self) -> Vec<MonitorInfo>;
    fn start_capture(&self, monitor: &MonitorInfo) -> Result<Capture>;
    fn input(&self) -> Option<Arc<dyn InputSink>>;
    fn clipboard(&self) -> Option<Arc<dyn Clipboard>>;
}

/// Shares one capture per monitor between every session watching it.
pub struct Hub {
    pub backend: Arc<dyn Backend>,
    live: Mutex<HashMap<String, Weak<Capture>>>,
}

impl Hub {
    pub fn new(backend: Arc<dyn Backend>) -> Hub {
        Hub {
            backend,
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.backend.monitors()
    }

    /// The monitor with `id`, or the first one.
    pub fn resolve(&self, id: Option<&str>) -> Result<MonitorInfo> {
        let mons = self.monitors();
        if mons.is_empty() {
            bail!("no monitors found");
        }
        Ok(match id {
            Some(id) => match mons.iter().find(|m| m.id == id) {
                Some(m) => m.clone(),
                None => bail!("no monitor named {id}"),
            },
            None => mons[0].clone(),
        })
    }

    pub fn capture(&self, monitor: &MonitorInfo) -> Result<Arc<Capture>> {
        let mut live = self.live.lock().unwrap();
        live.retain(|_, w| w.strong_count() > 0);
        if let Some(c) = live.get(&monitor.id).and_then(Weak::upgrade) {
            return Ok(c);
        }
        let c = Arc::new(self.backend.start_capture(monitor)?);
        live.insert(monitor.id.clone(), Arc::downgrade(&c));
        Ok(c)
    }
}

/// Pick a backend: `auto` tries KWin, then X11.
pub fn open_backend(kind: &str, synthetic_fps: u32) -> Result<Arc<dyn Backend>> {
    match kind {
        "kde" | "kwin" => Ok(Arc::new(kde::KdeBackend::connect()?)),
        "x11" => Ok(Arc::new(x11::X11Backend::connect()?)),
        "synthetic" => Ok(Arc::new(synthetic::SyntheticBackend::new(
            1920,
            1080,
            synthetic_fps,
        ))),
        "auto" => {
            let mut errors = Vec::new();
            if kde::wayland_available() {
                match kde::KdeBackend::connect() {
                    Ok(b) => return Ok(Arc::new(b)),
                    Err(e) => errors.push(format!("kde: {e:#}")),
                }
            }
            if std::env::var_os("DISPLAY").is_some() {
                match x11::X11Backend::connect() {
                    // Under GNOME or wlroots the X display is Xwayland, which only holds X
                    // windows: streaming it would show a black desktop. Say so instead.
                    Ok(b) if b.xwayland => errors.push(
                        "x11: DISPLAY is Xwayland inside a Wayland session, which is not the \
                         desktop; this Wayland desktop needs the portal backend (not supported \
                         yet), or log in to an X11 session"
                            .into(),
                    ),
                    Ok(b) => return Ok(Arc::new(b)),
                    Err(e) => errors.push(format!("x11: {e:#}")),
                }
            }
            if errors.is_empty() {
                bail!("no graphical session found (neither a Wayland socket nor DISPLAY)");
            }
            bail!("no capture backend worked: {}", errors.join("; "))
        }
        other => bail!("unknown backend {other} (auto, kde, x11, synthetic)"),
    }
}
