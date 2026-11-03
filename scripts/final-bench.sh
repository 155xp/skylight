#!/bin/sh
# Full measurement run against the installed service: local and (optionally) public latency, motion,
# encoder speed, and the server's memory while streaming. Results go to docs/results/.
set -u
cd "$(dirname "$0")/.."
mkdir -p docs/results
pid=$(systemctl --user show -p MainPID --value skylight)
# Sample the server's resident memory once a second while the benchmarks stream.
( while kill -0 "$pid" 2>/dev/null; do
    printf '%s %s\n' "$(date +%s)" "$(awk '/VmRSS/ {print $2}' /proc/$pid/status)"
    sleep 1
  done ) > docs/results/rss.txt &
sampler=$!
awk '/VmRSS/ {print "idle_rss_kb", $2}' /proc/$pid/status > docs/results/idle-rss.txt
node scripts/bench.mjs --samples 50 --seconds 10 --json docs/results/local.json --screenshot docs/results/local.png > docs/results/local.txt 2>&1
# The public run goes through whatever puts Skylight on the internet (a reverse proxy behind a
# tunnel, for example): set SKYLIGHT_PUBLIC_URL to that viewer URL, such as https://desk.example.com/.
if [ -n "${SKYLIGHT_PUBLIC_URL:-}" ]; then
  node scripts/bench.mjs --base "$SKYLIGHT_PUBLIC_URL" --samples 50 --seconds 10 --json docs/results/public.json --screenshot docs/results/public.png > docs/results/public.txt 2>&1
fi
kill "$sampler"
{
  ./target/release/skylight bench --backend synthetic --seconds 5 --mbps 50
  ./target/release/skylight bench --backend synthetic --seconds 5 --encoder software --mbps 50
  ./target/release/skylight bench --backend kde --seconds 1 --mbps 50
} > docs/results/encode.txt 2>&1
echo done
