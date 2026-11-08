# Wire protocol

Two WebSockets per viewer, both on the same origin as the server (`/ws` and `/input`).
Binary messages are little-endian. Control messages are JSON text frames with a `t` field.

## Main socket: `/ws`

### Handshake

1. The client opens the socket and sends, within 5 seconds:
   ```json
   {"t":"hello","ticket":"sk1....","codecs":["h264","hevc","av1"],"maxFps":120,
    "maxBitrate":20000000,"startBitrate":2400000,"monitor":"HDMI-A-1"}
   ```
   Only `ticket` is required. `codecs` is in preference order; the server takes the first one
   it can encode. `maxFps` should be the display's refresh rate. `startBitrate` is where the
   bitrate controller starts (bits per second, within the server's limits): the target the
   last session to this server settled on, so a slow link's first keyframe is sized for it.
2. The server verifies the ticket (see the README) and answers:
   ```json
   {"t":"welcome","server":"skylight 0.1.0","backend":"kde","session":"Ab3dE6gH",
    "monitors":[{"id":"HDMI-A-1","name":"…","x":0,"y":0,"logical_width":1920,
                 "logical_height":1080,"width":1920,"height":1080,"refresh_mhz":144000,"scale":1}],
    "monitor":"HDMI-A-1","scopes":["view","input"],"expires":1762183200,
    "clipboard":true,"clipboardMax":1048576,"input":true,"inputToken":"…"}
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
| `{"t":"clipboard","text":"…","paste":true}` | Set the remote clipboard (clipboard scope); with `paste` (and the input scope), also press Ctrl+V once it is set, or only V while the session holds a Ctrl. Keep the text within `clipboardMax` bytes as JSON (UTF-8, escaped): a longer text is refused with an `error`, and a message over `clipboardMax` + 4 KiB ends the session. |
| `{"t":"input-token"}` | Ask for a fresh one-time token after the input socket closed (input scope). Answered with `{"t":"input-token","token":"…"}`; it replaces the previous token, spent or not. |
| `{"t":"bench","action":"start","mode":"click"}` | Open the latency probe window (input scope); `mode` is `move`, `click`, `key`, `animate`, `scroll`, `page` (a page of text that turns to a new page on each click, for image quality) or `follow` (a square that follows the pointer, redrawn only when it moves). Answered with `{"t":"bench","probe":[x,y,w,h]}` in stream pixels. `"action":"stop"` closes it. In `animate`, `scroll` and `follow` modes, the top 48 rows carry a time code: 32 cells of 14×24 px, the server clock in units of 100 µs (black is a 1, most significant bit first), with the complement in the row below. |

The server also sends `{"t":"clipboard","text":"…"}` when the desktop's clipboard changes,
`{"t":"stats",…}` once a second (encoder, bitrate target, encode time, round trip, frames skipped
by pacing, and `lumaMax`, the brightest sampled luma 0-255 of the captured picture, which lets the
client notice a decoder that draws black), and `{"t":"error","message":"…"}`.

H.264 parameter sets always declare `max_num_reorder_frames = 0`: the server adds the VUI
`bitstream_restriction` when an encoder leaves it out, because without it a conformant decoder
holds back up to a full DPB of frames (16 at level 5.1) before it shows the first one.

Pacing: the server keeps at most about one round trip of frames unacknowledged. A viewer that
stops acknowledging for 3 seconds (a hidden tab, a reset decoder) is not waited for: the server
forgets what it had in flight and sends a keyframe. A `keyframe` request does the same at once.

## Input socket: `/input`

Opened right after `welcome`. The first message is `{"token":"<inputToken>"}` (single use). After
that every binary message is one input event, injected as soon as it is read. When the socket
closes while the session goes on (tunnels drop one now and then), the client asks for another token
with `input-token` on the main socket and opens a new one; a session can have up to four attached
at a time.

| Type | Layout |
|---|---|
| `0x10` pointer absolute | `u8 type, 3×u8 0, f32 x, f32 y` in stream pixels |
| `0x12` button | `u8 type, u8 button (0 left, 1 middle, 2 right, 3 back, 4 forward), u8 down, u8 0` |
| `0x13` scroll | `u8 type, 3×u8 0, f32 dx, f32 dy` in wheel notches |
| `0x14` key | `u8 type, u8 down, u16 evdev keycode` |
| `0x15` touch | `u8 type, u8 kind (0 down, 1 move, 2 up, 3 cancel), u8 id, u8 0, f32 x, f32 y` |
| `0x17` text | `u8 type`, then UTF-8 text, typed with a US layout (phone keyboards) |

Events that arrive together are injected together, with runs of pointer moves collapsed to the
last one. The same messages are accepted on the main socket as a fallback while the input socket
is not open. Keys outside evdev 1–248, power, sleep and radio (WLAN, WWAN, RFKILL) keys are
dropped. Every key, button and touch still held when a session ends is released, and nothing is
injected after that. (`0x11`, relative pointer motion, was removed: it could move the pointer off
the monitor a ticket is limited to.)

## HTTP: `/api/screenshot` and `/api/wait`

For programs that drive the desktop (agents, tests). Both take a ticket with the `view` scope as
`Authorization: Bearer <ticket>` (single use, like any ticket), and an optional `monitor=ID`.

- `GET /api/screenshot` returns the current image as PNG, with `X-Skylight-Seq` set to the frame
  store's sequence number.
- `GET /api/wait` returns `{"settled":true,"seq":N,"waitedMs":M}` once the screen has been still
  for `idle` ms (default 300). With `change`, it returns `{"changed":true,…}` at the first change
  after `since` (a sequence number, default: when the request arrived), so a change that lands
  between a screenshot and the wait is not missed. `region=x,y,w,h` (stream pixels) limits both to
  part of the screen; `timeout` (ms, default 10000) bounds the wait, after which the answer has
  `false`. It is answered from the capture's damage tracking, without screenshots or polling.

```sh
curl -H "Authorization: Bearer $(mint)" http://127.0.0.1:5490/api/screenshot > screen.png
curl -H "Authorization: Bearer $(mint)" "http://127.0.0.1:5490/api/wait?idle=500"
```
