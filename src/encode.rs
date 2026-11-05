//! Video encoding: NVENC first (H.264, HEVC or AV1; driven directly, see `nvenc.rs`, with
//! FFmpeg's NVENC wrapper as a fallback), then VAAPI, then x264, through libavcodec.
//!
//! Every encoder is set up the same way for interactive use: no B-frames, no lookahead, no
//! scene-cut keyframes, an effectively infinite GOP (keyframes only on request), CBR with a
//! short VBV so no single frame can stall the link, and output as soon as each frame is done.

use std::ffi::{CStr, CString};
use std::ptr;

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg_sys_next as ff;
use serde::{Deserialize, Serialize};

use crate::frame::{PixelFormat, Rect};
use crate::nvenc::{FrameOpts, NvEncoder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Codec> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" => Some(Codec::H264),
            "hevc" | "h265" | "hvc1" => Some(Codec::Hevc),
            "av1" | "av01" => Some(Codec::Av1),
            _ => None,
        }
    }

    pub fn id(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::Hevc => 2,
            Codec::Av1 => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Auto,
    Nvenc,
    Vaapi,
    Software,
}

#[derive(Clone, Debug)]
pub struct EncoderParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    /// VBV size in milliseconds of the target bitrate; caps how big any one frame can get.
    pub vbv_ms: u32,
    pub format: PixelFormat,
    pub backend: Backend,
    /// NVENC preset p1 (fastest) .. p7.
    pub nvenc_preset: u8,
    /// x264 preset name.
    pub x264_preset: String,
    /// `None`: CBR, every frame spends its share of the bitrate (sharpest on a fast LAN).
    /// `Some(q)`: capped quality mode, where a frame that changes a few pixels costs a few
    /// kilobytes: VBR with the average at the cap on NVENC (FFmpeg drops the VBV when NVENC's
    /// own constant-quality target is used, so `q` is not passed there), capped CRF `q` on x264.
    pub cq: Option<u8>,
    /// Give the first keyframe a bigger budget too, like every forced keyframe after it (where
    /// the encoder does that).
    pub first_key_boost: bool,
}

impl Default for EncoderParams {
    fn default() -> Self {
        EncoderParams {
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 20_000_000,
            vbv_ms: 60,
            format: PixelFormat::Bgrx,
            backend: Backend::Auto,
            nvenc_preset: 2,
            x264_preset: "superfast".into(),
            cq: Some(23),
            first_key_boost: true,
        }
    }
}

/// How an encoder takes a new bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateChange {
    /// Reconfigured between two frames; the stream just continues (NVENC direct, x264).
    InPlace,
    /// Applied, but the encoder resets and sends a keyframe (FFmpeg's NVENC wrapper).
    Keyframe,
    /// Only by opening a new encoder, which starts with a keyframe (VA-API, x265, SVT-AV1,
    /// OpenH264: their FFmpeg wrappers read the rate once).
    Reopen,
}

/// What an encoder can do beyond plain frames, so the session uses each speed trick only where
/// the API allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub rate: RateChange,
    /// Lower the QP of a frame on request (refinement): NVENC's QP map, or FFmpeg's
    /// region-of-interest offsets on x264, x265 and VA-API (where the driver supports ROI).
    pub sharpen: bool,
    /// Give every forced keyframe a bigger budget for that one frame, without a reset (NVENC
    /// direct). x264 applies a new budget only from the next frame, so it gets the boost for its
    /// first keyframe only, by opening with it; the others have to start at their normal budget.
    pub keyframe_boost: bool,
    /// How much each refinement frame lowers the QP, one entry per frame (where `sharpen`).
    /// NVENC's QP map takes large offsets well: each refinement spends up to the VBV cap and text
    /// comes out sharp (a page of text settles near 39 dB instead of 19-24 dB). Through FFmpeg's
    /// region-of-interest offsets, the encoder's rate control raises the frame's QP to make up for
    /// large ones and draws down its buffer, which starves the next change, so they stay mild.
    pub refine: &'static [u8],
}

/// Refinement QP offsets for NVENC's QP map and for FFmpeg's region-of-interest offsets.
const REFINE_QP_MAP: &[u8] = &[16, 24, 32];
const REFINE_ROI: &[u8] = &[4, 8, 8];

impl Caps {
    /// The capabilities of an encoder, by name ("nvenc" is the direct NVENC path).
    pub fn of(name: &str, nvenc_dynamic_bitrate: bool) -> Caps {
        let (rate, sharpen, keyframe_boost) = match name {
            "nvenc" if nvenc_dynamic_bitrate => (RateChange::InPlace, true, true),
            "nvenc" => (RateChange::Keyframe, true, false),
            "libx264" => (RateChange::InPlace, true, false),
            "libx265" => (RateChange::Reopen, true, false),
            n if n.ends_with("_nvenc") => (RateChange::Keyframe, false, false),
            n if n.ends_with("_vaapi") => (RateChange::Reopen, true, false),
            _ => (RateChange::Reopen, false, false),
        };
        Caps {
            rate,
            sharpen,
            keyframe_boost,
            refine: if name == "nvenc" {
                REFINE_QP_MAP
            } else {
                REFINE_ROI
            },
        }
    }
}

pub struct VideoEncoder {
    /// Set when NVENC is driven directly; the FFmpeg fields below are then unused.
    nv: Option<NvEncoder>,
    caps: Caps,
    /// The encoder was opened with a keyframe-sized budget, to drop after the first frame.
    first_boosted: bool,
    ctx: *mut ff::AVCodecContext,
    /// Packed RGB input the caller writes into.
    src: *mut ff::AVFrame,
    /// Converted frame for encoders that need YUV in system memory (x264) or a GPU upload (VAAPI).
    conv: *mut ff::AVFrame,
    sws: *mut ff::SwsContext,
    /// Converts one band of `BAND` rows, for frames where only a few bands changed (even sizes
    /// only; null otherwise).
    band_sws: *mut ff::SwsContext,
    /// Bands of `conv` that are behind `src`: everything at first, then what `changed` reports.
    stale: Vec<bool>,
    /// The caller took `input()` without saying what it changed: convert the whole frame.
    whole: bool,
    hw_device: *mut ff::AVBufferRef,
    pkt: *mut ff::AVPacket,
    pts: i64,
    pub name: String,
    pub hardware: bool,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    bitrate: u64,
    vbv_ms: u32,
}

// The raw pointers are owned exclusively by this struct and only touched from one thread at a time.
unsafe impl Send for VideoEncoder {}

/// Rows per band in the partial colour conversion (even, for 4:2:0 chroma).
const BAND: u32 = 16;

fn averr(code: i32) -> String {
    let mut buf = [0 as libc::c_char; 128];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn set_opt(ctx: *mut ff::AVCodecContext, key: &str, val: &str) {
    let k = CString::new(key).unwrap();
    let v = CString::new(val).unwrap();
    // Unknown options are skipped on purpose: ffmpeg builds differ in what each encoder exposes.
    let r = unsafe {
        ff::av_opt_set(
            ctx as *mut _,
            k.as_ptr(),
            v.as_ptr(),
            ff::AV_OPT_SEARCH_CHILDREN,
        )
    };
    if r < 0 {
        tracing::debug!("encoder option {key}={val} not applied: {}", averr(r));
    }
}

fn src_pix_fmt(f: PixelFormat) -> ff::AVPixelFormat {
    match f {
        PixelFormat::Bgrx => ff::AVPixelFormat::AV_PIX_FMT_BGR0,
        PixelFormat::Rgbx => ff::AVPixelFormat::AV_PIX_FMT_RGB0,
    }
}

/// Quiet ffmpeg's own logging unless asked for.
pub fn init_logging(verbose: bool) {
    unsafe {
        ff::av_log_set_level(if verbose {
            ff::AV_LOG_INFO
        } else {
            ff::AV_LOG_ERROR
        });
    }
}

/// Names of the encoders to try for a codec and backend, best first: NVENC (driven directly,
/// then through FFmpeg), VA-API (Intel, AMD), then software. `nvenc` is the direct path; set
/// SKYLIGHT_NVENC_DIRECT=0 to use FFmpeg's wrapper instead. Each is tried at runtime, so a
/// machine without a GPU (or without NVIDIA's driver) simply ends up on software.
pub fn candidates(codec: Codec, backend: Backend) -> Vec<&'static str> {
    let direct = std::env::var("SKYLIGHT_NVENC_DIRECT").map_or(true, |v| v != "0");
    let nv_direct: &[&'static str] = if direct { &["nvenc"] } else { &[] };
    let nv = match codec {
        Codec::H264 => "h264_nvenc",
        Codec::Hevc => "hevc_nvenc",
        Codec::Av1 => "av1_nvenc",
    };
    let va = match codec {
        Codec::H264 => "h264_vaapi",
        Codec::Hevc => "hevc_vaapi",
        Codec::Av1 => "av1_vaapi",
    };
    let sw: &[&'static str] = match codec {
        Codec::H264 => &["libx264", "libopenh264"],
        Codec::Hevc => &["libx265"],
        Codec::Av1 => &["libsvtav1"],
    };
    match backend {
        Backend::Nvenc => [nv_direct, &[nv]].concat(),
        Backend::Vaapi => vec![va],
        Backend::Software => sw.to_vec(),
        Backend::Auto => [nv_direct, &[nv, va][..], sw].concat(),
    }
}

/// VA-API render nodes to try: SKYLIGHT_VAAPI_DEVICE if set, else every /dev/dri/renderD*.
fn vaapi_nodes() -> Vec<String> {
    if let Ok(d) = std::env::var("SKYLIGHT_VAAPI_DEVICE") {
        return vec![d];
    }
    let mut v: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path().to_string_lossy().into_owned())
                .filter(|p| p.contains("renderD"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl VideoEncoder {
    /// Open the best available encoder for `p`, falling back down the list.
    pub fn open(p: &EncoderParams) -> Result<VideoEncoder> {
        let mut errors = Vec::new();
        for name in candidates(p.codec, p.backend) {
            match Self::open_named(name, p) {
                Ok(e) => return Ok(e),
                Err(e) => errors.push(format!("{name}: {e:#}")),
            }
        }
        bail!("no usable {:?} encoder ({})", p.codec, errors.join("; "))
    }

    pub fn open_named(name: &str, p: &EncoderParams) -> Result<VideoEncoder> {
        if p.width < 16 || p.height < 16 || p.width > 8192 || p.height > 8192 {
            bail!("unsupported size {}x{}", p.width, p.height);
        }
        if name.ends_with("_vaapi") {
            // Machines with two GPUs (an NVIDIA card next to an Intel iGPU, say) have several
            // render nodes, and only some of their VA-API drivers encode: try each. Ask for
            // capped VBR first, then whatever rate control the driver offers.
            let mut errors = Vec::new();
            for node in vaapi_nodes() {
                for rc in ["VBR", "CBR"] {
                    match Self::open_ffmpeg(name, p, Some((&node, rc))) {
                        Ok(e) => return Ok(e),
                        Err(e) => errors.push(format!("{node} {rc}: {e:#}")),
                    }
                }
            }
            if errors.is_empty() {
                bail!("no /dev/dri render node");
            }
            bail!("{}", errors.join("; "));
        }
        Self::open_ffmpeg(name, p, None)
    }

    fn open_ffmpeg(
        name: &str,
        p: &EncoderParams,
        vaapi: Option<(&str, &str)>,
    ) -> Result<VideoEncoder> {
        if name == "nvenc" {
            let nv = NvEncoder::open(p)?;
            let caps = Caps::of("nvenc", nv.dynamic_bitrate());
            return Ok(VideoEncoder {
                nv: Some(nv),
                caps,
                first_boosted: false,
                ctx: ptr::null_mut(),
                src: ptr::null_mut(),
                conv: ptr::null_mut(),
                sws: ptr::null_mut(),
                band_sws: ptr::null_mut(),
                stale: Vec::new(),
                whole: true,
                hw_device: ptr::null_mut(),
                pkt: ptr::null_mut(),
                pts: 0,
                name: match p.codec {
                    Codec::H264 => "nvenc-h264",
                    Codec::Hevc => "nvenc-hevc",
                    Codec::Av1 => "nvenc-av1",
                }
                .into(),
                hardware: true,
                codec: p.codec,
                width: p.width,
                height: p.height,
                fps: p.fps.max(1),
                bitrate: p.bitrate,
                vbv_ms: p.vbv_ms.max(1),
            });
        }
        let cname = CString::new(name).unwrap();
        let codec = unsafe { ff::avcodec_find_encoder_by_name(cname.as_ptr()) };
        if codec.is_null() {
            bail!("not built into this ffmpeg");
        }
        let nvenc = name.ends_with("_nvenc");
        let vaapi_opts = vaapi;
        let vaapi = vaapi.is_some();
        let hardware = nvenc || vaapi;
        let mut enc = VideoEncoder {
            nv: None,
            caps: Caps::of(name, false),
            first_boosted: false,
            ctx: ptr::null_mut(),
            src: ptr::null_mut(),
            conv: ptr::null_mut(),
            sws: ptr::null_mut(),
            band_sws: ptr::null_mut(),
            stale: Vec::new(),
            whole: true,
            hw_device: ptr::null_mut(),
            pkt: ptr::null_mut(),
            pts: 0,
            name: name.to_string(),
            hardware,
            codec: p.codec,
            width: p.width,
            height: p.height,
            fps: p.fps.max(1),
            bitrate: p.bitrate,
            vbv_ms: p.vbv_ms.max(1),
        };
        unsafe {
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            enc.ctx = ctx;
            // Encoders want even dimensions for 4:2:0; the input frame keeps the real size and
            // the extra row/column (if any) repeats the edge.
            let ew = (p.width + 1) & !1;
            let eh = (p.height + 1) & !1;
            (*ctx).width = ew as i32;
            (*ctx).height = eh as i32;
            (*ctx).time_base = ff::AVRational {
                num: 1,
                den: enc.fps as i32,
            };
            (*ctx).framerate = ff::AVRational {
                num: enc.fps as i32,
                den: 1,
            };
            (*ctx).gop_size = i32::MAX;
            (*ctx).max_b_frames = 0;
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            enc.apply_rate(p.bitrate);
            if name == "libx264" && p.first_key_boost {
                // A viewer's first frame is a keyframe that repaints everything: open with four
                // times the usual budget so it comes out sharp, and drop to normal after it.
                (*ctx).bit_rate = (p.bitrate * 4) as i64;
                (*ctx).rc_max_rate = (p.bitrate * 4) as i64;
                (*ctx).rc_buffer_size =
                    (p.bitrate * 4 * enc.vbv_ms as u64 / 1000).min(i32::MAX as u64) as i32;
                enc.first_boosted = true;
            }

            let src_fmt = src_pix_fmt(p.format);
            if nvenc {
                (*ctx).pix_fmt = src_fmt;
                set_opt(ctx, "preset", &format!("p{}", p.nvenc_preset.clamp(1, 7)));
                set_opt(ctx, "tune", "ull");
                set_opt(ctx, "rc", if p.cq.is_some() { "vbr" } else { "cbr" });
                set_opt(ctx, "zerolatency", "1");
                set_opt(ctx, "delay", "0");
                set_opt(ctx, "forced-idr", "1");
                set_opt(ctx, "no-scenecut", "1");
                set_opt(ctx, "rc-lookahead", "0");
                set_opt(ctx, "multipass", "disabled");
                set_opt(ctx, "b_ref_mode", "disabled");
                set_opt(ctx, "surfaces", "4");
                set_opt(ctx, "aud", "0");
                set_opt(ctx, "spatial-aq", "1");
                if p.codec == Codec::H264 {
                    set_opt(ctx, "profile", "high");
                    set_opt(ctx, "coder", "cabac");
                } else if p.codec == Codec::Hevc {
                    set_opt(ctx, "profile", "main");
                }
            } else if let Some((node, rc_mode)) = vaapi_opts {
                let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
                let node = CString::new(node).unwrap();
                let r = ff::av_hwdevice_ctx_create(
                    &mut dev,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    node.as_ptr(),
                    ptr::null_mut(),
                    0,
                );
                if r < 0 {
                    bail!("VAAPI device: {}", averr(r));
                }
                enc.hw_device = dev;
                let frames = ff::av_hwframe_ctx_alloc(dev);
                let fc = (*frames).data as *mut ff::AVHWFramesContext;
                (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                (*fc).width = ew as i32;
                (*fc).height = eh as i32;
                (*fc).initial_pool_size = 4;
                let r = ff::av_hwframe_ctx_init(frames);
                if r < 0 {
                    let mut f = frames;
                    ff::av_buffer_unref(&mut f);
                    bail!("VAAPI frames: {}", averr(r));
                }
                (*ctx).hw_frames_ctx = frames;
                (*ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                set_opt(ctx, "rc_mode", rc_mode);
                // One frame in the pipeline: each frame is out before the next goes in.
                set_opt(ctx, "async_depth", "1");
                if p.codec == Codec::H264 {
                    set_opt(ctx, "profile", "high");
                }
            } else {
                (*ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV420P;
                (*ctx).thread_count = 0;
                match name {
                    "libx264" => {
                        set_opt(ctx, "preset", &p.x264_preset);
                        set_opt(ctx, "tune", "zerolatency");
                        if let Some(q) = p.cq {
                            // Capped CRF: quality-driven, never above the VBV cap.
                            set_opt(ctx, "crf", &q.to_string());
                        }
                        set_opt(ctx, "forced-idr", "1");
                        set_opt(
                            ctx,
                            "x264-params",
                            "scenecut=0:rc-lookahead=0:sync-lookahead=0:sliced-threads=1",
                        );
                    }
                    "libx265" => {
                        set_opt(ctx, "preset", "ultrafast");
                        set_opt(ctx, "tune", "zerolatency");
                        set_opt(ctx, "forced-idr", "1");
                    }
                    "libsvtav1" => {
                        set_opt(ctx, "preset", "12");
                        set_opt(ctx, "svtav1-params", "pred-struct=1:lookahead=0");
                    }
                    _ => {}
                }
            }
            if !nvenc {
                (*ctx).colorspace = ff::AVColorSpace::AVCOL_SPC_BT470BG;
                (*ctx).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            }

            let r = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                bail!("open: {}", averr(r));
            }

            // NVENC takes the RGB frame directly, so it has to be the padded size.
            enc.src = if nvenc {
                alloc_frame(src_fmt, ew, eh)?
            } else {
                alloc_frame(src_fmt, p.width, p.height)?
            };
            if !nvenc {
                let dst_fmt = if vaapi {
                    ff::AVPixelFormat::AV_PIX_FMT_NV12
                } else {
                    ff::AVPixelFormat::AV_PIX_FMT_YUV420P
                };
                enc.conv = alloc_frame(dst_fmt, ew, eh)?;
                enc.sws = ff::sws_getContext(
                    p.width as i32,
                    p.height as i32,
                    src_fmt,
                    ew as i32,
                    eh as i32,
                    dst_fmt,
                    ff::SWS_POINT,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if enc.sws.is_null() {
                    bail!("sws_getContext failed");
                }
                let threads = CString::new("threads").unwrap();
                ff::av_opt_set_int(enc.sws as *mut _, threads.as_ptr(), 0, 0);
                // Point sampling at the same size works row by row (chroma pairs of rows), so
                // converting a band on its own gives the same bytes as converting the frame.
                if p.width == ew && p.height == eh && eh >= BAND {
                    enc.band_sws = ff::sws_getContext(
                        ew as i32,
                        BAND as i32,
                        src_fmt,
                        ew as i32,
                        BAND as i32,
                        dst_fmt,
                        ff::SWS_POINT,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null(),
                    );
                    enc.stale = vec![true; eh.div_ceil(BAND) as usize];
                }
            }
            enc.pkt = ff::av_packet_alloc();
        }
        Ok(enc)
    }

    unsafe fn apply_rate(&mut self, bitrate: u64) {
        unsafe {
            let ctx = self.ctx;
            (*ctx).bit_rate = bitrate as i64;
            (*ctx).rc_max_rate = bitrate as i64;
            (*ctx).rc_buffer_size =
                (bitrate * self.vbv_ms as u64 / 1000).min(i32::MAX as u64) as i32;
        }
        self.bitrate = bitrate;
    }

    /// Change the bitrate cap between frames. What that costs depends on `caps().rate`: nothing
    /// (NVENC direct, x264), a keyframe (FFmpeg's NVENC), or nothing at all happens and the
    /// caller has to open a new encoder (`Reopen`).
    pub fn set_bitrate(&mut self, bitrate: u64) {
        let bitrate = bitrate.clamp(250_000, 400_000_000);
        if let Some(nv) = &mut self.nv {
            if let Err(e) = nv.set_bitrate(bitrate) {
                tracing::warn!("NVENC bitrate change failed: {e:#}");
            }
            self.bitrate = nv.bitrate();
            return;
        }
        if bitrate != self.bitrate {
            unsafe { self.apply_rate(bitrate) };
        }
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    pub fn bitrate(&self) -> u64 {
        self.bitrate
    }

    /// The packed-RGB frame the caller fills in, and its stride in bytes. It keeps its contents
    /// between frames, so only what changed needs writing.
    pub fn input(&mut self) -> Result<(&mut [u8], usize)> {
        if let Some(nv) = &mut self.nv {
            return nv.input();
        }
        self.whole = true;
        unsafe {
            ff::av_frame_make_writable(self.src);
            let stride = (*self.src).linesize[0] as usize;
            let len = stride * self.height as usize;
            Ok((
                std::slice::from_raw_parts_mut((*self.src).data[0], len),
                stride,
            ))
        }
    }

    /// Narrow what the last `input()` changed to `rects`, so encoders that convert the picture
    /// first (software, VA-API) convert only the rows around them. Nothing changed is `&[]`.
    pub fn changed(&mut self, rects: &[Rect]) {
        if self.band_sws.is_null() {
            return;
        }
        self.whole = false;
        for r in rects {
            if r.w == 0 || r.h == 0 {
                continue;
            }
            let last = ((r.y + r.h - 1) / BAND) as usize;
            for b in (r.y / BAND) as usize..=last.min(self.stale.len() - 1) {
                self.stale[b] = true;
            }
        }
    }

    /// Bring `conv` up to date with `src`: the whole frame, or only the stale bands when they
    /// are few (banding the whole frame is slower than one pass).
    unsafe fn convert(&mut self) {
        unsafe {
            let bands = self.stale.iter().filter(|s| **s).count();
            if self.whole || self.band_sws.is_null() || bands * 10 > self.stale.len() * 7 {
                ff::sws_scale(
                    self.sws,
                    (*self.src).data.as_ptr() as *const *const u8,
                    (*self.src).linesize.as_ptr(),
                    0,
                    self.height as i32,
                    (*self.conv).data.as_ptr(),
                    (*self.conv).linesize.as_ptr(),
                );
            } else {
                let (s, d) = (&*self.src, &*self.conv);
                for (b, _) in self.stale.iter().enumerate().filter(|(_, s)| **s) {
                    // The last band overlaps the one before when the height is not a multiple
                    // of BAND; converting rows twice gives the same bytes.
                    let y = (b as u32 * BAND).min(self.height - BAND) as usize;
                    // swscale reads four plane pointers from each array.
                    let sp = [
                        s.data[0].add(y * s.linesize[0] as usize) as *const u8,
                        ptr::null(),
                        ptr::null(),
                        ptr::null(),
                    ];
                    let dp = [
                        d.data[0].add(y * d.linesize[0] as usize),
                        d.data[1].add(y / 2 * d.linesize[1] as usize),
                        d.data[2].add(y / 2 * d.linesize[2] as usize),
                        ptr::null_mut(),
                    ];
                    ff::sws_scale(
                        self.band_sws,
                        sp.as_ptr(),
                        s.linesize.as_ptr(),
                        0,
                        BAND as i32,
                        dp.as_ptr(),
                        d.linesize.as_ptr(),
                    );
                }
            }
            self.stale.fill(false);
            self.whole = false;
        }
    }

    /// Encode the current input frame. Appends the bitstream (Annex B for H.264/HEVC, OBUs for
    /// AV1) to `out`.
    pub fn encode(&mut self, keyframe: bool, out: &mut Vec<u8>) -> Result<bool> {
        self.encode_with(
            FrameOpts {
                keyframe,
                ..Default::default()
            },
            out,
        )
    }

    /// `encode` with per-frame options (honoured where the encoder supports them).
    pub fn encode_with(&mut self, opts: FrameOpts, out: &mut Vec<u8>) -> Result<bool> {
        if let Some(nv) = &mut self.nv {
            return nv.encode(opts, out);
        }
        let keyframe = opts.keyframe;
        unsafe {
            let r = self.encode_ffmpeg(keyframe, opts.sharpen, out);
            if self.first_boosted {
                // The first keyframe went out with the bigger budget; back to normal (x264
                // applies this from the next frame on).
                self.first_boosted = false;
                self.apply_rate(self.bitrate);
            }
            r
        }
    }

    unsafe fn encode_ffmpeg(
        &mut self,
        keyframe: bool,
        sharpen: u8,
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        unsafe {
            let mut hw_frame: *mut ff::AVFrame = ptr::null_mut();
            let frame = if self.conv.is_null() {
                self.src
            } else {
                ff::av_frame_make_writable(self.conv);
                self.convert();
                if self.hw_device.is_null() {
                    self.conv
                } else {
                    // A fresh surface from the pool per frame: the encoder may still hold the
                    // previous one.
                    hw_frame = ff::av_frame_alloc();
                    let mut r = ff::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, hw_frame, 0);
                    if r >= 0 {
                        r = ff::av_hwframe_transfer_data(hw_frame, self.conv, 0);
                    }
                    if r < 0 {
                        ff::av_frame_free(&mut hw_frame);
                        bail!("upload: {}", averr(r));
                    }
                    hw_frame
                }
            };
            ff::av_frame_remove_side_data(
                frame,
                ff::AVFrameSideDataType::AV_FRAME_DATA_REGIONS_OF_INTEREST,
            );
            if sharpen > 0 && self.caps.sharpen {
                // The whole frame as one region with a negative QP offset (as a fraction of the
                // QP range, which FFmpeg's wrappers map onto each encoder's scale).
                let sd = ff::av_frame_new_side_data(
                    frame,
                    ff::AVFrameSideDataType::AV_FRAME_DATA_REGIONS_OF_INTEREST,
                    std::mem::size_of::<ff::AVRegionOfInterest>(),
                );
                if !sd.is_null() {
                    let roi = (*sd).data as *mut ff::AVRegionOfInterest;
                    *roi = ff::AVRegionOfInterest {
                        self_size: std::mem::size_of::<ff::AVRegionOfInterest>() as u32,
                        top: 0,
                        bottom: (*frame).height,
                        left: 0,
                        right: (*frame).width,
                        qoffset: ff::AVRational {
                            num: -(sharpen.min(51) as i32),
                            den: 51,
                        },
                    };
                }
            }
            (*frame).pts = self.pts;
            self.pts += 1;
            (*frame).pict_type = if keyframe {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            if keyframe {
                (*frame).flags |= ff::AV_FRAME_FLAG_KEY;
            } else {
                (*frame).flags &= !ff::AV_FRAME_FLAG_KEY;
            }
            let r = ff::avcodec_send_frame(self.ctx, frame);
            if !hw_frame.is_null() {
                ff::av_frame_free(&mut hw_frame);
            }
            if r < 0 {
                bail!("send_frame: {}", averr(r));
            }
            let mut key = false;
            loop {
                let r = ff::avcodec_receive_packet(self.ctx, self.pkt);
                if r == ff::AVERROR(libc::EAGAIN) || r == ff::AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    bail!("receive_packet: {}", averr(r));
                }
                let p = &*self.pkt;
                out.extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
                key |= p.flags & ff::AV_PKT_FLAG_KEY != 0;
                ff::av_packet_unref(self.pkt);
            }
            Ok(key)
        }
    }
}

unsafe fn alloc_frame(fmt: ff::AVPixelFormat, w: u32, h: u32) -> Result<*mut ff::AVFrame> {
    unsafe {
        let f = ff::av_frame_alloc();
        if f.is_null() {
            return Err(anyhow!("av_frame_alloc failed"));
        }
        (*f).format = fmt as i32;
        (*f).width = w as i32;
        (*f).height = h as i32;
        let r = ff::av_frame_get_buffer(f, 64);
        if r < 0 {
            let mut f = f;
            ff::av_frame_free(&mut f);
            return Err(anyhow!("av_frame_get_buffer: {}", averr(r))).context("allocating frame");
        }
        Ok(f)
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        self.nv = None;
        unsafe {
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&mut self.ctx);
            }
            for f in [&mut self.src, &mut self.conv] {
                if !f.is_null() {
                    ff::av_frame_free(f);
                }
            }
            for c in [self.sws, self.band_sws] {
                if !c.is_null() {
                    ff::sws_freeContext(c);
                }
            }
            if !self.pkt.is_null() {
                ff::av_packet_free(&mut self.pkt);
            }
            if !self.hw_device.is_null() {
                ff::av_buffer_unref(&mut self.hw_device);
            }
        }
    }
}

/// Which encoders this machine can actually open, for `skylight doctor` and the hello message.
pub fn probe(codecs: &[Codec]) -> Vec<(Codec, String)> {
    let mut out = Vec::new();
    for &c in codecs {
        let p = EncoderParams {
            codec: c,
            width: 256,
            height: 256,
            ..Default::default()
        };
        if let Ok(e) = VideoEncoder::open(&p) {
            out.push((c, e.name.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(enc: &mut VideoEncoder, shift: u32) {
        let (w, h) = (enc.width, enc.height);
        let (buf, stride) = enc.input().unwrap();
        for y in 0..h {
            for x in 0..w {
                let i = y as usize * stride + x as usize * 4;
                buf[i] = ((x + shift) & 255) as u8;
                buf[i + 1] = (y & 255) as u8;
                buf[i + 2] = ((x ^ y) & 255) as u8;
                buf[i + 3] = 255;
            }
        }
    }

    #[test]
    fn software_h264_produces_idr_then_p() {
        let p = EncoderParams {
            width: 320,
            height: 240,
            backend: Backend::Software,
            bitrate: 2_000_000,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open(&p).expect("libx264 should be available");
        let mut out = Vec::new();
        gradient(&mut enc, 0);
        assert!(enc.encode(true, &mut out).unwrap());
        // Annex B with SPS (type 7) in front of the IDR.
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert!(
            out.windows(5)
                .any(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 7)
        );
        out.clear();
        gradient(&mut enc, 3);
        assert!(!enc.encode(false, &mut out).unwrap());
        assert!(!out.is_empty());
        enc.set_bitrate(1_000_000);
        out.clear();
        gradient(&mut enc, 6);
        enc.encode(false, &mut out).unwrap();
        out.clear();
        assert!(enc.encode(true, &mut out).unwrap(), "keyframe on request");
    }

    /// Pseudo-random noise: hard to compress, so rate control and quantization show.
    fn noise(enc: &mut VideoEncoder, seed: u32) -> Vec<u8> {
        let (w, h) = (enc.width as usize, enc.height as usize);
        let (buf, stride) = enc.input().unwrap();
        let mut img = vec![0u8; w * h * 4];
        let mut x = seed.wrapping_mul(2654435761) | 1;
        for y in 0..h {
            for c in 0..w {
                // Smooth gradients plus a little noise, like a photo or a game frame.
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let n = (x & 31) as u8;
                let px = [
                    ((c * 255 / w) as u8).wrapping_add(n),
                    ((y * 255 / h) as u8).wrapping_add(n),
                    (((c + y + seed as usize) * 3) as u8).wrapping_add(n),
                    255,
                ];
                img[(y * w + c) * 4..(y * w + c) * 4 + 4].copy_from_slice(&px);
                buf[y * stride + c * 4..y * stride + c * 4 + 4].copy_from_slice(&px);
            }
        }
        img
    }

    fn software(w: u32, h: u32, bitrate: u64, cq: Option<u8>) -> VideoEncoder {
        let p = EncoderParams {
            width: w,
            height: h,
            bitrate,
            cq,
            vbv_ms: 34,
            backend: Backend::Software,
            ..Default::default()
        };
        VideoEncoder::open_named("libx264", &p).expect("libx264 should be available")
    }

    #[test]
    fn encoders_are_tried_hardware_first_then_software() {
        assert_eq!(
            candidates(Codec::H264, Backend::Auto),
            [
                "nvenc",
                "h264_nvenc",
                "h264_vaapi",
                "libx264",
                "libopenh264"
            ]
        );
        assert_eq!(
            candidates(Codec::H264, Backend::Software),
            ["libx264", "libopenh264"]
        );
        assert_eq!(candidates(Codec::H264, Backend::Vaapi), ["h264_vaapi"]);
        assert_eq!(
            candidates(Codec::Hevc, Backend::Nvenc),
            ["nvenc", "hevc_nvenc"]
        );
    }

    #[test]
    fn each_encoder_says_what_it_can_do() {
        assert_eq!(Caps::of("nvenc", true).rate, RateChange::InPlace);
        assert!(Caps::of("nvenc", true).keyframe_boost);
        assert_eq!(Caps::of("h264_nvenc", false).rate, RateChange::Keyframe);
        assert!(!Caps::of("h264_nvenc", false).sharpen);
        let va = Caps::of("h264_vaapi", false);
        assert_eq!(
            (va.rate, va.sharpen, va.keyframe_boost),
            (RateChange::Reopen, true, false)
        );
        let x264 = Caps::of("libx264", false);
        assert_eq!(
            (x264.rate, x264.sharpen, x264.keyframe_boost),
            (RateChange::InPlace, true, false)
        );
        assert_eq!(Caps::of("libopenh264", false).rate, RateChange::Reopen);
        // Strong refinement only where it is a QP map; every encoder refines three times.
        assert_eq!(Caps::of("nvenc", true).refine, [16, 24, 32]);
        assert_eq!(Caps::of("libx264", false).refine, [4, 8, 8]);
        assert_eq!(Caps::of("h264_vaapi", false).refine.len(), 3);
    }

    #[test]
    fn auto_always_finds_an_encoder() {
        // On a machine without any GPU this is x264; it must never come up empty.
        let p = EncoderParams {
            width: 320,
            height: 240,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open(&p).unwrap();
        gradient(&mut enc, 0);
        let mut out = Vec::new();
        assert!(enc.encode(true, &mut out).unwrap());
    }

    #[test]
    fn converting_only_changed_bands_matches_converting_everything() {
        // 40 rows: the last band overlaps the one before it.
        let mut enc = software(64, 40, 2_000_000, None);
        let planes = |enc: &VideoEncoder| unsafe {
            (0..3)
                .map(|p| {
                    let rows = if p == 0 { 40 } else { 20 };
                    let n = (*enc.conv).linesize[p] as usize * rows;
                    std::slice::from_raw_parts((*enc.conv).data[p], n).to_vec()
                })
                .collect::<Vec<_>>()
        };
        let mut out = Vec::new();
        noise(&mut enc, 1);
        enc.encode(true, &mut out).unwrap();
        for (seed, r) in [
            (2u8, Rect::new(3, 5, 10, 4)),
            (3, Rect::new(40, 30, 20, 10)),
        ] {
            let (buf, stride) = enc.input().unwrap();
            for y in r.y as usize..(r.y + r.h) as usize {
                for x in r.x as usize..(r.x + r.w) as usize {
                    let v = (x * 7 + y * 13) as u8 ^ seed.wrapping_mul(77);
                    buf[y * stride + x * 4..][..4].copy_from_slice(&[v, v / 2, 255 - v, 255]);
                }
            }
            enc.changed(&[r]);
            out.clear();
            enc.encode(false, &mut out).unwrap();
            let banded = planes(&enc);
            enc.whole = true;
            unsafe { enc.convert() };
            assert_eq!(banded, planes(&enc), "change at {r:?}");
        }
    }

    #[test]
    fn x264_changes_bitrate_without_a_keyframe() {
        let mut enc = software(320, 240, 2_000_000, Some(23));
        assert_eq!(enc.caps().rate, RateChange::InPlace);
        let mut out = Vec::new();
        noise(&mut enc, 1);
        assert!(enc.encode(true, &mut out).unwrap());
        let mut sizes = Vec::new();
        for (i, rate) in [6_000_000, 500_000].into_iter().enumerate() {
            enc.set_bitrate(rate);
            let mut total = 0;
            for f in 0..10 {
                noise(&mut enc, 2 + (i * 10 + f) as u32);
                out.clear();
                assert!(
                    !enc.encode(false, &mut out).unwrap(),
                    "change {i} made a keyframe"
                );
                total += out.len();
            }
            sizes.push(total);
        }
        // The lower rate really takes effect.
        assert!(sizes[1] * 3 < sizes[0], "{sizes:?}");
    }

    #[test]
    fn x264_first_keyframe_gets_a_bigger_budget_then_normal_frames() {
        // 1 Mbit/s with a 34 ms VBV, in bitrate mode: a normal frame may be about 4 kB.
        let mut enc = software(640, 360, 1_000_000, None);
        let normal_vbv = 1_000_000 / 8 * 34 / 1000;
        let mut out = Vec::new();
        noise(&mut enc, 1);
        assert!(enc.encode(true, &mut out).unwrap());
        assert!(
            out.len() > normal_vbv * 2,
            "keyframe {} B, normal cap {normal_vbv} B",
            out.len()
        );
        // A couple of frames later everything is held to the normal cap again (with some slack for
        // x264's VBV model).
        let mut sizes = Vec::new();
        for i in 0..6 {
            noise(&mut enc, 10 + i);
            out.clear();
            assert!(!enc.encode(false, &mut out).unwrap());
            sizes.push(out.len());
        }
        assert!(sizes[2..].iter().all(|&b| b < normal_vbv * 2), "{sizes:?}");
    }

    #[test]
    fn x264_first_keyframe_keeps_the_normal_budget_when_asked() {
        let p = EncoderParams {
            width: 640,
            height: 360,
            bitrate: 1_000_000,
            cq: None,
            vbv_ms: 34,
            backend: Backend::Software,
            first_key_boost: false,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open_named("libx264", &p).unwrap();
        let normal_vbv = 1_000_000 / 8 * 34 / 1000;
        let mut out = Vec::new();
        noise(&mut enc, 1);
        assert!(enc.encode(true, &mut out).unwrap());
        assert!(out.len() < normal_vbv * 2, "keyframe {} B", out.len());
    }

    #[test]
    fn sharpening_refines_a_still_image_on_x264() {
        // The case refinement is for: a big change squeezed under a low cap, then the same image
        // re-encoded. With a QP offset the refinement must get measurably closer to the source.
        let run = |sharpen: u8| {
            // 16 Mbit/s: room under the cap for the refinement to spend more bits. Nearer the cap
            // (8 Mbit/s) the gain swings between 0.2 and 1.7 dB with x264's thread count.
            let mut enc = software(640, 360, 16_000_000, Some(23));
            let mut dec = crate::bench::Decoder::new(Codec::H264, 640, 360).unwrap();
            let mut out = Vec::new();
            enc.input().unwrap().0.fill(0);
            enc.encode(true, &mut out).unwrap();
            let img = noise(&mut enc, 7);
            dec.decode_psnr(&out, &img, true).unwrap();
            out.clear();
            enc.encode(false, &mut out).unwrap();
            dec.decode_psnr(&out, &img, true).unwrap();
            let mut psnr = 0.0;
            for _ in 0..3 {
                out.clear();
                enc.encode_with(
                    FrameOpts {
                        keyframe: false,
                        sharpen,
                    },
                    &mut out,
                )
                .unwrap();
                psnr = dec.decode_psnr(&out, &img, true).unwrap();
            }
            psnr
        };
        let (plain, sharp) = (run(0), run(8));
        assert!(
            sharp > plain + 2.0,
            "sharpened {sharp:.2} dB vs plain {plain:.2} dB"
        );
    }

    #[test]
    fn odd_sizes_are_padded() {
        let p = EncoderParams {
            width: 333,
            height: 211,
            backend: Backend::Software,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open(&p).unwrap();
        gradient(&mut enc, 0);
        let mut out = Vec::new();
        assert!(enc.encode(true, &mut out).unwrap());
    }
}

#[cfg(test)]
mod nvenc {
    use super::*;

    /// Needs an NVIDIA GPU: `cargo test -- --ignored`. Documents why the session changes the
    /// bitrate only rarely on FFmpeg's NVENC wrapper (the fallback): every change is a keyframe.
    #[test]
    #[ignore]
    fn bitrate_change_sends_a_keyframe() {
        let p = EncoderParams {
            width: 640,
            height: 360,
            backend: Backend::Nvenc,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open_named("h264_nvenc", &p).unwrap();
        assert_eq!(enc.caps().rate, RateChange::Keyframe);
        let mut out = Vec::new();
        let mut keys = Vec::new();
        for i in 0..6u32 {
            let (buf, _) = enc.input().unwrap();
            buf.iter_mut()
                .enumerate()
                .for_each(|(j, b)| *b = ((j as u32 + i * 7) % 251) as u8);
            if i == 3 {
                enc.set_bitrate(enc.bitrate() * 2);
            }
            out.clear();
            keys.push(enc.encode(i == 0, &mut out).unwrap());
        }
        assert_eq!(keys, [true, false, false, true, false, false]);
    }

    /// The direct NVENC path: bitrate changes in place (no keyframe), and the input buffer keeps
    /// its pixels between frames, so the session only writes what changed.
    #[test]
    #[ignore]
    fn direct_nvenc_keeps_input_and_changes_rate_in_place() {
        let p = EncoderParams {
            width: 640,
            height: 360,
            backend: Backend::Nvenc,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open_named("nvenc", &p).unwrap();
        assert_eq!(enc.caps().rate, RateChange::InPlace);
        let mut out = Vec::new();
        let (buf, _) = enc.input().unwrap();
        buf.iter_mut()
            .enumerate()
            .for_each(|(j, b)| *b = ((j * 7) % 251) as u8);
        assert!(enc.encode(true, &mut out).unwrap());
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        let key = out.len();
        let mut sizes = Vec::new();
        for i in 0..6 {
            if i == 3 {
                enc.set_bitrate(enc.bitrate() / 2);
            }
            out.clear();
            // Nothing written: the same image again must cost next to nothing.
            let _ = enc.input().unwrap();
            assert!(
                !enc.encode(false, &mut out).unwrap(),
                "frame {i} became a keyframe"
            );
            sizes.push(out.len());
        }
        assert!(
            sizes.iter().all(|&s| s < key / 20),
            "unchanged frames should be tiny (key {key}, then {sizes:?})"
        );
    }
}
