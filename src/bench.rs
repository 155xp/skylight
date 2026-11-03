//! `skylight bench`: capture-to-encoded timing on this machine, plus how much the idle
//! refinement frames sharpen a still image (PSNR measured by decoding the stream back).

use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;

use crate::capture::{self, Hub};
use crate::clock;
use crate::encode::{self, Codec, EncoderParams, VideoEncoder};
use crate::frame::{Signal, WAKE_FRAME};

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

pub fn run(
    backend: &str,
    seconds: u64,
    enc_backend: encode::Backend,
    codec: Codec,
    bitrate: u64,
    preset: u8,
    cq: Option<u8>,
) -> Result<()> {
    let b = capture::open_backend(backend, 144)?;
    let hub = Hub::new(b);
    let mon = hub.resolve(None)?;
    let cap = hub.capture(&mon)?;
    let sig = Signal::new();
    cap.store.subscribe(&sig);
    let deadline = Instant::now() + Duration::from_secs(5);
    while cap.store.shape().0 == 0 {
        if Instant::now() > deadline {
            bail!("no frame from the capture backend within 5 s");
        }
        sig.wait(Duration::from_millis(100));
    }
    let (w, h, format, _) = cap.store.shape();
    let fps = (mon.refresh_mhz / 1000).max(30);
    let params = EncoderParams {
        codec,
        width: w,
        height: h,
        fps,
        bitrate,
        format,
        backend: enc_backend,
        nvenc_preset: preset,
        cq,
        ..Default::default()
    };
    let mut enc = VideoEncoder::open(&params)?;
    println!(
        "source:  {} {}x{} ({} backend), encoder {} at {:.0} Mbit/s, {} fps cap",
        mon.id,
        w,
        h,
        backend,
        enc.name,
        bitrate as f64 / 1e6,
        fps
    );

    let (mut last_seq, mut last_gen) = (0, u32::MAX);
    let mut encode_ms = Vec::new();
    let mut total_ms = Vec::new();
    let mut sizes = Vec::new();
    let mut out = Vec::new();
    let mut key = true;
    let start = Instant::now();
    let mut first_key_bytes = 0;
    while start.elapsed() < Duration::from_secs(seconds) {
        let (buf, stride) = enc.input()?;
        let r = cap.store.sync_into(last_seq, last_gen, w, h, buf, stride);
        if r.size_mismatch {
            bail!("the source changed size during the benchmark");
        }
        if r.damage.is_empty() && !key {
            sig.wait(Duration::from_millis(50));
            continue;
        }
        last_seq = r.seq;
        last_gen = r.generation;
        let t0 = Instant::now();
        out.clear();
        enc.encode(key, &mut out)?;
        let e = t0.elapsed().as_secs_f64() * 1000.0;
        if key {
            first_key_bytes = out.len();
        } else {
            encode_ms.push(e);
            total_ms.push((clock::now_us().saturating_sub(r.capture_us)) as f64 / 1000.0);
            sizes.push(out.len() as f64);
        }
        key = false;
        let _ = sig.wait(Duration::from_millis(0)) & WAKE_FRAME;
    }
    let n = encode_ms.len();
    if n == 0 {
        println!(
            "no frames changed during the run (an idle desktop sends nothing); move a window and retry"
        );
    } else {
        let secs = start.elapsed().as_secs_f64();
        let bytes: f64 = sizes.iter().sum();
        println!(
            "frames:  {n} in {secs:.1} s ({:.1} fps of changed content)",
            n as f64 / secs
        );
        println!(
            "encode:  mean {:.2} ms, p50 {:.2}, p95 {:.2}, p99 {:.2}, max {:.2}",
            encode_ms.iter().sum::<f64>() / n as f64,
            pct(&mut encode_ms, 0.5),
            pct(&mut encode_ms, 0.95),
            pct(&mut encode_ms, 0.99),
            pct(&mut encode_ms, 1.0)
        );
        println!(
            "capture to encoded: p50 {:.2} ms, p95 {:.2} ms (includes waiting for the copy and the encode)",
            pct(&mut total_ms, 0.5),
            pct(&mut total_ms, 0.95)
        );
        println!(
            "size:    keyframe {:.1} kB, delta mean {:.1} kB, p95 {:.1} kB, {:.1} Mbit/s",
            first_key_bytes as f64 / 1000.0,
            bytes / n as f64 / 1000.0,
            pct(&mut sizes, 0.95) / 1000.0,
            bytes * 8.0 / secs / 1e6
        );
    }
    drop(enc);

    // Still-image quality.
    let mut enc = VideoEncoder::open(&params)?;
    let mut img = vec![0u8; w as usize * h as usize * 4];
    let (buf, stride) = enc.input()?;
    let r = cap.store.sync_into(0, u32::MAX, w, h, buf, stride);
    if r.size_mismatch {
        return Ok(());
    }
    for y in 0..h as usize {
        img[y * w as usize * 4..(y + 1) * w as usize * 4]
            .copy_from_slice(&buf[y * stride..y * stride + w as usize * 4]);
    }
    let mut dec = Decoder::new(codec, w, h)?;
    let mut stream = Vec::new();
    let mut report = Vec::new();
    // A keyframe of the image (what a new viewer gets), then the case refinement is for: the
    // image arriving as one big change on top of a black screen, squeezed under the frame cap,
    // followed by the refinement frames the session sends once the screen is still.
    stream.clear();
    enc.encode(true, &mut stream)?;
    let psnr = dec.decode_psnr(&stream, &img, format == crate::frame::PixelFormat::Bgrx)?;
    report.push(format!(
        "keyframe {psnr:.1} dB ({:.1} kB)",
        stream.len() as f64 / 1000.0
    ));
    drop(enc);
    let mut enc = VideoEncoder::open(&params)?;
    let mut dec = Decoder::new(codec, w, h)?;
    enc.input()?.0.fill(0);
    stream.clear();
    enc.encode(true, &mut stream)?;
    dec.decode_psnr(&stream, &img, true)?;
    let (buf, stride) = enc.input()?;
    for y in 0..h as usize {
        buf[y * stride..y * stride + w as usize * 4]
            .copy_from_slice(&img[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    for (i, label) in ["change", "refine 1", "refine 2", "refine 3"]
        .iter()
        .enumerate()
    {
        stream.clear();
        let sharpen = if i == 0 {
            0
        } else {
            crate::session::refine_sharpen(i as u8 - 1)
        };
        enc.encode_with(
            crate::nvenc::FrameOpts {
                keyframe: false,
                sharpen,
            },
            &mut stream,
        )?;
        let psnr = dec.decode_psnr(&stream, &img, format == crate::frame::PixelFormat::Bgrx)?;
        report.push(format!(
            "{label}: {:.1} dB ({:.1} kB)",
            psnr,
            stream.len() as f64 / 1000.0
        ));
    }
    println!("still image PSNR (luma): {}", report.join(", "));
    Ok(())
}

/// Software decoder used only to measure what the browser would see.
struct Decoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
}

impl Decoder {
    fn new(codec: Codec, _w: u32, _h: u32) -> Result<Decoder> {
        let id = match codec {
            Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
        };
        unsafe {
            let c = ff::avcodec_find_decoder(id);
            if c.is_null() {
                bail!("no decoder for {codec:?}");
            }
            let ctx = ff::avcodec_alloc_context3(c);
            if ff::avcodec_open2(ctx, c, ptr::null_mut()) < 0 {
                bail!("cannot open the decoder");
            }
            Ok(Decoder {
                ctx,
                frame: ff::av_frame_alloc(),
                pkt: ff::av_packet_alloc(),
            })
        }
    }

    /// Decode one access unit and return luma PSNR against the BGRx/RGBx source.
    fn decode_psnr(&mut self, data: &[u8], src: &[u8], bgr: bool) -> Result<f64> {
        unsafe {
            ff::av_new_packet(self.pkt, data.len() as i32);
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            if ff::avcodec_send_packet(self.ctx, self.pkt) < 0 {
                bail!("decoder rejected the packet");
            }
            ff::av_packet_unref(self.pkt);
            if ff::avcodec_receive_frame(self.ctx, self.frame) < 0 {
                bail!("decoder produced no frame");
            }
            let f = &*self.frame;
            let (w, h) = (f.width as usize, f.height as usize);
            let mut se = 0f64;
            let mut n = 0usize;
            let src_w = (src.len() / 4) / h.max(1);
            for y in 0..h {
                let row = std::slice::from_raw_parts(f.data[0].add(y * f.linesize[0] as usize), w);
                for (x, &yv) in row.iter().enumerate().take(src_w) {
                    let p = &src[(y * src_w + x) * 4..];
                    let (r, g, b) = if bgr {
                        (p[2], p[1], p[0])
                    } else {
                        (p[0], p[1], p[2])
                    };
                    // BT.601 limited range, the matrix the encoders signal.
                    let yr =
                        16.0 + (65.481 * r as f64 + 128.553 * g as f64 + 24.966 * b as f64) / 255.0;
                    let d = yv as f64 - yr;
                    se += d * d;
                    n += 1;
                }
            }
            let mse = se / n.max(1) as f64;
            Ok(if mse <= 1e-9 {
                99.0
            } else {
                10.0 * (255.0f64 * 255.0 / mse).log10()
            })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
        }
    }
}
