# Configuration and use

## Configuration

`skylight serve` reads these from the environment (the service loads `~/.config/skylight/env`)
or from the matching flags. Run `skylight serve --help` for the full list.

| Variable / flag | Default | |
|---|---|---|
| `SKYLIGHT_TICKET_KEY` / `--ticket-key` | required | trusted ticket public keys, comma-separated: files or base64url keys |
| `SKYLIGHT_AUDIENCE` / `--audience` | the hostname | this server's name; tickets must carry it as `aud` |
| `SKYLIGHT_ALLOW_ORIGINS` / `--allow-origin` | none | extra browser origins allowed to open the WebSockets |
| `SKYLIGHT_LISTEN` / `--listen` | `127.0.0.1:5490` | loopback address to listen on |
| `SKYLIGHT_BACKEND` / `--backend` | `auto` | `kde`, `x11` or `synthetic` |
| `SKYLIGHT_ENCODER` / `--encoder` | `auto` | `nvenc`, `vaapi` or `software` |
| `SKYLIGHT_PROBE_DISPLAY` / `--probe-display` | `$DISPLAY` | X display for the latency probe window |
| `--max-fps` | 144 | frame-rate cap |
| `--max-mbps` / `--start-mbps` | 50 / 16 | bitrate cap and starting point |
| `--max-sessions` | 4 | concurrent viewers |

Backend and encoder details, including the VA-API and NVENC variables, are in
[DESIGN.md](DESIGN.md).

## Using the viewer

- **Keyboard:** keys map by physical position. On a Mac, Cmd sends Ctrl by default (Settings can
  switch it to Super).
- **Clipboard:** pasting sends your clipboard first, and copying on the remote desktop fills
  yours.
- **Phones:** trackpad mode by default (drag to move, tap to click, two-finger tap to right-click,
  two-finger drag to scroll, pinch to zoom). The keyboard button opens the on-screen keyboard.
- **Displays:** each monitor is offered by name, plus an "All" stream when there's more than one.
- **Latency test:** Settings → Measure runs click, key and move probes from your device.


## Limitations

- No audio yet.
- GNOME and wlroots Wayland sessions need an xdg-desktop-portal backend, which doesn't exist yet.
- Everything runs over TCP, so on a lossy link (Wi-Fi, mobile) one lost packet holds up the
  frames behind it.
- Safari, iPhone and macOS clients haven't been measured on real devices yet.
