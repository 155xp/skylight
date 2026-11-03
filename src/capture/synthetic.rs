//! A moving test pattern: a scrolling grid of "text" stripes and a bouncing square. Used by the
//! tests and `skylight bench` so the encode and transport path can be measured without a display.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{Backend, Capture, Clipboard, InputSink, MonitorInfo};
use crate::clock;
use crate::frame::{CursorPos, FrameStore, PixelFormat, Rect};

pub struct SyntheticBackend {
    width: u32,
    height: u32,
    fps: u32,
}

impl SyntheticBackend {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        SyntheticBackend {
            width,
            height,
            fps: fps.max(1),
        }
    }
}

struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Paint frame `n` of the pattern into `buf` (BGRx, stride `w * 4`); returns the damaged rects.
pub fn paint(buf: &mut [u8], w: u32, h: u32, n: u64) -> Vec<Rect> {
    let stride = w as usize * 4;
    let first = n == 0;
    if first {
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * stride + x * 4;
                // Light background with dark one-pixel "glyph" strokes every few pixels, the kind
                // of high-frequency detail that shows encoder blur on text.
                let ink =
                    (y % 18 < 12) && (x % 7 == 0 || (x / 3 + y / 2) % 11 == 0) && (y % 18 > 2);
                let v = if ink { 30 } else { 245 };
                buf[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
    }
    // A 160x160 square moving diagonally, the "window being dragged".
    let size = 160u32.min(w / 2).min(h / 2);
    let span_x = (w - size) as u64;
    let span_y = (h - size) as u64;
    let pos = |k: u64| {
        let px = (k * 7) % (2 * span_x);
        let py = (k * 5) % (2 * span_y);
        let x = if px > span_x { 2 * span_x - px } else { px };
        let y = if py > span_y { 2 * span_y - py } else { py };
        (x as u32, y as u32)
    };
    let (x, y) = pos(n);
    let mut damage = Vec::new();
    if !first {
        let (ox, oy) = pos(n - 1);
        // Restore the background where the square was.
        for yy in oy..oy + size {
            for xx in ox..ox + size {
                let i = yy as usize * stride + xx as usize * 4;
                let ink =
                    (yy % 18 < 12) && (xx % 7 == 0 || (xx / 3 + yy / 2) % 11 == 0) && (yy % 18 > 2);
                let v = if ink { 30 } else { 245 };
                buf[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        damage.push(Rect::new(ox, oy, size, size));
    }
    let hue = (n % 255) as u8;
    for yy in y..y + size {
        for xx in x..x + size {
            let i = yy as usize * stride + xx as usize * 4;
            buf[i..i + 4].copy_from_slice(&[hue, 90, 255 - hue, 255]);
        }
    }
    damage.push(Rect::new(x, y, size, size));
    if first {
        return vec![Rect::new(0, 0, w, h)];
    }
    damage
}

impl Backend for SyntheticBackend {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        vec![MonitorInfo {
            id: "synthetic".into(),
            name: "Test pattern".into(),
            x: 0,
            y: 0,
            logical_width: self.width,
            logical_height: self.height,
            width: self.width,
            height: self.height,
            refresh_mhz: self.fps * 1000,
            scale: 1.0,
        }]
    }

    fn start_capture(&self, monitor: &MonitorInfo) -> Result<Capture> {
        let store = FrameStore::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (w, h, fps) = (self.width, self.height, self.fps);
        let st = store.clone();
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("synthetic".into())
            .spawn(move || {
                let mut buf = vec![0u8; (w * h * 4) as usize];
                let period = Duration::from_secs_f64(1.0 / fps as f64);
                let start = Instant::now();
                let mut n = 0u64;
                while !flag.load(Ordering::Relaxed) {
                    let damage = paint(&mut buf, w, h, n);
                    st.update(
                        w,
                        h,
                        PixelFormat::Bgrx,
                        &buf,
                        w as usize * 4,
                        Some(&damage),
                        clock::now_us(),
                        0,
                    );
                    st.set_cursor(
                        CursorPos {
                            x: (n % w as u64) as i32,
                            y: (h / 2) as i32,
                            visible: true,
                        },
                        None,
                    );
                    n += 1;
                    let next = start + period * n as u32;
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(d);
                    }
                }
            })?;
        Ok(Capture {
            store,
            monitor: monitor.clone(),
            _guard: Box::new(Stop(stop)),
        })
    }

    fn input(&self) -> Option<Arc<dyn InputSink>> {
        None
    }

    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        None
    }
}
