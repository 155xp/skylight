# Skylight

A low-latency remote desktop for Linux that runs in the browser. It streams KDE Plasma (Wayland)
or X11 with NVENC, VA-API or x264, decodes with WebCodecs, and needs no client install.
Access uses short-lived signed tickets, not passwords.

![The built-in viewer streaming a Linux desktop](docs/results/viewer-desktop.png)

## Requirements

- KDE Plasma 6 on Wayland, or any X11 session (GNOME and wlroots are not supported yet)
- PipeWire and FFmpeg libraries; a GPU is optional
- Rust 1.88+, clang and pkg-config to build
- A browser with WebCodecs (current Chrome, Edge, Safari or Firefox)

## Quick start

```sh
# Debian 13 / Ubuntu build dependencies
sudo apt install libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev \
  libswscale-dev plasma-wayland-protocols clang pkg-config

cargo build --release
install -Dm755 target/release/skylight ~/.local/bin/skylight
~/.local/bin/skylight install --service       # KWin permissions + systemd user service
skylight keygen --out-dir ~/.config/skylight  # ticket key pair

cat > ~/.config/skylight/env <<EOF
SKYLIGHT_TICKET_KEY=$HOME/.config/skylight/ticket-public.txt
SKYLIGHT_AUDIENCE=my-desktop
SKYLIGHT_PROBE_DISPLAY=:1
EOF
chmod 600 ~/.config/skylight/env
systemctl --user enable --now skylight
skylight doctor                               # check backend, encoders, capture

# Open the viewer locally with a one-time link
skylight ticket --key ~/.config/skylight/ticket-private.pem --audience my-desktop \
  --url http://127.0.0.1:5490/
```

Skylight only listens on loopback. To reach it from other devices, put a TLS reverse proxy in
front of it and mint tickets from your own app; see [INTEGRATION.md](INTEGRATION.md).

## Docs

- [Configuration and use](docs/USAGE.md)
- [Integration: proxy, tickets, embedding](INTEGRATION.md)
- [Security](docs/SECURITY.md) · [Design](docs/DESIGN.md) · [Benchmarks](docs/BENCHMARKS.md)
- [Protocol](docs/PROTOCOL.md) · [Development](docs/DEVELOPMENT.md)

## License

MIT, see [LICENSE](LICENSE). Wayland protocol files in `protocols/` are LGPL-2.1-or-later.
