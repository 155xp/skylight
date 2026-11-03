//! Keeping the picture alive: notice when a capture or an encoder stops producing a real image,
//! recover on our own, and report what happened at `/healthz`.
//!
//! * [`Watchdog`] runs in every encoder thread and decides when the capture it reads from has to
//!   be reopened: the source closed or failed, it never delivered a first frame, it went black,
//!   or the pointer moved and nothing came back from the compositor.
//! * [`OutputCheck`] decodes a keyframe now and then and compares it with the captured image, so
//!   an encoder that turns out black frames is dropped for the next one in line.
//! * [`Incidents`] keeps the last few recoveries for `/healthz`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ffmpeg_sys_next as ff;
use serde::Serialize;

use crate::encode::Codec;
use crate::frame::{PixelFormat, StoreState};

/// A frame whose brightest sampled pixel is darker than this is black.
pub const BLACK_LUMA: u8 = 16;

/// Brightest and mean luma (0-255) of a packed 4-byte-per-pixel image, from a 32x18 grid.
pub fn sample_luma(data: &[u8], width: u32, height: u32, format: PixelFormat) -> (u8, u8) {
    if width == 0 || height == 0 || data.len() < width as usize * height as usize * 4 {
        return (0, 0);
    }
    let (cols, rows) = (32u32, 18u32);
    let (mut max, mut sum) = (0u32, 0u32);
    for j in 0..rows {
        let y = (j * 2 + 1) * height / (rows * 2);
        for i in 0..cols {
            let x = (i * 2 + 1) * width / (cols * 2);
            let p = (y as usize * width as usize + x as usize) * 4;
            let (r, g, b) = match format {
                PixelFormat::Bgrx => (data[p + 2], data[p + 1], data[p]),
                PixelFormat::Rgbx => (data[p], data[p + 1], data[p + 2]),
            };
            let l = (r as u32 * 54 + g as u32 * 183 + b as u32 * 19) >> 8;
            max = max.max(l);
            sum += l;
        }
    }
    (max as u8, (sum / (cols * rows)) as u8)
}

/// Waits before reopening a capture again for the same reason: quick at first, then backing off
/// so a desktop that really is black (a dark video, a black wallpaper) costs one reopen every few
/// minutes and nothing else.
fn backoff(n: u32) -> Duration {
    Duration::from_secs([3, 10, 30, 60, 120, 300][(n as usize).min(5)])
}

/// How long a fresh capture may take to deliver its first frame.
const FIRST_FRAME: Duration = Duration::from_secs(3);
/// How long the image may stay black before the capture is reopened.
const BLACK_FOR: Duration = Duration::from_secs(3);
/// How long after a pointer move the compositor may stay silent (no frame, no cursor update).
const POINTER_SILENCE_US: u64 = 2_500_000;

pub struct Watchdog {
    black_since: Option<Instant>,
    /// Server clock of the first pointer move the capture has not answered yet.
    pointer_wait_us: Option<u64>,
    last_reopen: Option<Instant>,
    reopens: u32,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchdog {
    pub fn new() -> Watchdog {
        Watchdog {
            black_since: None,
            pointer_wait_us: None,
            last_reopen: None,
            reopens: 0,
        }
    }

    /// Why the capture should be reopened now, if it should. `age` is how long the capture has
    /// been running and `last_pointer_us` the server clock of the latest pointer move.
    pub fn check(
        &mut self,
        st: &StoreState,
        age: Duration,
        last_pointer_us: u64,
        now: Instant,
        now_us: u64,
    ) -> Option<&'static str> {
        if st.width > 0 && st.luma_max < BLACK_LUMA {
            self.black_since.get_or_insert(now);
        } else {
            self.black_since = None;
        }
        if last_pointer_us > st.activity_us {
            self.pointer_wait_us.get_or_insert(last_pointer_us);
        } else {
            self.pointer_wait_us = None;
        }
        let reason = if st.error.is_some() {
            "capture source closed"
        } else if st.frames_in == 0 && age >= FIRST_FRAME {
            "no first frame"
        } else if self
            .black_since
            .is_some_and(|t| now.duration_since(t) >= BLACK_FOR && age >= BLACK_FOR)
        {
            "black frames"
        } else if self
            .pointer_wait_us
            .is_some_and(|t| now_us.saturating_sub(t) >= POINTER_SILENCE_US)
            && age.as_micros() as u64 >= POINTER_SILENCE_US
        {
            "no updates after pointer input"
        } else {
            return None;
        };
        // A closed source is reopened at once the first time; everything else waits its turn.
        let wait = if st.error.is_some() && self.reopens == 0 {
            Duration::ZERO
        } else {
            backoff(self.reopens)
        };
        if self
            .last_reopen
            .is_some_and(|t| now.duration_since(t) < wait)
        {
            return None;
        }
        Some(reason)
    }

    /// Call after reopening (successfully or not).
    pub fn reopened(&mut self, now: Instant) {
        self.last_reopen = Some(now);
        self.reopens += 1;
        self.black_since = None;
        self.pointer_wait_us = None;
    }

    /// Call when the capture produced a real (not black) frame. Five quiet minutes after the
    /// last reopen, the backoff starts over.
    pub fn healthy(&mut self) {
        if self
            .last_reopen
            .is_some_and(|t| t.elapsed() >= Duration::from_secs(300))
        {
            self.reopens = 0;
        }
    }
}

/// Decodes single keyframes to check that the encoder's output is a real picture.
pub struct OutputCheck {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    pub codec: Codec,
}

// Owned by one encoder thread.
unsafe impl Send for OutputCheck {}

impl Drop for OutputCheck {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

impl OutputCheck {
    /// `None` when this FFmpeg has no decoder for the codec.
    pub fn new(codec: Codec) -> Option<OutputCheck> {
        let id = match codec {
            Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
        };
        unsafe {
            let dec = ff::avcodec_find_decoder(id);
            if dec.is_null() {
                return None;
            }
            let ctx = ff::avcodec_alloc_context3(dec);
            (*ctx).thread_count = 1;
            if ff::avcodec_open2(ctx, dec, std::ptr::null_mut()) < 0 {
                let mut ctx = ctx;
                ff::avcodec_free_context(&mut ctx);
                return None;
            }
            Some(OutputCheck {
                ctx,
                frame: ff::av_frame_alloc(),
                pkt: ff::av_packet_alloc(),
                codec,
            })
        }
    }

    /// Brightest luma (0-255, full range) of a keyframe decoded on its own, or `None` when it
    /// does not decode.
    pub fn luma_max(&mut self, keyframe: &[u8]) -> Option<u8> {
        unsafe {
            ff::avcodec_flush_buffers(self.ctx);
            if ff::av_new_packet(self.pkt, keyframe.len() as i32) < 0 {
                return None;
            }
            std::ptr::copy_nonoverlapping(keyframe.as_ptr(), (*self.pkt).data, keyframe.len());
            (*self.pkt).flags |= ff::AV_PKT_FLAG_KEY;
            let sent = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if sent < 0 {
                return None;
            }
            ff::avcodec_send_packet(self.ctx, std::ptr::null());
            let mut got = None;
            while ff::avcodec_receive_frame(self.ctx, self.frame) >= 0 {
                if got.is_none() {
                    got = Some(self.frame_luma_max());
                }
                ff::av_frame_unref(self.frame);
            }
            ff::avcodec_flush_buffers(self.ctx);
            got
        }
    }

    unsafe fn frame_luma_max(&self) -> u8 {
        unsafe {
            let f = &*self.frame;
            let (w, h) = (f.width.max(1) as usize, f.height.max(1) as usize);
            let y = f.data[0];
            let stride = f.linesize[0] as usize;
            let full = f.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG;
            let mut max = 0u8;
            for j in 0..18 {
                for i in 0..32 {
                    let (x, yy) = ((i * 2 + 1) * w / 64, (j * 2 + 1) * h / 36);
                    max = max.max(*y.add(yy * stride + x));
                }
            }
            if full {
                max
            } else {
                ((max.saturating_sub(16) as u32 * 255) / 219).min(255) as u8
            }
        }
    }
}

#[derive(Clone, Serialize)]
pub struct Incident {
    /// Unix time in milliseconds.
    pub at: u64,
    pub monitor: String,
    pub what: String,
}

/// The last few recoveries, newest last.
#[derive(Default)]
pub struct Incidents(Mutex<VecDeque<Incident>>);

impl Incidents {
    pub fn record(&self, monitor: &str, what: String) {
        tracing::warn!(monitor, "{what}");
        let mut v = self.0.lock().unwrap();
        v.push_back(Incident {
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            monitor: monitor.to_string(),
            what,
        });
        while v.len() > 20 {
            v.pop_front();
        }
    }

    pub fn recent(&self) -> Vec<Incident> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
    use crate::frame::FrameStore;

    fn store_with(px: [u8; 4]) -> std::sync::Arc<FrameStore> {
        let s = FrameStore::new();
        let img: Vec<u8> = px.iter().copied().cycle().take(64 * 36 * 4).collect();
        s.update(
            64,
            36,
            PixelFormat::Bgrx,
            &img,
            64 * 4,
            None,
            clock::now_us(),
            0,
        );
        s
    }

    #[test]
    fn luma_of_black_and_white() {
        assert_eq!(
            sample_luma(&[0; 64 * 36 * 4], 64, 36, PixelFormat::Bgrx),
            (0, 0)
        );
        let white = vec![255; 64 * 36 * 4];
        assert_eq!(sample_luma(&white, 64, 36, PixelFormat::Rgbx).0, 255);
    }

    #[test]
    fn black_capture_is_reopened_with_backoff() {
        let s = store_with([0, 0, 0, 0]);
        let mut w = Watchdog::new();
        let t0 = Instant::now();
        let st = s.lock();
        let age = Duration::from_secs(10);
        assert_eq!(w.check(&st, age, 0, t0, clock::now_us()), None);
        let t = t0 + BLACK_FOR;
        assert_eq!(
            w.check(&st, age, 0, t, clock::now_us()),
            Some("black frames")
        );
        w.reopened(t);
        // Right after a reopen the backoff holds the next one back.
        assert_eq!(w.check(&st, age, 0, t + BLACK_FOR, clock::now_us()), None);
        assert_eq!(
            w.check(&st, age, 0, t + Duration::from_secs(11), clock::now_us()),
            Some("black frames")
        );
    }

    #[test]
    fn a_real_picture_is_left_alone() {
        let s = store_with([90, 140, 200, 0]);
        let mut w = Watchdog::new();
        let st = s.lock();
        let t = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            w.check(&st, Duration::from_secs(60), 0, t, clock::now_us()),
            None
        );
    }

    #[test]
    fn pointer_moves_without_updates_mean_a_stale_capture() {
        let s = store_with([90, 140, 200, 0]);
        let mut w = Watchdog::new();
        let st = s.lock();
        let moved = st.activity_us + 1;
        let age = Duration::from_secs(60);
        let now = Instant::now();
        assert_eq!(w.check(&st, age, moved, now, moved + 1_000_000), None);
        assert_eq!(
            w.check(&st, age, moved, now, moved + POINTER_SILENCE_US),
            Some("no updates after pointer input")
        );
    }

    #[test]
    fn missing_first_frame_and_closed_source() {
        let s = FrameStore::new();
        let mut w = Watchdog::new();
        let now = Instant::now();
        assert_eq!(w.check(&s.lock(), Duration::from_secs(1), 0, now, 0), None);
        assert_eq!(
            w.check(&s.lock(), FIRST_FRAME, 0, now, 0),
            Some("no first frame")
        );
        s.set_error("gone".into());
        let mut w = Watchdog::new();
        assert_eq!(
            w.check(&s.lock(), Duration::ZERO, 0, now, 0),
            Some("capture source closed")
        );
    }

    #[test]
    fn output_check_sees_black_and_picture() {
        use crate::encode::{Backend, EncoderParams, VideoEncoder};
        let Some(mut check) = OutputCheck::new(Codec::H264) else {
            return;
        };
        for (fill, bright) in [(0u8, false), (200u8, true)] {
            let p = EncoderParams {
                width: 320,
                height: 192,
                backend: Backend::Software,
                ..Default::default()
            };
            let mut enc = VideoEncoder::open(&p).expect("libx264");
            let (buf, _) = enc.input().unwrap();
            buf.fill(fill);
            let mut out = Vec::new();
            assert!(enc.encode(true, &mut out).unwrap());
            let l = check.luma_max(&out).expect("decodes");
            assert_eq!(l >= BLACK_LUMA, bright, "luma {l}");
        }
    }
}
