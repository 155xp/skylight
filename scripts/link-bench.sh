#!/bin/sh
# The whole benchmark matrix against the test bed (scripts/testbed.sh): loopback, an emulated
# 47 ms path, and two slow links (10 Mbit/s at 40 ms, 5 Mbit/s at 60 ms) through
# scripts/netem.mjs. JSON and text land in the directory given as the first argument.
set -u
cd "$(dirname "$0")/.."
out=${1:?usage: $0 out-dir [samples]}
n=${2:-30}
mkdir -p "$out"
dir=${TESTBED_DIR:-${TMPDIR:-/tmp}/skylight-testbed}
port=${TESTBED_PORT:-5491}
pid=$(cat "$dir/server.pid")
b() { node scripts/bench.mjs --key "$dir/ticket-private.pem" --aud testbed --samples "$n" "$@"; }
link() { # name rtt mbps
  node scripts/netem.mjs --listen 5591 --to "127.0.0.1:$port" --rtt "$2" --mbps "$3" >/dev/null &
  np=$!
  sleep 0.5
  b --base http://127.0.0.1:5591/ --modes click,key --seconds 8 --json "$out/$1.json" >"$out/$1.txt" 2>&1
  b --base http://127.0.0.1:5591/ --modes '' --motion scroll --seconds 8 --json "$out/$1-scroll.json" >"$out/$1-scroll.txt" 2>&1
  kill $np
}
b --base "http://127.0.0.1:$port/" --seconds 8 --server-pid "$pid" --idle 10 --json "$out/local.json" >"$out/local.txt" 2>&1
b --base "http://127.0.0.1:$port/" --modes '' --motion scroll --seconds 8 --json "$out/local-scroll.json" >"$out/local-scroll.txt" 2>&1
link wan47 47 0
link slow10 40 10
link slow5 60 5
grep -H -E "^(click|key|move|motion|glass|idle|      input to displayed)" "$out"/*.txt
