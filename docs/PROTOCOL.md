# Wire protocol

Two WebSockets per viewer, both on the same origin as the server (`/ws` and `/input`).
Binary messages are little-endian. Control messages are JSON text frames with a `t` field.

## Main socket: `/ws`

### Handshake

1. The client opens the socket and sends, within 5 seconds:
   ```json
   {"t":"hello","ticket":"sk1....","codecs":["h264","hevc","av1"],"maxFps":120,
    "maxBitrate":20000000,"monitor":"HDMI-A-1"}
   ```
   Only `ticket` is required. `codecs` is in preference order; the server takes the first one
   it can encode. `maxFps` should be the display's refresh rate.
2. The server verifies the ticket (see the README) and answers:
   ```json
   {"t":"welcome","server":"skylight 0.1.0","backend":"kde","session":"Ab3dE6gH",
    "monitors":[{"id":"HDMI-A-1","name":"…","x":0,"y":0,"logical_width":1920,
                 "logical_height":1080,"width":1920,"height":1080,"refresh_mhz":144000,"scale":1}],
    "monitor":"HDMI-A-1","scopes":["view","input"],"expires":1762183200,
    "clipboard":true,"input":true,"inputToken":"…"}
   ```
   or `{"t":"error","message":"unauthorized: ticket expired"}` and a close.
3. Before the first frame the server announces the stream, and again whenever it changes:
   ```json
   {"t":"stream","codec":"h264","encoder":"h264_nvenc","hardware":true,"width":1920,
    "height":1080,"fps":120,"monitor":"HDMI-A-1"}
   ```

### Server to client, binary

| Type | Layout |
|---|---|
| `0x01` video | `u8 type, u8 flags, u8 codec, u8 0, u32 frame_id, u16 width, u16 height, u64 capture_us, u32 queue_us, u32 encode_us`, then one access unit. 28-byte header. |
| `0x02` cursor shape | `u8 type, 3×u8 0, u32 serial, u16 w, u16 h, u16 hot_x, u16 hot_y`, then `w×h` RGBA pixels (straight alpha). |
| `0x03` cursor position | `u8 type, u8 visible, 2×u8 0, i32 x, i32 y` in stream pixels. |

Video flags: `1` keyframe, `2` refinement (the same image re-encoded at a higher bitrate once the
screen goes still; leave it out of latency statistics). Codec: `1` H.264, `2` HEVC, `3` AV1.
H.264 and HEVC access units are Annex B; keyframes carry their parameter sets in-band. AV1 is a
temporal unit of OBUs. `capture_us` is the server clock (microseconds since it started) at which
the compositor produced the frame; `queue_us` is capture to encode start, `encode_us` the encode.

### Client to server

| Message | Meaning |
|---|---|
| `0x16` ack: `u8 type, 3×u8 0, u32 frame_id, u32 decode_us` | Sent the moment a frame is decoded. Drives pacing and bitrate. |
| `{"t":"ping","id":1,"c":12345.6}` | Answered with `{"t":"pong","id":1,"c":…,"s":server_us}`; the client uses the tightest round trip to map the server clock onto its own. |
| `{"t":"keyframe"}` | After a decoder error. Rate-limited to 2 per second. |
| `{"t":"settings","maxFps":60,"maxBitrate":8000000,"codec":"hevc"}` | Any subset. Clamped to the server's limits. |
| `{"t":"monitor","id":"DP-1"}` | Switch displays (refused if the ticket names one). |
| `{"t":"clipboard","text":"…"}` | Set the remote clipboard (clipboard scope). |
| `{"t":"bench","action":"start","mode":"click"}` | Open the latency probe window (input scope); `mode` is `move`, `click`, `key` or `animate`. Answered with `{"t":"bench","probe":[x,y,w,h]}` in stream pixels. `"action":"stop"` closes it. |

The server also sends `{"t":"clipboard","text":"…"}` when the desktop's clipboard changes,
`{"t":"stats",…}` once a second (encoder, bitrate target, encode time, round trip, frames skipped
by pacing, and `lumaMax`, the brightest sampled luma 0-255 of the captured picture, which lets the
client notice a decoder that draws black), and `{"t":"error","message":"…"}`.

H.264 parameter sets always declare `max_num_reorder_frames = 0`: the server adds the VUI
`bitstream_restriction` when an encoder leaves it out, because without it a conformant decoder
holds back up to a full DPB of frames (16 at level 5.1) before it shows the first one.

## Input socket: `/input`

Opened right after `welcome`. The first message is `{"token":"<inputToken>"}` (single use). After
that every binary message is one input event, injected as soon as it is read:

| Type | Layout |
|---|---|
| `0x10` pointer absolute | `u8 type, 3×u8 0, f32 x, f32 y` in stream pixels |
| `0x11` pointer relative | `u8 type, 3×u8 0, f32 dx, f32 dy` |
| `0x12` button | `u8 type, u8 button (0 left, 1 middle, 2 right, 3 back, 4 forward), u8 down, u8 0` |
| `0x13` scroll | `u8 type, 3×u8 0, f32 dx, f32 dy` in wheel notches |
| `0x14` key | `u8 type, u8 down, u16 evdev keycode` |
| `0x15` touch | `u8 type, u8 kind (0 down, 1 move, 2 up, 3 cancel), u8 id, u8 0, f32 x, f32 y` |
| `0x17` text | `u8 type`, then UTF-8 text, typed with a US layout (phone keyboards) |

The same messages are accepted on the main socket as a fallback while the input socket is not
open. Keys outside evdev 1–248, and power and sleep keys, are dropped. Every key and button still
held when a session ends is released.
