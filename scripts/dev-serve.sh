#!/bin/sh
# Restart a development server on 127.0.0.1:5490 (logs in /tmp/skylight.log), serving the web
# client from web/ so edits show up on reload. SKYLIGHT_AUDIENCE defaults to the hostname; set
# SKYLIGHT_ALLOW_ORIGINS to let your own app's origin open sockets.
set -e
cd "$(dirname "$0")/.."
if [ -f /tmp/skylight.pid ]; then kill "$(cat /tmp/skylight.pid)" 2>/dev/null || true; sleep 0.3; fi
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
eval "$(systemctl --user show-environment | grep -E '^(DISPLAY|XAUTHORITY)=' | sed 's/^/export /')"
nohup ./target/release/skylight serve \
  --ticket-key "$HOME/.config/skylight/ticket-public.txt" --web-dir web "$@" > /tmp/skylight.log 2>&1 &
echo $! > /tmp/skylight.pid
sleep 1
tail -3 /tmp/skylight.log
