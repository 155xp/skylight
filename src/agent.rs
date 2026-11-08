//! HTTP endpoints for programs that drive the desktop (computer-use agents, tests, scripts):
//!
//! * `GET /api/screenshot[?monitor=ID]`: the current image as PNG. `X-Skylight-Seq` carries the
//!   frame store's sequence number, to pass to `/api/wait?since=`.
//! * `GET /api/wait[?idle=300][&change][&region=x,y,w,h][&since=SEQ][&timeout=10000][&monitor=ID]`:
//!   returns once the screen (or the region) has been still for `idle` ms, or with `change`, at
//!   the first change after `since` (default: when the request arrived). Answered from the
//!   capture's damage tracking: no polling, no screenshots.
//!
//! Both take a ticket like a viewer does, as `Authorization: Bearer <ticket>`, and need the
//! `view` scope (and the ticket's `mon`, if it names one). Tickets are single use, so mint one
//! per request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::capture::Capture;
use crate::clock;
use crate::frame::{PixelFormat, Rect, Signal};
use crate::session::App;
use crate::ticket::Claims;

/// A refusal: status and message, turned into a response at the end.
type Reject = (StatusCode, String);

fn reject(code: StatusCode, msg: impl Into<String>) -> Reject {
    (code, msg.into())
}

fn authorize(app: &App, headers: &HeaderMap) -> Result<Claims, Reject> {
    let ticket = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            reject(
                StatusCode::UNAUTHORIZED,
                "a ticket is required (Authorization: Bearer)",
            )
        })?;
    let claims = app
        .verifier
        .verify(ticket.trim(), clock::unix_secs())
        .map_err(|e| {
            tracing::warn!("rejected agent ticket: {e}");
            reject(StatusCode::UNAUTHORIZED, format!("unauthorized: {e}"))
        })?;
    if !claims.allows("view") {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "the ticket lacks the view scope",
        ));
    }
    Ok(claims)
}

/// The capture for the requested monitor, with its first image in the store.
fn open_capture(app: &App, claims: &Claims, monitor: Option<&str>) -> Result<Arc<Capture>, Reject> {
    if let (Some(m), Some(want)) = (&claims.mon, monitor)
        && m != want
    {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "the ticket is for another monitor",
        ));
    }
    let bad = |e: anyhow::Error| reject(StatusCode::NOT_FOUND, format!("{e:#}"));
    let mon = app
        .hub
        .resolve(claims.mon.as_deref().or(monitor))
        .map_err(bad)?;
    let cap = app.hub.capture(&mon).map_err(bad)?;
    let sig = Signal::new();
    cap.store.subscribe(&sig);
    let deadline = Instant::now() + Duration::from_secs(5);
    while cap.store.shape().0 == 0 {
        if Instant::now() > deadline {
            return Err(reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "no image from the capture yet",
            ));
        }
        sig.wait(Duration::from_millis(50));
    }
    Ok(cap)
}

/// Bring changes an on-demand source is holding into the store, waiting at most `max`.
fn pull_pending(cap: &Capture, sig: &Signal, max: Duration) {
    let deadline = Instant::now() + max;
    while cap.store.pending() && Instant::now() < deadline {
        cap.store.request_frame();
        sig.wait(Duration::from_millis(10));
    }
}

#[derive(Deserialize)]
pub struct ShotQuery {
    monitor: Option<String>,
}

pub async fn screenshot(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<ShotQuery>,
) -> Response {
    let claims = match authorize(&app, &headers) {
        Ok(c) => c,
        Err(r) => return r.into_response(),
    };
    let res = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, u64), Reject> {
        let cap = open_capture(&app, &claims, q.monitor.as_deref())?;
        let sig = Signal::new();
        cap.store.subscribe(&sig);
        pull_pending(&cap, &sig, Duration::from_millis(200));
        let (w, h, format, px, seq) = cap
            .store
            .snapshot()
            .ok_or_else(|| reject(StatusCode::SERVICE_UNAVAILABLE, "no image"))?;
        let png = png(&px, w, h, format)
            .map_err(|e| reject(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        Ok((png, seq))
    })
    .await;
    match res {
        Ok(Ok((png, seq))) => {
            let mut r = ([(header::CONTENT_TYPE, "image/png")], png).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r.headers_mut()
                .insert("x-skylight-seq", HeaderValue::from(seq));
            r
        }
        Ok(Err(r)) => r.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
pub struct WaitQuery {
    idle: Option<u64>,
    timeout: Option<u64>,
    change: Option<String>,
    region: Option<String>,
    since: Option<u64>,
    monitor: Option<String>,
}

fn parse_region(s: &str) -> Option<Rect> {
    let v: Vec<u32> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (v.len() == 4
        && v[2] > 0
        && v[3] > 0
        && v[0].checked_add(v[2]).is_some()
        && v[1].checked_add(v[3]).is_some())
    .then(|| Rect::new(v[0], v[1], v[2], v[3]))
}

pub async fn wait(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<WaitQuery>,
) -> Response {
    let claims = match authorize(&app, &headers) {
        Ok(c) => c,
        Err(r) => return r.into_response(),
    };
    let region = match q.region.as_deref().map(parse_region) {
        Some(None) => return (StatusCode::BAD_REQUEST, "region is x,y,w,h").into_response(),
        Some(r) => r,
        None => None,
    };
    let idle_us = q.idle.unwrap_or(300).clamp(10, 60_000) * 1000;
    let timeout = Duration::from_millis(q.timeout.unwrap_or(10_000).clamp(10, 120_000));
    let change = q.change.is_some();
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, Reject> {
        let cap = open_capture(&app, &claims, q.monitor.as_deref())?;
        let store = &cap.store;
        let sig = Signal::new();
        store.subscribe(&sig);
        let start = Instant::now();
        let since = q.since.unwrap_or_else(|| store.lock().seq);
        loop {
            // Changes an on-demand source has not grabbed yet: pull them in to see where they are.
            if store.pending() {
                store.request_frame();
            }
            let seq = store.lock().seq;
            let waited = start.elapsed();
            let done = |key: &str, v: bool| {
                serde_json::json!({ key: v, "seq": seq, "waitedMs": waited.as_millis() as u64 })
            };
            if change {
                if store.changed_since(since, region) {
                    return Ok(done("changed", true));
                }
            } else {
                let still_us = clock::now_us().saturating_sub(store.last_change(region));
                if still_us >= idle_us && !store.pending() {
                    return Ok(done("settled", true));
                }
            }
            if waited >= timeout {
                return Ok(done(if change { "changed" } else { "settled" }, false));
            }
            sig.wait(Duration::from_millis(20).min(timeout - waited));
        }
    })
    .await;
    match res {
        Ok(Ok(v)) => {
            let mut r =
                ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        Ok(Err(r)) => r.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// PNG through libavcodec's encoder, at a fast compression level.
fn png(px: &[u8], w: u32, h: u32, format: PixelFormat) -> anyhow::Result<Vec<u8>> {
    use ffmpeg_sys_next as ff;
    use std::ptr;
    unsafe {
        let codec = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_PNG);
        if codec.is_null() {
            anyhow::bail!("this FFmpeg has no PNG encoder");
        }
        let mut ctx = ff::avcodec_alloc_context3(codec);
        (*ctx).width = w as i32;
        (*ctx).height = h as i32;
        (*ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_RGB24;
        (*ctx).time_base = ff::AVRational { num: 1, den: 1 };
        (*ctx).compression_level = 3;
        let mut frame = ff::av_frame_alloc();
        let mut pkt = ff::av_packet_alloc();
        let result = (|| {
            if ff::avcodec_open2(ctx, codec, ptr::null_mut()) < 0 {
                anyhow::bail!("cannot open the PNG encoder");
            }
            (*frame).format = ff::AVPixelFormat::AV_PIX_FMT_RGB24 as i32;
            (*frame).width = w as i32;
            (*frame).height = h as i32;
            if ff::av_frame_get_buffer(frame, 32) < 0 {
                anyhow::bail!("out of memory");
            }
            let stride = (*frame).linesize[0] as usize;
            let (r, b) = match format {
                PixelFormat::Bgrx => (2, 0),
                PixelFormat::Rgbx => (0, 2),
            };
            for y in 0..h as usize {
                let dst = std::slice::from_raw_parts_mut(
                    (*frame).data[0].add(y * stride),
                    w as usize * 3,
                );
                let src = &px[y * w as usize * 4..(y + 1) * w as usize * 4];
                for (d, s) in dst
                    .as_chunks_mut::<3>()
                    .0
                    .iter_mut()
                    .zip(src.as_chunks::<4>().0)
                {
                    d[0] = s[r];
                    d[1] = s[1];
                    d[2] = s[b];
                }
            }
            if ff::avcodec_send_frame(ctx, frame) < 0
                || ff::avcodec_send_frame(ctx, ptr::null()) < 0
            {
                anyhow::bail!("PNG encode failed");
            }
            let mut out = Vec::new();
            while ff::avcodec_receive_packet(ctx, pkt) >= 0 {
                out.extend_from_slice(std::slice::from_raw_parts(
                    (*pkt).data,
                    (*pkt).size as usize,
                ));
                ff::av_packet_unref(pkt);
            }
            Ok(out)
        })();
        ff::av_packet_free(&mut pkt);
        ff::av_frame_free(&mut frame);
        ff::avcodec_free_context(&mut ctx);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_that_wrap_around_are_refused() {
        assert_eq!(parse_region("10,20,30,40"), Some(Rect::new(10, 20, 30, 40)));
        assert_eq!(parse_region("4294967295,0,10,10"), None);
        assert_eq!(parse_region("0,4294967290,10,10"), None);
        assert_eq!(parse_region("0,0,0,10"), None);
    }
}
