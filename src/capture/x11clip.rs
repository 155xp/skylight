//! The X11 clipboard: CLIPBOARD text both ways, through a hidden window of our own on a separate
//! connection. XFixes says when another client takes the selection; its text is then fetched (in
//! chunks, for big ones, the INCR way). Text from the browser makes the window the owner, and it
//! answers requests for it until another client takes over.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::watch;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::Clipboard;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        UTF8_STRING,
        TARGETS,
        TEXT,
        INCR,
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        SKYLIGHT_CLIPBOARD,
    }
}

struct Shared {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    /// The last text from the browser, served while the window owns the selection (only the
    /// owner is asked, so it can stay after another client takes over).
    offering: Mutex<Option<Arc<str>>>,
    /// Counts `set_text` calls: a desktop selection fetched before the latest one is stale. Held
    /// while the watch is updated, so the check and the update are one step.
    sets: Mutex<u64>,
    watch_tx: watch::Sender<Option<Arc<str>>>,
}

pub struct X11Clipboard(Arc<Shared>);

impl X11Clipboard {
    /// On `display`, or `$DISPLAY`.
    pub fn open(display: Option<&str>) -> Result<X11Clipboard> {
        let (conn, screen) = x11rb::connect(display).context("connecting to the X server")?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        let win = conn.generate_id()?;
        conn.create_window(
            0,
            win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.xfixes_select_selection_input(
            win,
            atoms.CLIPBOARD,
            SelectionEventMask::SET_SELECTION_OWNER,
        )?;
        // Whatever is on the clipboard already.
        if conn.get_selection_owner(atoms.CLIPBOARD)?.reply()?.owner != NONE {
            request(&conn, win, &atoms, CURRENT_TIME)?;
        }
        conn.flush()?;
        let shared = Arc::new(Shared {
            conn,
            win,
            atoms,
            offering: Mutex::new(None),
            sets: Mutex::new(0),
            watch_tx: watch::channel(None).0,
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("x11-clipboard".into())
            .spawn(move || {
                if let Err(e) = run(&s) {
                    tracing::warn!("X11 clipboard stopped: {e:#}");
                }
            })?;
        Ok(X11Clipboard(shared))
    }
}

impl Clipboard for X11Clipboard {
    fn set_text(&self, text: String) {
        let s = &self.0;
        let text: Arc<str> = text.into();
        let mut sets = s.sets.lock().unwrap();
        *sets += 1;
        *s.offering.lock().unwrap() = Some(text.clone());
        s.watch_tx.send_replace(Some(text));
        drop(sets);
        // A round trip: once it is back, the server owns the change, so a paste typed next (on
        // the input connection) asks for the new text.
        let _ = s
            .conn
            .set_selection_owner(s.win, s.atoms.CLIPBOARD, CURRENT_TIME);
        let _ = s
            .conn
            .get_selection_owner(s.atoms.CLIPBOARD)
            .map(|c| c.reply());
    }

    fn watch(&self) -> watch::Receiver<Option<Arc<str>>> {
        self.0.watch_tx.subscribe()
    }
}

/// Ask the owner for its text, into our window's property.
fn request(conn: &RustConnection, win: Window, atoms: &Atoms, time: u32) -> Result<()> {
    conn.convert_selection(
        win,
        atoms.CLIPBOARD,
        atoms.UTF8_STRING,
        atoms.SKYLIGHT_CLIPBOARD,
        time,
    )?;
    Ok(())
}

fn run(s: &Shared) -> Result<()> {
    let a = &s.atoms;
    // The text of a transfer in chunks, while one is going on.
    let mut incr: Option<Vec<u8>> = None;
    // `sets` when the selection now arriving was asked for (the one in `open` came first).
    let mut asked = 0;
    loop {
        match s.conn.wait_for_event()? {
            Event::XfixesSelectionNotify(e) if e.owner != s.win && e.owner != NONE => {
                incr = None;
                asked = *s.sets.lock().unwrap();
                request(&s.conn, s.win, a, e.selection_timestamp)?;
            }
            Event::SelectionNotify(e) if e.requestor == s.win && e.property != NONE => {
                let r = take(s)?;
                if r.type_ == a.INCR {
                    // Deleting the property (done) asks for the first chunk.
                    incr = Some(Vec::new());
                } else {
                    publish(s, asked, r.value);
                }
            }
            Event::PropertyNotify(e)
                if e.window == s.win
                    && e.atom == a.SKYLIGHT_CLIPBOARD
                    && e.state == Property::NEW_VALUE
                    && incr.is_some() =>
            {
                let chunk = take(s)?.value;
                let buf = incr.as_mut().unwrap();
                if chunk.is_empty() {
                    publish(s, asked, incr.take().unwrap());
                } else if buf.len() + chunk.len() > super::CLIPBOARD_MAX {
                    incr = None;
                } else {
                    buf.extend_from_slice(&chunk);
                }
            }
            Event::SelectionRequest(e) => answer(s, &e)?,
            _ => {}
        }
        s.conn.flush()?;
    }
}

fn take(s: &Shared) -> Result<xproto::GetPropertyReply> {
    Ok(s.conn
        .get_property(
            true,
            s.win,
            s.atoms.SKYLIGHT_CLIPBOARD,
            AtomEnum::ANY,
            0,
            (super::CLIPBOARD_MAX / 4 + 1) as u32,
        )?
        .reply()?)
}

/// Hand a desktop selection to the sessions, unless the browser set newer text meanwhile.
fn publish(s: &Shared, asked: u64, bytes: Vec<u8>) {
    let sets = s.sets.lock().unwrap();
    if bytes.len() <= super::CLIPBOARD_MAX && *sets == asked {
        let text: Arc<str> = String::from_utf8_lossy(&bytes).into();
        s.watch_tx.send_replace(Some(text));
    }
}

/// Give a requestor our text, in the form it asked for, or refuse.
fn answer(s: &Shared, e: &SelectionRequestEvent) -> Result<()> {
    let a = &s.atoms;
    // Clients from before ICCCM 2 leave the property out; the target names it then.
    let prop = if e.property == NONE {
        e.target
    } else {
        e.property
    };
    let text = s.offering.lock().unwrap().clone();
    let ok = match text {
        Some(text) if e.selection == a.CLIPBOARD => {
            let string: u32 = AtomEnum::STRING.into();
            if e.target == a.TARGETS {
                let targets = [a.TARGETS, a.UTF8_STRING, a.TEXT_PLAIN_UTF8, a.TEXT, string];
                s.conn
                    .change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        prop,
                        AtomEnum::ATOM,
                        &targets,
                    )
                    .is_ok()
            } else if [a.UTF8_STRING, a.TEXT_PLAIN_UTF8, a.TEXT].contains(&e.target) {
                let kind = if e.target == a.TEXT {
                    a.UTF8_STRING
                } else {
                    e.target
                };
                s.conn
                    .change_property8(PropMode::REPLACE, e.requestor, prop, kind, text.as_bytes())
                    .is_ok()
            } else if e.target == string {
                let latin1: Vec<u8> = text
                    .chars()
                    .map(|c| u8::try_from(c as u32).unwrap_or(b'?'))
                    .collect();
                s.conn
                    .change_property8(PropMode::REPLACE, e.requestor, prop, string, &latin1)
                    .is_ok()
            } else {
                false
            }
        }
        _ => false,
    };
    let notify = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: e.time,
        requestor: e.requestor,
        selection: e.selection,
        target: e.target,
        property: if ok { prop } else { NONE },
    };
    s.conn
        .send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Another client on the same display, as a desktop app would be.
    struct App {
        conn: RustConnection,
        win: Window,
        atoms: Atoms,
    }

    impl App {
        fn new(display: &str) -> Option<App> {
            let (conn, screen) = x11rb::connect(Some(display)).ok()?;
            let root = conn.setup().roots[screen].root;
            let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
            let win = conn.generate_id().ok()?;
            conn.create_window(
                0,
                win,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .ok()?;
            conn.flush().ok()?;
            Some(App { conn, win, atoms })
        }

        fn next_event(&self, until: Instant) -> Option<Event> {
            while Instant::now() < until {
                if let Some(e) = self.conn.poll_for_event().unwrap() {
                    return Some(e);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            None
        }

        /// Paste what `owner` offers. Right away: whoever set the clipboard made sure the server
        /// took the change before returning, as a paste can follow at once.
        fn paste(&self, owner: Window) -> Option<String> {
            let a = &self.atoms;
            assert_eq!(
                self.owner(),
                owner,
                "the clipboard changed hands before returning"
            );
            self.conn
                .convert_selection(self.win, a.CLIPBOARD, a.UTF8_STRING, a.TEXT, CURRENT_TIME)
                .unwrap();
            self.conn.flush().unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            while let Some(e) = self.next_event(until) {
                if let Event::SelectionNotify(e) = e {
                    if e.property == NONE {
                        return None;
                    }
                    let r = self
                        .conn
                        .get_property(true, self.win, a.TEXT, AtomEnum::ANY, 0, 1 << 20)
                        .unwrap()
                        .reply()
                        .unwrap();
                    return Some(String::from_utf8(r.value).unwrap());
                }
            }
            None
        }

        fn owner(&self) -> Window {
            let c = self.conn.get_selection_owner(self.atoms.CLIPBOARD).unwrap();
            c.reply().unwrap().owner
        }

        fn own(&self) {
            let a = &self.atoms;
            self.conn
                .set_selection_owner(self.win, a.CLIPBOARD, CURRENT_TIME)
                .unwrap();
            assert_eq!(self.owner(), self.win);
        }

        fn next_request(&self) -> SelectionRequestEvent {
            let until = Instant::now() + Duration::from_secs(2);
            while let Some(e) = self.next_event(until) {
                if let Event::SelectionRequest(e) = e {
                    return e;
                }
            }
            panic!("nobody asked for the copied text");
        }

        fn reply(&self, e: &SelectionRequestEvent, text: &str) {
            let a = &self.atoms;
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    e.requestor,
                    e.property,
                    a.UTF8_STRING,
                    text.as_bytes(),
                )
                .unwrap();
            let n = SelectionNotifyEvent {
                response_type: xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: e.time,
                requestor: e.requestor,
                selection: e.selection,
                target: e.target,
                property: e.property,
            };
            self.conn
                .send_event(false, e.requestor, EventMask::NO_EVENT, n)
                .unwrap();
            self.conn.flush().unwrap();
        }
    }

    /// Needs a throwaway X server with no other clients, never a real desktop (its clipboard is
    /// overwritten): `SKYLIGHT_TEST_DISPLAY=:99` with Xvfb on :99. Does nothing without one.
    #[test]
    fn text_goes_both_ways() {
        let Ok(display) = std::env::var("SKYLIGHT_TEST_DISPLAY") else {
            eprintln!("SKYLIGHT_TEST_DISPLAY not set; skipped");
            return;
        };
        let app = App::new(&display).expect("SKYLIGHT_TEST_DISPLAY should be an X display");
        let clip = X11Clipboard::open(Some(&display)).unwrap();
        let ours = clip.0.win;
        clip.set_text("from the browser: héllo".into());
        assert_eq!(app.paste(ours).as_deref(), Some("from the browser: héllo"));

        let mut rx = clip.watch();
        rx.borrow_and_update();
        app.own();
        app.reply(&app.next_request(), "from an app: ünïcode");
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until && !rx.has_changed().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(rx.borrow().as_deref(), Some("from an app: ünïcode"));

        // The app's text arrives after newer text from the browser: the browser's stands.
        app.own();
        let late = app.next_request();
        clip.set_text("newer, from the browser".into());
        app.reply(&late, "older, from the app");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(rx.borrow().as_deref(), Some("newer, from the browser"));

        // The browser takes the clipboard back right after an app took it: still pasted.
        app.own();
        clip.set_text("right after".into());
        assert_eq!(app.paste(ours).as_deref(), Some("right after"));

        // Over and over: the change is in place the moment set_text returns, every time.
        for i in 0..500 {
            app.own();
            clip.set_text(format!("paste {i}"));
            assert_eq!(app.owner(), ours, "round {i}");
        }
    }
}
