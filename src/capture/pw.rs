//! Reads a PipeWire video node (from KWin's screencast) into a [`FrameStore`].
//!
//! Buffers are negotiated as shared memory (memfd) with BGRx/RGBx pixels, plus three metadata
//! blocks: the frame header (for the compositor's timestamp), the damage region list and the
//! cursor (position and bitmap, so the browser can draw the pointer locally with no latency).

use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::sys as spa_sys;
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id};

use crate::clock;
use crate::frame::{CursorPos, CursorShape, FrameStore, PixelFormat, Rect};

pub struct PwCapture {
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for PwCapture {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct UserData {
    store: Arc<FrameStore>,
    width: u32,
    height: u32,
    format: Option<PixelFormat>,
    cursor_serial: u32,
    cursor: CursorPos,
    damage: Vec<Rect>,
}

fn pod_bytes(obj: Object) -> Vec<u8> {
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .expect("serializing a SPA pod")
    .0
    .into_inner()
}

fn prop(key: u32, value: Value) -> Property {
    Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    }
}

fn int_range(default: i32, min: i32, max: i32) -> Value {
    Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range { default, min, max },
    )))
}

fn enum_format(width: u32, height: u32, max_fps: u32) -> Vec<u8> {
    use pw::spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    use pw::spa::param::video::VideoFormat as V;
    let formats = [V::BGRx, V::BGRx, V::BGRA, V::RGBx, V::RGBA];
    let obj = Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            prop(
                F::MediaType.as_raw(),
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            prop(
                F::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            prop(
                F::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(formats[0].as_raw()),
                        alternatives: formats[1..].iter().map(|f| Id(f.as_raw())).collect(),
                    },
                ))),
            ),
            prop(
                F::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: spa::utils::Rectangle {
                            width: width.max(1),
                            height: height.max(1),
                        },
                        min: spa::utils::Rectangle {
                            width: 1,
                            height: 1,
                        },
                        max: spa::utils::Rectangle {
                            width: 8192,
                            height: 8192,
                        },
                    },
                ))),
            ),
            prop(
                F::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: spa::utils::Fraction { num: 0, denom: 1 },
                        min: spa::utils::Fraction { num: 0, denom: 1 },
                        max: spa::utils::Fraction {
                            num: 1000,
                            denom: 1,
                        },
                    },
                ))),
            ),
            prop(
                F::VideoMaxFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: spa::utils::Fraction {
                            num: max_fps.max(1),
                            denom: 1,
                        },
                        min: spa::utils::Fraction { num: 1, denom: 1 },
                        max: spa::utils::Fraction {
                            num: 1000,
                            denom: 1,
                        },
                    },
                ))),
            ),
        ],
    };
    pod_bytes(obj)
}

const REGION: i32 = std::mem::size_of::<spa_sys::spa_meta_region>() as i32;

fn cursor_meta_size(w: i32, h: i32) -> i32 {
    (std::mem::size_of::<spa_sys::spa_meta_cursor>()
        + std::mem::size_of::<spa_sys::spa_meta_bitmap>()) as i32
        + w * h * 4
}

fn buffer_params() -> Vec<Vec<u8>> {
    let meta = |ty: u32, size: Value| Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa_sys::SPA_PARAM_Meta,
        properties: vec![
            prop(spa_sys::SPA_PARAM_META_type, Value::Id(Id(ty))),
            prop(spa_sys::SPA_PARAM_META_size, size),
        ],
    };
    let mem = (1i32 << spa_sys::SPA_DATA_MemFd) | (1i32 << spa_sys::SPA_DATA_MemPtr);
    vec![
        pod_bytes(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_ParamBuffers,
            id: spa_sys::SPA_PARAM_Buffers,
            properties: vec![
                prop(spa_sys::SPA_PARAM_BUFFERS_buffers, int_range(4, 2, 16)),
                prop(
                    spa_sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Flags {
                            default: mem,
                            flags: vec![],
                        },
                    ))),
                ),
            ],
        }),
        pod_bytes(meta(
            spa_sys::SPA_META_Header,
            Value::Int(std::mem::size_of::<spa_sys::spa_meta_header>() as i32),
        )),
        pod_bytes(meta(
            spa_sys::SPA_META_VideoDamage,
            int_range(REGION * 16, REGION, REGION * 16),
        )),
        pod_bytes(meta(
            spa_sys::SPA_META_Cursor,
            int_range(
                cursor_meta_size(64, 64),
                cursor_meta_size(1, 1),
                cursor_meta_size(512, 512),
            ),
        )),
    ]
}

impl PwCapture {
    pub fn start(
        node: u32,
        store: Arc<FrameStore>,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> Result<PwCapture> {
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let max_fps = (refresh_mhz / 1000).clamp(30, 1000);
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                let res = run(node, store, width, height, max_fps, stop_rx, &ready_tx);
                if let Err(e) = res {
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
            })?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(PwCapture {
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(anyhow!("PipeWire: {e}")),
            Err(_) => Err(anyhow!("PipeWire did not start")),
        }
    }
}

fn run(
    node: u32,
    store: Arc<FrameStore>,
    width: u32,
    height: u32,
    max_fps: u32,
    stop_rx: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamRc::new(
        core,
        "skylight",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let data = UserData {
        store: store.clone(),
        width: 0,
        height: 0,
        format: None,
        cursor_serial: 0,
        cursor: CursorPos::default(),
        damage: Vec::with_capacity(16),
    };
    let err_store = store.clone();
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| {
            tracing::debug!(?old, ?new, "PipeWire stream state");
            match new {
                pw::stream::StreamState::Error(e) => {
                    err_store.set_error(format!("PipeWire stream error: {e}"))
                }
                // The node went away under a running stream (we only disconnect on drop).
                pw::stream::StreamState::Unconnected
                    if !matches!(old, pw::stream::StreamState::Connecting) =>
                {
                    err_store.set_error("PipeWire stream disconnected".into())
                }
                _ => {}
            }
        })
        .param_changed(|stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = pw::spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            use pw::spa::param::video::VideoFormat as V;
            ud.format = match info.format() {
                V::BGRx | V::BGRA => Some(PixelFormat::Bgrx),
                V::RGBx | V::RGBA => Some(PixelFormat::Rgbx),
                _ => None,
            };
            ud.width = info.size().width;
            ud.height = info.size().height;
            tracing::info!(width = ud.width, height = ud.height, format = ?info.format(), "PipeWire format");
            let params = buffer_params();
            let mut pods: Vec<&Pod> = params.iter().filter_map(|b| Pod::from_bytes(b)).collect();
            if let Err(e) = stream.update_params(&mut pods) {
                tracing::warn!("update_params: {e}");
            }
        })
        .process(|stream, ud| unsafe { process(stream, ud) })
        .register()?;

    let fmt = enum_format(width, height, max_fps);
    let mut params = [Pod::from_bytes(&fmt).ok_or_else(|| anyhow!("bad format pod"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let ml = mainloop.clone();
    let _stop = stop_rx.attach(mainloop.loop_(), move |_| ml.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

unsafe fn find_meta(buf: *mut spa_sys::spa_buffer, ty: u32) -> Option<&'static spa_sys::spa_meta> {
    unsafe {
        for i in 0..(*buf).n_metas as usize {
            let m = &*(*buf).metas.add(i);
            if m.type_ == ty && !m.data.is_null() {
                return Some(m);
            }
        }
    }
    None
}

unsafe fn process(stream: &pw::stream::Stream, ud: &mut UserData) {
    unsafe {
        loop {
            let b = stream.dequeue_raw_buffer();
            if b.is_null() {
                return;
            }
            handle_buffer((*b).buffer, ud);
            stream.queue_raw_buffer(b);
        }
    }
}

unsafe fn handle_buffer(buf: *mut spa_sys::spa_buffer, ud: &mut UserData) {
    unsafe {
        ud.store.touch();
        let now_us = clock::now_us();
        let mut delay_us = 0u64;
        if let Some(m) = find_meta(buf, spa_sys::SPA_META_Header) {
            let h = &*(m.data as *const spa_sys::spa_meta_header);
            if h.pts > 0 {
                let d = clock::monotonic_ns().saturating_sub(h.pts as u64) / 1000;
                if d < 1_000_000 {
                    delay_us = d;
                }
            }
        }

        if let Some(m) = find_meta(buf, spa_sys::SPA_META_Cursor) {
            read_cursor(m, ud);
        }

        if (*buf).n_datas == 0 || ud.format.is_none() || ud.width == 0 {
            return;
        }
        let d = &*(*buf).datas;
        let chunk = &*d.chunk;
        if d.data.is_null()
            || chunk.size == 0
            || chunk.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0
        {
            return; // cursor-only update
        }
        let stride = if chunk.stride > 0 {
            chunk.stride as usize
        } else {
            ud.width as usize * 4
        };
        let needed = stride * (ud.height as usize - 1) + ud.width as usize * 4;
        let avail = (d.maxsize as usize).saturating_sub(chunk.offset as usize);
        if avail < needed {
            return;
        }
        let pixels =
            std::slice::from_raw_parts((d.data as *const u8).add(chunk.offset as usize), needed);

        ud.damage.clear();
        let mut have_damage_meta = false;
        if let Some(m) = find_meta(buf, spa_sys::SPA_META_VideoDamage) {
            have_damage_meta = true;
            let n = m.size as usize / std::mem::size_of::<spa_sys::spa_meta_region>();
            let regions = std::slice::from_raw_parts(m.data as *const spa_sys::spa_meta_region, n);
            for r in regions {
                if r.region.size.width == 0 || r.region.size.height == 0 {
                    break;
                }
                let x = r.region.position.x.max(0) as u32;
                let y = r.region.position.y.max(0) as u32;
                ud.damage
                    .push(Rect::new(x, y, r.region.size.width, r.region.size.height));
            }
        }
        let damage = if have_damage_meta && !ud.damage.is_empty() {
            Some(&ud.damage[..])
        } else {
            None
        };
        ud.store.update(
            ud.width,
            ud.height,
            ud.format.unwrap(),
            pixels,
            stride,
            damage,
            // KWin's buffer timestamp is not the capture time on an idle screen, so frames are
            // stamped on arrival; the compositor's own figure is kept for the stats only.
            now_us,
            delay_us,
        );
    }
}

unsafe fn read_cursor(m: &spa_sys::spa_meta, ud: &mut UserData) {
    unsafe {
        let c = &*(m.data as *const spa_sys::spa_meta_cursor);
        if c.id == 0 {
            return;
        }
        let mut shape = None;
        let mut visible = ud.cursor.visible;
        if c.bitmap_offset as usize >= std::mem::size_of::<spa_sys::spa_meta_cursor>() {
            let bm = &*((m.data as *const u8).add(c.bitmap_offset as usize)
                as *const spa_sys::spa_meta_bitmap);
            if bm.offset == 0 || bm.size.width == 0 || bm.size.height == 0 {
                visible = false;
            } else if (bm.offset as usize) >= std::mem::size_of::<spa_sys::spa_meta_bitmap>() {
                visible = true;
                let w = bm.size.width.min(512);
                let h = bm.size.height.min(512);
                let stride = if bm.stride > 0 {
                    bm.stride as usize
                } else {
                    w as usize * 4
                };
                let src = (bm as *const _ as *const u8).add(bm.offset as usize);
                let end = bm.offset as usize
                    + stride * (h as usize - 1)
                    + w as usize * 4
                    + c.bitmap_offset as usize;
                if end <= m.size as usize {
                    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                    for y in 0..h as usize {
                        let row = std::slice::from_raw_parts(src.add(y * stride), w as usize * 4);
                        rgba.extend_from_slice(row);
                    }
                    to_rgba(bm.format, &mut rgba);
                    ud.cursor_serial = ud.cursor_serial.wrapping_add(1);
                    shape = Some(CursorShape {
                        serial: ud.cursor_serial,
                        width: w,
                        height: h,
                        hot_x: c.hotspot.x.max(0) as u32,
                        hot_y: c.hotspot.y.max(0) as u32,
                        rgba,
                    });
                }
            }
        }
        ud.cursor = CursorPos {
            x: c.position.x,
            y: c.position.y,
            visible,
        };
        ud.store.set_cursor(ud.cursor, shape);
    }
}

/// Convert a cursor bitmap to straight-alpha RGBA in place.
fn to_rgba(format: u32, px: &mut [u8]) {
    use pw::spa::param::video::VideoFormat as V;
    let f = V::from_raw(format);
    for p in px.as_chunks_mut::<4>().0 {
        let (r, g, b, a) = match f {
            V::BGRA | V::BGRx => (p[2], p[1], p[0], p[3]),
            V::ARGB | V::xRGB => (p[1], p[2], p[3], p[0]),
            V::ABGR | V::xBGR => (p[3], p[2], p[1], p[0]),
            _ => (p[0], p[1], p[2], p[3]),
        };
        p.copy_from_slice(&[r, g, b, a]);
    }
    // KWin hands over premultiplied pixels; undo that so the browser can use them as a CSS cursor.
    let premultiplied = px
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
    if premultiplied {
        for p in px.as_chunks_mut::<4>().0 {
            let a = p[3] as u32;
            if a > 0 && a < 255 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
    }
}
