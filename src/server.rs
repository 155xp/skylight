//! HTTP side: the WebSocket endpoint plus the static viewer and embeddable client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::agent;
use crate::session::{self, App};

struct Web {
    app: Arc<App>,
    dir: Option<PathBuf>,
}

const ASSETS: &[(&str, &str, &str)] = &[
    (
        "index.html",
        "text/html; charset=utf-8",
        include_str!("../web/index.html"),
    ),
    (
        "skylight.js",
        "text/javascript; charset=utf-8",
        include_str!("../web/skylight.js"),
    ),
    (
        "viewer.js",
        "text/javascript; charset=utf-8",
        include_str!("../web/viewer.js"),
    ),
    (
        "viewer.css",
        "text/css; charset=utf-8",
        include_str!("../web/viewer.css"),
    ),
];

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; \
    connect-src 'self' ws: wss:; worker-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

fn harden(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    r
}

async fn asset(State(web): State<Arc<Web>>, path: Option<Path<String>>) -> Response {
    let name = path.map(|p| p.0).unwrap_or_else(|| "index.html".into());
    let Some((_, ctype, builtin)) = ASSETS.iter().find(|(n, _, _)| *n == name) else {
        return harden(StatusCode::NOT_FOUND.into_response());
    };
    let body = match &web.dir {
        // Development: serve the files from disk so edits show up on reload.
        Some(dir) => tokio::fs::read_to_string(dir.join(&name))
            .await
            .unwrap_or_else(|_| builtin.to_string()),
        None => builtin.to_string(),
    };
    harden(([(header::CONTENT_TYPE, *ctype)], body).into_response())
}

fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
        // Non-browser clients send no Origin; they still need a ticket.
        return true;
    };
    if allowed.iter().any(|a| a == origin || a == "*") {
        return true;
    }
    // Same origin as the request (the built-in viewer).
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    origin.split_once("://").map(|(_, rest)| rest) == Some(host)
}

async fn ws(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&headers, &web.app.config.allowed_origins) {
        tracing::warn!(origin = ?headers.get(header::ORIGIN), "websocket from a disallowed origin");
        return StatusCode::FORBIDDEN.into_response();
    }
    let app = web.app.clone();
    upgrade
        .max_message_size(1 << 21)
        .write_buffer_size(0)
        .on_upgrade(move |socket| session::run(socket, app))
}

/// The input channel: its own socket, so a click never waits behind anything else.
async fn input_ws(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&headers, &web.app.config.allowed_origins) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let app = web.app.clone();
    upgrade
        .max_message_size(1 << 16)
        .write_buffer_size(0)
        .on_upgrade(move |socket| session::run_input(socket, app))
}

async fn agent_screenshot(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    q: Query<agent::ShotQuery>,
) -> Response {
    agent::screenshot(State(web.app.clone()), headers, q).await
}

async fn agent_wait(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    q: Query<agent::WaitQuery>,
) -> Response {
    agent::wait(State(web.app.clone()), headers, q).await
}

pub async fn serve(app: Arc<App>, listen: SocketAddr, dir: Option<PathBuf>) -> Result<()> {
    if !listen.ip().is_loopback() {
        bail!(
            "refusing to listen on {listen}: skylight only binds to loopback and expects an authenticating proxy in front"
        );
    }
    let web = Arc::new(Web { app, dir });
    let router = Router::new()
        .route("/", get(asset))
        .route("/{file}", get(asset))
        .route("/ws", get(ws))
        .route("/input", get(input_ws))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/screenshot", get(agent_screenshot))
        .route("/api/wait", get(agent_wait))
        .with_state(web);
    use axum::serve::ListenerExt;
    // Small writes (input, acks, the tail of a frame) must never wait for Nagle.
    let listener = tokio::net::TcpListener::bind(listen).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    tracing::info!("listening on http://{listen}");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        if let Some(o) = origin {
            m.insert(header::ORIGIN, o.parse().unwrap());
        }
        m.insert(header::HOST, host.parse().unwrap());
        m
    }

    #[test]
    fn origin_rules() {
        let allowed = vec!["https://app.example.com".to_string()];
        assert!(origin_allowed(
            &h(Some("https://app.example.com"), "desk.example.com"),
            &allowed
        ));
        assert!(origin_allowed(
            &h(Some("http://127.0.0.1:5490"), "127.0.0.1:5490"),
            &allowed
        ));
        assert!(!origin_allowed(
            &h(Some("https://evil.example"), "desk.example.com"),
            &allowed
        ));
        assert!(origin_allowed(&h(None, "127.0.0.1:5490"), &allowed));
    }
}
