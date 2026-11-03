//! X11: MIT-SHM capture driven by XDamage, cursor shape from XFixes, input through XTEST.
//!
//! Only damaged regions are read back. XDamage reports the rectangles that changed, the capture
//! thread waits for those events (no polling while the screen is idle), coalesces them up to the
//! frame-rate cap and pulls the bounding box over shared memory.

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
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::{Event, damage, shm, xfixes, xtest};
use x11rb::rust_connection::RustConnection;

use super::{Backend, Capture, Clipboard, InputSink, MonitorInfo, TouchKind};
use crate::clock;
use crate::frame::{CursorPos, CursorShape, FrameStore, PixelFormat, Rect};

pub struct X11Backend {
    conn: Arc<RustConnection>,
    root: xproto::Window,
    input: Arc<X11Input>,
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
        let conn = Arc::new(conn);
        let input = Arc::new(X11Input {
            conn: conn.clone(),
            root,
            scroll: Mutex::new((0.0, 0.0)),
        });
        Ok(X11Backend { conn, root, input })
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
        let stop = Arc::new(AtomicBool::new(false));
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
        None
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
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

fn capture_loop(
    mon: &MonitorInfo,
    store: &FrameStore,
    stop: &AtomicBool,
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
    let (seg, shmid) = ShmSeg::new(w as usize * h as usize * 4)?;
    let shmseg = conn.generate_id()?;
    conn.shm_attach(shmseg, shmid as u32, false)?.check()?;
    unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };

    let dmg = conn.generate_id()?;
    conn.damage_create(dmg, root, damage::ReportLevel::NON_EMPTY)?;
    let region = conn.generate_id()?;
    conn.xfixes_create_region(region, &[])?;
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    conn.flush()?;
    let _ = ready.send(Ok(()));

    let mon_rect = (mon.x, mon.y, w as i32, h as i32);
    let min_interval = Duration::from_micros(1_000_000 / 240);
    let mut last_grab = Instant::now() - min_interval;
    let mut first = true;
    let mut dirty = true;
    let mut cursor_dirty = true;
    let mut last_pointer = (i32::MIN, i32::MIN);
    let mut cursor_serial = 0u32;
    let fd = conn.stream().as_raw_fd();

    while !stop.load(Ordering::Relaxed) {
        while let Some(ev) = conn.poll_for_event()? {
            match ev {
                Event::DamageNotify(_) => dirty = true,
                Event::XfixesCursorNotify(_) => cursor_dirty = true,
                _ => {}
            }
        }
        if dirty && last_grab.elapsed() >= min_interval {
            dirty = false;
            last_grab = Instant::now();
            conn.damage_subtract(dmg, x11rb::NONE, region)?;
            let rects = conn.xfixes_fetch_region(region)?.reply()?.rectangles;
            let mut damage = Vec::new();
            for r in rects {
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
                let t = clock::now_us();
                conn.shm_get_image(
                    root,
                    mon.x as i16,
                    mon.y as i16,
                    w as u16,
                    h as u16,
                    !0,
                    xproto::ImageFormat::Z_PIXMAP.into(),
                    shmseg,
                    0,
                )?
                .reply()?;
                store.update(
                    w,
                    h,
                    PixelFormat::Bgrx,
                    seg.bytes(),
                    w as usize * 4,
                    if first { None } else { Some(&damage) },
                    t,
                    0,
                );
                first = false;
            }
        }
        if cursor_dirty {
            cursor_dirty = false;
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
        conn.flush()?;
        // Sleep until the X server has something for us, or briefly when a grab is pending.
        let timeout_ms = if dirty { 1 } else { 16 };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    }
    let _ = conn.shm_detach(shmseg);
    let _ = conn.flush();
    Ok(())
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
