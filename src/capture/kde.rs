//! KWin (KDE Plasma on Wayland).
//!
//! KWin keeps two protocols for trusted clients: `zkde_screencast_unstable_v1` hands out a
//! PipeWire node for any output without the portal's "share your screen" dialog, and
//! `org_kde_kwin_fake_input` injects pointer, keyboard and touch events. KWin only advertises
//! them to a client whose executable is listed in an installed `.desktop` file under
//! `X-KDE-Wayland-Interfaces`; `skylight install` writes that file.

use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use super::{Backend, Capture, Clipboard, InputSink, MonitorInfo, TouchKind};
use crate::frame::FrameStore;

#[allow(
    dead_code,
    non_camel_case_types,
    non_upper_case_globals,
    unused_imports,
    clippy::all
)]
pub mod protocols {
    pub mod fake_input {
        use wayland_client;
        use wayland_client::protocol::*;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/fake-input.xml");
    }
    pub mod screencast {
        use wayland_client;
        use wayland_client::protocol::*;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
    }
}

use protocols::fake_input::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use protocols::screencast::zkde_screencast_stream_unstable_v1::{
    self as sc_stream, ZkdeScreencastStreamUnstableV1,
};
use protocols::screencast::zkde_screencast_unstable_v1::{self as sc, ZkdeScreencastUnstableV1};

pub fn wayland_available() -> bool {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    std::path::Path::new(&dir).join(name).exists()
}

#[derive(Default, Clone)]
struct OutputState {
    global: u32,
    name: String,
    description: String,
    make_model: String,
    px_w: u32,
    px_h: u32,
    refresh_mhz: u32,
    scale: i32,
    logical: Option<(i32, i32, i32, i32)>,
    done: bool,
}

struct OutputEntry {
    proxy: wl_output::WlOutput,
    state: OutputState,
}

#[derive(Default)]
struct Shared {
    outputs: Mutex<Vec<OutputEntry>>,
}

struct OutputData {
    global: u32,
}

enum StreamEvent {
    Created(u32),
    Failed(String),
    Closed,
}

struct StreamData {
    tx: Mutex<Option<mpsc::Sender<StreamEvent>>>,
    /// Set once the stream runs, so a later close or failure reaches the sessions reading it.
    store: Mutex<Option<Arc<FrameStore>>>,
}

struct ClipState {
    watch_tx: tokio::sync::watch::Sender<Option<Arc<str>>>,
    /// Text we offer as the selection owner, if any.
    offering: Option<Arc<str>>,
    source: Option<zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
}

#[derive(Default)]
struct OfferData {
    mimes: Mutex<Vec<String>>,
}

struct State {
    shared: Arc<Shared>,
    clip: Arc<Mutex<ClipState>>,
}

pub struct KdeBackend {
    conn: Connection,
    qh: QueueHandle<State>,
    shared: Arc<Shared>,
    screencast: ZkdeScreencastUnstableV1,
    fake_input: Option<Arc<KdeInput>>,
    clipboard: Option<Arc<KdeClipboard>>,
}

impl KdeBackend {
    pub fn connect() -> Result<KdeBackend> {
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            // SAFETY: called once at startup, before any other thread reads the environment.
            unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", format!("/run/user/{}", libc::getuid()))
            };
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            unsafe { std::env::set_var("WAYLAND_DISPLAY", "wayland-0") };
        }
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("reading Wayland globals")?;
        let qh = queue.handle();
        let shared = Arc::new(Shared::default());
        let (watch_tx, _) = tokio::sync::watch::channel(None);
        let clip = Arc::new(Mutex::new(ClipState {
            watch_tx,
            offering: None,
            source: None,
        }));
        let mut state = State {
            shared: shared.clone(),
            clip: clip.clone(),
        };

        let names: Vec<String> = globals
            .contents()
            .with_list(|l| l.iter().map(|g| g.interface.clone()).collect());
        let screencast: ZkdeScreencastUnstableV1 = globals.bind(&qh, 1..=5, ()).map_err(|_| {
            if names.iter().any(|n| n == "org_kde_plasma_shell") {
                anyhow!(
                    "KWin did not grant zkde_screencast_unstable_v1 to this executable ({}). Run `skylight install` so KWin trusts it, then restart skylight",
                    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
                )
            } else {
                anyhow!("this compositor is not KWin (no zkde_screencast_unstable_v1)")
            }
        })?;
        let fake_input: Option<OrgKdeKwinFakeInput> = globals.bind(&qh, 1..=5, ()).ok();
        if let Some(fi) = &fake_input {
            fi.authenticate("Skylight".into(), "Remote desktop input".into());
        }
        let xdg_mgr: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1> =
            globals.bind(&qh, 1..=3, ()).ok();
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                let out: wl_output::WlOutput = globals.registry().bind(
                    g.name,
                    g.version.min(4),
                    &qh,
                    OutputData { global: g.name },
                );
                if let Some(m) = &xdg_mgr {
                    m.get_xdg_output(&out, &qh, OutputData { global: g.name });
                }
                shared.outputs.lock().unwrap().push(OutputEntry {
                    proxy: out,
                    state: OutputState {
                        global: g.name,
                        scale: 1,
                        ..Default::default()
                    },
                });
            }
        });
        let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=1, ()).ok();
        let dc_mgr: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1> =
            globals.bind(&qh, 1..=2, ()).ok();
        let mut clipboard = None;
        if let (Some(seat), Some(mgr)) = (&seat, &dc_mgr) {
            let device = mgr.get_data_device(seat, &qh, ());
            clipboard = Some(Arc::new(KdeClipboard {
                conn: conn.clone(),
                qh: qh.clone(),
                mgr: mgr.clone(),
                device,
                clip: clip.clone(),
            }));
        }
        // Two roundtrips: the first delivers output events, the second the xdg-output ones.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;

        std::thread::Builder::new()
            .name("wayland".into())
            .spawn(move || {
                loop {
                    if let Err(e) = queue.blocking_dispatch(&mut state) {
                        // Every capture and input path depends on this connection (KWin
                        // restarted or the session ended). Exit so the service manager starts a
                        // fresh server that connects again.
                        tracing::error!("Wayland connection lost, exiting: {e}");
                        std::process::exit(1);
                    }
                }
            })?;

        let fake_input = fake_input.map(|fi| {
            Arc::new(KdeInput {
                conn: conn.clone(),
                fi,
            })
        });
        if fake_input.is_none() {
            tracing::warn!("KWin did not grant org_kde_kwin_fake_input; input is disabled");
        }
        Ok(KdeBackend {
            conn,
            qh,
            shared,
            screencast,
            fake_input,
            clipboard,
        })
    }

    fn output_proxy(&self, id: &str) -> Option<wl_output::WlOutput> {
        let outs = self.shared.outputs.lock().unwrap();
        outs.iter()
            .find(|o| o.state.name == id)
            .map(|o| o.proxy.clone())
    }
}

fn monitor_from(o: &OutputState) -> Option<MonitorInfo> {
    if o.name.is_empty() || o.px_w == 0 {
        return None;
    }
    let scale = o.scale.max(1);
    let (x, y, lw, lh) = o
        .logical
        .unwrap_or((0, 0, o.px_w as i32 / scale, o.px_h as i32 / scale));
    Some(MonitorInfo {
        id: o.name.clone(),
        name: if !o.make_model.trim().is_empty() {
            o.make_model.trim().to_string()
        } else {
            o.description.clone()
        },
        x,
        y,
        logical_width: lw.max(1) as u32,
        logical_height: lh.max(1) as u32,
        width: o.px_w,
        height: o.px_h,
        refresh_mhz: o.refresh_mhz,
        scale: o.px_w as f64 / lw.max(1) as f64,
    })
}

impl Backend for KdeBackend {
    fn name(&self) -> &'static str {
        "kde"
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        let outs = self.shared.outputs.lock().unwrap();
        let mut mons: Vec<MonitorInfo> =
            outs.iter().filter_map(|o| monitor_from(&o.state)).collect();
        mons.sort_by_key(|m| (m.x, m.y));
        if mons.len() > 1 && self.screencast.version() >= 3 {
            let x0 = mons.iter().map(|m| m.x).min().unwrap();
            let y0 = mons.iter().map(|m| m.y).min().unwrap();
            let x1 = mons
                .iter()
                .map(|m| m.x + m.logical_width as i32)
                .max()
                .unwrap();
            let y1 = mons
                .iter()
                .map(|m| m.y + m.logical_height as i32)
                .max()
                .unwrap();
            let scale = mons.iter().map(|m| m.scale).fold(1.0, f64::max);
            mons.push(MonitorInfo {
                id: "*".into(),
                name: "All displays".into(),
                x: x0,
                y: y0,
                logical_width: (x1 - x0) as u32,
                logical_height: (y1 - y0) as u32,
                width: ((x1 - x0) as f64 * scale) as u32,
                height: ((y1 - y0) as f64 * scale) as u32,
                refresh_mhz: mons.iter().map(|m| m.refresh_mhz).max().unwrap_or(60_000),
                scale,
            });
        }
        mons
    }

    fn start_capture(&self, monitor: &MonitorInfo) -> Result<Capture> {
        self.capture(monitor, monitor.id == "*")
    }

    /// Odd attempts capture the monitor's rectangle as a region instead of the output itself.
    /// KWin renders a region stream separately from the output, so a broken output stream (black
    /// buffers after a mode change or a GPU reset) does not take the region stream with it.
    fn restart_capture(&self, monitor: &MonitorInfo, attempt: u32) -> Result<Capture> {
        let region = monitor.id == "*" || (attempt % 2 == 1 && self.screencast.version() >= 3);
        self.capture(monitor, region)
    }

    fn input(&self) -> Option<Arc<dyn InputSink>> {
        self.fake_input.clone().map(|i| i as Arc<dyn InputSink>)
    }

    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        self.clipboard.clone().map(|c| c as Arc<dyn Clipboard>)
    }
}

impl KdeBackend {
    fn capture(&self, monitor: &MonitorInfo, region: bool) -> Result<Capture> {
        let (tx, rx) = mpsc::channel();
        let data = StreamData {
            tx: Mutex::new(Some(tx)),
            store: Mutex::new(None),
        };
        let pointer = sc::Pointer::Metadata as u32;
        let stream = if region {
            self.screencast.stream_region(
                monitor.x,
                monitor.y,
                monitor.logical_width,
                monitor.logical_height,
                monitor.scale,
                pointer,
                &self.qh,
                data,
            )
        } else {
            let out = self
                .output_proxy(&monitor.id)
                .ok_or_else(|| anyhow!("output {} went away", monitor.id))?;
            self.screencast.stream_output(&out, pointer, &self.qh, data)
        };
        self.conn.flush()?;
        let node = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(StreamEvent::Created(node)) => node,
            Ok(StreamEvent::Failed(e)) => bail!("KWin refused the screencast: {e}"),
            Ok(StreamEvent::Closed) => bail!("KWin closed the screencast"),
            Err(_) => bail!("KWin did not answer the screencast request"),
        };
        tracing::info!(monitor = %monitor.id, node, region, "KWin screencast started");
        let store = FrameStore::new();
        if let Some(d) = stream.data::<StreamData>() {
            *d.store.lock().unwrap() = Some(store.clone());
        }
        let pw = super::pw::PwCapture::start(
            node,
            store.clone(),
            monitor.width,
            monitor.height,
            monitor.refresh_mhz,
        )?;
        let guard = KdeStreamGuard {
            stream,
            conn: self.conn.clone(),
            _pw: pw,
            _rx: Mutex::new(rx),
        };
        Ok(Capture {
            store,
            monitor: monitor.clone(),
            _guard: Box::new(guard),
        })
    }
}

struct KdeStreamGuard {
    stream: ZkdeScreencastStreamUnstableV1,
    conn: Connection,
    _pw: super::pw::PwCapture,
    _rx: Mutex<mpsc::Receiver<StreamEvent>>,
}

impl Drop for KdeStreamGuard {
    fn drop(&mut self) {
        // Stop reading from PipeWire first (field drop order would do it after), then close.
        self.stream.close();
        let _ = self.conn.flush();
    }
}

pub struct KdeInput {
    conn: Connection,
    fi: OrgKdeKwinFakeInput,
}

impl InputSink for KdeInput {
    fn pointer_abs(&self, x: f64, y: f64) {
        self.fi.pointer_motion_absolute(x, y);
    }
    fn button(&self, button: u32, down: bool) {
        self.fi.button(button, down as u32);
    }
    fn scroll(&self, dx: f64, dy: f64) {
        // KWin forwards these as wl_pointer.axis values, where one wheel notch is 15.
        if dy != 0.0 {
            self.fi.axis(0, dy * 15.0);
        }
        if dx != 0.0 {
            self.fi.axis(1, dx * 15.0);
        }
    }
    fn key(&self, code: u32, down: bool) {
        if self.fi.version() >= 4 {
            self.fi.keyboard_key(code, down as u32);
        }
    }
    fn touch(&self, kind: TouchKind, id: u32, x: f64, y: f64) {
        if self.fi.version() < 2 {
            return;
        }
        match kind {
            TouchKind::Down => self.fi.touch_down(id, x, y),
            TouchKind::Move => self.fi.touch_motion(id, x, y),
            TouchKind::Up => self.fi.touch_up(id),
            TouchKind::Cancel => self.fi.touch_cancel(),
        }
        self.fi.touch_frame();
    }
    fn flush(&self) {
        let _ = self.conn.flush();
    }
}

pub struct KdeClipboard {
    conn: Connection,
    qh: QueueHandle<State>,
    mgr: zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    device: zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    clip: Arc<Mutex<ClipState>>,
}

const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

impl Clipboard for KdeClipboard {
    fn set_text(&self, text: String) {
        let text: Arc<str> = text.into();
        let source = self.mgr.create_data_source(&self.qh, ());
        for m in TEXT_MIMES {
            source.offer(m.into());
        }
        {
            let mut c = self.clip.lock().unwrap();
            if let Some(old) = c.source.take() {
                old.destroy();
            }
            c.offering = Some(text.clone());
            c.source = Some(source.clone());
            c.watch_tx.send_replace(Some(text));
        }
        // Input goes through this same connection, which KWin reads in order: a Wayland app that
        // pastes after this gets the new selection (an Xwayland app, once KWin copied it over).
        self.device.set_selection(Some(&source));
        let _ = self.conn.flush();
    }

    fn watch(&self) -> tokio::sync::watch::Receiver<Option<Arc<str>>> {
        self.clip.lock().unwrap().watch_tx.subscribe()
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// --- Wayland event handling -------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Hot-plugged outputs show up on the next restart; KWin keeps existing globals stable.
    }
}

impl Dispatch<wl_output::WlOutput, OutputData> for State {
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut outs = st.shared.outputs.lock().unwrap();
        let Some(o) = outs.iter_mut().find(|o| o.state.global == data.global) else {
            return;
        };
        let s = &mut o.state;
        match ev {
            wl_output::Event::Geometry { make, model, .. } => {
                s.make_model = format!("{make} {model}")
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if let WEnum::Value(f) = flags
                    && f.contains(wl_output::Mode::Current)
                {
                    s.px_w = width as u32;
                    s.px_h = height as u32;
                    s.refresh_mhz = refresh as u32;
                }
            }
            wl_output::Event::Scale { factor } => s.scale = factor,
            wl_output::Event::Name { name } => s.name = name,
            wl_output::Event::Description { description } => s.description = description,
            wl_output::Event::Done => s.done = true,
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, OutputData> for State {
    fn event(
        st: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        ev: zxdg_output_v1::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut outs = st.shared.outputs.lock().unwrap();
        let Some(o) = outs.iter_mut().find(|o| o.state.global == data.global) else {
            return;
        };
        let l = o.state.logical.get_or_insert((0, 0, 0, 0));
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                l.0 = x;
                l.1 = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                l.2 = width;
                l.3 = height;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: sc::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, StreamData> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        ev: sc_stream::Event,
        data: &StreamData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let msg = match ev {
            sc_stream::Event::Created { node } => StreamEvent::Created(node),
            sc_stream::Event::Failed { error } => StreamEvent::Failed(error),
            sc_stream::Event::Closed => StreamEvent::Closed,
        };
        if let Some(store) = data.store.lock().unwrap().as_ref() {
            match &msg {
                StreamEvent::Failed(e) => store.set_error(format!("KWin screencast failed: {e}")),
                StreamEvent::Closed => store.set_error("KWin closed the screencast".into()),
                StreamEvent::Created(_) => {}
            }
        }
        if let Some(tx) = data.tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }
}

impl Dispatch<OrgKdeKwinFakeInput, ()> for State {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinFakeInput,
        _: protocols::fake_input::org_kde_kwin_fake_input::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        ev: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        let Event::Selection { id: Some(offer) } = ev else {
            if let Event::Selection { id: None } = ev {
                st.clip.lock().unwrap().offering = None;
            }
            return;
        };
        let mimes = offer
            .data::<OfferData>()
            .map(|d| d.mimes.lock().unwrap().clone())
            .unwrap_or_default();
        let Some(mime) = TEXT_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m)) else {
            offer.destroy();
            return;
        };
        let Ok((r, w)) = pipe() else { return };
        offer.receive(mime.to_string(), w.as_fd());
        drop(w);
        let _ = conn.flush();
        offer.destroy();
        let clip = st.clip.clone();
        std::thread::spawn(move || {
            let mut f = std::fs::File::from(r);
            let mut buf = Vec::new();
            let _ = Read::by_ref(&mut f)
                .take(super::CLIPBOARD_MAX as u64)
                .read_to_end(&mut buf);
            if let Ok(text) = String::from_utf8(buf) {
                let c = clip.lock().unwrap();
                let same = c.watch_tx.borrow().as_deref() == Some(text.as_str());
                if !same && !text.is_empty() {
                    c.watch_tx.send_replace(Some(text.into()));
                }
            }
        });
    }

    event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, OfferData::default()),
    ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, OfferData> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        ev: zwlr_data_control_offer_v1::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = ev {
            data.mimes.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
    fn event(
        st: &mut Self,
        src: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        ev: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                let text = st.clip.lock().unwrap().offering.clone();
                if let Some(text) = text {
                    std::thread::spawn(move || {
                        let mut f = std::fs::File::from(fd);
                        let _ = f.write_all(text.as_bytes());
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                let mut c = st.clip.lock().unwrap();
                if c.source.as_ref().is_some_and(|s| s == src) {
                    c.source = None;
                    c.offering = None;
                }
                src.destroy();
            }
            _ => {}
        }
    }
}
