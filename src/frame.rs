//! The shared frame store between a capture backend and the encoders that read from it.
//!
//! A capture thread writes only the damaged rectangles of each new frame into one persistent
//! buffer. Every encoder keeps its own copy and pulls just the rectangles that changed since the
//! sequence number it last saw, so an idle desktop costs nothing and a blinking cursor costs a
//! few kilobytes of memcpy instead of a full frame.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

/// Byte order of a 4-byte pixel as laid out in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// B, G, R, X (the usual little-endian xRGB8888 desktop format).
    Bgrx,
    /// R, G, B, X.
    Rgbx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// Clamp to a `width` x `height` surface; `None` when nothing is left.
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }

    pub fn clamp(&self, width: u32, height: u32) -> Option<Rect> {
        if self.x >= width || self.y >= height {
            return None;
        }
        let w = self.w.min(width - self.x);
        let h = self.h.min(height - self.y);
        (w > 0 && h > 0).then_some(Rect::new(self.x, self.y, w, h))
    }
}

/// Copy `rect` between two 4-byte-per-pixel images.
pub fn copy_rect(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, r: Rect) {
    let row_bytes = r.w as usize * 4;
    let x_off = r.x as usize * 4;
    for row in r.y as usize..(r.y + r.h) as usize {
        let s = row * src_stride + x_off;
        let d = row * dst_stride + x_off;
        dst[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
    }
}

/// A cursor image, RGBA with straight alpha.
#[derive(Clone, Debug, Default)]
pub struct CursorShape {
    pub serial: u32,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CursorPos {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

/// Wake-up flags for a consumer thread.
pub const WAKE_FRAME: u32 = 1;
pub const WAKE_CURSOR: u32 = 2;
pub const WAKE_COMMAND: u32 = 4;
pub const WAKE_STOP: u32 = 8;

/// A tiny wake-up primitive: a set of flags plus a condition variable.
#[derive(Default)]
pub struct Signal {
    flags: Mutex<u32>,
    cv: Condvar,
}

impl Signal {
    pub fn new() -> Arc<Signal> {
        Arc::new(Signal::default())
    }

    pub fn raise(&self, bits: u32) {
        let mut f = self.flags.lock().unwrap();
        *f |= bits;
        self.cv.notify_all();
    }

    /// Wait until any flag is set or the timeout passes; returns and clears the flags.
    pub fn wait(&self, timeout: Duration) -> u32 {
        let mut f = self.flags.lock().unwrap();
        if *f == 0 {
            f = self.cv.wait_timeout(f, timeout).unwrap().0;
        }
        std::mem::take(&mut *f)
    }
}

const DAMAGE_LOG: usize = 512;

pub struct StoreState {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Tightly packed pixels, stride = width * 4.
    pub data: Vec<u8>,
    /// Bumped on every content update.
    pub seq: u64,
    /// Bumped when size or format changes; consumers then take a full copy.
    pub generation: u32,
    /// Recent damage: sequence number, rectangle, and capture time (server clock, µs).
    damage: VecDeque<(u64, Rect, u64)>,
    /// Oldest sequence number whose damage is still in the log.
    damage_floor: u64,
    /// Capture time of the newest content, in the server clock (µs).
    pub capture_us: u64,
    /// How long the compositor took to hand the frame over, when it reports a timestamp (µs).
    pub compositor_delay_us: u64,
    pub cursor: CursorPos,
    pub cursor_shape: Option<Arc<CursorShape>>,
    pub error: Option<String>,
    /// Frames received from the capture source, for stats.
    pub frames_in: u64,
    /// Server clock (µs) of the last sign of life from the source: a frame, or a buffer that only
    /// moved the cursor. The watchdog compares it with pointer input.
    pub activity_us: u64,
    /// Brightest and mean luma (0-255) of the current image, sampled on every update.
    pub luma_max: u8,
    pub luma_mean: u8,
}

/// One monitor's live image, shared by every session watching it.
pub struct FrameStore {
    state: Mutex<StoreState>,
    subscribers: Mutex<Vec<Weak<Signal>>>,
    /// Sources that grab on demand (X11): how to ask for a grab, whether changes are waiting to
    /// be grabbed, and how many consumers pull frames rather than wait for them.
    requester: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pending: AtomicBool,
    pullers: AtomicUsize,
}

/// Held by a consumer that asks for frames with [`FrameStore::request_frame`].
pub struct PullGuard(Arc<FrameStore>);

impl Drop for PullGuard {
    fn drop(&mut self) {
        self.0.pullers.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What a consumer got from [`FrameStore::sync_into`].
pub struct SyncResult {
    pub seq: u64,
    pub generation: u32,
    pub capture_us: u64,
    /// Rectangles that were copied; empty when nothing changed.
    pub damage: Vec<Rect>,
    pub full: bool,
    /// The store's image is not the size the caller asked for; nothing was copied.
    pub size_mismatch: bool,
}

impl FrameStore {
    pub fn new() -> Arc<FrameStore> {
        Arc::new(FrameStore {
            state: Mutex::new(StoreState {
                width: 0,
                height: 0,
                format: PixelFormat::Bgrx,
                data: Vec::new(),
                seq: 0,
                generation: 0,
                damage: VecDeque::new(),
                damage_floor: 0,
                capture_us: 0,
                compositor_delay_us: 0,
                cursor: CursorPos::default(),
                cursor_shape: None,
                error: None,
                frames_in: 0,
                activity_us: 0,
                luma_max: 0,
                luma_mean: 0,
            }),
            subscribers: Mutex::new(Vec::new()),
            requester: Mutex::new(None),
            pending: AtomicBool::new(false),
            pullers: AtomicUsize::new(0),
        })
    }

    /// Become a consumer that pulls: from now on an on-demand source grabs only when asked.
    pub fn pull(self: &Arc<Self>) -> PullGuard {
        self.pullers.fetch_add(1, Ordering::Relaxed);
        PullGuard(self.clone())
    }

    /// Whether any consumer pulls frames.
    pub fn pulled(&self) -> bool {
        self.pullers.load(Ordering::Relaxed) > 0
    }

    /// For on-demand sources: how a consumer asks for a grab.
    pub fn set_requester(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.requester.lock().unwrap() = Some(f);
    }

    pub fn clear_requester(&self) {
        *self.requester.lock().unwrap() = None;
    }

    /// The source has changes it has not grabbed yet; consumers are woken to ask for them.
    pub fn mark_pending(&self) {
        if !self.pending.swap(true, Ordering::Relaxed) {
            self.notify(WAKE_FRAME);
        }
    }

    pub fn clear_pending(&self) {
        self.pending.store(false, Ordering::Relaxed);
    }

    /// Changes are waiting at the source; `request_frame` brings them in.
    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    /// Ask an on-demand source to grab its pending changes now.
    pub fn request_frame(&self) {
        if let Some(f) = &*self.requester.lock().unwrap() {
            f();
        }
    }

    pub fn subscribe(&self, s: &Arc<Signal>) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|w| w.strong_count() > 0);
        subs.push(Arc::downgrade(s));
    }

    fn notify(&self, bits: u32) {
        let subs = self.subscribers.lock().unwrap();
        for s in subs.iter().filter_map(|w| w.upgrade()) {
            s.raise(bits);
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state.lock().unwrap()
    }

    /// Write a new frame. `damage` of `None` means "everything changed".
    /// `src` is `height` rows of `src_stride` bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        src: &[u8],
        src_stride: usize,
        damage: Option<&[Rect]>,
        capture_us: u64,
        compositor_delay_us: u64,
    ) {
        {
            let mut st = self.state.lock().unwrap();
            let resized = st.width != width || st.height != height || st.format != format;
            if resized {
                st.width = width;
                st.height = height;
                st.format = format;
                st.data = vec![0; width as usize * height as usize * 4];
                st.generation += 1;
                st.damage.clear();
            }
            let stride = width as usize * 4;
            let full = Rect::new(0, 0, width, height);
            let rects: Vec<Rect> = match damage {
                Some(d) if !resized => {
                    let mut v: Vec<Rect> =
                        d.iter().filter_map(|r| r.clamp(width, height)).collect();
                    let area: u64 = v.iter().map(Rect::area).sum();
                    if area * 2 > full.area() {
                        v = vec![full];
                    }
                    v
                }
                _ => vec![full],
            };
            if rects.is_empty() {
                return;
            }
            let StoreState { data, .. } = &mut *st;
            for r in &rects {
                copy_rect(src, src_stride, data, stride, *r);
            }
            st.seq += 1;
            let seq = st.seq;
            for r in rects {
                st.damage.push_back((seq, r, capture_us));
            }
            while st.damage.len() > DAMAGE_LOG {
                let (s, _, _) = st.damage.pop_front().unwrap();
                st.damage_floor = s;
            }
            st.capture_us = capture_us;
            st.compositor_delay_us = compositor_delay_us;
            st.frames_in += 1;
            st.activity_us = st.activity_us.max(capture_us);
            (st.luma_max, st.luma_mean) =
                crate::health::sample_luma(&st.data, width, height, format);
        }
        self.pending.store(false, Ordering::Relaxed);
        self.notify(WAKE_FRAME);
    }

    /// The source is alive even though nothing changed (for example a cursor-only buffer).
    pub fn touch(&self) {
        let mut st = self.state.lock().unwrap();
        st.activity_us = st.activity_us.max(crate::clock::now_us());
    }

    pub fn set_cursor(&self, pos: CursorPos, shape: Option<CursorShape>) {
        let changed = {
            let mut st = self.state.lock().unwrap();
            st.activity_us = st.activity_us.max(crate::clock::now_us());
            let mut changed = st.cursor != pos;
            st.cursor = pos;
            if let Some(shape) = shape {
                let same = st.cursor_shape.as_ref().is_some_and(|s| {
                    s.width == shape.width
                        && s.height == shape.height
                        && s.hot_x == shape.hot_x
                        && s.hot_y == shape.hot_y
                        && s.rgba == shape.rgba
                });
                if !same {
                    st.cursor_shape = Some(Arc::new(shape));
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.notify(WAKE_CURSOR);
        }
    }

    pub fn set_error(&self, err: String) {
        self.state.lock().unwrap().error = Some(err);
        self.notify(WAKE_FRAME);
    }

    /// Whether anything inside `region` (everything when `None`) changed after `seq`.
    pub fn changed_since(&self, seq: u64, region: Option<Rect>) -> bool {
        let st = self.state.lock().unwrap();
        if st.seq <= seq {
            return false;
        }
        if seq < st.damage_floor {
            return true;
        }
        st.damage
            .iter()
            .rev()
            .take_while(|(s, _, _)| *s > seq)
            .any(|(_, r, _)| region.is_none_or(|g| g.intersects(r)))
    }

    /// When `region` last changed (server clock, µs). If the damage log no longer reaches back
    /// that far, the time of its oldest entry, which is later than the real answer.
    pub fn last_change(&self, region: Option<Rect>) -> u64 {
        let st = self.state.lock().unwrap();
        st.damage
            .iter()
            .rev()
            .find(|(_, r, _)| region.is_none_or(|g| g.intersects(r)))
            .or(st.damage.front())
            .map_or(st.capture_us, |(_, _, t)| *t)
    }

    /// A copy of the current image: width, height, format, pixels (stride = width * 4), seq.
    pub fn snapshot(&self) -> Option<(u32, u32, PixelFormat, Vec<u8>, u64)> {
        let st = self.state.lock().unwrap();
        (st.width > 0).then(|| (st.width, st.height, st.format, st.data.clone(), st.seq))
    }

    /// Size, format and generation of the current image.
    pub fn shape(&self) -> (u32, u32, PixelFormat, u32) {
        let st = self.state.lock().unwrap();
        (st.width, st.height, st.format, st.generation)
    }

    /// Bring a consumer's private copy up to date. `dst` holds `height` rows of `dst_stride`
    /// bytes and must match `want_w` x `want_h`; pass the `seq`/`generation` it last saw. A
    /// generation mismatch forces a full copy. If the store's size differs from `want_w` x
    /// `want_h`, nothing is copied and `size_mismatch` is set so the caller can reallocate.
    pub fn sync_into(
        &self,
        last_seq: u64,
        last_gen: u32,
        want_w: u32,
        want_h: u32,
        dst: &mut [u8],
        dst_stride: usize,
    ) -> SyncResult {
        let st = self.state.lock().unwrap();
        let stride = st.width as usize * 4;
        let mut res = SyncResult {
            seq: st.seq,
            generation: st.generation,
            capture_us: st.capture_us,
            damage: Vec::new(),
            full: false,
            size_mismatch: false,
        };
        if st.width != want_w || st.height != want_h || st.data.is_empty() {
            res.size_mismatch = true;
            return res;
        }
        if st.seq == last_seq && st.generation == last_gen {
            return res;
        }
        let full_rect = Rect::new(0, 0, st.width, st.height);
        let mut full = st.generation != last_gen || last_seq < st.damage_floor;
        if !full {
            let mut total = 0u64;
            for (s, r, _) in st.damage.iter().rev() {
                if *s <= last_seq {
                    break;
                }
                total += r.area();
                res.damage.push(*r);
            }
            full = total * 2 > full_rect.area();
        }
        if full {
            res.damage = vec![full_rect];
            res.full = true;
        }
        for r in &res.damage {
            copy_rect(&st.data, stride, dst, dst_stride, *r);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w * h * 4) as usize]
    }

    #[test]
    fn partial_damage_reaches_consumer() {
        let store = FrameStore::new();
        store.update(8, 8, PixelFormat::Bgrx, &solid(8, 8, 1), 32, None, 1, 0);
        let mut mine = vec![0u8; 8 * 8 * 4];
        let r = store.sync_into(0, 0, 8, 8, &mut mine, 32);
        assert!(r.full);
        assert!(mine.iter().all(|&b| b == 1));

        // Only a 2x2 square changes; pixels outside the damage must not be copied.
        let next = solid(8, 8, 9);
        store.update(
            8,
            8,
            PixelFormat::Bgrx,
            &next,
            32,
            Some(&[Rect::new(2, 2, 2, 2)]),
            2,
            0,
        );
        let r = store.sync_into(r.seq, r.generation, 8, 8, &mut mine, 32);
        assert!(!r.full);
        assert_eq!(r.damage, vec![Rect::new(2, 2, 2, 2)]);
        let px = |x: usize, y: usize| mine[(y * 8 + x) * 4];
        assert_eq!(px(2, 2), 9);
        assert_eq!(px(3, 3), 9);
        assert_eq!(px(0, 0), 1);
        assert_eq!(px(4, 4), 1);

        // Nothing new: no damage.
        let again = store.sync_into(r.seq, r.generation, 8, 8, &mut mine, 32);
        assert!(again.damage.is_empty());
    }

    #[test]
    fn resize_forces_full_copy() {
        let store = FrameStore::new();
        store.update(4, 4, PixelFormat::Bgrx, &solid(4, 4, 1), 16, None, 1, 0);
        let mut mine = vec![0u8; 4 * 4 * 4];
        let r = store.sync_into(0, 0, 4, 4, &mut mine, 16);
        store.update(
            6,
            2,
            PixelFormat::Bgrx,
            &solid(6, 2, 3),
            24,
            Some(&[Rect::new(0, 0, 1, 1)]),
            2,
            0,
        );
        let stale = store.sync_into(r.seq, r.generation, 4, 4, &mut mine, 16);
        assert!(stale.size_mismatch);
        let mut resized = vec![0u8; 6 * 2 * 4];
        let r2 = store.sync_into(r.seq, r.generation, 6, 2, &mut resized, 24);
        assert!(r2.full);
        assert!(resized.iter().all(|&b| b == 3));
    }

    #[test]
    fn large_damage_collapses_to_full_frame() {
        let store = FrameStore::new();
        store.update(10, 10, PixelFormat::Bgrx, &solid(10, 10, 0), 40, None, 1, 0);
        let mut mine = vec![0u8; 10 * 10 * 4];
        let r = store.sync_into(0, 0, 10, 10, &mut mine, 40);
        store.update(
            10,
            10,
            PixelFormat::Bgrx,
            &solid(10, 10, 5),
            40,
            Some(&[Rect::new(0, 0, 10, 8)]),
            2,
            0,
        );
        let r2 = store.sync_into(r.seq, r.generation, 10, 10, &mut mine, 40);
        assert!(r2.full);
        assert!(mine.iter().all(|&b| b == 5));
    }

    #[test]
    fn rect_clamp() {
        assert_eq!(
            Rect::new(5, 5, 10, 10).clamp(8, 8),
            Some(Rect::new(5, 5, 3, 3))
        );
        assert_eq!(Rect::new(9, 0, 1, 1).clamp(8, 8), None);
    }
}
