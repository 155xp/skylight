//! The latency probe: a small always-on-top X11 window (shown through Xwayland on KDE) that
//! redraws in response to input, so the browser can time the whole loop a user feels.
//!
//! * `move`:  white while the pointer is over the left half, black over the right half.
//! * `click`: each left click flips the window between white and black.
//! * `key`:   each key press flips between a blank page and a page of the typed glyph, drawn
//!   with an X core font, the way a terminal draws text.
//!
//! The browser triggers the input, then watches the decoded video for the change. That covers
//! input over the network, injection, the app redrawing, compositing, capture, encode, the
//! network again and decode.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, *};
use x11rb::wrapper::ConnectionExt as _;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Move,
    Click,
    Key,
    /// Redraws continuously (a bar sweeping across), for frame-rate and bandwidth tests. A strip
    /// along the top carries the time of each redraw (see [`TIMECODE_BITS`]), so the browser can
    /// time every frame from the app drawing it to the viewer showing it.
    Animate,
    /// Like `Animate`, but a large window of text scrolling smoothly, the way a terminal or a web
    /// page scrolls: much more detail per frame, for bandwidth and slow-link tests.
    Scroll,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "move" => Some(Mode::Move),
            "click" => Some(Mode::Click),
            "key" => Some(Mode::Key),
            "animate" => Some(Mode::Animate),
            "scroll" => Some(Mode::Scroll),
            _ => None,
        }
    }
}

/// The time code: 32 cells along the top of the animated probe, each `TIMECODE_CELL` pixels wide.
/// The top row is the server clock in units of 100 µs (black is a 1, most significant bit first),
/// the row under it is its complement, so a frame captured halfway through a redraw is rejected.
pub const TIMECODE_BITS: u32 = 32;
pub const TIMECODE_CELL: u16 = 14;
pub const TIMECODE_ROW: u16 = 24;

fn atom(conn: &impl Connection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

pub fn run(x: i32, y: i32, size: u16, mode: Mode) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("connecting to X (set DISPLAY)")?;
    let screen = &conn.setup().roots[screen_num];
    let (white, black, root) = (screen.white_pixel, screen.black_pixel, screen.root);
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        x as i16,
        y as i16,
        size,
        size,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(white).event_mask(
            EventMask::POINTER_MOTION
                | EventMask::EXPOSURE
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::KEY_PRESS
                | EventMask::ENTER_WINDOW
                | EventMask::LEAVE_WINDOW
                | EventMask::FOCUS_CHANGE
                | EventMask::STRUCTURE_NOTIFY,
        ),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Skylight latency probe",
    )?;
    // Ask the window manager for exactly this place and size, no frame, above everything.
    let mut hints = [0u32; 18];
    hints[0] = 1 | 2 | 8 | 16 | 32; // USPosition | USSize | PSize | PMinSize | PMaxSize
    hints[1] = x as u32;
    hints[2] = y as u32;
    hints[3] = size as u32;
    hints[4] = size as u32;
    hints[5] = size as u32;
    hints[6] = size as u32;
    hints[7] = size as u32;
    hints[8] = size as u32;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NORMAL_HINTS,
        AtomEnum::WM_SIZE_HINTS,
        &hints,
    )?;
    let motif = atom(&conn, "_MOTIF_WM_HINTS")?;
    conn.change_property32(PropMode::REPLACE, win, motif, motif, &[2, 0, 0, 0, 0])?;
    let state = atom(&conn, "_NET_WM_STATE")?;
    let above = atom(&conn, "_NET_WM_STATE_ABOVE")?;
    let skip = atom(&conn, "_NET_WM_STATE_SKIP_TASKBAR")?;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        state,
        AtomEnum::ATOM,
        &[above, skip],
    )?;
    let font = conn.generate_id()?;
    if conn.open_font(font, b"10x20")?.check().is_err() {
        conn.open_font(font, b"fixed")?.check()?;
    }
    let gc = conn.generate_id()?;
    conn.create_gc(
        gc,
        win,
        &CreateGCAux::new()
            .foreground(black)
            .background(white)
            .font(font),
    )?;
    conn.map_window(win)?;
    conn.flush()?;

    let fill = |color: u32| -> Result<()> {
        conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
        conn.poly_fill_rectangle(
            win,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: size,
                height: size,
            }],
        )?;
        conn.change_gc(gc, &ChangeGCAux::new().foreground(black))?;
        Ok(())
    };
    let glyphs = |c: u8| -> Result<()> {
        fill(white)?;
        let line = vec![c; (size as usize / 10).max(1)];
        let mut row = 18i16;
        while row < size as i16 {
            conn.image_text8(win, gc, 2, row, &line)?;
            row += 20;
        }
        Ok(())
    };

    // white / black for move and click; blank / glyph page for key.
    let mut dark = false;
    let mut glyph: u8 = b'k';
    let mut reported = false;
    let redraw = |dark: bool, glyph: u8| -> Result<()> {
        match (mode, dark) {
            (Mode::Key, true) => glyphs(glyph)?,
            (_, true) => fill(black)?,
            (_, false) => fill(white)?,
        }
        conn.flush()?;
        Ok(())
    };
    if mode == Mode::Animate || mode == Mode::Scroll {
        // Sweep a black bar across a shifting grey, as fast as the X server takes it; the
        // compositor turns that into one new frame per display refresh.
        let start = std::time::Instant::now();
        // The server clock's zero on CLOCK_MONOTONIC, so the time code is in the server's clock.
        let origin_ns: u64 = std::env::var("SKYLIGHT_CLOCK_ORIGIN_NS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        loop {
            while conn.poll_for_event()?.is_some() {}
            let t = start.elapsed().as_secs_f64();
            if mode == Mode::Scroll {
                // 300 px/s of 20 px lines; each line's text depends on its index, so content
                // really moves instead of repeating.
                fill(white)?;
                let pos = (t * 300.0) as i64;
                let first = pos / 20;
                let mut row = 48 + 18 - (pos % 20) as i16;
                let mut i = first;
                while row < size as i16 + 20 {
                    let line: Vec<u8> = (0..(size as usize / 10))
                        .map(|c| {
                            let h = (i as usize).wrapping_mul(2654435761) ^ c.wrapping_mul(40503);
                            if (h >> 7) % 6 == 0 { b' ' } else { b'!' + ((h >> 3) % 90) as u8 }
                        })
                        .collect();
                    conn.image_text8(win, gc, 2, row, &line)?;
                    row += 20;
                    i += 1;
                }
            } else {
                let shade = (128.0 + 100.0 * (t * 2.0).sin()) as u32;
                let grey = (shade << 16) | (shade << 8) | shade;
                fill(grey)?;
                let x = ((t * 600.0) as u32 % (size as u32 + 40)) as i16 - 40;
                conn.poly_fill_rectangle(
                    win,
                    gc,
                    &[Rectangle {
                        x,
                        y: 0,
                        width: 40,
                        height: size,
                    }],
                )?;
            }
            let code = (crate::clock::monotonic_ns().saturating_sub(origin_ns) / 100_000) as u32;
            let mut ones = Vec::new();
            let mut zeros = Vec::new();
            for bit in 0..TIMECODE_BITS {
                let x = (bit as u16 * TIMECODE_CELL) as i16;
                let set = code >> (TIMECODE_BITS - 1 - bit) & 1 == 1;
                let cell = |row: u16| Rectangle {
                    x,
                    y: (row * TIMECODE_ROW) as i16,
                    width: TIMECODE_CELL,
                    height: TIMECODE_ROW,
                };
                ones.push(cell(if set { 0 } else { 1 }));
                zeros.push(cell(if set { 1 } else { 0 }));
            }
            conn.change_gc(gc, &ChangeGCAux::new().foreground(white))?;
            conn.poly_fill_rectangle(win, gc, &zeros)?;
            conn.change_gc(gc, &ChangeGCAux::new().foreground(black))?;
            conn.poly_fill_rectangle(win, gc, &ones)?;
            conn.flush()?;
            if !reported && start.elapsed() > std::time::Duration::from_millis(250) {
                let pos = conn.translate_coordinates(win, root, 0, 0)?.reply()?;
                println!("READY {} {} {} {}", pos.dst_x, pos.dst_y, size, size);
                use std::io::Write;
                std::io::stdout().flush()?;
                reported = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let debug = std::env::var_os("SKYLIGHT_PROBE_DEBUG").is_some();
    let start_debug = std::time::Instant::now();
    loop {
        let event = conn.wait_for_event()?;
        if debug {
            let t = start_debug.elapsed().as_millis();
            match &event {
                Event::ButtonPress(e) => eprintln!("{t} press b{} at {},{} state {:?}", e.detail, e.event_x, e.event_y, e.state),
                Event::ButtonRelease(e) => eprintln!("{t} release b{} at {},{}", e.detail, e.event_x, e.event_y),
                Event::EnterNotify(e) => eprintln!("{t} enter mode {:?}", e.mode),
                Event::LeaveNotify(e) => eprintln!("{t} leave mode {:?}", e.mode),
                Event::FocusIn(e) => eprintln!("{t} focus-in {:?}", e.mode),
                Event::FocusOut(e) => eprintln!("{t} focus-out {:?}", e.mode),
                Event::MotionNotify(_) | Event::Expose(_) => {}
                other => eprintln!("{t} {other:?}"),
            }
        }
        match event {
            Event::Expose(_) => {
                redraw(dark, glyph)?;
                if !reported {
                    // Tell the caller where the window really landed. The first expose can come
                    // before the window manager has moved it into place, so give it a moment.
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let pos = conn.translate_coordinates(win, root, 0, 0)?.reply()?;
                    println!("READY {} {} {} {}", pos.dst_x, pos.dst_y, size, size);
                    use std::io::Write;
                    std::io::stdout().flush()?;
                    reported = true;
                }
            }
            Event::MotionNotify(e) if mode == Mode::Move => {
                let want = (e.event_x as i32) >= size as i32 / 2;
                if want != dark {
                    dark = want;
                    redraw(dark, glyph)?;
                }
            }
            Event::ButtonPress(e) if mode == Mode::Click && e.detail == 1 => {
                dark = !dark;
                redraw(dark, glyph)?;
            }
            Event::KeyPress(e) if mode == Mode::Key => {
                // Any key: draw a page of a letter that depends on the keycode.
                glyph = b'a' + (e.detail % 26);
                dark = !dark;
                redraw(dark, glyph)?;
            }
            _ => {}
        }
    }
}
