//! NVENC driven directly through NVIDIA's Video Codec SDK API (no FFmpeg in between).
//!
//! Going direct buys three things the FFmpeg wrapper cannot do:
//!
//! * **Bitrate changes in place.** `nvEncReconfigureEncoder` without a reset, so the rate
//!   controller can follow the link every few hundred milliseconds instead of paying a keyframe
//!   per change.
//! * **Damage-only input.** The encoder's own input buffer is the frame the session writes its
//!   damaged rectangles into, so an unchanged pixel is never copied again (FFmpeg copies the
//!   whole frame into that buffer on every encode).
//! * **Per-frame quality control.** A QP offset map per frame (sharper refinement frames after
//!   motion stops) and a bigger budget for the one frame that is a forced keyframe.
//!
//! The encoder runs synchronously, one frame at a time, with no reordering and an infinite GOP.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::encode::{Codec, EncoderParams};
use crate::frame::PixelFormat;
use crate::nvenc_sys::*;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}

// From nvEncodeAPI.h (static consts there, so they are not in the generated bindings).
const CODEC_H264: GUID = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const CODEC_HEVC: GUID = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
const CODEC_AV1: GUID = guid(
    0x0a352289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
const PROFILE_H264_HIGH: GUID = guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
const PROFILE_HEVC_MAIN: GUID = guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
const PROFILE_AV1_MAIN: GUID = guid(
    0x5f2a39f5,
    0xf14e,
    0x4f95,
    [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
);
const PRESETS: [GUID; 7] = [
    guid(
        0xfc0a8d3e,
        0x45f8,
        0x4cf8,
        [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
    ),
    guid(
        0xf581cfb8,
        0x88d6,
        0x4381,
        [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab],
    ),
    guid(
        0x36850110,
        0x3a07,
        0x441f,
        [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
    ),
    guid(
        0x90a7b826,
        0xdf06,
        0x4862,
        [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
    ),
    guid(
        0x21c6e6b4,
        0x297a,
        0x4cba,
        [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3],
    ),
    guid(
        0x8e75c279,
        0x6299,
        0x4ab6,
        [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5],
    ),
    guid(
        0x84848c12,
        0x6f71,
        0x4c13,
        [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74],
    ),
];

const fn struct_version(v: u32) -> u32 {
    NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}
const OPEN_SESSION_VER: u32 = struct_version(1);
const CAPS_PARAM_VER: u32 = struct_version(1);
const CREATE_INPUT_VER: u32 = struct_version(1);
const CREATE_BITSTREAM_VER: u32 = struct_version(1);
const LOCK_INPUT_VER: u32 = struct_version(1);
const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
const INIT_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const RECONFIGURE_VER: u32 = struct_version(1) | (1 << 31);
const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);
const FUNCTION_LIST_VER: u32 = struct_version(2);

type Status = NVENCSTATUS;
type Enc = *mut c_void;

/// `NV_ENCODE_API_FUNCTION_LIST`, with the entries Skylight calls typed and the rest opaque.
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    open_encode_session: *mut c_void,
    get_encode_guid_count: *mut c_void,
    get_encode_profile_guid_count: *mut c_void,
    get_encode_profile_guids: *mut c_void,
    get_encode_guids: *mut c_void,
    get_input_format_count: *mut c_void,
    get_input_formats: *mut c_void,
    get_encode_caps:
        Option<unsafe extern "C" fn(Enc, GUID, *mut NV_ENC_CAPS_PARAM, *mut i32) -> Status>,
    get_encode_preset_count: *mut c_void,
    get_encode_preset_guids: *mut c_void,
    get_encode_preset_config: *mut c_void,
    initialize_encoder: Option<unsafe extern "C" fn(Enc, *mut NV_ENC_INITIALIZE_PARAMS) -> Status>,
    create_input_buffer:
        Option<unsafe extern "C" fn(Enc, *mut NV_ENC_CREATE_INPUT_BUFFER) -> Status>,
    destroy_input_buffer: Option<unsafe extern "C" fn(Enc, NV_ENC_INPUT_PTR) -> Status>,
    create_bitstream_buffer:
        Option<unsafe extern "C" fn(Enc, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> Status>,
    destroy_bitstream_buffer: Option<unsafe extern "C" fn(Enc, NV_ENC_OUTPUT_PTR) -> Status>,
    encode_picture: Option<unsafe extern "C" fn(Enc, *mut NV_ENC_PIC_PARAMS) -> Status>,
    lock_bitstream: Option<unsafe extern "C" fn(Enc, *mut NV_ENC_LOCK_BITSTREAM) -> Status>,
    unlock_bitstream: Option<unsafe extern "C" fn(Enc, NV_ENC_OUTPUT_PTR) -> Status>,
    lock_input_buffer: Option<unsafe extern "C" fn(Enc, *mut NV_ENC_LOCK_INPUT_BUFFER) -> Status>,
    unlock_input_buffer: Option<unsafe extern "C" fn(Enc, NV_ENC_INPUT_PTR) -> Status>,
    get_encode_stats: *mut c_void,
    get_sequence_params: *mut c_void,
    register_async_event: *mut c_void,
    unregister_async_event: *mut c_void,
    map_input_resource: *mut c_void,
    unmap_input_resource: *mut c_void,
    destroy_encoder: Option<unsafe extern "C" fn(Enc) -> Status>,
    invalidate_ref_frames: *mut c_void,
    open_encode_session_ex:
        Option<unsafe extern "C" fn(*mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut Enc) -> Status>,
    register_resource: *mut c_void,
    unregister_resource: *mut c_void,
    reconfigure_encoder:
        Option<unsafe extern "C" fn(Enc, *mut NV_ENC_RECONFIGURE_PARAMS) -> Status>,
    reserved1: *mut c_void,
    create_mv_buffer: *mut c_void,
    destroy_mv_buffer: *mut c_void,
    run_motion_estimation_only: *mut c_void,
    get_last_error_string: Option<unsafe extern "C" fn(Enc) -> *const libc::c_char>,
    set_io_cuda_streams: *mut c_void,
    get_encode_preset_config_ex: Option<
        unsafe extern "C" fn(
            Enc,
            GUID,
            GUID,
            NV_ENC_TUNING_INFO,
            *mut NV_ENC_PRESET_CONFIG,
        ) -> Status,
    >,
    get_sequence_param_ex: *mut c_void,
    restore_encoder_state: *mut c_void,
    lookahead_picture: *mut c_void,
    reserved2: [*mut c_void; 275],
}

const _: () = assert!(std::mem::size_of::<FunctionList>() == 8 + (43 + 275) * 8);

/// The two driver libraries, loaded once per process.
struct Api {
    fns: FunctionList,
    cu_ctx_create: unsafe extern "C" fn(*mut *mut c_void, u32, i32) -> i32,
    cu_ctx_destroy: unsafe extern "C" fn(*mut c_void) -> i32,
    cu_ctx_pop: unsafe extern "C" fn(*mut *mut c_void) -> i32,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn dlsym<T>(lib: *mut c_void, name: &str) -> Result<T> {
    let n = CString::new(name).unwrap();
    let p = unsafe { libc::dlsym(lib, n.as_ptr()) };
    if p.is_null() {
        bail!("{name} missing");
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

fn dlopen(name: &str) -> Result<*mut c_void> {
    let n = CString::new(name).unwrap();
    let lib = unsafe { libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if lib.is_null() {
        bail!("{name} not found (no NVIDIA driver?)");
    }
    Ok(lib)
}

/// The driver's libraries are opened at runtime, never linked: a machine without NVIDIA's driver
/// gets an error here and the encoder list moves on to VA-API or software.
fn load() -> Result<Api> {
    load_from("libcuda.so.1", "libnvidia-encode.so.1")
}

fn load_from(cuda_lib: &str, nvenc_lib: &str) -> Result<Api> {
    let cuda = dlopen(cuda_lib)?;
    let nvenc = dlopen(nvenc_lib)?;
    let cu_init: unsafe extern "C" fn(u32) -> i32 = dlsym(cuda, "cuInit")?;
    if unsafe { cu_init(0) } != 0 {
        bail!("cuInit failed");
    }
    let max_version: unsafe extern "C" fn(*mut u32) -> Status =
        dlsym(nvenc, "NvEncodeAPIGetMaxSupportedVersion")?;
    let mut v = 0u32;
    unsafe { max_version(&mut v) };
    let have = (v >> 4, v & 0xf);
    if have < (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION) {
        bail!(
            "the driver supports NVENC API {}.{}, {}.{} is needed",
            have.0,
            have.1,
            NVENCAPI_MAJOR_VERSION,
            NVENCAPI_MINOR_VERSION
        );
    }
    let create: unsafe extern "C" fn(*mut FunctionList) -> Status =
        dlsym(nvenc, "NvEncodeAPICreateInstance")?;
    let mut fns: FunctionList = unsafe { std::mem::zeroed() };
    fns.version = FUNCTION_LIST_VER;
    let r = unsafe { create(&mut fns) };
    if r != NV_ENC_SUCCESS {
        bail!("NvEncodeAPICreateInstance: {r}");
    }
    Ok(Api {
        fns,
        cu_ctx_create: dlsym(cuda, "cuCtxCreate_v2")?,
        cu_ctx_destroy: dlsym(cuda, "cuCtxDestroy_v2")?,
        cu_ctx_pop: dlsym(cuda, "cuCtxPopCurrent_v2")?,
    })
}

fn api() -> Result<&'static Api> {
    static API: std::sync::OnceLock<std::result::Result<Api, String>> = std::sync::OnceLock::new();
    API.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// How long the shared CUDA context outlives the last encoder that used it.
const CONTEXT_LINGER: Duration = Duration::from_secs(60);

/// The CUDA context every encoder in the process opens its session on. Creating one takes about
/// 40 ms, most of what opening an encoder costs, so encoders share one, and it stays for a minute
/// after the last of them closes: a viewer that reconnects, a second viewer or a codec switch gets
/// its encoder without that wait. Then it goes, since it holds driver memory.
struct Context {
    ctx: *mut c_void,
    users: usize,
    /// Bumped each time the last user lets go, so a delayed release only ends its own idle spell.
    idle_epoch: u64,
}

// The handle is only ever passed to the driver, which allows that from any thread.
unsafe impl Send for Context {}

static CONTEXT: Mutex<Option<Context>> = Mutex::new(None);

fn acquire_context(api: &'static Api) -> Result<*mut c_void> {
    let mut slot = CONTEXT.lock().unwrap();
    if let Some(c) = slot.as_mut() {
        c.users += 1;
        return Ok(c.ctx);
    }
    let mut ctx = ptr::null_mut();
    // Device 0; CUDA picks the first NVIDIA GPU.
    if unsafe { (api.cu_ctx_create)(&mut ctx, 0, 0) } != 0 {
        bail!("cuCtxCreate failed");
    }
    // Creating it made it current on this thread. NVENC is handed the context, so leave no thread
    // holding it (this one may end long before the context does).
    let mut popped = ptr::null_mut();
    unsafe { (api.cu_ctx_pop)(&mut popped) };
    *slot = Some(Context {
        ctx,
        users: 1,
        idle_epoch: 0,
    });
    Ok(ctx)
}

fn release_context(api: &'static Api) {
    let mut slot = CONTEXT.lock().unwrap();
    let Some(c) = slot.as_mut() else { return };
    c.users -= 1;
    if c.users > 0 {
        return;
    }
    c.idle_epoch += 1;
    let epoch = c.idle_epoch;
    drop(slot);
    let _ = std::thread::Builder::new()
        .name("cuda-linger".into())
        .spawn(move || {
            std::thread::sleep(CONTEXT_LINGER);
            let mut slot = CONTEXT.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|c| c.users == 0 && c.idle_epoch == epoch)
            {
                let c = slot.take().unwrap();
                unsafe { (api.cu_ctx_destroy)(c.ctx) };
            }
        });
}

/// Per-frame choices beyond "keyframe or not".
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameOpts {
    pub keyframe: bool,
    /// Lower every block's QP by this much (a refinement frame: same image, more detail).
    pub sharpen: u8,
}

pub struct NvEncoder {
    api: &'static Api,
    cuda: *mut c_void,
    enc: Enc,
    input: NV_ENC_INPUT_PTR,
    output: NV_ENC_OUTPUT_PTR,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    config: Box<NV_ENC_CONFIG>,
    locked: Option<(*mut u8, usize)>,
    buffer_format: NV_ENC_BUFFER_FORMAT,
    qp_map: Vec<i8>,
    frame: u64,
    width: u32,
    height: u32,
    bitrate: u64,
    vbv_ms: u32,
    fps: u32,
    /// The cap is raised for this one frame (a keyframe) and restored after it.
    boosted: bool,
    /// The first keyframe gets the raise too (`EncoderParams::first_key_boost`).
    boost_first: bool,
}

unsafe impl Send for NvEncoder {}

impl NvEncoder {
    pub fn open(p: &EncoderParams) -> Result<NvEncoder> {
        let api = api()?;
        let f = &api.fns;
        let cuda = acquire_context(api)?;
        let mut e = NvEncoder {
            api,
            cuda,
            enc: ptr::null_mut(),
            input: ptr::null_mut(),
            output: ptr::null_mut(),
            init: Box::new(unsafe { std::mem::zeroed() }),
            config: Box::new(unsafe { std::mem::zeroed() }),
            locked: None,
            buffer_format: match p.format {
                PixelFormat::Bgrx => NV_ENC_BUFFER_FORMAT_ARGB,
                PixelFormat::Rgbx => NV_ENC_BUFFER_FORMAT_ABGR,
            },
            qp_map: Vec::new(),
            frame: 0,
            width: (p.width + 1) & !1,
            height: (p.height + 1) & !1,
            bitrate: p.bitrate,
            vbv_ms: p.vbv_ms.max(1),
            fps: p.fps.max(1),
            boosted: false,
            boost_first: p.first_key_boost,
        };
        let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { std::mem::zeroed() };
        open.version = OPEN_SESSION_VER;
        open.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
        open.device = cuda;
        open.apiVersion = NVENCAPI_VERSION;
        let r = unsafe { f.open_encode_session_ex.unwrap()(&mut open, &mut e.enc) };
        if r != NV_ENC_SUCCESS {
            e.enc = ptr::null_mut();
            bail!("open session: status {r}");
        }
        let codec_guid = match p.codec {
            Codec::H264 => CODEC_H264,
            Codec::Hevc => CODEC_HEVC,
            Codec::Av1 => CODEC_AV1,
        };
        let preset = PRESETS[(p.nvenc_preset.clamp(1, 7) - 1) as usize];
        let mut pc: NV_ENC_PRESET_CONFIG = unsafe { std::mem::zeroed() };
        pc.version = PRESET_CONFIG_VER;
        pc.presetCfg.version = CONFIG_VER;
        e.check(unsafe {
            f.get_encode_preset_config_ex.unwrap()(
                e.enc,
                codec_guid,
                preset,
                NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut pc,
            )
        })
        .map_err(|err| anyhow!("{err} (does this GPU encode {:?}?)", p.codec))?;
        *e.config = pc.presetCfg;
        let c = &mut *e.config;
        c.version = CONFIG_VER;
        c.gopLength = NVENC_INFINITE_GOPLENGTH;
        c.frameIntervalP = 1;
        let rc = &mut c.rcParams;
        rc.rateControlMode = if p.cq.is_some() {
            NV_ENC_PARAMS_RC_VBR
        } else {
            NV_ENC_PARAMS_RC_CBR
        };
        rc.multiPass = NV_ENC_MULTI_PASS_DISABLED;
        rc.set_enableLookahead(0);
        rc.lookaheadDepth = 0;
        rc.set_zeroReorderDelay(1);
        rc.set_enableAQ(1);
        rc.set_disableIadapt(1);
        rc.qpMapMode = NV_ENC_QP_MAP_DELTA;
        unsafe {
            match p.codec {
                Codec::H264 => {
                    c.profileGUID = PROFILE_H264_HIGH;
                    let h = &mut c.encodeCodecConfig.h264Config;
                    h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    h.set_repeatSPSPPS(1);
                    h.set_outputAUD(0);
                    h.set_enableIntraRefresh(0);
                    h.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
                    h.chromaFormatIDC = 1;
                }
                Codec::Hevc => {
                    c.profileGUID = PROFILE_HEVC_MAIN;
                    let h = &mut c.encodeCodecConfig.hevcConfig;
                    h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    h.set_repeatSPSPPS(1);
                    h.set_outputAUD(0);
                    h.set_enableIntraRefresh(0);
                    h.set_chromaFormatIDC(1);
                }
                Codec::Av1 => {
                    c.profileGUID = PROFILE_AV1_MAIN;
                    let a = &mut c.encodeCodecConfig.av1Config;
                    a.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    a.set_repeatSeqHdr(1);
                    a.set_outputAnnexBFormat(0);
                    a.set_enableIntraRefresh(0);
                    a.set_chromaFormatIDC(1);
                }
            }
        }
        let init = &mut *e.init;
        init.version = INIT_PARAMS_VER;
        init.encodeGUID = codec_guid;
        init.presetGUID = preset;
        init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.encodeWidth = e.width;
        init.encodeHeight = e.height;
        init.darWidth = e.width;
        init.darHeight = e.height;
        init.maxEncodeWidth = e.width;
        init.maxEncodeHeight = e.height;
        init.frameRateNum = e.fps;
        init.frameRateDen = 1;
        init.enablePTD = 1;
        init.bufferFormat = e.buffer_format;
        init.encodeConfig = &mut *e.config;
        e.apply_rate(e.bitrate, e.normal_vbv(e.bitrate));
        let r = unsafe { f.initialize_encoder.unwrap()(e.enc, &mut *e.init) };
        e.check(r)?;

        let mut ib: NV_ENC_CREATE_INPUT_BUFFER = unsafe { std::mem::zeroed() };
        ib.version = CREATE_INPUT_VER;
        ib.width = e.width;
        ib.height = e.height;
        ib.bufferFmt = e.buffer_format;
        let r = unsafe { f.create_input_buffer.unwrap()(e.enc, &mut ib) };
        e.check(r)?;
        e.input = ib.inputBuffer;
        let mut ob: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
        ob.version = CREATE_BITSTREAM_VER;
        let r = unsafe { f.create_bitstream_buffer.unwrap()(e.enc, &mut ob) };
        e.check(r)?;
        e.output = ob.bitstreamBuffer;

        // One QP offset per macroblock (H.264), CTB (HEVC, 32 px) or superblock (AV1, 64 px).
        let block = match p.codec {
            Codec::H264 => 16,
            Codec::Hevc => 32,
            Codec::Av1 => 64,
        };
        e.qp_map = vec![0; (e.width.div_ceil(block) * e.height.div_ceil(block)) as usize];
        // Start from black, like a freshly allocated FFmpeg frame.
        let (buf, _) = e.input()?;
        buf.fill(0);
        Ok(e)
    }

    fn check(&self, r: Status) -> Result<()> {
        if r == NV_ENC_SUCCESS {
            return Ok(());
        }
        let msg = if self.enc.is_null() {
            String::new()
        } else {
            unsafe {
                let s = self.api.fns.get_last_error_string.unwrap()(self.enc);
                if s.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned()
                }
            }
        };
        bail!("NVENC status {r}: {msg}")
    }

    /// Whether this GPU changes bitrate without a new keyframe.
    pub fn dynamic_bitrate(&self) -> bool {
        let mut caps: NV_ENC_CAPS_PARAM = unsafe { std::mem::zeroed() };
        caps.version = CAPS_PARAM_VER;
        caps.capsToQuery = NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE;
        let mut v = 0;
        let r = unsafe {
            self.api.fns.get_encode_caps.unwrap()(self.enc, self.init.encodeGUID, &mut caps, &mut v)
        };
        r == NV_ENC_SUCCESS && v != 0
    }

    /// The rate cap and the VBV (the largest any one frame may be), both in bits.
    fn apply_rate(&mut self, bitrate: u64, vbv: u64) {
        let rc = &mut self.config.rcParams;
        let b = bitrate.min(u32::MAX as u64) as u32;
        let vbv = vbv.min(u32::MAX as u64) as u32;
        rc.averageBitRate = b;
        rc.maxBitRate = b;
        rc.vbvBufferSize = vbv;
        rc.vbvInitialDelay = vbv;
    }

    fn normal_vbv(&self, bitrate: u64) -> u64 {
        bitrate * self.vbv_ms as u64 / 1000
    }

    fn reconfigure(&mut self) -> Result<()> {
        let mut rp: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        rp.version = RECONFIGURE_VER;
        rp.reInitEncodeParams = *self.init;
        rp.reInitEncodeParams.encodeConfig = &mut *self.config;
        // resetEncoder and forceIDR stay 0: the stream simply continues at the new rate.
        let r = unsafe { self.api.fns.reconfigure_encoder.unwrap()(self.enc, &mut rp) };
        self.check(r)
    }

    pub fn bitrate(&self) -> u64 {
        self.bitrate
    }

    /// Change the bitrate cap between frames, in place.
    pub fn set_bitrate(&mut self, bitrate: u64) -> Result<()> {
        if bitrate == self.bitrate && !self.boosted {
            return Ok(());
        }
        self.bitrate = bitrate;
        self.boosted = false;
        self.apply_rate(bitrate, self.normal_vbv(bitrate));
        self.reconfigure()
    }

    /// The frame the caller fills in (padded size), and its stride in bytes.
    pub fn input(&mut self) -> Result<(&mut [u8], usize)> {
        if self.locked.is_none() {
            let mut l: NV_ENC_LOCK_INPUT_BUFFER = unsafe { std::mem::zeroed() };
            l.version = LOCK_INPUT_VER;
            l.inputBuffer = self.input;
            self.check(unsafe { self.api.fns.lock_input_buffer.unwrap()(self.enc, &mut l) })?;
            self.locked = Some((l.bufferDataPtr as *mut u8, l.pitch as usize));
        }
        let (p, pitch) = self.locked.unwrap();
        Ok((
            unsafe { std::slice::from_raw_parts_mut(p, pitch * self.height as usize) },
            pitch,
        ))
    }

    /// Encode the current input frame and append its bitstream to `out`; returns whether it
    /// is a keyframe.
    pub fn encode(&mut self, opts: FrameOpts, out: &mut Vec<u8>) -> Result<bool> {
        let (_, pitch) = self.input()?;
        let f = &self.api.fns;
        self.check(unsafe { f.unlock_input_buffer.unwrap()(self.enc, self.input) })?;
        self.locked = None;

        if opts.keyframe && !self.boosted && (self.frame > 0 || self.boost_first) {
            // A forced keyframe repaints everything at once. Give that one frame four times the
            // usual budget, so it comes out sharp instead of smeared, then drop back on the next.
            let rate = (self.bitrate * 4).max(16_000_000);
            self.apply_rate(rate, 4 * self.normal_vbv(self.bitrate));
            self.reconfigure()?;
            self.boosted = true;
        } else if !opts.keyframe && self.boosted {
            let b = self.bitrate;
            self.set_bitrate(b)?;
        }

        let mut pp: NV_ENC_PIC_PARAMS = unsafe { std::mem::zeroed() };
        pp.version = PIC_PARAMS_VER;
        pp.inputWidth = self.width;
        pp.inputHeight = self.height;
        pp.inputPitch = pitch as u32;
        pp.inputBuffer = self.input;
        pp.outputBitstream = self.output;
        pp.bufferFmt = self.buffer_format;
        pp.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        pp.inputTimeStamp = self.frame;
        pp.frameIdx = self.frame as u32;
        self.frame += 1;
        if opts.keyframe {
            pp.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        if opts.sharpen > 0 {
            self.qp_map.fill(-(opts.sharpen.min(51) as i8));
            pp.qpDeltaMap = self.qp_map.as_mut_ptr();
            pp.qpDeltaMapSize = self.qp_map.len() as u32;
        }
        let r = unsafe { f.encode_picture.unwrap()(self.enc, &mut pp) };
        if r == NV_ENC_ERR_NEED_MORE_INPUT {
            return Ok(false);
        }
        self.check(r)?;
        let mut lb: NV_ENC_LOCK_BITSTREAM = unsafe { std::mem::zeroed() };
        lb.version = LOCK_BITSTREAM_VER;
        lb.outputBitstream = self.output;
        self.check(unsafe { f.lock_bitstream.unwrap()(self.enc, &mut lb) })?;
        out.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                lb.bitstreamBufferPtr as *const u8,
                lb.bitstreamSizeInBytes as usize,
            )
        });
        let key = lb.pictureType == NV_ENC_PIC_TYPE_IDR || lb.pictureType == NV_ENC_PIC_TYPE_I;
        self.check(unsafe { f.unlock_bitstream.unwrap()(self.enc, self.output) })?;
        Ok(key)
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        let f = &self.api.fns;
        unsafe {
            if !self.enc.is_null() {
                if self.locked.take().is_some() {
                    f.unlock_input_buffer.unwrap()(self.enc, self.input);
                }
                if !self.input.is_null() {
                    f.destroy_input_buffer.unwrap()(self.enc, self.input);
                }
                if !self.output.is_null() {
                    f.destroy_bitstream_buffer.unwrap()(self.enc, self.output);
                }
                f.destroy_encoder.unwrap()(self.enc);
            }
            if !self.cuda.is_null() {
                release_context(self.api);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_driver_is_an_error_not_a_crash() {
        let e = load_from(
            "libcuda-not-installed.so.1",
            "libnvidia-encode-not-installed.so.1",
        )
        .err()
        .expect("loading a library that does not exist must fail");
        assert!(format!("{e:#}").contains("not found"), "{e:#}");
    }
}
