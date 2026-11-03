//! H.264 bitstream fix-ups applied to every access unit before it leaves the server.
//!
//! Encoders that never reorder frames (no B-frames) often leave out the SPS VUI
//! `bitstream_restriction` fields. Without them a conformant decoder has to assume that frames
//! may arrive out of order, so it keeps up to a whole DPB of pictures before it shows the first
//! one: 4 frames at 1080p level 4.2, 16 at level 5.1 (what NVENC picks for 1080p at 120 fps).
//! Chromium's hardware decoders do exactly that. A desktop that sits still sends a keyframe and a
//! couple of refinements and nothing more, so such a browser shows a black screen until enough
//! later frames push the first one out. [`no_reorder`] declares `max_num_reorder_frames = 0` in
//! every SPS, the same fix WebRTC applies, and decoders then show each frame as soon as it is
//! decoded.

/// Profiles whose SPS carries the chroma format, bit depths and scaling matrices.
const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

const NAL_SPS: u8 = 7;

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Some(v as u32)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)? as u64;
        Some(((1u64 << zeros) - 1 + rest) as u32)
    }
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    n: u8,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b & 1) as u8;
        self.n += 1;
        if self.n == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.n = 0;
        }
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit(((x >> i) & 1) as u32);
        }
    }

    /// `rbsp_trailing_bits`: a stop bit, then zeros up to the byte boundary.
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        while self.n != 0 {
            self.bit(0);
        }
        self.out
    }
}

/// Reads syntax elements and writes them back unchanged.
struct Copier<'a> {
    r: BitReader<'a>,
    w: BitWriter,
}

impl Copier<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let v = self.r.bits(n)?;
        self.w.bits(v, n);
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bits(1)? == 1)
    }

    fn ue(&mut self) -> Option<u32> {
        let v = self.r.ue()?;
        self.w.ue(v);
        Some(v)
    }

    /// `se(v)` has the same bits as `ue(v)`; only the mapping to a value differs.
    fn se(&mut self) -> Option<()> {
        self.ue().map(|_| ())
    }

    fn scaling_list(&mut self, size: usize) -> Option<()> {
        let (mut last, mut next) = (8i32, 8i32);
        for _ in 0..size {
            if next != 0 {
                let code = self.r.ue()?;
                self.w.ue(code);
                let delta = if code % 2 == 1 {
                    code.div_ceil(2) as i32
                } else {
                    -((code / 2) as i32)
                };
                next = (last + delta + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }

    fn hrd(&mut self) -> Option<()> {
        let cpb_cnt = self.ue()? + 1;
        self.bits(8)?; // bit_rate_scale, cpb_size_scale
        for _ in 0..cpb_cnt.min(32) {
            self.ue()?;
            self.ue()?;
            self.bits(1)?;
        }
        self.bits(20)?; // four 5-bit delay lengths
        Some(())
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
}

/// Copies an SPS up to `bitstream_restriction_flag`, writing the VUI as present even when it
/// was not. Returns `max_num_ref_frames` and whether the original has a bitstream restriction.
fn copy_until_restriction(c: &mut Copier) -> Option<(u32, bool)> {
    let profile = c.bits(8)?;
    c.bits(16)?; // constraint flags, level_idc
    c.ue()?; // seq_parameter_set_id
    if HIGH_PROFILES.contains(&profile) {
        let chroma = c.ue()?;
        if chroma == 3 {
            c.bits(1)?;
        }
        c.ue()?;
        c.ue()?;
        c.bits(1)?;
        if c.flag()? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if c.flag()? {
                    c.scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    c.ue()?; // log2_max_frame_num_minus4
    match c.ue()? {
        0 => {
            c.ue()?;
        }
        1 => {
            c.bits(1)?;
            c.se()?;
            c.se()?;
            for _ in 0..c.ue()?.min(255) {
                c.se()?;
            }
        }
        _ => {}
    }
    let max_refs = c.ue()?;
    c.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    c.ue()?;
    c.ue()?;
    if !c.flag()? {
        c.bits(1)?; // mb_adaptive_frame_field_flag
    }
    c.bits(1)?; // direct_8x8_inference_flag
    if c.flag()? {
        for _ in 0..4 {
            c.ue()?;
        }
    }
    let vui = c.r.bit()? == 1;
    c.w.bit(1);
    if !vui {
        // Aspect ratio, overscan, video signal, chroma location, timing, NAL HRD, VCL HRD and
        // pic_struct: all absent.
        c.w.bits(0, 8);
        return Some((max_refs, false));
    }
    if c.flag()? && c.bits(8)? == 255 {
        c.bits(32)?;
    }
    if c.flag()? {
        c.bits(1)?;
    }
    if c.flag()? {
        c.bits(4)?;
        if c.flag()? {
            c.bits(24)?;
        }
    }
    if c.flag()? {
        c.ue()?;
        c.ue()?;
    }
    if c.flag()? {
        c.bits(32)?;
        c.bits(32)?;
        c.bits(1)?;
    }
    let nal_hrd = c.flag()?;
    if nal_hrd {
        c.hrd()?;
    }
    let vcl_hrd = c.flag()?;
    if vcl_hrd {
        c.hrd()?;
    }
    if nal_hrd || vcl_hrd {
        c.bits(1)?; // low_delay_hrd_flag
    }
    c.bits(1)?; // pic_struct_present_flag
    Some((max_refs, c.r.bit()? == 1))
}

/// The SPS RBSP (after the NAL header) with a `bitstream_restriction` that says frames are never
/// reordered, or `None` when the encoder already describes its reordering or the SPS is not
/// something this parser understands.
fn rewrite_sps(rbsp: &[u8]) -> Option<Vec<u8>> {
    let mut c = Copier {
        r: BitReader { data: rbsp, pos: 0 },
        w: BitWriter::default(),
    };
    let (max_refs, restricted) = copy_until_restriction(&mut c)?;
    if restricted {
        return None;
    }
    c.w.bit(1); // bitstream_restriction_flag
    c.w.bit(1); // motion_vectors_over_pic_boundaries_flag
    c.w.ue(2); // max_bytes_per_pic_denom (the default)
    c.w.ue(1); // max_bits_per_mb_denom (the default)
    c.w.ue(16); // log2_max_mv_length_horizontal (the default)
    c.w.ue(16); // log2_max_mv_length_vertical (the default)
    c.w.ue(0); // max_num_reorder_frames
    c.w.ue(max_refs.max(1)); // max_dec_frame_buffering
    Some(c.w.finish())
}

/// Start-code positions in an Annex B buffer: (prefix start, payload start).
fn start_codes(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let prefix = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
            v.push((prefix, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    v
}

/// Rewrites every SPS in an Annex B access unit so it declares that frames are never reordered
/// (see the module docs). Returns `None` when nothing needed to change. Only the parameter sets
/// before the first slice are looked at, so the cost does not grow with the frame size.
pub fn no_reorder(au: &[u8]) -> Option<Vec<u8>> {
    let codes = start_codes(&au[..au.len().min(4096)]);
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;
    for (k, &(_, start)) in codes.iter().enumerate() {
        let Some(&header) = au.get(start) else { break };
        let kind = header & 0x1f;
        if (1..=5).contains(&kind) {
            break; // slice data from here on
        }
        if kind != NAL_SPS {
            continue;
        }
        let end = codes.get(k + 1).map_or(au.len(), |c| c.0);
        let Some(rbsp) = rewrite_sps(&unescape(&au[start + 1..end])) else {
            continue;
        };
        let o = out.get_or_insert_with(|| Vec::with_capacity(au.len() + 16));
        o.extend_from_slice(&au[copied..=start]);
        escape(&rbsp, o);
        copied = end;
    }
    let mut o = out?;
    o.extend_from_slice(&au[copied..]);
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// `max_num_reorder_frames` and `max_dec_frame_buffering` of an SPS NAL, if it has them.
    fn restriction(nal: &[u8]) -> Option<(u32, u32)> {
        let rbsp = unescape(&nal[1..]);
        let mut c = Copier {
            r: BitReader {
                data: &rbsp,
                pos: 0,
            },
            w: BitWriter::default(),
        };
        if !copy_until_restriction(&mut c)?.1 {
            return None;
        }
        c.r.bit()?;
        for _ in 0..4 {
            c.r.ue()?;
        }
        Some((c.r.ue()?, c.r.ue()?))
    }

    /// SPS from NVENC (High, level 4.2 and 5.1, timing and NAL HRD in the VUI, no
    /// bitstream_restriction), as it sends them for 1080p at 60 and 120 fps.
    const NVENC_SPS_60: &str =
        "6764002aac2b200f0044fcb80b5020205400000fa0000753038000003d090000109a0bbcb828";
    const NVENC_SPS_120: &str =
        "67640033ac2b200f0044fcb80b5020205400000fa0000ea6038000003d090000109a0bbcb828";
    const PPS: &str = "68eb8f2c";

    fn au(sps: &str) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 9, 0xf0];
        v.extend([0, 0, 0, 1]);
        v.extend(hex(sps));
        v.extend([0, 0, 0, 1]);
        v.extend(hex(PPS));
        v.extend([0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01]);
        v
    }

    #[test]
    fn adds_bitstream_restriction_to_nvenc_sps() {
        for sps in [NVENC_SPS_60, NVENC_SPS_120] {
            let input = au(sps);
            let out = no_reorder(&input).expect("rewritten");
            let codes = start_codes(&out);
            let sps_nal = &out[codes[1].1..codes[2].0];
            assert_eq!(restriction(sps_nal), Some((0, 3)));
            // Everything before the SPS payload and from the PPS on is untouched.
            assert_eq!(&out[..codes[1].1 + 4], &input[..codes[1].1 + 4]);
            assert!(out.ends_with(&input[input.len() - 18..]));
            // The fields before the VUI are byte-identical (profile, level, sizes, cropping).
            assert_eq!(&sps_nal[..12], &hex(sps)[..12]);
            // A second pass finds nothing to do.
            assert!(no_reorder(&out).is_none());
        }
    }

    #[test]
    fn leaves_frames_without_sps_alone() {
        let delta = [0, 0, 0, 1, 0x41, 0x9a, 0x00, 0x00, 0x03, 0x00, 0x10];
        assert!(no_reorder(&delta).is_none());
        assert!(no_reorder(&[]).is_none());
    }

    #[test]
    fn sps_without_vui_gets_one() {
        // Constrained Baseline 640x480 with no VUI at all.
        let mut w = BitWriter::default();
        w.bits(66, 8);
        w.bits(0xc0, 8);
        w.bits(30, 8);
        w.ue(0); // seq_parameter_set_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // pic_order_cnt_type
        w.ue(1); // max_num_ref_frames
        w.bit(0);
        w.ue(39);
        w.ue(29);
        w.bit(1); // frame_mbs_only_flag
        w.bit(1);
        w.bit(0); // frame_cropping_flag
        w.bit(0); // vui_parameters_present_flag
        let mut nal = vec![0, 0, 0, 1, 0x67];
        escape(&w.finish(), &mut nal);
        assert_eq!(restriction(&nal[4..]), None);
        let out = no_reorder(&nal).expect("rewritten");
        assert_eq!(restriction(&out[4..]), Some((0, 1)));
        assert!(no_reorder(&out).is_none());
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let raw = [0u8, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7];
        let mut esc = Vec::new();
        escape(&raw, &mut esc);
        assert!(!esc.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
        assert_eq!(unescape(&esc), raw);
    }
}
