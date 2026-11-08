//! What a session does to the desktop's input: injecting a viewer's events in stream pixels,
//! remembering what it holds so all of it is released when the session ends, and when input came
//! (the encoder paces itself by it). Also the keys a session may send, and typed text (from a
//! phone keyboard) turned into key presses.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::capture::{self, InputSink, MonitorInfo, TouchKind};
use crate::clock;
use crate::protocol::Input;

/// evdev codes used here.
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_ENTER: u32 = 28;
pub const KEY_TAB: u32 = 15;
pub const KEY_SPACE: u32 = 57;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_V: u32 = 47;

/// The evdev keycode and whether Shift is needed to type `c` on a US layout.
pub fn us_key(c: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const DIGITS: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => (KEY_SPACE, false),
        '\n' | '\r' => (KEY_ENTER, false),
        '\t' => (KEY_TAB, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        // Smart punctuation from phone keyboards, typed as its plain equivalent.
        '\u{2018}' | '\u{2019}' => (40, false),
        '\u{201c}' | '\u{201d}' => (40, true),
        '\u{2013}' | '\u{2014}' => (12, false),
        _ => return None,
    })
}

/// A session may press any key on a normal keyboard (evdev 1..=248) but nothing beyond, and none
/// of power, sleep, wake, suspend or the radio switches (WLAN, WWAN, RFKILL): pressed remotely,
/// those cut off the very session that sent them, with no way to undo it from afar.
pub fn key_allowed(code: u32) -> bool {
    (1..=248).contains(&code) && !matches!(code, 116 | 142 | 143 | 205 | 238 | 246 | 247)
}

/// When input arrived (server clock, µs), by kind. Shared by a session's input and its encoder,
/// which answers input at once (see `stream::INPUT_FAST_PATH_US`) and refines once it pauses.
#[derive(Default)]
pub struct Activity {
    /// The last input event of any kind.
    pub any_us: AtomicU64,
    /// The last click, key press or touch.
    pub discrete_us: AtomicU64,
    /// When the current run of pointer motion or scrolling began, after a pause of at least
    /// `MOTION_PAUSE_US`, and its latest event.
    pub burst_us: AtomicU64,
    motion_us: AtomicU64,
    /// The last pointer move to a new spot, which the compositor has to answer with a cursor
    /// update; the capture watchdog relies on that.
    pub pointer_us: AtomicU64,
    pointer_at: Mutex<(f32, f32)>,
}

/// Pointer motion or scrolling after this long without any starts a new run.
const MOTION_PAUSE_US: u64 = 50_000;

impl Activity {
    pub fn new() -> Arc<Activity> {
        Arc::new(Activity {
            pointer_at: Mutex::new((-1.0, -1.0)),
            ..Default::default()
        })
    }

    fn record(&self, batch: &[Input], now: u64) {
        self.any_us.store(now, Ordering::Relaxed);
        let discrete = batch.iter().any(|i| match i {
            Input::Button { .. } | Input::Key { .. } | Input::Text(_) => true,
            Input::Touch { kind, .. } => *kind != TouchKind::Move,
            _ => false,
        });
        let motion = batch.iter().any(|i| match i {
            Input::PointerAbs { .. } | Input::Scroll { .. } => true,
            Input::Touch { kind, .. } => *kind == TouchKind::Move,
            _ => false,
        });
        if discrete {
            self.discrete_us.store(now, Ordering::Relaxed);
        }
        if motion {
            let last = self.motion_us.swap(now, Ordering::Relaxed);
            if now.saturating_sub(last) >= MOTION_PAUSE_US {
                self.burst_us.store(now, Ordering::Relaxed);
            }
        }
        if let Some(&Input::PointerAbs { x, y }) = batch
            .iter()
            .rev()
            .find(|i| matches!(i, Input::PointerAbs { .. }))
        {
            let mut at = self.pointer_at.lock().unwrap();
            if (x - at.0).abs() + (y - at.1).abs() >= 1.0 {
                *at = (x, y);
                self.pointer_us.store(now, Ordering::Relaxed);
            }
        }
    }
}

/// What a session holds down on the desktop, released all at once when it ends. After that
/// (`closed`) nothing more is injected, so no press can land after its release.
#[derive(Default)]
struct Held {
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
    touches: HashSet<u32>,
    closed: bool,
}

/// One session's input, shared by its main socket and its separate input socket.
pub struct InputCtx {
    sink: Arc<dyn InputSink>,
    /// The monitor on screen; its encoder switches it, so input maps to the picture it shows.
    mon: Arc<Mutex<MonitorInfo>>,
    activity: Arc<Activity>,
    held: Mutex<Held>,
    /// Input sockets attached right now (a new one can attach while a dropped one lingers).
    pub sockets: AtomicUsize,
    closed: tokio::sync::watch::Sender<bool>,
}

impl InputCtx {
    pub fn new(
        sink: Arc<dyn InputSink>,
        mon: Arc<Mutex<MonitorInfo>>,
        activity: Arc<Activity>,
    ) -> InputCtx {
        InputCtx {
            sink,
            mon,
            activity,
            held: Mutex::new(Held::default()),
            sockets: AtomicUsize::new(0),
            closed: tokio::sync::watch::Sender::new(false),
        }
    }

    /// Inject a batch in order, with runs of pointer moves collapsed to the last one (a backlog
    /// of moves is only worth its end point), then flush once.
    pub fn handle_batch(&self, batch: Vec<Input>) {
        self.inject_held(&mut self.held.lock().unwrap(), batch);
    }

    fn inject_held(&self, held: &mut Held, batch: Vec<Input>) {
        if held.closed {
            return;
        }
        self.activity.record(&batch, clock::now_us());
        let mon = self.mon.lock().unwrap().clone();
        let mut it = batch.into_iter().peekable();
        while let Some(inp) = it.next() {
            if matches!(inp, Input::PointerAbs { .. })
                && matches!(it.peek(), Some(Input::PointerAbs { .. }))
            {
                continue;
            }
            inject(self.sink.as_ref(), &mon, inp, held);
        }
        self.sink.flush();
    }

    /// Paste what the session just put on the clipboard: V under a Ctrl the viewer already holds,
    /// or a whole Ctrl+V. Sent from here, after the clipboard was set, so the two keep their order.
    pub fn paste(&self) {
        // One lock throughout, so a Ctrl released meanwhile cannot turn this into a bare V.
        let mut held = self.held.lock().unwrap();
        let ctrl = held.keys.contains(&KEY_LEFTCTRL) || held.keys.contains(&KEY_RIGHTCTRL);
        let key = |code, down| Input::Key { code, down };
        let mut keys = vec![key(KEY_V as u16, true), key(KEY_V as u16, false)];
        if !ctrl {
            keys.insert(0, key(KEY_LEFTCTRL as u16, true));
            keys.push(key(KEY_LEFTCTRL as u16, false));
        }
        self.inject_held(&mut held, keys);
    }

    /// Release everything held and refuse any input from now on.
    pub fn shutdown(&self) {
        let mut held = self.held.lock().unwrap();
        held.closed = true;
        for k in held.keys.drain() {
            self.sink.key(k, false);
        }
        for b in held.buttons.drain() {
            self.sink.button(b, false);
        }
        // One cancel ends every contact.
        if let Some(id) = held.touches.drain().next() {
            self.sink.touch(TouchKind::Cancel, id, 0.0, 0.0);
        }
        self.sink.flush();
        drop(held);
        self.closed.send_replace(true);
    }

    /// Resolves once the session has shut down.
    pub async fn closed(&self) {
        let _ = self.closed.subscribe().wait_for(|c| *c).await;
    }
}

/// Map a point in stream pixels to the compositor's global logical coordinates.
fn to_global(mon: &MonitorInfo, x: f32, y: f32) -> (f64, f64) {
    let x = (x as f64).clamp(0.0, mon.width.saturating_sub(1) as f64);
    let y = (y as f64).clamp(0.0, mon.height.saturating_sub(1) as f64);
    (
        mon.x as f64 + x * mon.logical_width as f64 / mon.width.max(1) as f64,
        mon.y as f64 + y * mon.logical_height as f64 / mon.height.max(1) as f64,
    )
}

fn set(held: &mut HashSet<u32>, code: u32, down: bool) {
    if down {
        held.insert(code);
    } else {
        held.remove(&code);
    }
}

fn inject(sink: &dyn InputSink, mon: &MonitorInfo, inp: Input, held: &mut Held) {
    match inp {
        Input::PointerAbs { x, y } => {
            let (gx, gy) = to_global(mon, x, y);
            sink.pointer_abs(gx, gy);
        }
        Input::Button { button, down } => {
            let b = match button {
                0 => capture::BTN_LEFT,
                1 => capture::BTN_MIDDLE,
                2 => capture::BTN_RIGHT,
                3 => capture::BTN_SIDE,
                4 => capture::BTN_EXTRA,
                _ => return,
            };
            set(&mut held.buttons, b, down);
            sink.button(b, down);
        }
        Input::Scroll { dx, dy } => {
            sink.scroll(dx.clamp(-50.0, 50.0) as f64, dy.clamp(-50.0, 50.0) as f64)
        }
        Input::Key { code, down } => {
            let code = code as u32;
            if !key_allowed(code) {
                return;
            }
            set(&mut held.keys, code, down);
            sink.key(code, down);
        }
        Input::Touch { kind, id, x, y } => {
            let id = id as u32;
            match kind {
                TouchKind::Down => {
                    held.touches.insert(id);
                }
                TouchKind::Up => {
                    held.touches.remove(&id);
                }
                TouchKind::Cancel => held.touches.clear(),
                TouchKind::Move => {}
            }
            let (gx, gy) = to_global(mon, x, y);
            sink.touch(kind, id, gx, gy);
        }
        Input::Text(t) => {
            for c in t.chars().take(4096) {
                let Some((code, shift)) = us_key(c) else {
                    continue;
                };
                if shift {
                    sink.key(KEY_LEFTSHIFT, true);
                }
                sink.key(code, true);
                sink.key(code, false);
                if shift {
                    sink.key(KEY_LEFTSHIFT, false);
                }
            }
        }
        Input::Ack { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[test]
    fn us_layout() {
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('Q'), Some((16, true)));
        assert_eq!(us_key('0'), Some((11, false)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('\u{2019}'), Some((40, false)));
        assert_eq!(us_key('é'), None);
    }

    #[test]
    fn power_keys_blocked() {
        assert!(key_allowed(30));
        assert!(!key_allowed(116)); // KEY_POWER
        assert!(!key_allowed(142)); // KEY_SLEEP
        assert!(!key_allowed(0));
        assert!(!key_allowed(0x110));
        assert!(!key_allowed(247)); // KEY_RFKILL
    }

    fn session() -> (InputCtx, Arc<capture::fake::Recorder>) {
        let rec = Arc::new(capture::fake::Recorder::default());
        let mon = Arc::new(Mutex::new(capture::fake::monitor("A", 0)));
        (InputCtx::new(rec.clone(), mon, Activity::new()), rec)
    }

    fn key(code: u16, down: bool) -> Input {
        Input::Key { code, down }
    }

    #[test]
    fn shutdown_releases_everything_and_then_nothing_gets_through() {
        let (ctx, rec) = session();
        ctx.handle_batch(vec![
            key(29, true),
            Input::Button {
                button: 0,
                down: true,
            },
            Input::Touch {
                kind: TouchKind::Down,
                id: 3,
                x: 1.0,
                y: 1.0,
            },
        ]);
        ctx.shutdown();
        let released = rec.events()[3..].to_vec();
        assert_eq!(released, ["key 29 up", "button 272 up", "touch Cancel 3"]);
        // A press that arrives after the release is dropped, so it cannot stay down.
        ctx.handle_batch(vec![key(30, true)]);
        assert_eq!(rec.events().len(), 6);
    }

    #[test]
    fn lifted_touches_are_not_cancelled_again() {
        let (ctx, rec) = session();
        ctx.handle_batch(vec![
            Input::Touch {
                kind: TouchKind::Down,
                id: 1,
                x: 0.0,
                y: 0.0,
            },
            Input::Touch {
                kind: TouchKind::Up,
                id: 1,
                x: 0.0,
                y: 0.0,
            },
        ]);
        ctx.shutdown();
        assert_eq!(rec.events(), ["touch Down 1", "touch Up 1"]);
    }

    #[test]
    fn paste_presses_v_under_the_ctrl_already_held() {
        let (ctx, rec) = session();
        ctx.paste();
        assert_eq!(
            rec.events(),
            ["key 29 down", "key 47 down", "key 47 up", "key 29 up"]
        );
        let (ctx, rec) = session();
        ctx.handle_batch(vec![key(97, true)]);
        ctx.paste();
        assert_eq!(rec.events(), ["key 97 down", "key 47 down", "key 47 up"]);
    }

    #[test]
    fn closed_resolves_after_shutdown_even_if_awaited_late() {
        let (ctx, _) = session();
        ctx.shutdown();
        // A watch keeps its value, so a waiter that comes after the shutdown still sees it.
        assert!(ctx.closed().now_or_never().is_some());
    }

    #[test]
    fn a_drag_is_one_run_of_motion_and_a_click_always_counts() {
        let t = Activity::new();
        let mv = |x: f32| Input::PointerAbs { x, y: 0.0 };
        // A mouse moving at 250 Hz: one run, from its first event.
        for i in 0..50u64 {
            t.record(&[mv(i as f32)], 1_000_000 + i * 4_000);
        }
        assert_eq!(t.burst_us.load(Ordering::Relaxed), 1_000_000);
        assert_eq!(t.discrete_us.load(Ordering::Relaxed), 0);
        // A click in the middle of it is a click.
        let click = Input::Button {
            button: 0,
            down: true,
        };
        t.record(&[mv(1.0), click], 1_200_000);
        assert_eq!(t.discrete_us.load(Ordering::Relaxed), 1_200_000);
        assert_eq!(t.burst_us.load(Ordering::Relaxed), 1_000_000);
        // Motion after a pause starts a new run; so does a finger moving on a touch screen.
        t.record(&[mv(2.0)], 1_300_000);
        assert_eq!(t.burst_us.load(Ordering::Relaxed), 1_300_000);
        let touch = |kind| Input::Touch {
            kind,
            id: 0,
            x: 0.0,
            y: 0.0,
        };
        t.record(&[touch(TouchKind::Down)], 1_400_000);
        assert_eq!(t.discrete_us.load(Ordering::Relaxed), 1_400_000);
        t.record(&[touch(TouchKind::Move)], 1_410_000);
        assert_eq!(t.burst_us.load(Ordering::Relaxed), 1_410_000);
    }

    #[test]
    fn maps_stream_pixels_to_logical_coordinates() {
        // A 2x HiDPI monitor to the right of another one.
        let m = MonitorInfo {
            id: "DP-1".into(),
            name: String::new(),
            x: 1920,
            y: 0,
            logical_width: 1280,
            logical_height: 720,
            width: 2560,
            height: 1440,
            refresh_mhz: 60_000,
            scale: 2.0,
        };
        assert_eq!(to_global(&m, 0.0, 0.0), (1920.0, 0.0));
        assert_eq!(to_global(&m, 1280.0, 720.0), (2560.0, 360.0));
        // Out-of-range points are clamped to the monitor.
        let (x, y) = to_global(&m, 99999.0, -5.0);
        assert!(x < 1920.0 + 1280.0 && y == 0.0);
    }
}
