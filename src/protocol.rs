//! Wire format. Video, cursor and input travel as small binary messages; rare control messages
//! are JSON text frames. All integers are little-endian. See docs/PROTOCOL.md.

use serde::{Deserialize, Serialize};

use crate::capture::{MonitorInfo, TouchKind};

// Server to client.
pub const MSG_VIDEO: u8 = 1;
pub const MSG_CURSOR_SHAPE: u8 = 2;
pub const MSG_CURSOR_POS: u8 = 3;

pub const VIDEO_HEADER: usize = 28;
pub const FLAG_KEY: u8 = 1;
pub const FLAG_REFINE: u8 = 2;

// Client to server.
pub const IN_POINTER_ABS: u8 = 0x10;
pub const IN_POINTER_REL: u8 = 0x11;
pub const IN_BUTTON: u8 = 0x12;
pub const IN_SCROLL: u8 = 0x13;
pub const IN_KEY: u8 = 0x14;
pub const IN_TOUCH: u8 = 0x15;
pub const IN_ACK: u8 = 0x16;
pub const IN_TEXT: u8 = 0x17;

pub struct VideoHeader {
    pub flags: u8,
    pub codec: u8,
    pub frame_id: u32,
    pub width: u16,
    pub height: u16,
    /// When the compositor produced the frame, server clock (µs).
    pub capture_us: u64,
    /// Capture to encoder start (µs).
    pub queue_us: u32,
    /// Encode duration (µs).
    pub encode_us: u32,
}

pub fn video_message(h: &VideoHeader, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(VIDEO_HEADER + payload.len());
    m.extend_from_slice(&[MSG_VIDEO, h.flags, h.codec, 0]);
    m.extend_from_slice(&h.frame_id.to_le_bytes());
    m.extend_from_slice(&h.width.to_le_bytes());
    m.extend_from_slice(&h.height.to_le_bytes());
    m.extend_from_slice(&h.capture_us.to_le_bytes());
    m.extend_from_slice(&h.queue_us.to_le_bytes());
    m.extend_from_slice(&h.encode_us.to_le_bytes());
    debug_assert_eq!(m.len(), VIDEO_HEADER);
    m.extend_from_slice(payload);
    m
}

pub fn cursor_shape_message(
    serial: u32,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    rgba: &[u8],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(16 + rgba.len());
    m.extend_from_slice(&[MSG_CURSOR_SHAPE, 0, 0, 0]);
    m.extend_from_slice(&serial.to_le_bytes());
    m.extend_from_slice(&(w as u16).to_le_bytes());
    m.extend_from_slice(&(h as u16).to_le_bytes());
    m.extend_from_slice(&(hot_x as u16).to_le_bytes());
    m.extend_from_slice(&(hot_y as u16).to_le_bytes());
    m.extend_from_slice(rgba);
    m
}

pub fn cursor_pos_message(x: i32, y: i32, visible: bool) -> Vec<u8> {
    let mut m = Vec::with_capacity(12);
    m.extend_from_slice(&[MSG_CURSOR_POS, visible as u8, 0, 0]);
    m.extend_from_slice(&x.to_le_bytes());
    m.extend_from_slice(&y.to_le_bytes());
    m
}

#[derive(Debug, PartialEq)]
pub enum Input {
    PointerAbs {
        x: f32,
        y: f32,
    },
    PointerRel {
        dx: f32,
        dy: f32,
    },
    /// 0 left, 1 middle, 2 right, 3 back, 4 forward.
    Button {
        button: u8,
        down: bool,
    },
    /// In wheel notches.
    Scroll {
        dx: f32,
        dy: f32,
    },
    Key {
        code: u16,
        down: bool,
    },
    Touch {
        kind: TouchKind,
        id: u8,
        x: f32,
        y: f32,
    },
    Ack {
        frame_id: u32,
        decode_us: u32,
    },
    Text(String),
}

fn f32_at(b: &[u8], i: usize) -> Option<f32> {
    let v = f32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?);
    v.is_finite().then_some(v)
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

pub fn parse_input(b: &[u8]) -> Option<Input> {
    Some(match *b.first()? {
        IN_POINTER_ABS => Input::PointerAbs {
            x: f32_at(b, 4)?,
            y: f32_at(b, 8)?,
        },
        IN_POINTER_REL => Input::PointerRel {
            dx: f32_at(b, 4)?,
            dy: f32_at(b, 8)?,
        },
        IN_BUTTON => Input::Button {
            button: *b.get(1)?,
            down: *b.get(2)? != 0,
        },
        IN_SCROLL => Input::Scroll {
            dx: f32_at(b, 4)?,
            dy: f32_at(b, 8)?,
        },
        IN_KEY => Input::Key {
            code: u16::from_le_bytes(b.get(2..4)?.try_into().ok()?),
            down: *b.get(1)? != 0,
        },
        IN_TOUCH => Input::Touch {
            kind: match *b.get(1)? {
                0 => TouchKind::Down,
                1 => TouchKind::Move,
                2 => TouchKind::Up,
                _ => TouchKind::Cancel,
            },
            id: *b.get(2)?,
            x: f32_at(b, 4)?,
            y: f32_at(b, 8)?,
        },
        IN_ACK => Input::Ack {
            frame_id: u32_at(b, 4)?,
            decode_us: u32_at(b, 8)?,
        },
        IN_TEXT => Input::Text(String::from_utf8(b.get(1..)?.to_vec()).ok()?),
        _ => return None,
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ClientMsg {
    Hello {
        ticket: String,
        #[serde(default)]
        codecs: Vec<String>,
        #[serde(default, rename = "maxFps")]
        max_fps: Option<u32>,
        #[serde(default)]
        monitor: Option<String>,
        #[serde(default, rename = "maxBitrate")]
        max_bitrate: Option<u64>,
        /// What the client's last session on this path settled at (see `Pacer::new`'s start).
        #[serde(default, rename = "startBitrate")]
        start_bitrate: Option<u64>,
    },
    Ping {
        id: u64,
        c: f64,
    },
    Monitor {
        id: String,
    },
    Settings {
        #[serde(default, rename = "maxFps")]
        max_fps: Option<u32>,
        #[serde(default, rename = "maxBitrate")]
        max_bitrate: Option<u64>,
        #[serde(default)]
        codec: Option<String>,
    },
    Keyframe,
    Clipboard {
        text: String,
    },
    Bench {
        action: String,
        #[serde(default)]
        mode: Option<String>,
    },
    /// A fresh one-time token for another input socket, after the last one closed.
    #[serde(rename = "input-token")]
    InputToken,
}

/// First message on the input socket.
#[derive(Debug, Deserialize)]
pub struct Attach {
    pub token: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ServerMsg<'a> {
    Welcome {
        server: &'a str,
        backend: &'a str,
        session: &'a str,
        monitors: &'a [MonitorInfo],
        monitor: &'a str,
        scopes: &'a [String],
        expires: u64,
        clipboard: bool,
        input: bool,
        /// One-time token that attaches the separate input socket (`/input`) to this session.
        #[serde(rename = "inputToken", skip_serializing_if = "Option::is_none")]
        input_token: Option<&'a str>,
    },
    Stream {
        codec: &'a str,
        encoder: &'a str,
        hardware: bool,
        width: u32,
        height: u32,
        fps: u32,
        monitor: &'a str,
    },
    Pong {
        id: u64,
        c: f64,
        s: u64,
    },
    Clipboard {
        text: &'a str,
    },
    Bench {
        probe: Option<[u32; 4]>,
        error: Option<String>,
    },
    #[serde(rename = "input-token")]
    InputToken {
        token: &'a str,
    },
    Error {
        message: &'a str,
    },
}

pub fn json(m: &ServerMsg) -> String {
    serde_json::to_string(m).expect("server message serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inputs() {
        let mut m = vec![IN_POINTER_ABS, 0, 0, 0];
        m.extend_from_slice(&12.5f32.to_le_bytes());
        m.extend_from_slice(&7.0f32.to_le_bytes());
        assert_eq!(parse_input(&m), Some(Input::PointerAbs { x: 12.5, y: 7.0 }));
        assert_eq!(
            parse_input(&[IN_KEY, 1, 30, 0]),
            Some(Input::Key {
                code: 30,
                down: true
            })
        );
        assert_eq!(
            parse_input(&[IN_BUTTON, 2, 0, 0]),
            Some(Input::Button {
                button: 2,
                down: false
            })
        );
        assert_eq!(parse_input(b"\x17hi"), Some(Input::Text("hi".into())));
        // Truncated or non-finite input is dropped, never panics.
        assert_eq!(parse_input(&[IN_POINTER_ABS, 0, 0]), None);
        let mut nan = vec![IN_SCROLL, 0, 0, 0];
        nan.extend_from_slice(&f32::NAN.to_le_bytes());
        nan.extend_from_slice(&0f32.to_le_bytes());
        assert_eq!(parse_input(&nan), None);
        assert_eq!(parse_input(&[]), None);
        assert_eq!(parse_input(&[0xff]), None);
    }

    #[test]
    fn video_header_layout() {
        let h = VideoHeader {
            flags: FLAG_KEY,
            codec: 1,
            frame_id: 7,
            width: 1920,
            height: 1080,
            capture_us: 99,
            queue_us: 3,
            encode_us: 4,
        };
        let m = video_message(&h, &[0xaa]);
        assert_eq!(m.len(), VIDEO_HEADER + 1);
        assert_eq!(&m[..4], &[MSG_VIDEO, FLAG_KEY, 1, 0]);
        assert_eq!(u32::from_le_bytes(m[4..8].try_into().unwrap()), 7);
        assert_eq!(u16::from_le_bytes(m[8..10].try_into().unwrap()), 1920);
        assert_eq!(u64::from_le_bytes(m[12..20].try_into().unwrap()), 99);
        assert_eq!(m[28], 0xaa);
    }

    #[test]
    fn input_token_messages() {
        let m: ClientMsg = serde_json::from_str(r#"{"t":"input-token"}"#).unwrap();
        assert!(matches!(m, ClientMsg::InputToken));
        assert_eq!(
            json(&ServerMsg::InputToken { token: "abc" }),
            r#"{"t":"input-token","token":"abc"}"#
        );
    }

    #[test]
    fn hello_parses() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"t":"hello","ticket":"x","codecs":["h264"],"maxFps":120}"#)
                .unwrap();
        assert!(matches!(
            m,
            ClientMsg::Hello {
                max_fps: Some(120),
                start_bitrate: None,
                ..
            }
        ));
        let m: ClientMsg =
            serde_json::from_str(r#"{"t":"hello","ticket":"x","startBitrate":1500000}"#).unwrap();
        assert!(matches!(
            m,
            ClientMsg::Hello {
                start_bitrate: Some(1_500_000),
                ..
            }
        ));
    }
}
