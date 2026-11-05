//! X11: MIT-SHM capture driven by XDamage, cursor shape from XFixes, input through XTEST, the
//! clipboard through a window of our own (`x11clip`).
//!
//! Only damaged regions are read back. XDamage reports each changed rectangle in its event, the
//! capture thread sleeps until such events arrive (pointer motion comes as XInput 2 events, so an
//! idle screen costs nothing), coalesces them up to 240 Hz and pulls just the box around what
//! changed over shared memory.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::{Event, damage, shm, xfixes, xinput, xtest};
use x11rb::rust_connection::RustConnection;

use super::x11clip::X11Clipboard;
use super::{Backend, Capture, Clipboard, InputSink, MonitorInfo, TouchKind};
use crate::clock;
use crate::frame::{CursorPos, CursorShape, FrameStore, PixelFormat, Rect};

pub struct X11Backend {
    conn: Arc<RustConnection>,
    root: xproto::Window,
    input: Arc<X11Input>,
    clipboard: Option<Arc<X11Clipboard>>,
    /// The X server is Xwayland inside a Wayland session: its root window is not the desktop.
    pub xwayland: bool,
}

impl X11Backend {
    pub fn connect() -> Result<X11Backend> {
        let (conn, screen) = x11rb::connect(None).context("connecting to the X server")?;
        let root = conn.setup().roots[screen].root;
        for (name, what) in [
            (shm::X11_EXTENSION_NAME, "MIT-SHM"),
            (damage::X11_EXTENSION_NAME, "DAMAGE"),
            (xfixes::X11_EXTENSION_NAME, "XFIXES"),
            (xtest::X11_EXTENSION_NAME, "XTEST"),
        ] {
            if conn.extension_information(name)?.is_none() {
                bail!("the X server lacks the {what} extension");
            }
        }
        let xwayland = conn.extension_information("XWAYLAND")?.is_some();
        let conn = Arc::new(conn);
        let input = Arc::new(X11Input {
            conn: conn.clone(),
            root,
            scroll: Mutex::new((0.0, 0.0)),
        });
        let clipboard = X11Clipboard::open(None)
            .inspect_err(|e| tracing::warn!("no X11 clipboard: {e:#}"))
            .ok()
            .map(Arc::new);
        Ok(X11Backend {
            conn,
            root,
            input,
            clipboard,
            xwayland,
        })
    }
}

impl Backend for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        let mut out = Vec::new();
        if let Ok(reply) = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.reply().map_err(Into::into))
        {
            for m in reply.monitors {
                let name = self
                    .conn
                    .get_atom_name(m.name)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                    .unwrap_or_else(|| format!("monitor-{}", out.len()));
                out.push(MonitorInfo {
                    id: name.clone(),
                    name,
                    x: m.x as i32,
                    y: m.y as i32,
                    logical_width: m.width as u32,
                    logical_height: m.height as u32,
                    width: m.width as u32,
                    height: m.height as u32,
                    refresh_mhz: 60_000,
                    scale: 1.0,
                });
            }
        }
        if out.is_empty() {
            let s = &self.conn.setup().roots[0];
            out.push(MonitorInfo {
                id: "screen".into(),
                name: "X screen".into(),
                x: 0,
                y: 0,
                logical_width: s.width_in_pixels as u32,
                logical_height: s.height_in_pixels as u32,
                width: s.width_in_pixels as u32,
                height: s.height_in_pixels as u32,
                refresh_mhz: 60_000,
                scale: 1.0,
            });
        }
        out
    }

    fn start_capture(&self, monitor: &MonitorInfo) -> Result<Capture> {
        let store = FrameStore::new();
        let stop = Waker::new()?;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (st, flag, mon) = (store.clone(), stop.clone(), monitor.clone());
        std::thread::Builder::new()
            .name("x11-capture".into())
            .spawn(move || {
                if let Err(e) = capture_loop(&mon, &st, &flag, &ready_tx) {
                    let msg = format!("{e:#}");
                    let _ = ready_tx.send(Err(msg.clone()));
                    st.set_error(msg);
                }
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Capture {
                store,
                monitor: monitor.clone(),
                _guard: Box::new(StopOnDrop(stop)),
            }),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("X11 capture did not start")),
        }
    }

    fn input(&self) -> Option<Arc<dyn InputSink>> {
        Some(self.input.clone())
    }

    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        self.clipboard.clone().map(|c| c as Arc<dyn Clipboard>)
    }
}

/// Wakes the capture thread out of `poll`: to stop, or because a viewer wants a frame now.
struct Waker {
    stop: AtomicBool,
    requested: AtomicBool,
    fd: i32,
}

impl Waker {
    fn new() -> Result<Arc<Waker>> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            bail!("eventfd: {}", std::io::Error::last_os_error());
        }
        Ok(Arc::new(Waker {
            stop: AtomicBool::new(false),
            requested: AtomicBool::new(false),
            fd,
        }))
    }

    fn wake(&self) {
        let one = 1u64;
        unsafe { libc::write(self.fd, &one as *const u64 as *const _, 8) };
    }

    fn drain(&self) {
        let mut v = 0u64;
        unsafe { libc::read(self.fd, &mut v as *mut u64 as *mut _, 8) };
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

struct StopOnDrop(Arc<Waker>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Relaxed);
        self.0.wake();
    }
}

/// Where grabbed pixels land.
enum Grab {
    /// A segment the X server writes into directly.
    Shm { seg: ShmSeg, shmseg: u32 },
    /// A buffer filled from GetImage replies.
    Plain(Vec<u8>),
}

/// A System V shared memory segment attached both here and in the X server.
struct ShmSeg {
    addr: *mut u8,
    len: usize,
}

impl ShmSeg {
    fn new(len: usize) -> Result<(ShmSeg, i32)> {
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if id < 0 {
                bail!("shmget: {}", std::io::Error::last_os_error());
            }
            let addr = libc::shmat(id, std::ptr::null(), 0);
            if addr as isize == -1 {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                bail!("shmat: {}", std::io::Error::last_os_error());
            }
            Ok((
                ShmSeg {
                    addr: addr as *mut u8,
                    len,
                },
                id,
            ))
        }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.addr, self.len) }
    }
}

impl Drop for ShmSeg {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.addr as *const _);
        }
    }
}

/// Without a viewer pulling frames (`skylight bench`), changes are grabbed at up to this rate.
const PUSH_INTERVAL: Duration = Duration::from_micros(1_000_000 / 240);

fn capture_loop(
    mon: &MonitorInfo,
    store: &FrameStore,
    waker: &Arc<Waker>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let depth = conn.setup().roots[screen].root_depth;
    if depth != 24 && depth != 32 {
        bail!("unsupported X root depth {depth}");
    }
    conn.damage_query_version(1, 1)?.reply()?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    let (w, h) = (mon.width, mon.height);
    let stride = w as usize * 4;
    // Shared memory when the X server can attach our segment; plain GetImage over the socket
    // when it cannot (another user or IPC namespace, a remote server), which is slower but works.
    let mut grab = match ShmSeg::new(stride * h as usize) {
        Ok((seg, shmid)) => {
            let shmseg = conn.generate_id()?;
            let ok = conn
                .shm_attach(shmseg, shmid as u32, false)
                .ok()
                .and_then(|c| c.check().ok())
                .is_some();
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            if ok {
                Grab::Shm { seg, shmseg }
            } else {
                tracing::warn!(
                    "the X server cannot attach shared memory; capturing through GetImage"
                );
                Grab::Plain(vec![0; stride * h as usize])
            }
        }
        Err(e) => {
            tracing::warn!("no shared memory ({e:#}); capturing through GetImage");
            Grab::Plain(vec![0; stride * h as usize])
        }
    };

    // One event when the screen goes from unchanged to changed; the region is fetched (and
    // reset) at grab time. Per-rectangle events would cost the X server an event for every
    // drawing request, which slows down the very apps being watched.
    let dmg = conn.generate_id()?;
    conn.damage_create(dmg, root, damage::ReportLevel::NON_EMPTY)?;
    let region = conn.generate_id()?;
    conn.xfixes_create_region(region, &[])?;
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    // Pointer motion as events (XInput 2 raw motion, which XTEST input produces too), so the
    // pointer position needs no polling. Without XInput 2, fall back to polling it.
    let xi2 = conn
        .extension_information(xinput::X11_EXTENSION_NAME)?
        .is_some()
        && conn
            .xinput_xi_query_version(2, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some()
        && conn
            .xinput_xi_select_events(
                root,
                &[xinput::EventMask {
                    deviceid: 1, // all master devices
                    mask: vec![xinput::XIEventMask::RAW_MOTION],
                }],
            )
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
    conn.flush()?;
    // Viewers ask for a frame when they are ready to encode one: the grab then happens right
    // before the encode, so the frame is as fresh as it can be and nothing is grabbed for nobody.
    {
        let w = waker.clone();
        store.set_requester(Box::new(move || {
            w.requested.store(true, Ordering::Relaxed);
            w.wake();
        }));
    }
    let _ = ready.send(Ok(()));

    let mon_rect = (mon.x, mon.y, w as i32, h as i32);
    let mut last_grab = Instant::now() - PUSH_INTERVAL;
    let mut first = true;
    let mut dirty = false;
    let mut cursor_dirty = true;
    let mut pointer_dirty = true;
    let mut last_pointer = (i32::MIN, i32::MIN);
    let mut cursor_serial = 0u32;
    let fd = conn.stream().as_raw_fd();
    let mut early: Option<Event> = None;

    while !waker.stopped() {
        while let Some(ev) = early
            .take()
            .map_or_else(|| conn.poll_for_event(), |e| Ok(Some(e)))?
        {
            match ev {
                Event::DamageNotify(_) => {
                    dirty = true;
                    store.mark_pending();
                }
                Event::XfixesCursorNotify(_) => cursor_dirty = true,
                Event::XinputRawMotion(_) => pointer_dirty = true,
                _ => {}
            }
        }
        let pulled = store.pulled();
        let requested = waker.requested.swap(false, Ordering::Relaxed);
        let due = first
            || dirty
                && if pulled {
                    requested
                } else {
                    last_grab.elapsed() >= PUSH_INTERVAL
                };
        if due {
            last_grab = Instant::now();
            dirty = false;
            conn.damage_subtract(dmg, x11rb::NONE, region)?;
            let mut damage = Vec::new();
            for r in conn.xfixes_fetch_region(region)?.reply()?.rectangles {
                let x0 = (r.x as i32).max(mon_rect.0);
                let y0 = (r.y as i32).max(mon_rect.1);
                let x1 = (r.x as i32 + r.width as i32).min(mon_rect.0 + mon_rect.2);
                let y1 = (r.y as i32 + r.height as i32).min(mon_rect.1 + mon_rect.3);
                if x1 > x0 && y1 > y0 {
                    damage.push(Rect::new(
                        (x0 - mon_rect.0) as u32,
                        (y0 - mon_rect.1) as u32,
                        (x1 - x0) as u32,
                        (y1 - y0) as u32,
                    ));
                }
            }
            if first || !damage.is_empty() {
                // Read back only the box around what changed. The X server packs its rows
                // (`area.w` pixels apart) at the start of the segment.
                let area = if first {
                    Rect::new(0, 0, w, h)
                } else {
                    bounds(&damage)
                };
                let t = clock::now_us();
                let (x, y) = (
                    (mon.x + area.x as i32) as i16,
                    (mon.y + area.y as i32) as i16,
                );
                let pixels: &[u8] = match &mut grab {
                    Grab::Shm { seg, shmseg } => {
                        conn.shm_get_image(
                            root,
                            x,
                            y,
                            area.w as u16,
                            area.h as u16,
                            !0,
                            xproto::ImageFormat::Z_PIXMAP.into(),
                            *shmseg,
                            0,
                        )?
                        .reply()?;
                        seg.bytes()
                    }
                    Grab::Plain(buf) => {
                        let img = conn
                            .get_image(
                                xproto::ImageFormat::Z_PIXMAP,
                                root,
                                x,
                                y,
                                area.w as u16,
                                area.h as u16,
                                !0,
                            )?
                            .reply()?;
                        let n = img.data.len().min(buf.len());
                        buf[..n].copy_from_slice(&img.data[..n]);
                        buf
                    }
                };
                store.update_region(
                    w,
                    h,
                    PixelFormat::Bgrx,
                    pixels,
                    area.w as usize * 4,
                    area,
                    if first { None } else { Some(&damage) },
                    t,
                    0,
                );
                first = false;
            } else {
                // Damage elsewhere on the X screen (another monitor): nothing for this one.
                store.clear_pending();
            }
        }
        if cursor_dirty {
            cursor_dirty = false;
            pointer_dirty = true;
            if let Ok(img) = conn.xfixes_get_cursor_image()?.reply() {
                cursor_serial += 1;
                let mut rgba = Vec::with_capacity(img.cursor_image.len() * 4);
                for p in &img.cursor_image {
                    let a = (p >> 24) & 0xff;
                    let un = |c: u32| {
                        (c * 255 + a / 2)
                            .checked_div(a)
                            .map_or(0, |v| v.min(255) as u8)
                    };
                    rgba.extend_from_slice(&[
                        un((p >> 16) & 0xff),
                        un((p >> 8) & 0xff),
                        un(p & 0xff),
                        a as u8,
                    ]);
                }
                let pos = CursorPos {
                    x: img.x as i32 - mon.x,
                    y: img.y as i32 - mon.y,
                    visible: true,
                };
                last_pointer = (pos.x, pos.y);
                store.set_cursor(
                    pos,
                    Some(CursorShape {
                        serial: cursor_serial,
                        width: img.width as u32,
                        height: img.height as u32,
                        hot_x: img.xhot as u32,
                        hot_y: img.yhot as u32,
                        rgba,
                    }),
                );
            }
        }
        // Pointer position for clients that draw the cursor themselves (touch devices).
        if pointer_dirty || !xi2 {
            pointer_dirty = false;
            if let Ok(p) = conn.query_pointer(root)?.reply() {
                let pos = (p.root_x as i32 - mon.x, p.root_y as i32 - mon.y);
                if pos != last_pointer {
                    last_pointer = pos;
                    store.set_cursor(
                        CursorPos {
                            x: pos.0,
                            y: pos.1,
                            visible: true,
                        },
                        None,
                    );
                }
            }
        }
        conn.flush()?;
        // Waiting for replies above may have queued events inside the connection, where poll()
        // cannot see them; handle those before sleeping.
        if let Some(ev) = conn.poll_for_event()? {
            early = Some(ev);
            continue;
        }
        // Sleep until the X server has something for us, a viewer asks for a frame, or the
        // capture is stopped. Without a viewer pulling, wake when the next grab is allowed;
        // without XInput 2, poll the pointer at 60 Hz.
        let timeout = if dirty && !pulled {
            Some(PUSH_INTERVAL.saturating_sub(last_grab.elapsed()))
        } else if xi2 {
            None
        } else {
            Some(Duration::from_millis(16))
        };
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        let mut pfd = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: waker.fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe {
            libc::ppoll(
                pfd.as_mut_ptr(),
                2,
                ts.as_ref().map_or(std::ptr::null(), |t| t as *const _),
                std::ptr::null(),
            )
        };
        if pfd[1].revents != 0 {
            waker.drain();
        }
    }
    store.clear_requester();
    if let Grab::Shm { shmseg, .. } = &grab {
        let _ = conn.shm_detach(*shmseg);
    }
    let _ = conn.flush();
    Ok(())
}

fn bounds(rects: &[Rect]) -> Rect {
    let x0 = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = rects.iter().map(|r| r.x + r.w).max().unwrap_or(0);
    let y1 = rects.iter().map(|r| r.y + r.h).max().unwrap_or(0);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

pub struct X11Input {
    conn: Arc<RustConnection>,
    root: xproto::Window,
    scroll: Mutex<(f64, f64)>,
}

impl X11Input {
    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.root, x, y, 0);
    }

    fn click(&self, button: u8) {
        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0);
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0);
    }
}

impl InputSink for X11Input {
    fn pointer_abs(&self, x: f64, y: f64) {
        self.fake(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            x.round() as i16,
            y.round() as i16,
        );
    }
    fn pointer_rel(&self, dx: f64, dy: f64) {
        self.fake(
            xproto::MOTION_NOTIFY_EVENT,
            1,
            dx.round() as i16,
            dy.round() as i16,
        );
    }
    fn button(&self, button: u32, down: bool) {
        let b = match button {
            super::BTN_LEFT => 1,
            super::BTN_MIDDLE => 2,
            super::BTN_RIGHT => 3,
            super::BTN_SIDE => 8,
            super::BTN_EXTRA => 9,
            _ => return,
        };
        let ty = if down {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(ty, b, 0, 0);
    }
    fn scroll(&self, dx: f64, dy: f64) {
        // X11 only knows wheel clicks (buttons 4-7); carry the remainder to the next event.
        let mut acc = self.scroll.lock().unwrap();
        acc.0 += dx;
        acc.1 += dy;
        while acc.1 >= 1.0 {
            self.click(5);
            acc.1 -= 1.0;
        }
        while acc.1 <= -1.0 {
            self.click(4);
            acc.1 += 1.0;
        }
        while acc.0 >= 1.0 {
            self.click(7);
            acc.0 -= 1.0;
        }
        while acc.0 <= -1.0 {
            self.click(6);
            acc.0 += 1.0;
        }
    }
    fn key(&self, code: u32, down: bool) {
        let ty = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        if code + 8 <= 255 {
            self.fake(ty, (code + 8) as u8, 0, 0);
        }
    }
    fn touch(&self, kind: TouchKind, _id: u32, x: f64, y: f64) {
        // Plain X11 has no touch injection through XTEST; emulate a left-button drag.
        match kind {
            TouchKind::Down => {
                self.pointer_abs(x, y);
                self.button(super::BTN_LEFT, true);
            }
            TouchKind::Move => self.pointer_abs(x, y),
            TouchKind::Up | TouchKind::Cancel => self.button(super::BTN_LEFT, false),
        }
    }
    fn flush(&self) {
        let _ = self.conn.flush();
    }
}
