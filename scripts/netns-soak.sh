#!/bin/sh
# scripts/soak.mjs over a path made bad by the kernel (tc netem), with nothing on the host
# touched: a private network namespace (no root needed) holds the test bed, the browser and its
# own loopback, and netem shapes that loopback only. Packets cross it once each way, so the round
# trip is twice the delay.
#
#   scripts/netns-soak.sh <skylight binary> <profile> [soak args...]
#
# Profiles (what a VPN exit and a TCP relay in front of the server look like):
#   vpn   60±25 ms each way, 1% loss, 4 Mbit/s, 1280-byte MTU
#   bad   80±40 ms each way, 3% loss, 1.5 Mbit/s, 1280-byte MTU
#   clean 0 ms (the namespace's loopback, unshaped)
set -eu
cd "$(dirname "$0")/.."
bin=$(realpath "${1:?usage: $0 <binary> <profile> [soak args...]}")
profile=${2:-vpn}
shift 2
if [ -z "${SKYLIGHT_IN_NETNS:-}" ]; then
  SKYLIGHT_IN_NETNS=1 exec unshare --user --map-root-user --net "$0" "$bin" "$profile" "$@"
fi
ip link set lo up
case "$profile" in
  vpn) ip link set lo mtu 1280; tc qdisc add dev lo root netem delay 60ms 25ms distribution normal loss 1% rate 4mbit limit 10000 ;;
  bad) ip link set lo mtu 1280; tc qdisc add dev lo root netem delay 80ms 40ms distribution normal loss 3% rate 1500kbit limit 10000 ;;
  clean) ;;
  *) echo "unknown profile $profile" >&2; exit 2 ;;
esac
command -v ethtool >/dev/null && ethtool -K lo tso off gso off gro off 2>/dev/null || true
export TESTBED_PORT=${TESTBED_PORT:-5592} TESTBED_DISPLAY=${TESTBED_DISPLAY:-:73}
export TESTBED_DIR=${TESTBED_DIR:-${TMPDIR:-/tmp}/skylight-netns-$$}
scripts/testbed.sh start "$bin" >/dev/null
trap 'scripts/testbed.sh stop' EXIT
node scripts/soak.mjs --base "http://127.0.0.1:$TESTBED_PORT/" --key "$TESTBED_DIR/ticket-private.pem" "$@"
