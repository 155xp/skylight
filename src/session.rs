//! One browser connection: authentication, the encoder thread, input and clipboard.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::capture::{self, Capture, Hub, InputSink, MonitorInfo};
use crate::clock;
use crate::encode::{self, Codec, EncoderParams, VideoEncoder};
use crate::frame::{Signal, WAKE_COMMAND, WAKE_STOP};
use crate::input;
use crate::pacer::Pacer;
use crate::probe;
use crate::protocol::{self as proto, ClientMsg, Input, ServerMsg};
use crate::ticket::{self, Claims, Verifier};

pub struct Config {
    pub max_fps: u32,
    pub max_bitrate: u64,
    pub start_bitrate: u64,
    pub min_bitrate: u64,
    pub encoder: encode::Backend,
    pub nvenc_preset: u8,
    pub vbv_ms: u32,
    /// Constant-quality target under the bitrate cap; `None` for plain CBR.
    pub cq: Option<u8>,
    pub refine: bool,
    pub allowed_origins: Vec<String>,
    pub max_sessions: usize,
    /// X display for the latency probe window (`skylight probe-window`).
    pub probe_display: Option<String>,
}

const MAX_UNAUTHENTICATED: usize = 32;

pub struct App {
    pub hub: Hub,
    pub verifier: Verifier,
    pub config: Config,
    pub sessions: AtomicUsize,
    /// Sockets that have not shown a valid ticket yet.
    pub unauthenticated: AtomicUsize,
    /// Input sockets waiting to attach, by one-time token.
    pub input_tokens: Mutex<HashMap<String, Arc<InputCtx>>>,
    pub encoders: Vec<(Codec, String)>,
}

/// Everything input needs, shared by the session and its separate input socket.
pub struct InputCtx {
    sink: Arc<dyn InputSink>,
    mon: Mutex<MonitorInfo>,
    held: Mutex<HeldKeys>,
    /// Server clock of the last input event; the encoder skips its frame-rate cap right after.
    last_input_us: Arc<AtomicU64>,
    closed: AtomicBool,
    close: tokio::sync::Notify,
}

impl InputCtx {
    fn handle(&self, inp: Input) {
        self.last_input_us.store(clock::now_us(), Ordering::Relaxed);
        let mon = self.mon.lock().unwrap().clone();
        handle_input(
            self.sink.as_ref(),
            &mon,
            inp,
            &mut self.held.lock().unwrap(),
        );
    }

    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.close.notify_waiters();
        self.held.lock().unwrap().release_all(self.sink.as_ref());
    }
}

/// The input socket: authenticated by the token from `welcome`, then every message is one input
/// event, injected the moment it arrives.
pub async fn run_input(socket: WebSocket, app: Arc<App>) {
    let (_sink, mut stream) = socket.split();
    let token = match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<proto::Attach>(&t)
            .ok()
            .map(|a| a.token),
        _ => None,
    };
    let Some(ctx) = token.and_then(|t| app.input_tokens.lock().unwrap().remove(&t)) else {
        return;
    };
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(inp) = proto::parse_input(&b)
                        && !matches!(inp, Input::Ack { .. })
                    {
                        ctx.handle(inp);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = ctx.close.notified() => break,
        }
        if ctx.closed.load(Ordering::SeqCst) {
            break;
        }
    }
}

enum Cmd {
    Keyframe,
    Settings {
        max_fps: Option<u32>,
        max_bitrate: Option<u64>,
        codec: Option<Codec>,
    },
    Switch(Arc<Capture>),
}

/// State shared between the async session task and its encoder thread.
struct Shared {
    sig: Arc<Signal>,
    cmds: Mutex<VecDeque<Cmd>>,
    pacer: Mutex<Pacer>,
    decode_us: Mutex<(u64, u32)>,
}

impl Shared {
    fn send(&self, c: Cmd) {
        self.cmds.lock().unwrap().push_back(c);
        self.sig.raise(WAKE_COMMAND);
    }
}

fn text(s: String) -> Message {
    Message::Text(s.into())
}

/// Run a WebSocket session to completion.
pub async fn run(socket: WebSocket, app: Arc<App>) {
    let (mut sink, mut stream) = socket.split();
    // Unauthenticated sockets get 5 s and a global cap, so nobody can pile them up.
    if app.unauthenticated.fetch_add(1, Ordering::SeqCst) >= MAX_UNAUTHENTICATED {
        app.unauthenticated.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    let hello = match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<ClientMsg>(&t).ok(),
        _ => None,
    };
    app.unauthenticated.fetch_sub(1, Ordering::SeqCst);
    let Some(ClientMsg::Hello {
        ticket,
        codecs,
        max_fps,
        monitor,
        max_bitrate,
    }) = hello
    else {
        let _ = sink
            .send(text(proto::json(&ServerMsg::Error {
                message: "expected hello",
            })))
            .await;
        return;
    };
    let claims = match app.verifier.verify(&ticket, clock::unix_secs()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("rejected ticket: {e}");
            let _ = sink
                .send(text(proto::json(&ServerMsg::Error {
                    message: &format!("unauthorized: {e}"),
                })))
                .await;
            return;
        }
    };
    if app.sessions.fetch_add(1, Ordering::SeqCst) >= app.config.max_sessions {
        app.sessions.fetch_sub(1, Ordering::SeqCst);
        let _ = sink
            .send(text(proto::json(&ServerMsg::Error {
                message: "too many sessions",
            })))
            .await;
        return;
    }
    let session_id = ticket::random_id()[..8].to_string();
    tracing::info!(session = %session_id, sub = %claims.sub, scope = ?claims.scope, "session start");
    let started = Instant::now();
    let result = serve(
        &app,
        &claims,
        &session_id,
        sink,
        stream,
        codecs,
        max_fps,
        monitor,
        max_bitrate,
    )
    .await;
    app.sessions.fetch_sub(1, Ordering::SeqCst);
    tracing::info!(session = %session_id, secs = started.elapsed().as_secs(), result = ?result.err().map(|e| e.to_string()), "session end");
}

fn pick_codec(app: &App, wanted: &[String]) -> Codec {
    for w in wanted {
        if let Some(c) = Codec::parse(w)
            && app.encoders.iter().any(|(have, _)| *have == c)
        {
            return c;
        }
    }
    Codec::H264
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    app: &Arc<App>,
    claims: &Claims,
    session_id: &str,
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    codecs: Vec<String>,
    max_fps: Option<u32>,
    monitor: Option<String>,
    max_bitrate: Option<u64>,
) -> anyhow::Result<()> {
    let wanted_mon = claims.mon.clone().or(monitor);
    let mut mon = app.hub.resolve(wanted_mon.as_deref())?;
    let mut cap = app.hub.capture(&mon)?;
    let monitors = app.hub.monitors();
    let input_sink = if claims.allows("input") {
        app.hub.backend.input()
    } else {
        None
    };
    let last_input_us = Arc::new(AtomicU64::new(0));
    let input = input_sink.map(|sink| {
        Arc::new(InputCtx {
            sink,
            mon: Mutex::new(mon.clone()),
            held: Mutex::new(HeldKeys::default()),
            last_input_us: last_input_us.clone(),
            closed: AtomicBool::new(false),
            close: tokio::sync::Notify::new(),
        })
    });
    let input_token = input.as_ref().map(|ctx| {
        let t = ticket::random_id();
        app.input_tokens
            .lock()
            .unwrap()
            .insert(t.clone(), ctx.clone());
        t
    });
    let clipboard = if claims.allows("clipboard") {
        app.hub.backend.clipboard()
    } else {
        None
    };
    let expires = clock::unix_secs() + claims.ses.unwrap_or(12 * 3600).min(ticket::MAX_SESSION);
    let welcome = ServerMsg::Welcome {
        server: concat!("skylight ", env!("CARGO_PKG_VERSION")),
        backend: app.hub.backend.name(),
        session: session_id,
        monitors: &monitors,
        monitor: &mon.id,
        scopes: &claims.scope,
        expires,
        clipboard: clipboard.is_some(),
        input: input.is_some(),
        input_token: input_token.as_deref(),
    };
    sink.send(text(proto::json(&welcome))).await?;

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let fps = max_fps
        .unwrap_or(app.config.max_fps)
        .clamp(1, app.config.max_fps);
    let max_bps = max_bitrate
        .unwrap_or(app.config.max_bitrate)
        .clamp(app.config.min_bitrate, app.config.max_bitrate);
    let shared = Arc::new(Shared {
        sig: Signal::new(),
        cmds: Mutex::new(VecDeque::new()),
        pacer: Mutex::new(Pacer::new(
            app.config.start_bitrate,
            app.config.min_bitrate,
            max_bps,
            fps,
        )),
        decode_us: Mutex::new((0, 0)),
    });
    let codec = pick_codec(app, &codecs);
    let enc_thread = {
        let (shared, out, app, cap) = (shared.clone(), out_tx.clone(), app.clone(), cap.clone());
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                encoder_loop(shared, cap, out, codec, fps, &app.config, &last_input_us)
            })?
    };

    let mut clip_rx = clipboard.as_ref().map(|c| {
        let mut rx = c.watch();
        rx.borrow_and_update();
        rx
    });
    let mut probe: Option<tokio::process::Child> = None;
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(expires.saturating_sub(clock::unix_secs()));
    let mut last_keyframe_req = Instant::now() - Duration::from_secs(1);

    let res: anyhow::Result<()> = loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break Ok(()),
                };
                match msg {
                    Message::Binary(b) => {
                        let Some(inp) = proto::parse_input(&b) else { continue };
                        if let Input::Ack { frame_id, decode_us } = inp {
                            let now = clock::now_us();
                            shared.pacer.lock().unwrap().on_ack(frame_id, now);
                            let mut d = shared.decode_us.lock().unwrap();
                            d.0 += decode_us as u64;
                            d.1 += 1;
                            shared.sig.raise(WAKE_COMMAND);
                        } else if let Some(ctx) = &input {
                            // Fallback when the input socket could not be opened.
                            ctx.handle(inp);
                        }
                    }
                    Message::Text(t) => {
                        let Ok(cm) = serde_json::from_str::<ClientMsg>(&t) else { continue };
                        match cm {
                            ClientMsg::Ping { id, c } => {
                                let _ = out_tx.send(text(proto::json(&ServerMsg::Pong { id, c, s: clock::now_us() })));
                            }
                            ClientMsg::Keyframe => {
                                if last_keyframe_req.elapsed() > Duration::from_millis(500) {
                                    last_keyframe_req = Instant::now();
                                    shared.send(Cmd::Keyframe);
                                }
                            }
                            ClientMsg::Settings { max_fps, max_bitrate, codec } => {
                                let codec = codec.and_then(|c| Codec::parse(&c)).filter(|c| app.encoders.iter().any(|(h, _)| h == c));
                                shared.send(Cmd::Settings {
                                    max_fps: max_fps.map(|f| f.clamp(1, app.config.max_fps)),
                                    max_bitrate: max_bitrate.map(|b| b.clamp(app.config.min_bitrate, app.config.max_bitrate)),
                                    codec,
                                });
                            }
                            ClientMsg::Monitor { id } => {
                                if claims.mon.as_ref().is_some_and(|m| *m != id) {
                                    continue;
                                }
                                match app.hub.resolve(Some(&id)).and_then(|m| Ok((app.hub.capture(&m)?, m))) {
                                    Ok((c, m)) => {
                                        if let Some(ctx) = &input {
                                            *ctx.mon.lock().unwrap() = m.clone();
                                        }
                                        mon = m;
                                        cap = c.clone();
                                        shared.send(Cmd::Switch(c));
                                    }
                                    Err(e) => {
                                        let _ = out_tx.send(text(proto::json(&ServerMsg::Error { message: &format!("{e:#}") })));
                                    }
                                }
                            }
                            ClientMsg::Clipboard { text: t } => {
                                if let Some(c) = &clipboard && t.len() <= 1 << 20 {
                                    c.set_text(t);
                                    if let Some(rx) = &mut clip_rx {
                                        rx.borrow_and_update();
                                    }
                                }
                            }
                            ClientMsg::Bench { action, mode } => {
                                if let Some(mut p) = probe.take() {
                                    let _ = p.kill().await;
                                }
                                if action == "start" {
                                    let mode = mode.as_deref().and_then(probe::Mode::parse).unwrap_or(probe::Mode::Move);
                                    let msg = if input.is_none() {
                                        ServerMsg::Bench { probe: None, error: Some("the latency probe needs the input scope".into()) }
                                    } else {
                                        match start_probe(&app.config, &mon, mode).await {
                                            Ok((child, rect)) => {
                                                probe = Some(child);
                                                ServerMsg::Bench { probe: Some(rect), error: None }
                                            }
                                            Err(e) => ServerMsg::Bench { probe: None, error: Some(format!("{e:#}")) },
                                        }
                                    };
                                    let _ = out_tx.send(text(proto::json(&msg)));
                                }
                            }
                            ClientMsg::Hello { .. } => {}
                        }
                    }
                    Message::Close(_) => break Ok(()),
                    _ => {}
                }
            }
            changed = async { match &mut clip_rx { Some(rx) => rx.changed().await.is_ok(), None => std::future::pending().await } } => {
                if !changed {
                    clip_rx = None;
                    continue;
                }
                let t = clip_rx.as_mut().unwrap().borrow_and_update().clone();
                if let Some(t) = t {
                    let _ = out_tx.send(text(proto::json(&ServerMsg::Clipboard { text: &t })));
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = out_tx.send(text(proto::json(&ServerMsg::Error { message: "session expired" })));
                break Ok(());
            }
        }
    };

    if let Some(ctx) = &input {
        ctx.shutdown();
    }
    if let Some(t) = &input_token {
        app.input_tokens.lock().unwrap().remove(t);
    }
    if let Some(mut p) = probe {
        let _ = p.kill().await;
    }
    shared.sig.raise(WAKE_STOP);
    let _ = tokio::task::spawn_blocking(move || enc_thread.join()).await;
    drop(cap);
    drop(out_tx);
    let _ = writer.await;
    res
}

#[derive(Default)]
struct HeldKeys {
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
}

impl HeldKeys {
    fn release_all(&mut self, sink: &dyn InputSink) {
        for k in self.keys.drain() {
            sink.key(k, false);
        }
        for b in self.buttons.drain() {
            sink.button(b, false);
        }
        sink.flush();
    }
}

/// Map a point in stream pixels to the compositor's global logical coordinates.
fn to_global(mon: &MonitorInfo, x: f32, y: f32) -> (f64, f64) {
    let x = (x as f64).clamp(0.0, mon.width.saturating_sub(1) as f64);
    let y = (y as f64).clamp(0.0, mon.height.saturating_sub(1) as f64);
    (
        mon.x as f64 + x * mon.logical_width as f64 / mon.width.max(1) as f64,
        mon.y as f64 + y * mon.logical_height as f64 / mon.height.max(1) as f64,
    )
}

fn handle_input(sink: &dyn InputSink, mon: &MonitorInfo, inp: Input, held: &mut HeldKeys) {
    match inp {
        Input::PointerAbs { x, y } => {
            let (gx, gy) = to_global(mon, x, y);
            sink.pointer_abs(gx, gy);
        }
        Input::PointerRel { dx, dy } => sink.pointer_rel(
            dx.clamp(-2000.0, 2000.0) as f64,
            dy.clamp(-2000.0, 2000.0) as f64,
        ),
        Input::Button { button, down } => {
            let b = match button {
                0 => capture::BTN_LEFT,
                1 => capture::BTN_MIDDLE,
                2 => capture::BTN_RIGHT,
                3 => capture::BTN_SIDE,
                4 => capture::BTN_EXTRA,
                _ => return,
            };
            if down {
                held.buttons.insert(b);
            } else {
                held.buttons.remove(&b);
            }
            sink.button(b, down);
        }
        Input::Scroll { dx, dy } => {
            sink.scroll(dx.clamp(-50.0, 50.0) as f64, dy.clamp(-50.0, 50.0) as f64)
        }
        Input::Key { code, down } => {
            let code = code as u32;
            if !input::key_allowed(code) {
                return;
            }
            if down {
                held.keys.insert(code);
            } else {
                held.keys.remove(&code);
            }
            sink.key(code, down);
        }
        Input::Touch { kind, id, x, y } => {
            let (gx, gy) = to_global(mon, x, y);
            sink.touch(kind, id as u32, gx, gy);
        }
        Input::Text(t) => {
            for c in t.chars().take(4096) {
                let Some((code, shift)) = input::us_key(c) else {
                    continue;
                };
                if shift {
                    sink.key(input::KEY_LEFTSHIFT, true);
                }
                sink.key(code, true);
                sink.key(code, false);
                if shift {
                    sink.key(input::KEY_LEFTSHIFT, false);
                }
            }
        }
        Input::Ack { .. } => {}
    }
    sink.flush();
}

async fn start_probe(
    cfg: &Config,
    mon: &MonitorInfo,
    mode: probe::Mode,
) -> anyhow::Result<(tokio::process::Child, [u32; 4])> {
    use tokio::io::AsyncBufReadExt;
    let size = if mode == probe::Mode::Animate {
        480u32
    } else {
        160
    };
    let exe = std::env::current_exe()?;
    let mode = match mode {
        probe::Mode::Move => "move",
        probe::Mode::Click => "click",
        probe::Mode::Key => "key",
        probe::Mode::Animate => "animate",
    };
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args([
        "probe-window",
        "--x",
        &(mon.x + 40).to_string(),
        "--y",
        &(mon.y + 40).to_string(),
        "--size",
        &size.to_string(),
        "--mode",
        mode,
    ]);
    if let Some(d) = &cfg.probe_display {
        cmd.env("DISPLAY", d);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no probe stdout"))?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .map_err(|_| {
            anyhow::anyhow!("the probe window did not appear (is DISPLAY set for Xwayland?)")
        })??
        .unwrap_or_default();
    let v: Vec<i64> = line
        .strip_prefix("READY ")
        .unwrap_or("")
        .split(' ')
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() != 4 {
        anyhow::bail!("unexpected probe output: {line}");
    }
    // Root coordinates to stream pixels.
    let s = mon.width as f64 / mon.logical_width.max(1) as f64;
    let rect = [
        ((v[0] - mon.x as i64).max(0) as f64 * s) as u32,
        ((v[1] - mon.y as i64).max(0) as f64 * s) as u32,
        (v[2] as f64 * s) as u32,
        (v[3] as f64 * s) as u32,
    ];
    Ok((child, rect))
}

// --- Encoder thread ----------------------------------------------------------------------------

/// Re-encode the last image at a higher bitrate once the screen has been still this long, so
/// text sharpens after scrolling or dragging stops.
const REFINE_AFTER: Duration = Duration::from_millis(120);
const REFINE_FRAMES: u8 = 2;

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
        }
    }
}

/// For this long after any input, frames go out as soon as they exist, ignoring the frame-rate
/// cap, so the frame that answers a click or key press never waits for a pacing slot.
const INPUT_FAST_PATH_US: u64 = 250_000;

fn encoder_loop(
    sh: Arc<Shared>,
    mut cap: Arc<Capture>,
    out: UnboundedSender<Message>,
    mut codec: Codec,
    mut fps: u32,
    cfg: &Config,
    last_input_us: &AtomicU64,
) {
    let mut store = cap.store.clone();
    store.subscribe(&sh.sig);
    let mut enc: Option<VideoEncoder> = None;
    let (mut last_seq, mut last_gen) = (0u64, u32::MAX);
    let mut frame_id = 0u32;
    let mut need_key = true;
    let mut last_encode = Instant::now() - Duration::from_secs(1);
    let mut last_content = Instant::now();
    let mut refine_left = 0u8;
    let mut cursor_serial = u32::MAX;
    let mut cursor_pos = None;
    let mut stats = Stats::new();
    let mut payload = Vec::with_capacity(1 << 20);
    let mut timeout = Duration::from_millis(250);
    let mut announced = None;
    let mut input_answered = 0u64;
    let mut last_rate_change = Instant::now();

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
                    cap = c;
                    store = cap.store.clone();
                    store.subscribe(&sh.sig);
                    enc = None;
                    last_seq = 0;
                    last_gen = u32::MAX;
                    cursor_serial = u32::MAX;
                    cursor_pos = None;
                }
            }
        }

        // Cursor: shape when it changes, position always (it is 12 bytes).
        {
            let st = store.lock();
            if let Some(err) = &st.error {
                let _ = out.send(text(proto::json(&ServerMsg::Error { message: err })));
                return;
            }
            if let Some(shape) = &st.cursor_shape
                && shape.serial != cursor_serial
            {
                cursor_serial = shape.serial;
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
            if cursor_pos != Some(st.cursor) {
                cursor_pos = Some(st.cursor);
                let m = proto::cursor_pos_message(st.cursor.x, st.cursor.y, st.cursor.visible);
                let _ = out.send(Message::Binary(m.into()));
            }
        }

        let now = Instant::now();
        let now_us = clock::now_us();
        {
            let mut p = sh.pacer.lock().unwrap();
            if let Some(b) = p.adjust(now_us)
                && let Some(e) = &mut enc
            {
                // On NVENC a change costs a keyframe, so only follow big moves, at most every 3 s.
                // On a fast link the first one lands on the cap a few seconds in.
                let ratio = b.max(e.bitrate()) as f64 / b.min(e.bitrate()).max(1) as f64;
                let rarely = last_rate_change.elapsed() >= Duration::from_secs(3) && ratio >= 1.5;
                if !e.bitrate_change_is_keyframe() || rarely {
                    e.set_bitrate(b);
                    last_rate_change = Instant::now();
                }
            }
        }
        if stats.since.elapsed() >= Duration::from_secs(1) {
            send_stats(&out, &sh, &mut stats, enc.as_ref(), now_us);
        }

        let (w, h, format, generation) = store.shape();
        if w == 0 {
            continue;
        }
        let seq = store.lock().seq;
        let new_content = seq != last_seq || generation != last_gen;
        let refine_due = cfg.refine
            && refine_left > 0
            && !new_content
            && now.duration_since(last_content) >= REFINE_AFTER;
        if !new_content && !need_key && !refine_due {
            if cfg.refine && refine_left > 0 {
                timeout = REFINE_AFTER
                    .saturating_sub(now.duration_since(last_content))
                    .max(Duration::from_millis(1));
            }
            continue;
        }
        let recent_input =
            now_us.saturating_sub(last_input_us.load(Ordering::Relaxed)) < INPUT_FAST_PATH_US;
        let interval = if recent_input && new_content {
            Duration::ZERO
        } else {
            Duration::from_micros(1_000_000 / fps.max(1) as u64)
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

        if enc
            .as_ref()
            .is_none_or(|e| e.width != w || e.height != h || e.codec != codec)
        {
            let bitrate = sh.pacer.lock().unwrap().target_bps;
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
                ..Default::default()
            };
            match VideoEncoder::open(&params) {
                Ok(e) => {
                    tracing::info!(encoder = %e.name, width = w, height = h, fps, "encoder open");
                    enc = Some(e);
                }
                Err(e) => {
                    let _ = out.send(text(proto::json(&ServerMsg::Error {
                        message: &format!("encoder: {e:#}"),
                    })));
                    return;
                }
            }
            need_key = true;
            last_seq = 0;
            last_gen = u32::MAX;
        }
        let e = enc.as_mut().unwrap();
        let mon_id = cap.monitor.id.clone();
        if announced != Some((e.codec, w, h, mon_id.clone())) {
            announced = Some((e.codec, w, h, mon_id.clone()));
            let codec_name = match e.codec {
                Codec::H264 => "h264",
                Codec::Hevc => "hevc",
                Codec::Av1 => "av1",
            };
            let m = ServerMsg::Stream {
                codec: codec_name,
                encoder: &e.name,
                hardware: e.hardware,
                width: w,
                height: h,
                fps,
                monitor: &mon_id,
            };
            let _ = out.send(text(proto::json(&m)));
        }

        let (buf, stride) = e.input();
        let r = store.sync_into(last_seq, last_gen, w, h, buf, stride);
        if r.size_mismatch {
            enc = None;
            timeout = Duration::from_millis(1);
            continue;
        }
        last_seq = r.seq;
        last_gen = r.generation;
        // A refinement re-encodes the unchanged image. Under the constant-quality target that
        // lifts a frame that the VBV cap held back (the first frame of a big change) up to the
        // target, and costs almost nothing when it is already there.
        let refine = r.damage.is_empty() && !need_key;
        let t0 = Instant::now();
        // Pace from the start of each encode, so the cap is a real cap and not cap + encode time.
        last_encode = t0;
        let queue_us = clock::now_us().saturating_sub(r.capture_us);
        payload.clear();
        let key = match e.encode(need_key, &mut payload) {
            Ok(k) => k,
            Err(err) => {
                tracing::error!("encode failed: {err:#}");
                enc = None;
                continue;
            }
        };
        let encode_us = t0.elapsed().as_micros() as u64;
        if refine {
            refine_left -= 1;
            stats.refines += 1;
        } else {
            refine_left = REFINE_FRAMES;
            last_content = Instant::now();
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
            .on_send(frame_id, msg.len(), clock::now_us());
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
    }
}

fn send_stats(
    out: &UnboundedSender<Message>,
    sh: &Shared,
    s: &mut Stats,
    enc: Option<&VideoEncoder>,
    now_us: u64,
) {
    let secs = s.since.elapsed().as_secs_f64();
    let content = (s.frames - s.refines).max(1) as f64;
    let (dec_sum, dec_n) = std::mem::take(&mut *sh.decode_us.lock().unwrap());
    let p = sh.pacer.lock().unwrap();
    let v = serde_json::json!({
        "t": "stats",
        "fps": s.frames as f64 / secs,
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
        "decodeMs": if dec_n > 0 { dec_sum as f64 / dec_n as f64 / 1000.0 } else { 0.0 },
        "encoder": enc.map(|e| e.name.as_str()).unwrap_or(""),
    });
    drop(p);
    let _ = out.send(text(v.to_string()));
    *s = Stats::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stream_pixels_to_logical_coordinates() {
        // A 2x HiDPI monitor to the right of another one.
        let m = MonitorInfo {
            id: "DP-1".into(),
            name: String::new(),
            x: 1920,
            y: 0,
            logical_width: 1280,
            logical_height: 720,
            width: 2560,
            height: 1440,
            refresh_mhz: 60_000,
            scale: 2.0,
        };
        assert_eq!(to_global(&m, 0.0, 0.0), (1920.0, 0.0));
        assert_eq!(to_global(&m, 1280.0, 720.0), (2560.0, 360.0));
        // Out-of-range points are clamped to the monitor.
        let (x, y) = to_global(&m, 99999.0, -5.0);
        assert!(x < 1920.0 + 1280.0 && y == 0.0);
    }
}
