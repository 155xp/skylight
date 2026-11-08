//! One browser connection: admission, authentication, and the session's two sockets (control and
//! video on `/ws`, input on `/input`). The picture itself comes from its encoder thread
//! ([`crate::stream`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{FutureExt, SinkExt, StreamExt};

use crate::capture::{CLIPBOARD_MAX, Hub};
use crate::clock;
use crate::encode::{self, Codec};
use crate::frame::WAKE_STOP;
use crate::input::InputCtx;
use crate::pacer::Pacer;
use crate::probe;
use crate::protocol::{self as proto, ClientMsg, Hello, Input, ServerMsg};
use crate::stream::{self, Cmd, Shared};
use crate::ticket::{self, Claims, Verifier};

pub struct Config {
    pub max_fps: u32,
    /// Frame-rate cap for viewers whose best round trip is over `stream::REMOTE_RTT_US`; 0 for
    /// none.
    pub remote_max_fps: u32,
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

/// Sockets, on either endpoint, that have not shown a ticket or an input token yet.
const MAX_PENDING: usize = 32;
/// How long a new socket has to show one.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Input sockets one session may have attached at a time.
const MAX_INPUT_SOCKETS: usize = 4;

/// A viewer pings every second, so this much silence means the connection is gone without having
/// closed (a tunnel or NAT that stopped forwarding). Through a tunnel or a proxy the server's TCP
/// peer is a local process, so TCP itself never notices. Generous, because a background tab's
/// timers may run only once a minute.
const SILENT_VIEWER: Duration = Duration::from_secs(90);

/// A socket that takes no data for this long (a viewer that stopped reading) is closed, so what
/// is queued for it cannot grow without end; its session then ends.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

pub struct App {
    pub hub: Hub,
    pub verifier: Verifier,
    pub config: Config,
    pub encoders: Vec<(Codec, String)>,
    sessions: AtomicUsize,
    pending: AtomicUsize,
    /// Input sockets waiting to attach, by one-time token.
    input_tokens: Mutex<HashMap<String, Arc<InputCtx>>>,
}

impl App {
    pub fn new(
        hub: Hub,
        verifier: Verifier,
        config: Config,
        encoders: Vec<(Codec, String)>,
    ) -> App {
        App {
            hub,
            verifier,
            config,
            encoders,
            sessions: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            input_tokens: Mutex::new(HashMap::new()),
        }
    }

    /// The first codec in `wanted` that this server can encode.
    fn pick_codec<'a>(&self, wanted: impl IntoIterator<Item = &'a str>) -> Option<Codec> {
        wanted
            .into_iter()
            .filter_map(Codec::parse)
            .find(|c| self.encoders.iter().any(|(have, _)| have == c))
    }
}

/// One place under a cap, given back however its holder ends.
struct Slot<'a>(&'a AtomicUsize);

impl<'a> Slot<'a> {
    fn take(count: &'a AtomicUsize, cap: usize) -> Option<Slot<'a>> {
        let slot = Slot(count);
        (count.fetch_add(1, Ordering::SeqCst) < cap).then_some(slot)
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The first text message on a new socket, within [`HELLO_TIMEOUT`].
async fn first_text(stream: &mut futures_util::stream::SplitStream<WebSocket>) -> Option<String> {
    match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => Some(t.to_string()),
        _ => None,
    }
}

pub fn text(s: String) -> Message {
    Message::Text(s.into())
}

fn error(message: &str) -> Message {
    text(proto::json(&ServerMsg::Error { message }))
}

/// The input socket: authenticated by a one-time token (from `welcome`, or a fresh one from an
/// `input-token` request after a socket dropped), then every message is one input event, injected
/// the moment it arrives.
pub async fn run_input(socket: WebSocket, app: Arc<App>) {
    let (_sink, mut stream) = socket.split();
    let ctx = {
        let Some(_pending) = Slot::take(&app.pending, MAX_PENDING) else {
            return;
        };
        let token = first_text(&mut stream)
            .await
            .and_then(|t| serde_json::from_str::<proto::Attach>(&t).ok());
        match token.and_then(|t| app.input_tokens.lock().unwrap().remove(&t.token)) {
            Some(ctx) => ctx,
            None => return,
        }
    };
    let Some(_attached) = Slot::take(&ctx.sockets, MAX_INPUT_SOCKETS) else {
        return;
    };
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    // Take whatever else has already arrived too, and inject it as one batch.
                    let mut batch = Vec::new();
                    let mut closed = false;
                    let mut next = Some(b);
                    while let Some(b) = next.take() {
                        if let Some(inp) = proto::parse_input(&b)
                            && !matches!(inp, Input::Ack { .. })
                        {
                            batch.push(inp);
                        }
                        if batch.len() >= 256 {
                            ctx.handle_batch(std::mem::take(&mut batch));
                        }
                        match stream.next().now_or_never() {
                            Some(Some(Ok(Message::Binary(b)))) => next = Some(b),
                            Some(Some(Ok(Message::Close(_)))) | Some(None) | Some(Some(Err(_))) => closed = true,
                            // Nothing waiting (or a frame this socket ignores anyway).
                            _ => {}
                        }
                    }
                    ctx.handle_batch(batch);
                    if closed {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = ctx.closed() => break,
        }
    }
}

/// Run a WebSocket session to completion.
pub async fn run(socket: WebSocket, app: Arc<App>) {
    let (mut sink, mut stream) = socket.split();
    let hello = {
        let Some(_pending) = Slot::take(&app.pending, MAX_PENDING) else {
            return;
        };
        first_text(&mut stream)
            .await
            .and_then(|t| serde_json::from_str::<ClientMsg>(&t).ok())
    };
    let Some(ClientMsg::Hello(hello)) = hello else {
        let _ = sink.send(error("expected hello")).await;
        return;
    };
    let claims = match app.verifier.verify(&hello.ticket, clock::unix_secs()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("rejected ticket: {e}");
            let _ = sink.send(error(&format!("unauthorized: {e}"))).await;
            return;
        }
    };
    let Some(_session) = Slot::take(&app.sessions, app.config.max_sessions) else {
        let _ = sink.send(error("too many sessions")).await;
        return;
    };
    let session_id = ticket::random_id()[..8].to_string();
    tracing::info!(session = %session_id, sub = %claims.sub, scope = ?claims.scope, "session start");
    let started = Instant::now();
    let result = serve(&app, &claims, &session_id, sink, stream, hello).await;
    tracing::info!(session = %session_id, secs = started.elapsed().as_secs(), result = ?result.err().map(|e| e.to_string()), "session end");
}

/// A session's input and the one-time token that attaches its input socket. Dropped, however
/// the session ends, it releases everything held and the token stops working.
struct SessionInput<'a> {
    app: &'a App,
    ctx: Arc<InputCtx>,
    token: String,
}

impl<'a> SessionInput<'a> {
    fn new(app: &'a App, ctx: InputCtx) -> SessionInput<'a> {
        let mut s = SessionInput {
            app,
            ctx: Arc::new(ctx),
            token: String::new(),
        };
        s.renew();
        s
    }

    /// A fresh token in place of the last one, spent or not.
    fn renew(&mut self) -> &str {
        let t = ticket::random_id();
        let mut tokens = self.app.input_tokens.lock().unwrap();
        tokens.remove(&self.token);
        tokens.insert(t.clone(), self.ctx.clone());
        self.token = t;
        &self.token
    }
}

impl Drop for SessionInput<'_> {
    fn drop(&mut self) {
        self.app.input_tokens.lock().unwrap().remove(&self.token);
        self.ctx.shutdown();
    }
}

async fn serve(
    app: &Arc<App>,
    claims: &Claims,
    session_id: &str,
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    hello: Hello,
) -> anyhow::Result<()> {
    // A ticket that names a monitor gets that one; otherwise the one asked for, while it exists.
    let mon = match &claims.mon {
        Some(id) => app.hub.resolve(Some(id))?,
        None => app
            .hub
            .resolve(hello.monitor.as_deref())
            .or_else(|_| app.hub.resolve(None))?,
    };
    let cap = app.hub.capture(&mon)?;
    let monitors = app.hub.monitors();

    let fps = hello
        .max_fps
        .unwrap_or(app.config.max_fps)
        .clamp(1, app.config.max_fps);
    let max_bps = hello
        .max_bitrate
        .unwrap_or(app.config.max_bitrate)
        .clamp(app.config.min_bitrate, app.config.max_bitrate);
    let shared = Arc::new(Shared::new(
        Pacer::new(
            hello.start_bitrate.unwrap_or(app.config.start_bitrate),
            app.config.min_bitrate,
            max_bps,
            fps,
        ),
        mon.clone(),
    ));

    let mut input = claims
        .allows("input")
        .then(|| app.hub.backend.input())
        .flatten()
        .map(|sink| {
            let ctx = InputCtx::new(sink, shared.mon.clone(), shared.activity.clone());
            SessionInput::new(app, ctx)
        });
    let clipboard = claims
        .allows("clipboard")
        .then(|| app.hub.backend.clipboard())
        .flatten();
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
        clipboard_max: CLIPBOARD_MAX,
        input: input.is_some(),
        input_token: input.as_ref().map(|i| i.token.as_str()),
    };
    sink.send(text(proto::json(&welcome))).await?;

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, sink.send(m)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // The encoder thread owns the session's only reference to its capture, so a capture it
    // replaces (see `Hub::reopen`) closes as soon as the last session lets go of it. When it ends
    // (it gave up, or the socket closed), so does the session.
    let (enc_done_tx, mut enc_done) = tokio::sync::oneshot::channel::<()>();
    let enc_thread = {
        let (shared, out, app) = (shared.clone(), out_tx.clone(), app.clone());
        let codec = app
            .pick_codec(hello.codecs.iter().map(String::as_str))
            .unwrap_or(Codec::H264);
        let pinned = claims.mon.is_some();
        std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                let _done = enc_done_tx;
                stream::encoder_loop(shared, cap, out, codec, fps, &app, pinned)
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
    let mut heard = tokio::time::Instant::now();

    let res: anyhow::Result<()> = loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break Ok(()),
                };
                heard = tokio::time::Instant::now();
                match msg {
                    Message::Binary(b) => match proto::parse_input(&b) {
                        Some(Input::Ack { frame_id, decode_us }) => shared.on_ack(frame_id, decode_us),
                        // Fallback when the input socket could not be opened.
                        Some(inp) => if let Some(i) = &input {
                            i.ctx.handle_batch(vec![inp]);
                        },
                        None => {}
                    },
                    Message::Text(t) => {
                        let Ok(cm) = serde_json::from_str::<ClientMsg>(&t) else { continue };
                        match cm {
                            ClientMsg::Ping { id, c } => {
                                let _ = out_tx.send(text(proto::json(&ServerMsg::Pong { id, c, s: clock::now_us() })));
                            }
                            ClientMsg::Keyframe => {
                                if last_keyframe_req.elapsed() > Duration::from_millis(500) {
                                    last_keyframe_req = Instant::now();
                                    // The viewer dropped its decoder: nothing in flight will be
                                    // acknowledged, so do not wait for it.
                                    shared.pacer.lock().unwrap().forget_inflight();
                                    shared.send(Cmd::Keyframe);
                                }
                            }
                            ClientMsg::Settings { max_fps, max_bitrate, codec } => {
                                shared.send(Cmd::Settings {
                                    max_fps: max_fps.map(|f| f.clamp(1, app.config.max_fps)),
                                    max_bitrate: max_bitrate.map(|b| b.clamp(app.config.min_bitrate, app.config.max_bitrate)),
                                    codec: codec.as_deref().and_then(|c| app.pick_codec([c])),
                                });
                            }
                            ClientMsg::Monitor { id } => {
                                if claims.mon.as_ref().is_some_and(|m| *m != id) {
                                    continue;
                                }
                                match app.hub.resolve(Some(&id)).and_then(|m| app.hub.capture(&m)) {
                                    Ok(c) => shared.send(Cmd::Switch(c)),
                                    Err(e) => {
                                        let _ = out_tx.send(error(&format!("{e:#}")));
                                    }
                                }
                            }
                            ClientMsg::Clipboard { text: t, paste } => {
                                let Some(c) = &clipboard else { continue };
                                if t.len() > CLIPBOARD_MAX {
                                    let _ = out_tx.send(error("clipboard text too large"));
                                    continue;
                                }
                                c.set_text(t);
                                if let Some(rx) = &mut clip_rx {
                                    rx.borrow_and_update();
                                }
                                if paste && let Some(i) = &input {
                                    i.ctx.paste();
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
                                        let mon = shared.mon.lock().unwrap().clone();
                                        match probe::launch(app.config.probe_display.as_deref(), &mon, mode).await {
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
                            ClientMsg::InputToken => {
                                // The input socket closed (tunnels drop one now and then): a
                                // fresh token lets another one attach. Input travels on this
                                // socket meanwhile.
                                if let Some(i) = &mut input {
                                    let token = i.renew();
                                    let _ = out_tx.send(text(proto::json(&ServerMsg::InputToken { token })));
                                }
                            }
                            ClientMsg::Hello(_) => {}
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
            _ = &mut enc_done => break Err(anyhow::anyhow!("the stream ended")),
            _ = tokio::time::sleep_until(deadline) => {
                let _ = out_tx.send(error("session expired"));
                break Ok(());
            }
            _ = tokio::time::sleep_until(heard + SILENT_VIEWER) => {
                // Its slot, its encoder and its capture are needed by whoever reconnects.
                break Err(anyhow::anyhow!("the viewer went silent"));
            }
        }
    };

    drop(input);
    drop(probe);
    shared.sig.raise(WAKE_STOP);
    let _ = tokio::task::spawn_blocking(move || enc_thread.join()).await;
    drop(out_tx);
    let _ = writer.await;
    res
}
