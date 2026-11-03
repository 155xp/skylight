#!/bin/sh
# A throwaway benchmark target: a virtual X display plus a second Skylight on its own port and its
# own ticket keys, so measurements never touch a real desktop session or an installed service.
#
#   scripts/testbed.sh start [skylight binary] [-- extra serve flags]
#   scripts/testbed.sh stop
#
# Environment: TESTBED_PORT (5491), TESTBED_DISPLAY (:71), TESTBED_SIZE (1920x1080),
# TESTBED_DIR (${TMPDIR:-/tmp}/skylight-testbed), TESTBED_WEB (web). Benchmarks then use
#   node scripts/bench.mjs --base http://127.0.0.1:5491/ --key $TESTBED_DIR/ticket-private.pem --aud testbed
set -eu
cd "$(dirname "$0")/.."
port=${TESTBED_PORT:-5491}
disp=${TESTBED_DISPLAY:-:71}
size=${TESTBED_SIZE:-1920x1080}
dir=${TESTBED_DIR:-${TMPDIR:-/tmp}/skylight-testbed}
mkdir -p "$dir"

stop() {
  for f in server xvfb; do
    if [ -f "$dir/$f.pid" ]; then kill "$(cat "$dir/$f.pid")" 2>/dev/null || true; rm -f "$dir/$f.pid"; fi
  done
}

case "${1:-}" in
  stop) stop; exit 0 ;;
  start) shift ;;
  *) echo "usage: $0 start [binary] [-- serve flags] | stop" >&2; exit 2 ;;
esac
bin=./target/release/skylight
if [ $# -gt 0 ] && [ "$1" != "--" ]; then bin=$1; shift; fi
[ "${1:-}" = "--" ] && shift
[ -f "$dir/server.pid" ] && { kill "$(cat "$dir/server.pid")" 2>/dev/null || true; rm -f "$dir/server.pid"; sleep 0.3; }
[ -f "$dir/ticket-private.pem" ] || "$bin" keygen --out-dir "$dir" >/dev/null
if ! xdpyinfo -display "$disp" >/dev/null 2>&1; then
  Xvfb "$disp" -screen 0 "${size}x24" -nolisten tcp -dpi 96 >"$dir/xvfb.log" 2>&1 &
  echo $! >"$dir/xvfb.pid"
  sleep 0.5
fi
DISPLAY=$disp nohup "$bin" serve --backend x11 --listen "127.0.0.1:$port" --audience testbed \
  --ticket-key "$dir/ticket-public.txt" --probe-display "$disp" --web-dir "${TESTBED_WEB:-web}" "$@" \
  >"$dir/server.log" 2>&1 &
echo $! >"$dir/server.pid"
sleep 1
tail -2 "$dir/server.log"
