//! Whole sessions, end to end: the real server on a loopback port, a fake backend (see
//! [`crate::capture::fake`]) and WebSocket clients speaking the protocol.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::capture::fake::FakeBackend;
use crate::capture::{CLIPBOARD_MAX, Hub};
use crate::protocol::{FLAG_KEY, IN_KEY, MSG_VIDEO};
use crate::session::{App, Config};
use crate::ticket::{self, Claims};
use crate::{clock, encode};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    url: String,
    key: ed25519_dalek::SigningKey,
    backend: Arc<FakeBackend>,
}

async fn server(max_sessions: usize) -> Server {
    let key = ticket::generate();
    let backend = Arc::new(FakeBackend::new());
    let app = Arc::new(App::new(
        Hub::new(backend.clone()),
        ticket::Verifier::new(vec![key.verifying_key()], "test".into()),
        Config {
            max_fps: 30,
            remote_max_fps: 0,
            max_bitrate: 8_000_000,
            start_bitrate: 4_000_000,
            min_bitrate: 500_000,
            encoder: encode::Backend::Software,
            nvenc_preset: 2,
            vbv_ms: 34,
            cq: Some(23),
            refine: true,
            allowed_origins: Vec::new(),
            max_sessions,
            probe_display: None,
        },
        encode::probe(&[encode::Codec::H264]),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(crate::server::serve_on(listener, app, None));
    Server { url, key, backend }
}

impl Server {
    fn ticket(&self, scope: &[&str], mon: Option<&str>) -> String {
        let now = clock::unix_secs();
        ticket::mint(
            &self.key,
            &Claims {
                v: 1,
                aud: "test".into(),
                sub: "tester".into(),
                iat: now,
                exp: now + 60,
                jti: ticket::random_id(),
                scope: scope.iter().map(|s| s.to_string()).collect(),
                mon: mon.map(Into::into),
                ses: None,
            },
        )
    }

    async fn socket(&self, path: &str) -> Ws {
        tokio_tungstenite::connect_async(format!("{}{path}", self.url))
            .await
            .unwrap()
            .0
    }

    /// A session with `ticket`, and its `welcome` (or the error that came instead).
    async fn connect(&self, ticket: &str) -> (Ws, Value) {
        let mut ws = self.socket("/ws").await;
        send(
            &mut ws,
            json!({"t": "hello", "ticket": ticket, "codecs": ["h264"]}),
        )
        .await;
        let first = next_json(&mut ws, |_| true)
            .await
            .expect("an answer to hello");
        (ws, first)
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

/// The next JSON message that `want` accepts, skipping the rest (video, cursor, stats); `None`
/// once the socket closes or nothing comes for 5 s.
async fn next_json(ws: &mut Ws, want: impl Fn(&Value) -> bool) -> Option<Value> {
    next(ws, |m| match m {
        Message::Text(t) => serde_json::from_str(t).ok().filter(|v| want(v)),
        _ => None,
    })
    .await
}

async fn next<T>(ws: &mut Ws, want: impl Fn(&Message) -> Option<T>) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(m)) = ws.next().await {
            if let Some(v) = want(&m) {
                return Some(v);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

fn is(t: &'static str) -> impl Fn(&Value) -> bool {
    move |v| v["t"] == t
}

/// The socket closes (or says nothing more) within 5 s.
async fn closes(ws: &mut Ws) -> bool {
    next(ws, |m| matches!(m, Message::Close(_)).then_some(())).await;
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .is_ok_and(|m| !matches!(m, Some(Ok(_))))
}

fn key(code: u16, down: bool) -> Message {
    let c = code.to_le_bytes();
    Message::binary(vec![IN_KEY, down as u8, c[0], c[1]])
}

/// Wait until the backend recorded `event`.
async fn recorded(s: &Server, event: &str) -> bool {
    for _ in 0..100 {
        if s.backend.input.events().iter().any(|e| e == event) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_session_streams_a_keyframe_first() {
    let s = server(4).await;
    let (mut ws, welcome) = s.connect(&s.ticket(&["view"], None)).await;
    assert_eq!(welcome["t"], "welcome", "{welcome}");
    assert_eq!(welcome["input"], false);
    assert_eq!(welcome["clipboardMax"], CLIPBOARD_MAX);
    let stream = next_json(&mut ws, is("stream")).await.unwrap();
    assert_eq!(stream["codec"], "h264");
    let flags = next(&mut ws, |m| match m {
        Message::Binary(b) if b[0] == MSG_VIDEO => Some(b[1]),
        _ => None,
    })
    .await
    .unwrap();
    assert!(flags & FLAG_KEY != 0);
}

#[tokio::test]
async fn a_ticket_works_once() {
    let s = server(4).await;
    let t = s.ticket(&["view"], None);
    let (_ws, welcome) = s.connect(&t).await;
    assert_eq!(welcome["t"], "welcome");
    let (_ws, again) = s.connect(&t).await;
    assert_eq!(again["t"], "error");
    assert!(again["message"].as_str().unwrap().contains("unauthorized"));
}

#[tokio::test]
async fn sessions_give_their_slot_back() {
    let s = server(1).await;
    let (ws, welcome) = s.connect(&s.ticket(&["view"], None)).await;
    assert_eq!(welcome["t"], "welcome");
    let (_, full) = s.connect(&s.ticket(&["view"], None)).await;
    assert_eq!(full["message"], "too many sessions");
    drop(ws);
    let mut ok = false;
    for _ in 0..50 {
        if s.connect(&s.ticket(&["view"], None)).await.1["t"] == "welcome" {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "the slot came back once the first session closed");
}

#[tokio::test]
async fn view_only_sessions_inject_nothing() {
    let s = server(4).await;
    let (mut ws, welcome) = s.connect(&s.ticket(&["view"], None)).await;
    assert_eq!(welcome["input"], false);
    ws.send(key(30, true)).await.unwrap();
    send(&mut ws, json!({"t": "ping", "id": 1, "c": 0.0})).await;
    next_json(&mut ws, is("pong")).await.unwrap();
    assert!(s.backend.input.events().is_empty());
}

#[tokio::test]
async fn keys_held_on_the_input_socket_are_released_when_the_session_ends() {
    let s = server(4).await;
    let (ws, welcome) = s.connect(&s.ticket(&["view", "input"], None)).await;
    let token = welcome["inputToken"].as_str().unwrap();
    let mut input = s.socket("/input").await;
    send(&mut input, json!({ "token": token })).await;
    input.send(key(29, true)).await.unwrap();
    assert!(recorded(&s, "key 29 down").await);
    drop(ws);
    assert!(recorded(&s, "key 29 up").await);
    // The input socket closes with its session, and its token is spent.
    assert!(closes(&mut input).await);
    let mut again = s.socket("/input").await;
    send(&mut again, json!({ "token": token })).await;
    assert!(closes(&mut again).await);
}

#[tokio::test]
async fn paste_sets_the_clipboard_then_presses_ctrl_v() {
    let s = server(4).await;
    let (mut ws, _) = s
        .connect(&s.ticket(&["view", "input", "clipboard"], None))
        .await;
    send(
        &mut ws,
        json!({"t": "clipboard", "text": "hello", "paste": true}),
    )
    .await;
    assert!(recorded(&s, "key 47 up").await);
    assert_eq!(*s.backend.clipboard.text.lock().unwrap(), ["hello"]);
    assert_eq!(
        s.backend.input.events(),
        ["key 29 down", "key 47 down", "key 47 up", "key 29 up"]
    );
}

#[tokio::test]
async fn clipboard_text_over_the_limit_is_refused_and_the_session_goes_on() {
    let s = server(4).await;
    let (mut ws, _) = s
        .connect(&s.ticket(&["view", "input", "clipboard"], None))
        .await;
    let big = "x".repeat(CLIPBOARD_MAX + 1);
    send(
        &mut ws,
        json!({"t": "clipboard", "text": big, "paste": true}),
    )
    .await;
    let e = next_json(&mut ws, is("error")).await.unwrap();
    assert_eq!(e["message"], "clipboard text too large");
    send(&mut ws, json!({"t": "ping", "id": 1, "c": 0.0})).await;
    assert!(next_json(&mut ws, is("pong")).await.is_some());
    assert!(s.backend.clipboard.text.lock().unwrap().is_empty());
    assert!(s.backend.input.events().is_empty());
}

#[tokio::test]
async fn a_session_pinned_to_a_monitor_that_goes_away_ends() {
    let s = server(4).await;
    let (mut ws, welcome) = s.connect(&s.ticket(&["view"], Some("B"))).await;
    assert_eq!(welcome["monitor"], "B");
    next_json(&mut ws, is("stream")).await.unwrap();
    s.backend.unplug("B");
    // It never shows monitor A instead: it says why, and closes.
    let e = next_json(&mut ws, |v| v["t"] == "error" || v["t"] == "stream")
        .await
        .unwrap();
    assert_eq!(e["t"], "error", "{e}");
    assert!(closes(&mut ws).await);
}

#[tokio::test]
async fn an_unpinned_session_moves_to_another_monitor() {
    let s = server(4).await;
    let (mut ws, welcome) = s.connect(&s.ticket(&["view"], None)).await;
    assert_eq!(welcome["monitor"], "A");
    next_json(&mut ws, is("stream")).await.unwrap();
    s.backend.unplug("A");
    let stream = next_json(&mut ws, is("stream")).await.unwrap();
    assert_eq!(stream["monitor"], "B");
}
