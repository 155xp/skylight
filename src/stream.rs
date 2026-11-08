//! A session's encoder thread: takes frames from its capture, encodes them, and paces them to
//! what the viewer's link carries (see [`crate::pacer`]). It also replaces a capture or an
//! encoder that goes bad, and sharpens a still picture after motion stops.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use tokio::sync::mpsc::UnboundedSender;

use crate::capture::{Capture, Hub, MonitorInfo};
use crate::clock;
use crate::encode::{self, Codec, EncoderParams, RateChange, VideoEncoder};
use crate::frame::{CursorPos, FrameStore, PullGuard, Signal, WAKE_COMMAND, WAKE_STOP};
use crate::h264;
use crate::health::{self, OutputCheck, Watchdog};
use crate::input::Activity;
use crate::nvenc::FrameOpts;
use crate::pacer::Pacer;
use crate::protocol::{self as proto, ServerMsg};

/// How a server encodes, for every session.
pub struct Encoding {
    pub encoder: encode::Backend,
    pub nvenc_preset: u8,
    pub vbv_ms: u32,
    /// Constant-quality target under the bitrate cap; `None` for plain CBR.
    pub cq: Option<u8>,
    /// Sharpen a still picture after motion (see `REFINE_AFTER`).
    pub refine: bool,
    /// Frame-rate cap for viewers whose best round trip is over `REMOTE_RTT_US`; 0 for none.
    pub remote_max_fps: u32,
}

pub enum Cmd {
    Keyframe,
    Settings {
        max_fps: Option<u32>,
        max_bitrate: Option<u64>,
        codec: Option<Codec>,
    },
    Switch(Arc<Capture>),
}

/// State shared between the async session task and its encoder thread.
pub struct Shared {
    pub sig: Arc<Signal>,
    cmds: Mutex<VecDeque<Cmd>>,
    pub pacer: Mutex<Pacer>,
    decode_us: Mutex<(u64, u32)>,
    /// The monitor on screen, set by the encoder whenever it starts on a capture, so input maps
    /// to the picture the viewer sees.
    pub mon: Arc<Mutex<MonitorInfo>>,
    /// The ticket names this monitor, so when it goes away the session ends rather than show
    /// another one.
    pinned: bool,
    pub activity: Arc<Activity>,
}

impl Shared {
    pub fn new(pacer: Pacer, mon: MonitorInfo, pinned: bool) -> Shared {
        Shared {
            sig: Signal::new(),
            cmds: Mutex::new(VecDeque::new()),
            pacer: Mutex::new(pacer),
            decode_us: Mutex::new((0, 0)),
            mon: Arc::new(Mutex::new(mon)),
            pinned,
            activity: Activity::new(),
        }
    }

    pub fn send(&self, c: Cmd) {
        self.cmds.lock().unwrap().push_back(c);
        self.sig.raise(WAKE_COMMAND);
    }

    /// The viewer decoded `frame_id` in `decode_us`.
    pub fn on_ack(&self, frame_id: u32, decode_us: u32) {
        self.pacer.lock().unwrap().on_ack(frame_id, clock::now_us());
        let mut d = self.decode_us.lock().unwrap();
        d.0 += decode_us as u64;
        d.1 += 1;
        drop(d);
        self.sig.raise(WAKE_COMMAND);
    }
}

/// The capture being streamed, and how far into it this session is.
struct Source {
    cap: Arc<Capture>,
    store: Arc<FrameStore>,
    sig: Arc<Signal>,
    _pull: PullGuard,
    last_seq: u64,
    last_gen: u32,
    cursor_serial: u32,
    cursor_pos: Option<CursorPos>,
    since: Instant,
    /// The store's frame counter when the stats last read it.
    counted: u64,
}

impl Source {
    /// Start streaming `cap`: from its next frame on, with its cursor sent again, and input mapped
    /// to its monitor.
    fn new(cap: Arc<Capture>, sh: &Shared) -> Source {
        let store = cap.store.clone();
        store.subscribe(&sh.sig);
        let counted = store.lock().frames_in;
        *sh.mon.lock().unwrap() = cap.monitor.clone();
        Source {
            _pull: store.pull(),
            store,
            cap,
            sig: sh.sig.clone(),
            last_seq: 0,
            last_gen: u32::MAX,
            cursor_serial: u32::MAX,
            cursor_pos: None,
            since: Instant::now(),
            counted,
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.store.unsubscribe(&self.sig);
    }
}

/// Re-encode the last image at a higher bitrate once the screen has been still this long, so
/// text sharpens after scrolling or dragging stops.
const REFINE_AFTER: Duration = Duration::from_millis(120);
/// ...and at least this long after the last input event (µs).
const REFINE_QUIET_US: u64 = 300_000;
/// The longest gap that spacing frames by their cost may make (10 frames a second).
const MAX_FRAME_US: u64 = 100_000;

/// Time between frames of motion. Frames that take longer than `base_us` at the target bitrate
/// (scrolling text through 1 Mbit/s, where the encoder cannot make them small enough) go as
/// often as the bitrate carries them, evenly, rather than in bursts the pacer has to hold back.
fn frame_spacing_us(base_us: u64, frame_bits: f64, target_bps: u64) -> u64 {
    let cost_us = (frame_bits * 1e6 / target_bps.max(1) as f64) as u64;
    base_us.max(cost_us.min(MAX_FRAME_US))
}

/// How much each refinement frame lowers the QP on this encoder (see [`encode::Caps::refine`]),
/// one entry per refinement frame. `SKYLIGHT_SHARPEN` (comma-separated) overrides it, for
/// experiments.
pub fn refine_steps(caps: encode::Caps) -> &'static [u8] {
    static OVERRIDE: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    let o = OVERRIDE.get_or_init(|| {
        std::env::var("SKYLIGHT_SHARPEN")
            .ok()
            .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
    });
    match o {
        Some(v) => &v[..v.len().min(16)],
        None => caps.refine,
    }
}

struct Stats {
    since: Instant,
    frames: u32,
    refines: u32,
    bytes: u64,
    encode_us: u64,
    encode_max_us: u64,
    queue_us: u64,
    /// First frame after an input event: input received to frame received from the compositor,
    /// and input received to frame handed to the socket.
    input_capture_us: u64,
    input_sent_us: u64,
    input_n: u32,
    /// Frames the capture source delivered.
    captured: u64,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            since: Instant::now(),
            frames: 0,
            refines: 0,
            bytes: 0,
            encode_us: 0,
            encode_max_us: 0,
            queue_us: 0,
            input_capture_us: 0,
            input_sent_us: 0,
            input_n: 0,
            captured: 0,
        }
    }
}

/// For this long after any input, frames go out as soon as they exist, ignoring the frame-rate
/// cap, so the frame that answers a click or key press never waits for a pacing slot. Far away
/// (a tunnel, the internet) that holds after a click, a key press or a touch, and for the first
/// frame of a run of pointer motion or scrolling; the rest of a drag or a scroll goes at the frame
/// rate, which is the display's own. Faster, over a jittery path, most of those frames are never
/// seen, the pacer has to skip, and the ones that do show land unevenly. On a LAN they cost little
/// and keep the newest picture a few milliseconds fresher.
const INPUT_FAST_PATH_US: u64 = 250_000;

/// A viewer whose best round trip is longer than this is on a long path (`remote_max_fps`).
const REMOTE_RTT_US: u64 = 20_000;

/// An encoder that fails this many times in a row is skipped for [`ENCODER_TIMEOUT`].
const ENCODER_STRIKES: u32 = 2;
const ENCODER_TIMEOUT: Duration = Duration::from_secs(60);
/// Decode a keyframe to check the encoder's output at most this often (and always first thing).
const OUTPUT_CHECK_EVERY: Duration = Duration::from_secs(30);
/// Give up on a session (which then closes, so the client reconnects) after this many failed
/// reopens.
const MAX_REOPEN_FAILURES: u32 = 3;

/// Stream `cap` to `out` until the session stops it (`WAKE_STOP`), the socket closes, or nothing
/// can be captured or encoded any more (then an `error` goes out first).
pub fn encoder_loop(
    sh: Arc<Shared>,
    cap: Arc<Capture>,
    out: UnboundedSender<Message>,
    mut codec: Codec,
    mut fps: u32,
    hub: &Hub,
    cfg: &Encoding,
) {
    let last_input_us = &sh.activity.any_us;
    let mut src = Source::new(cap, &sh);
    let mut last_request = Instant::now() - Duration::from_secs(1);
    let mut last_request_seq = u64::MAX;
    let mut pull_slot: Option<Instant> = None;
    let mut dog = Watchdog::new();
    let mut next_check = Instant::now();
    let mut reopen_failures = 0u32;
    // Encoder id -> failures in a row, and when it may be tried first again.
    let mut strikes: HashMap<&'static str, u32> = HashMap::new();
    let mut skip: HashMap<&'static str, Instant> = HashMap::new();
    let mut output_check: Option<OutputCheck> = None;
    let mut last_output_check: Option<Instant> = None;
    let mut enc: Option<VideoEncoder> = None;
    let mut frame_id = 0u32;
    let mut need_key = true;
    let mut last_encode = Instant::now() - Duration::from_secs(1);
    let mut last_content = Instant::now();
    // Capture time (server clock) of the last frame of new content that went out.
    let mut last_capture_us = 0u64;
    let mut refine_left = 0u8;
    // What a frame of motion costs, in bits (a moving average; keyframes and refinements, big
    // on purpose, left out).
    let mut frame_bits = 0f64;
    let mut stats = Stats::new();
    let mut payload = Vec::with_capacity(1 << 20);
    // No waiting before the first pass: a capture this session shares with another viewer already
    // holds an image, and on a still screen nothing would wake the loop to send it.
    let mut timeout = Duration::ZERO;
    let mut announced = None;
    let mut input_answered = 0u64;
    let mut last_rate_change = Instant::now();
    let mut reopen = false;

    loop {
        let flags = sh.sig.wait(timeout);
        timeout = Duration::from_millis(250);
        if flags & WAKE_STOP != 0 || out.is_closed() {
            break;
        }
        let cmds: Vec<Cmd> = sh.cmds.lock().unwrap().drain(..).collect();
        for c in cmds {
            match c {
                Cmd::Keyframe => need_key = true,
                Cmd::Settings {
                    max_fps,
                    max_bitrate,
                    codec: c,
                } => {
                    if let Some(f) = max_fps {
                        fps = f;
                        sh.pacer.lock().unwrap().frame_interval_us = 1_000_000 / f as u64;
                    }
                    if let Some(b) = max_bitrate {
                        sh.pacer.lock().unwrap().set_max(b);
                    }
                    if let Some(c) = c
                        && c != codec
                    {
                        codec = c;
                        enc = None;
                    }
                }
                Cmd::Switch(c) => {
                    src = Source::new(c, &sh);
                    enc = None;
                    dog = Watchdog::new();
                }
            }
        }

        // Watchdog: replace a capture that closed, never started, went black or stopped
        // answering the pointer, or whose monitor changed or went away.
        let now = Instant::now();
        if now >= next_check {
            next_check = now + Duration::from_millis(250);
            let reason = {
                let st = src.store.lock();
                dog.check(
                    &st,
                    now.duration_since(src.since),
                    sh.activity.pointer_us.load(Ordering::Relaxed),
                    now,
                    clock::now_us(),
                )
            };
            let mon = hub.resolve(Some(&src.cap.monitor.id)).ok();
            let reason = match &mon {
                None => Some("the display went away"),
                Some(m) if reason.is_none() && *m != src.cap.monitor => Some("the display changed"),
                _ => reason,
            };
            if let Some(reason) = reason {
                dog.reopened(now);
                match hub.replace(&src.cap, mon, reason, sh.pinned) {
                    Ok(c) => {
                        reopen_failures = 0;
                        src = Source::new(c, &sh);
                        need_key = true;
                    }
                    Err(e) => {
                        reopen_failures += 1;
                        if reopen_failures >= MAX_REOPEN_FAILURES {
                            let message = format!("capture: {e:#}");
                            let _ =
                                out.send(proto::message(&ServerMsg::Error { message: &message }));
                            return;
                        }
                    }
                }
            }
        }

        // Cursor: shape when it changes, position always (it is 12 bytes).
        {
            let st = src.store.lock();
            if let Some(shape) = &st.cursor_shape
                && shape.serial != src.cursor_serial
            {
                src.cursor_serial = shape.serial;
                let m = proto::cursor_shape_message(
                    shape.serial,
                    shape.width,
                    shape.height,
                    shape.hot_x,
                    shape.hot_y,
                    &shape.rgba,
                );
                let _ = out.send(Message::Binary(m.into()));
            }
            if src.cursor_pos != Some(st.cursor) {
                src.cursor_pos = Some(st.cursor);
                let m = proto::cursor_pos_message(st.cursor.x, st.cursor.y, st.cursor.visible);
                let _ = out.send(Message::Binary(m.into()));
            }
        }

        let now = Instant::now();
        let now_us = clock::now_us();
        {
            let mut p = sh.pacer.lock().unwrap();
            p.adjust(now_us);
            let b = p.target_bps;
            if let Some(e) = &mut enc
                && b != e.bitrate()
            {
                // Encoders that reconfigure in place follow every move of more than a few
                // percent. Where a change costs a keyframe (FFmpeg's NVENC wrapper, or a new
                // encoder for VA-API and the other FFmpeg ones), only big moves, at most every 3 s.
                let ratio = b.max(e.bitrate()) as f64 / b.min(e.bitrate()).max(1) as f64;
                let rarely = last_rate_change.elapsed() >= Duration::from_secs(3) && ratio >= 1.5;
                match e.caps().rate {
                    RateChange::InPlace if ratio > 1.04 => e.set_bitrate(b),
                    RateChange::Keyframe if rarely => e.set_bitrate(b),
                    RateChange::Reopen if rarely => reopen = true,
                    _ => {}
                }
                if ratio > 1.04 && (e.caps().rate == RateChange::InPlace || rarely) {
                    last_rate_change = Instant::now();
                }
            }
            if p.stalled(now_us) {
                // The viewer stopped acknowledging (a hidden tab, a decoder reset); whatever it
                // missed is gone, so start over from a keyframe.
                need_key = true;
            }
        }
        if reopen {
            // Opened again below at the new target, starting with a keyframe.
            reopen = false;
            enc = None;
        }
        if stats.since.elapsed() >= Duration::from_secs(1) {
            let (frames_in, luma) = {
                let st = src.store.lock();
                (st.frames_in, st.luma_max)
            };
            stats.captured = frames_in.saturating_sub(src.counted);
            src.counted = frames_in;
            send_stats(&out, &sh, &mut stats, enc.as_ref(), now_us, luma);
        }

        let (w, h, format, generation) = src.store.shape();
        if w == 0 {
            continue;
        }
        let seq = src.store.lock().seq;
        let new_content = seq != src.last_seq || generation != src.last_gen;
        // Changes waiting at an on-demand source (X11): asked for below, once a frame may go.
        let pending = !new_content && src.store.pending();
        // Refinements go one at a time, each after the previous frame was acknowledged: on a
        // slow link a burst of them would sit in front of the frame that answers the next click.
        // They also wait for a pause in input, so text sharpens once typing stops rather than
        // between keystrokes.
        let link_idle = sh.pacer.lock().unwrap().inflight() == 0;
        let input_quiet = Duration::from_micros(REFINE_QUIET_US).saturating_sub(
            Duration::from_micros(now_us.saturating_sub(last_input_us.load(Ordering::Relaxed))),
        );
        let refine_wait = REFINE_AFTER
            .saturating_sub(now.duration_since(last_content))
            .max(input_quiet);
        let refine_due = cfg.refine
            && refine_left > 0
            && !new_content
            && !pending
            && link_idle
            && refine_wait.is_zero();
        if !new_content && !pending && !need_key && !refine_due {
            if cfg.refine && refine_left > 0 && link_idle {
                timeout = refine_wait.max(Duration::from_millis(1));
            }
            continue;
        }
        // Far away (a tunnel, the internet), frames arrive with the path's jitter: past
        // `remote_max_fps` more of them only make the motion less even and cost bytes.
        let (base_us, frame_us, remote) = {
            let mut p = sh.pacer.lock().unwrap();
            let remote = p.min_rtt_us > REMOTE_RTT_US;
            let f = if cfg.remote_max_fps > 0 && remote {
                fps.min(cfg.remote_max_fps)
            } else {
                fps
            };
            let base_us = 1_000_000 / f.max(1) as u64;
            p.frame_interval_us = frame_spacing_us(base_us, frame_bits, p.target_bps);
            (base_us, p.frame_interval_us, remote)
        };
        let since_input = |t: &AtomicU64| now_us.saturating_sub(t.load(Ordering::Relaxed));
        let times = &*sh.activity;
        let answering = if remote {
            since_input(&times.discrete_us) < INPUT_FAST_PATH_US
                || times.burst_us.load(Ordering::Relaxed) > last_capture_us
        } else {
            since_input(last_input_us) < INPUT_FAST_PATH_US
        };
        // The frame that answers input goes at once (see `INPUT_FAST_PATH_US`). Far away that is at
        // most every half frame: after a click while the mouse keeps moving, every compositor frame
        // (144 a second on a fast monitor) would otherwise go out, unevenly.
        let interval = if answering && (new_content || pending) {
            Duration::from_micros(if remote { base_us / 2 } else { 0 })
        } else {
            Duration::from_micros(frame_us)
        };
        let since = now.duration_since(last_encode);
        if since < interval && !need_key {
            timeout = interval - since;
            continue;
        }
        {
            let mut p = sh.pacer.lock().unwrap();
            if !p.can_send(now_us) {
                timeout = Duration::from_micros(
                    p.wait_hint_us(now_us).unwrap_or(5_000).clamp(1_000, 50_000),
                );
                continue;
            }
        }
        if pending && !need_key {
            // Grab now, so the frame is as fresh as possible; its arrival wakes this loop.
            // One request per batch of changes (and again if it went unanswered).
            if last_request_seq != seq
                || now.duration_since(last_request) > Duration::from_millis(20)
            {
                src.store.request_frame();
                last_request = now;
                last_request_seq = seq;
                // Pace from the request, not from when the grab lands.
                pull_slot.get_or_insert(now);
            }
            timeout = Duration::from_millis(20);
            continue;
        }

        if enc
            .as_ref()
            .is_none_or(|e| e.width != w || e.height != h || e.codec != codec)
        {
            let (bitrate, link_known) = {
                let p = sh.pacer.lock().unwrap();
                (p.target_bps, p.min_rtt_us > 0)
            };
            let params = EncoderParams {
                codec,
                width: w,
                height: h,
                fps,
                bitrate,
                vbv_ms: cfg.vbv_ms,
                format,
                backend: cfg.encoder,
                nvenc_preset: cfg.nvenc_preset,
                cq: cfg.cq,
                // Before the first frame is acknowledged nothing is known about the link (a
                // remembered target may never have been put to the test): a keyframe sized for a
                // fast one would keep a slow one waiting. It goes out at the usual budget instead,
                // and the refinement frames after it bring it to full sharpness.
                first_key_boost: link_known,
                ..Default::default()
            };
            let now = Instant::now();
            match VideoEncoder::open_avoiding(&params, |id| skip.get(id).is_some_and(|t| now < *t))
            {
                Ok(e) => {
                    tracing::info!(encoder = %e.name, width = w, height = h, fps, "encoder open");
                    enc = Some(e);
                    last_output_check = None;
                }
                Err(e) => {
                    let _ = out.send(proto::message(&ServerMsg::Error {
                        message: &format!("encoder: {e:#}"),
                    }));
                    return;
                }
            }
            need_key = true;
            src.last_seq = 0;
            src.last_gen = u32::MAX;
        }
        let e = enc.as_mut().unwrap();
        let mon_id = src.cap.monitor.id.clone();
        if announced != Some((e.codec, w, h, mon_id.clone())) {
            announced = Some((e.codec, w, h, mon_id.clone()));
            let m = ServerMsg::Stream {
                codec: e.codec.name(),
                encoder: &e.name,
                hardware: e.hardware,
                width: w,
                height: h,
                fps,
                monitor: &mon_id,
            };
            let _ = out.send(proto::message(&m));
        }

        let source_luma = src.store.lock().luma_max;
        let (buf, stride) = match e.input() {
            Ok(b) => b,
            Err(err) => {
                tracing::error!("encoder input: {err:#}");
                enc = None;
                timeout = Duration::from_millis(100);
                continue;
            }
        };
        let r = src
            .store
            .sync_into(src.last_seq, src.last_gen, w, h, buf, stride);
        if r.size_mismatch {
            enc = None;
            timeout = Duration::from_millis(1);
            continue;
        }
        e.changed(&r.damage);
        src.last_seq = r.seq;
        src.last_gen = r.generation;
        // A refinement re-encodes the unchanged image. Under the constant-quality target that
        // lifts a frame that the VBV cap held back (the first frame of a big change) up to the
        // target, and costs almost nothing when it is already there.
        let refine = r.damage.is_empty() && !need_key;
        let t0 = Instant::now();
        // Pace from the start of each encode (or the grab request that led to it), so the cap is
        // a real cap and not cap + encode time.
        last_encode = pull_slot.take().unwrap_or(t0);
        let queue_us = clock::now_us().saturating_sub(r.capture_us);
        payload.clear();
        let steps = refine_steps(e.caps());
        let opts = FrameOpts {
            keyframe: need_key,
            sharpen: if refine {
                steps
                    .get(steps.len().saturating_sub(refine_left as usize))
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            },
        };
        let key = match e.encode_with(opts, &mut payload) {
            Ok(k) => k,
            Err(err) => {
                let name = e.name.clone();
                let n = strikes.entry(e.id).or_insert(0);
                *n += 1;
                tracing::error!(encoder = %name, strikes = *n, "encode failed: {err:#}");
                if *n >= ENCODER_STRIKES {
                    *n = 0;
                    skip.insert(e.id, Instant::now() + ENCODER_TIMEOUT);
                    hub.incidents.record(
                        &src.cap.monitor.id,
                        format!("encoder {name} keeps failing ({err:#}); trying the next one"),
                    );
                }
                enc = None;
                need_key = true;
                continue;
            }
        };
        strikes.remove(e.id);
        if e.codec == Codec::H264
            && let Some(fixed) = h264::no_reorder(&payload)
        {
            payload = fixed;
        }
        let encode_us = t0.elapsed().as_micros() as u64;
        if refine {
            refine_left -= 1;
            stats.refines += 1;
        } else {
            refine_left = steps.len() as u8;
            last_content = Instant::now();
            last_capture_us = r.capture_us;
            timeout = REFINE_AFTER;
        }
        need_key = false;
        if payload.is_empty() {
            continue;
        }
        frame_id = frame_id.wrapping_add(1);
        let header = proto::VideoHeader {
            flags: if key { proto::FLAG_KEY } else { 0 }
                | if refine { proto::FLAG_REFINE } else { 0 },
            codec: e.codec.id(),
            frame_id,
            width: w as u16,
            height: h as u16,
            capture_us: if refine {
                clock::now_us()
            } else {
                r.capture_us
            },
            queue_us: if refine { 0 } else { queue_us as u32 },
            encode_us: encode_us as u32,
        };
        let msg = proto::video_message(&header, &payload);
        sh.pacer
            .lock()
            .unwrap()
            .on_send(frame_id, msg.len(), refine, clock::now_us());
        if !key && !refine {
            frame_bits += (msg.len() as f64 * 8.0 - frame_bits) / 8.0;
        }
        stats.frames += 1;
        stats.bytes += msg.len() as u64;
        stats.encode_us += encode_us;
        stats.encode_max_us = stats.encode_max_us.max(encode_us);
        if !refine {
            stats.queue_us += queue_us;
            let input_us = last_input_us.load(Ordering::Relaxed);
            let received_us = r.capture_us;
            if input_us > input_answered && received_us >= input_us {
                input_answered = input_us;
                let sent_us = clock::now_us();
                stats.input_capture_us += received_us - input_us;
                stats.input_sent_us += sent_us - input_us;
                stats.input_n += 1;
                tracing::debug!(
                    input_to_capture_ms = (received_us - input_us) as f64 / 1000.0,
                    input_to_sent_ms = (sent_us - input_us) as f64 / 1000.0,
                    "first frame after input"
                );
            }
        }
        if out.send(Message::Binary(msg.into())).is_err() {
            break;
        }
        if source_luma >= health::BLACK_LUMA {
            dog.healthy();
        }

        // Now and then, decode a keyframe and make sure the encoder's picture is not black when
        // the captured one is not. An encoder that fails this is dropped for the next in line.
        // (Only when the capture is clearly not black, so a dark picture is not taken for a
        // broken encoder.)
        if key
            && source_luma >= 3 * health::BLACK_LUMA
            && last_output_check.is_none_or(|t| t.elapsed() >= OUTPUT_CHECK_EVERY)
        {
            last_output_check = Some(Instant::now());
            if output_check.as_ref().is_none_or(|c| c.codec != e.codec) {
                output_check = OutputCheck::new(e.codec);
            }
            if let Some(check) = &mut output_check {
                let got = check.luma_max(&payload);
                if got.is_none_or(|l| l < health::BLACK_LUMA) {
                    let name = e.name.clone();
                    skip.insert(e.id, Instant::now() + ENCODER_TIMEOUT);
                    hub.incidents.record(
                        &src.cap.monitor.id,
                        match got {
                            Some(l) => format!(
                                "encoder {name} sends a black picture (luma {l}, captured {source_luma}); trying the next one"
                            ),
                            None => format!("encoder {name} sends keyframes that do not decode; trying the next one"),
                        },
                    );
                    enc = None;
                    need_key = true;
                }
            }
        }
    }
}

fn send_stats(
    out: &UnboundedSender<Message>,
    sh: &Shared,
    s: &mut Stats,
    enc: Option<&VideoEncoder>,
    now_us: u64,
    luma_max: u8,
) {
    let secs = s.since.elapsed().as_secs_f64();
    let content = (s.frames - s.refines).max(1) as f64;
    let (dec_sum, dec_n) = std::mem::take(&mut *sh.decode_us.lock().unwrap());
    let p = sh.pacer.lock().unwrap();
    let v = serde_json::json!({
        "t": "stats",
        "fps": s.frames as f64 / secs,
        "captureFps": s.captured as f64 / secs,
        "refines": s.refines,
        "kbps": s.bytes as f64 * 8.0 / secs / 1000.0,
        "targetKbps": p.target_bps / 1000,
        "deliveryKbps": p.delivery_bps(now_us) / 1000,
        "encodeMs": if s.frames > 0 { s.encode_us as f64 / s.frames as f64 / 1000.0 } else { 0.0 },
        "encodeMaxMs": s.encode_max_us as f64 / 1000.0,
        "queueMs": s.queue_us as f64 / content / 1000.0,
        "inputToCaptureMs": if s.input_n > 0 { s.input_capture_us as f64 / s.input_n as f64 / 1000.0 } else { 0.0 },
        "inputToSentMs": if s.input_n > 0 { s.input_sent_us as f64 / s.input_n as f64 / 1000.0 } else { 0.0 },
        "rttMs": p.srtt_us / 1000.0,
        "minRttMs": p.min_rtt_us as f64 / 1000.0,
        "inflight": p.inflight(),
        "skipped": p.skipped,
        "stalls": p.stalls,
        "decodeMs": if dec_n > 0 { dec_sum as f64 / dec_n as f64 / 1000.0 } else { 0.0 },
        "encoder": enc.map(|e| e.name.as_str()).unwrap_or(""),
        "lumaMax": luma_max,
    });
    drop(p);
    let _ = out.send(Message::Text(v.to_string().into()));
    *s = Stats::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_too_big_for_the_bitrate_are_spaced_out() {
        // 60 fps; 4 kB frames at 16 Mbit/s fit, the same frames at 1 Mbit/s take 32 ms each.
        assert_eq!(frame_spacing_us(16_667, 32_000.0, 16_000_000), 16_667);
        assert_eq!(frame_spacing_us(16_667, 32_000.0, 1_000_000), 32_000);
        assert_eq!(frame_spacing_us(16_667, 1e6, 1_000_000), MAX_FRAME_US);
        assert_eq!(frame_spacing_us(16_667, 0.0, 1_000_000), 16_667);
    }
}
