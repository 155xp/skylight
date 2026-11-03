//! Video encoding through libavcodec: NVENC first (H.264, HEVC or AV1), then VAAPI, then x264.
//!
//! Every encoder is set up the same way for interactive use: no B-frames, no lookahead, no
//! scene-cut keyframes, an effectively infinite GOP (keyframes only on request), CBR with a
//! short VBV so no single frame can stall the link, and output as soon as each frame is done.

use std::ffi::{CStr, CString};
use std::ptr;

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg_sys_next as ff;
use serde::{Deserialize, Serialize};

use crate::frame::PixelFormat;

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
        }
    }
}

pub struct VideoEncoder {
    ctx: *mut ff::AVCodecContext,
    /// Packed RGB input the caller writes into.
    src: *mut ff::AVFrame,
    /// Converted frame for encoders that need YUV in system memory (x264) or a GPU upload (VAAPI).
    conv: *mut ff::AVFrame,
    hw: *mut ff::AVFrame,
    sws: *mut ff::SwsContext,
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
    let r = unsafe { ff::av_opt_set(ctx as *mut _, k.as_ptr(), v.as_ptr(), ff::AV_OPT_SEARCH_CHILDREN) };
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

/// Names of the encoders to try for a codec and backend, best first.
pub fn candidates(codec: Codec, backend: Backend) -> Vec<&'static str> {
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
        Backend::Nvenc => vec![nv],
        Backend::Vaapi => vec![va],
        Backend::Software => sw.to_vec(),
        Backend::Auto => [&[nv, va][..], sw].concat(),
    }
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
        let cname = CString::new(name).unwrap();
        let codec = unsafe { ff::avcodec_find_encoder_by_name(cname.as_ptr()) };
        if codec.is_null() {
            bail!("not built into this ffmpeg");
        }
        let nvenc = name.ends_with("_nvenc");
        let vaapi = name.ends_with("_vaapi");
        let hardware = nvenc || vaapi;
        let mut enc = VideoEncoder {
            ctx: ptr::null_mut(),
            src: ptr::null_mut(),
            conv: ptr::null_mut(),
            hw: ptr::null_mut(),
            sws: ptr::null_mut(),
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
            } else if vaapi {
                let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
                let node = CString::new(
                    std::env::var("SKYLIGHT_VAAPI_DEVICE").unwrap_or("/dev/dri/renderD128".into()),
                )
                .unwrap();
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
                set_opt(ctx, "rc_mode", "CBR");
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
                if vaapi {
                    enc.hw = ff::av_frame_alloc();
                    let r = ff::av_hwframe_get_buffer((*ctx).hw_frames_ctx, enc.hw, 0);
                    if r < 0 {
                        bail!("VAAPI surface: {}", averr(r));
                    }
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

    /// Change the bitrate cap between frames. x264 reconfigures in place. FFmpeg's NVENC wrapper
    /// resets the encoder and sends a keyframe on every change (see `bitrate_change_is_keyframe`),
    /// so callers should change it rarely there.
    pub fn set_bitrate(&mut self, bitrate: u64) {
        let bitrate = bitrate.clamp(250_000, 400_000_000);
        if bitrate != self.bitrate {
            unsafe { self.apply_rate(bitrate) };
        }
    }

    pub fn bitrate_change_is_keyframe(&self) -> bool {
        self.name.ends_with("_nvenc")
    }

    pub fn bitrate(&self) -> u64 {
        self.bitrate
    }

    /// The packed-RGB frame the caller fills in, and its stride in bytes. Always writable.
    pub fn input(&mut self) -> (&mut [u8], usize) {
        unsafe {
            ff::av_frame_make_writable(self.src);
            let stride = (*self.src).linesize[0] as usize;
            let len = stride * self.height as usize;
            (
                std::slice::from_raw_parts_mut((*self.src).data[0], len),
                stride,
            )
        }
    }

    /// Encode the current input frame. Appends the bitstream (Annex B for H.264/HEVC, OBUs for
    /// AV1) to `out`.
    pub fn encode(&mut self, keyframe: bool, out: &mut Vec<u8>) -> Result<bool> {
        unsafe {
            let frame = if self.conv.is_null() {
                self.src
            } else {
                ff::av_frame_make_writable(self.conv);
                ff::sws_scale(
                    self.sws,
                    (*self.src).data.as_ptr() as *const *const u8,
                    (*self.src).linesize.as_ptr(),
                    0,
                    self.height as i32,
                    (*self.conv).data.as_ptr(),
                    (*self.conv).linesize.as_ptr(),
                );
                if self.hw.is_null() {
                    self.conv
                } else {
                    let r = ff::av_hwframe_transfer_data(self.hw, self.conv, 0);
                    if r < 0 {
                        bail!("upload: {}", averr(r));
                    }
                    self.hw
                }
            };
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
        unsafe {
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&mut self.ctx);
            }
            for f in [&mut self.src, &mut self.conv, &mut self.hw] {
                if !f.is_null() {
                    ff::av_frame_free(f);
                }
            }
            if !self.sws.is_null() {
                ff::sws_freeContext(self.sws);
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
        let (buf, stride) = enc.input();
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
    /// NVENC bitrate only rarely: FFmpeg turns every change into a keyframe.
    #[test]
    #[ignore]
    fn bitrate_change_sends_a_keyframe() {
        let p = EncoderParams {
            width: 640,
            height: 360,
            backend: Backend::Nvenc,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open(&p).unwrap();
        assert!(enc.bitrate_change_is_keyframe());
        let mut out = Vec::new();
        let mut keys = Vec::new();
        for i in 0..6u32 {
            let (buf, _) = enc.input();
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
}
