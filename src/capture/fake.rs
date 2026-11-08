//! A backend for tests: the synthetic test pattern on monitors a test can unplug, input that is
//! written down instead of injected, and a clipboard in memory.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use super::synthetic::SyntheticBackend;
use super::{Backend, Capture, Clipboard, InputSink, MonitorInfo, TouchKind};

/// Every input event, in order, as text: `key 30 down`, `button 272 up`, `touch Cancel 0`...
#[derive(Default)]
pub struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    pub fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn push(&self, e: String) {
        self.0.lock().unwrap().push(e);
    }
}

fn updown(down: bool) -> &'static str {
    if down { "down" } else { "up" }
}

impl InputSink for Recorder {
    fn pointer_abs(&self, x: f64, y: f64) {
        self.push(format!("pointer {x} {y}"));
    }
    fn button(&self, button: u32, down: bool) {
        self.push(format!("button {button} {}", updown(down)));
    }
    fn scroll(&self, dx: f64, dy: f64) {
        self.push(format!("scroll {dx} {dy}"));
    }
    fn key(&self, code: u32, down: bool) {
        self.push(format!("key {code} {}", updown(down)));
    }
    fn touch(&self, kind: TouchKind, id: u32, _x: f64, _y: f64) {
        self.push(format!("touch {kind:?} {id}"));
    }
}

#[derive(Default)]
pub struct MemClipboard {
    pub text: Mutex<Vec<String>>,
    tx: tokio::sync::watch::Sender<Option<Arc<str>>>,
}

impl Clipboard for MemClipboard {
    fn set_text(&self, text: String) {
        self.text.lock().unwrap().push(text);
    }
    fn watch(&self) -> tokio::sync::watch::Receiver<Option<Arc<str>>> {
        self.tx.subscribe()
    }
}

pub struct FakeBackend {
    pattern: SyntheticBackend,
    pub monitors: Mutex<Vec<MonitorInfo>>,
    pub input: Arc<Recorder>,
    pub clipboard: Arc<MemClipboard>,
}

pub fn monitor(id: &str, x: i32) -> MonitorInfo {
    MonitorInfo {
        id: id.into(),
        name: id.into(),
        x,
        y: 0,
        logical_width: 320,
        logical_height: 240,
        width: 320,
        height: 240,
        refresh_mhz: 30_000,
        scale: 1.0,
    }
}

impl FakeBackend {
    /// Monitors `A` and `B`, side by side.
    pub fn new() -> FakeBackend {
        FakeBackend {
            pattern: SyntheticBackend::new(320, 240, 30),
            monitors: Mutex::new(vec![monitor("A", 0), monitor("B", 320)]),
            input: Arc::default(),
            clipboard: Arc::default(),
        }
    }

    pub fn unplug(&self, id: &str) {
        self.monitors.lock().unwrap().retain(|m| m.id != id);
    }
}

impl Backend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.lock().unwrap().clone()
    }
    fn start_capture(&self, monitor: &MonitorInfo) -> Result<Capture> {
        if !self.monitors().contains(monitor) {
            bail!("no monitor {}", monitor.id);
        }
        self.pattern.start_capture(monitor)
    }
    fn input(&self) -> Option<Arc<dyn InputSink>> {
        Some(self.input.clone())
    }
    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        Some(self.clipboard.clone())
    }
}
