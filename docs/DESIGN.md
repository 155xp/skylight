# Design

How Skylight captures, encodes, paces and recovers. For the wire format see [PROTOCOL.md](PROTOCOL.md).

## Backends

| | Capture | Input | Clipboard |
|---|---|---|---|
| `kde` (KWin, Wayland) | `zkde_screencast_unstable_v1` → PipeWire memfd, damage and cursor metadata | `org_kde_kwin_fake_input` | `wlr-data-control` |
| `x11` | XDamage, then MIT-SHM (or plain GetImage where shared memory is unavailable); cursor from XFixes | XTEST | CLIPBOARD text, changes noticed through XFixes |
| `synthetic` | moving test pattern | none | none (used by `bench` and tests) |

`auto` picks KWin when a Wayland socket is there and KWin's protocols are offered, then X11. A
GNOME or wlroots Wayland session is refused with a message rather than streamed as a black
screen: its X display is Xwayland, which is not the desktop.

## Encoders

Tried in this order at runtime; the first that opens wins, so a machine without a GPU (or
without the vendor's driver) simply ends up on software. `SKYLIGHT_ENCODER` (or `--encoder`) limits
the search to one family. Two more variables tune it: `SKYLIGHT_VAAPI_DEVICE` picks the render node
VA-API uses (default: every `/dev/dri/renderD*` in turn), and `SKYLIGHT_NVENC_DIRECT=0` uses
FFmpeg's NVENC wrapper instead of driving NVENC directly.

| Encoder | Hardware | Bitrate changes | Sharper refinement frames | Keyframe budget boost |
|---|---|---|---|---|
| NVENC, driven directly (`nvenc-h264` etc.) | NVIDIA | in place | QP offset map | every forced keyframe |
| NVENC through FFmpeg (`h264_nvenc`) | NVIDIA | cost a keyframe, so only big moves, at most every 3 s | no | no |
| VA-API through FFmpeg (`h264_vaapi`) | Intel, AMD | reopen (a keyframe), so only big moves, at most every 3 s | region-of-interest QP offset, where the driver supports ROI | no |
| x264 | none | in place | region-of-interest QP offset, and a budget of their own | first keyframe |
| OpenH264, x265, SVT-AV1 | none | reopen, as VA-API | x265 only | no |

All of them run with no B-frames, no lookahead, no scene-cut keyframes, an infinite GOP
(keyframes on request only) and one frame in the pipeline at a time; the pacing, damage tracking,
refinement and the browser side are the same whatever the encoder. VA-API tries each render node
(machines with an NVIDIA card next to an Intel iGPU have two) and capped VBR before CBR. H.264 is
the default codec; HEVC and AV1 are used when the browser asks for them and an encoder exists.

The VA-API path is written against FFmpeg's API and is exercised by the fallback logic, but it has
not been measured on Intel or AMD hardware yet.

Per encoder, on the same test bed as the benchmarks below (virtual X display, 1920×1080, 40 samples
per input type, medians with p95 in brackets; "glass" is app draw to decoded frame under motion):

| Encoder | Click (ms) | Key (ms) | Glass, animation (ms) | Glass, scrolling text (ms) | Encode (ms) | 10 Mbit/s 40 ms: click / scrolling |
|---|---|---|---|---|---|---|
| NVENC direct | 6.6 (7.0) | 6.4 (7.1) | 6.5 (8.4), 59.7 fps | 6.5 (9.4), 59.2 fps | 1.8 | 48.0 / 30.4 ms, 58.5 fps |
| NVENC through FFmpeg | 8.7 (10.8) | 9.3 (11.0) | 6.5 (8.6), 59.6 fps | 7.3 (10.6), 58.4 fps | 3.0 | 50.0 / 31.6 ms, 58.2 fps |
| x264 (no GPU) | 12.2 (17.8) | 13.8 (20.1) | 8.1 (12.5), 59.7 fps | 8.7 (13.0), 59.6 fps | 5.3 | 56.0 / 34.1 ms, 53.6 fps |

With a viewer connected to a still screen, the server uses 0.1% of a core on every encoder. A build
from a clean container with no NVIDIA driver, CUDA or codec headers (`ci/Dockerfile`) picks x264 on
its own and streams the same test bed at 60 fps.

## Staying up

Skylight watches its own picture and repairs it instead of streaming black:

- **Capture.** Every session's encoder thread checks its capture four times a second. The capture
  is reopened when the source closes or fails (KWin closed the screencast, the PipeWire node went
  away), when it delivers no first frame within 3 s, when the picture has been black for 3 s, when
  the pointer moved and the compositor sent nothing back for 2.5 s, or when the monitor changed
  size or went away. On KWin a reopen alternates between the output stream and a region stream
  over the same monitor, two separate paths through the compositor. Reopens back off (3 s, 10 s,
  30 s, up to 5 min) so a desktop that really is black costs almost nothing. After three failed
  reopens in a row the session ends with an error, so the client reconnects or the host page falls
  back. If the compositor connection itself drops, the server exits and the service restarts it.
- **Encoder.** An encoder that fails twice in a row is skipped for a minute in favour of the next
  one (NVENC, VAAPI, software). The first keyframe from each encoder, then one every 30 s, is
  decoded on the server and compared with the captured picture; an encoder that sends black is
  skipped the same way.
- **Decoder.** H.264 parameter sets always declare that frames are never reordered, so browsers
  that follow the DPB rules strictly (Chromium's hardware decoders among them) show each frame at
  once instead of after up to 16 more. The client also flushes a decoder that holds frames and
  switches to software decoding if the picture stays stuck or black (`health` events, see
  [INTEGRATION.md](../INTEGRATION.md)).
- **Connections.** A connection that dies without closing (a tunnel or a NAT that stops
  forwarding) is noticed from both ends: the client gives up on it after 4 s without a message
  (the server sends stats and answers pings every second) and reconnects at once, and the server
  ends a session after 90 s without a word from its viewer, so the session slot and the encoder are
  free for whoever reconnects. A dropped input socket is replaced, with a fresh one-time token;
  input travels on the main socket meanwhile.
- **`/healthz`** returns JSON: `status` (`ok` or `degraded`), each running capture (size, frames,
  ms since the source last sent anything, black or not, error) and the last recoveries.

## What makes it fast

- **Separate input socket, injected in batches.** Every pointer move, click and key is one tiny
  binary message on `/input`. Whatever has arrived is injected at once, with a backlog of pointer
  moves collapsed to its last position, and flushed once. `TCP_NODELAY` is on everywhere, and
  Chrome's `pointerrawupdate` is used where available.
- **Frames that answer input skip the queue.** For 250 ms after any input, a new frame is encoded
  the instant it exists, ignoring the frame-rate cap. Far away, that holds for clicks, key presses
  and touches, and for the first frame of a drag or a scroll; the rest of a drag goes at the
  display's rate, since over a jittery path faster frames are never seen and make the pacer skip.
- **NVENC without a middleman.** Skylight drives NVENC through NVIDIA's API directly. The session
  writes the damaged rectangles straight into the encoder's own input buffer, so an unchanged
  pixel is never copied twice (1080p encode: 1.9 ms instead of 3.4 ms through FFmpeg). Bitrate
  changes reconfigure the encoder in place, without a keyframe. Encoders share one CUDA context,
  kept for a minute after the last one closes, so a new viewer's encoder opens in 15 ms. FFmpeg's
  NVENC wrapper, VAAPI and x264 remain as fallbacks.
- **Zero-latency encoding.** No B-frames, no lookahead, no scene-cut keyframes, an infinite GOP
  (keyframes only on request), ultra-low-latency tuning, and a VBV of 34 ms so no single frame can
  stall the link. Rate control is VBR capped at the target: a frame that changes a few pixels costs
  a few kilobytes. A forced keyframe gets four times the usual budget for that one frame, so a new
  viewer starts sharp.
- **Capture on demand (X11).** XDamage only marks the screen as changed; the encoder asks for a
  grab at the moment it is ready to encode, so a frame is about 1 ms old when encoding starts, and
  only the box around what changed is read back. Pointer motion arrives as XInput 2 events, so a
  still screen costs nothing. On KDE, KWin's damage rectangles decide what gets copied.
- **Decode on arrival, off the page's thread.** The video socket, a `VideoDecoder` with
  `optimizeForLatency` and an `OffscreenCanvas` live in a worker, so layout, charts or garbage
  collection on the page never delay a frame. Each frame is drawn the moment it is decoded, the
  newest one wins, and it is acknowledged right away. While frames arrive unevenly (a jittery
  tunnel), at most one is drawn per display refresh, so none is drawn over before it is seen and no
  refresh repeats one for want of a frame (`smooth`).
- **Pacing that never builds a queue.** At most one round trip's worth of frames is in flight, and
  if the oldest unacknowledged frame is older than the best recent round trip plus a small
  allowance, the next frame is skipped before it is encoded (so no reference is lost); the damage
  carries over to the next one. Over TCP, that is what keeps a slow link from buffering seconds of
  video. A viewer that stops acknowledging (a hidden tab) is not waited for.
- **Delay-based bitrate.** When round trips rise above their recent minimum, the target drops
  below what the browser actually acknowledged, and that rate is remembered as the path's
  ceiling. A higher round trip counts as a queue only if the stream could be filling it (it uses a
  third of its target, or the path delivers less than it sends), so a tunnel's drifting latency
  does not drag the target down. While the link is clear the target climbs, fast at first,
  carefully near the ceiling, and only while the content uses what it has.
- **Sharp text after motion.** Once the screen has been still for 120 ms (and input has paused for
  300 ms), three refinement frames re-encode the image with a lower QP, one at a time, each after
  the previous frame was acknowledged: by 16, 24 and 32 on NVENC's QP map, where a page of text
  settles near 39 dB, and by 4, 8 and 8 through FFmpeg's region-of-interest offsets, whose rate
  control makes up for larger ones.
- **Client-side cursor.** The cursor travels as metadata, never burned into the video. The browser
  shows it as a CSS cursor with the real shape, so pointer motion has no network latency at all.
  On a phone it is drawn as a sprite you steer like a trackpad.
