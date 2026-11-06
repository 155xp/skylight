# Skylight

A low-latency remote desktop for Linux that runs in the browser.

Skylight captures the desktop through KWin's screencast (Wayland) or MIT-SHM (X11), encodes only
what changed with NVENC, VA-API or x264, sends it over a WebSocket, and decodes it with WebCodecs
straight onto a canvas. Input travels on its own socket and is injected as soon as it arrives. The
browser draws the cursor, so pointer movement never waits on the network.

![The built-in viewer streaming a Linux desktop, with settings and stats open](docs/results/viewer-desktop.png)

## Features

- **Low latency:** about 7 ms from a click to the decoded frame over loopback on a test machine,
  and a steady 60 fps under motion down to a 10 Mbit/s link ([benchmarks](docs/BENCHMARKS.md)).
- **No client install.** Runs in any current browser with WebCodecs. H.264 by default, plus HEVC
  and AV1 when both ends support them.
- **KDE Plasma on Wayland** and **X11**, with keyboard, mouse and clipboard.
- **Hardware or software encoding**, chosen at runtime: NVENC, VA-API, or x264 when there's no
  GPU. An idle screen sends nothing.
- **Works from Mac and phone clients:** keys by physical position, Cmd mapped to Ctrl, two-way
  clipboard, trackpad-style touch control.
- **No passwords.** Access uses short-lived Ed25519-signed tickets minted by an app you control.
  Skylight listens on loopback only and holds only public keys.
- **Embeddable:** `web/skylight.js` is a dependency-free ES module ([INTEGRATION.md](INTEGRATION.md)).
- **Scriptable:** `/api/screenshot` and `/api/wait` let scripts see the screen and wait for it to
  settle ([PROTOCOL.md](docs/PROTOCOL.md)).

## Requirements

- Linux with **KDE Plasma 6 on Wayland**, or any **X11** session. GNOME and wlroots Wayland
  sessions are not supported yet.
- PipeWire and FFmpeg's libraries (libavcodec, libavutil, libswscale).
- Optional GPU: NVIDIA (NVENC) or Intel/AMD (VA-API). Without one, Skylight encodes in software.
- To build: Rust 1.88 or newer, clang and pkg-config. No GPU vendor SDK is needed.
- A browser with WebCodecs on an HTTPS page (or localhost): current Chrome, Edge, Safari or
  Firefox.

## Quick start

Install build dependencies (Debian 13 / Ubuntu):

```sh
sudo apt install libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev \
  libswscale-dev plasma-wayland-protocols clang pkg-config
```

Build, install and start the service:

```sh
cargo build --release
install -Dm755 target/release/skylight ~/.local/bin/skylight

# Let KWin trust this binary for screencast and input (writes a .desktop file), and install
# a systemd user service.
~/.local/bin/skylight install --service

# A ticket key pair. Whatever mints tickets keeps the private key; Skylight only needs the
# public one.
skylight keygen --out-dir ~/.config/skylight

cat > ~/.config/skylight/env <<EOF
SKYLIGHT_TICKET_KEY=$HOME/.config/skylight/ticket-public.txt
SKYLIGHT_AUDIENCE=my-desktop
SKYLIGHT_PROBE_DISPLAY=:1
EOF
chmod 600 ~/.config/skylight/env
systemctl --user enable --now skylight

skylight doctor     # backend, monitors, encoders, a test capture
```

`SKYLIGHT_PROBE_DISPLAY` is the X display used by the latency probe. On KDE that's Xwayland's
display, usually `:0` or `:1` (run `echo $DISPLAY` in a terminal on the desktop).

Open the viewer on the same machine with a one-time link. The ticket rides in the URL fragment,
which browsers never send to a server:

```sh
skylight ticket --key ~/.config/skylight/ticket-private.pem --audience my-desktop \
  --url http://127.0.0.1:5490/
```

To connect from other devices, put a TLS-terminating reverse proxy (or a private network) in
front of `127.0.0.1:5490` and have your own web app mint tickets for signed-in users.
[INTEGRATION.md](INTEGRATION.md) covers both, with a Caddy example.

To stop it: `systemctl --user disable --now skylight`.

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
[docs/DESIGN.md](docs/DESIGN.md).

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

## Documentation

- [INTEGRATION.md](INTEGRATION.md): reverse proxy, minting tickets, embedding the viewer
- [docs/SECURITY.md](docs/SECURITY.md): threat model and what protects the desktop
- [docs/DESIGN.md](docs/DESIGN.md): backends, encoders, recovery, and what makes it fast
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md): latency, frame rate, bandwidth and footprint
- [docs/PROTOCOL.md](docs/PROTOCOL.md): wire format and the HTTP API
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md): tests, benchmarks and the CI container

## License

MIT, see [LICENSE](LICENSE). The Wayland protocol files in `protocols/` keep their own license
(LGPL-2.1-or-later), see [protocols/README.md](protocols/README.md).
