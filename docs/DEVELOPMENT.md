# Development, tests and benchmarks

```sh
cargo test                       # unit tests (tickets, pacing, framing, damage, encoder)
cargo test -- --ignored          # also the NVENC checks (need a GPU)
cargo clippy --all-targets
pnpm install && pnpm test        # ticket interop between the Node minter and the Rust verifier
skylight bench                   # encoder speed and still-image quality on this machine
node scripts/bench.mjs           # end to end in Chromium: latency, frame rate, bandwidth
node scripts/bench.mjs --base https://desk.example.com/   # the same through your public URL
node scripts/bench.mjs --connects 7   # time to the first frame, and back after a dropped socket
node scripts/bench.mjs --motion follow   # dragging: frame pacing, frames never shown, repeats
node scripts/bench.mjs --quality 8 --mbps 16   # a page of text after a change, against /api/screenshot
./scripts/final-bench.sh         # all of the above plus server RSS, into docs/results/
                                 # (set SKYLIGHT_PUBLIC_URL to include the public run)
node scripts/screenshots.mjs     # the viewer screenshots in docs/results/

scripts/ci.sh                    # build, test, and check no GPU vendor library is linked
docker build -t skylight-ci ci && docker run --rm -v "$PWD:/src:ro" -e CARGO_TARGET_DIR=/tmp/t \
  skylight-ci scripts/ci.sh      # the same in a clean container without any vendor packages
scripts/testbed.sh start         # a second server on a virtual X display, with its own keys
SKYLIGHT_ENCODER=software scripts/testbed.sh start   # the same, forced onto one encoder family
scripts/link-bench.sh out/       # against it: loopback, a 47 ms path, 10 and 5 Mbit/s links
node scripts/netem.mjs --rtt 60 --mbps 5   # just the emulated link, in front of any server
node scripts/netem.mjs --rtt 45 --swing 35:4000 --kill /input:600 --hang /ws:3000
                                 # a tunnel whose latency drifts and whose sockets die
```

The browser scripts need Chromium at `/usr/bin/chromium` and the service's private key at
`~/.config/skylight/ticket-private.pem` (or `--key`); tickets use the hostname as `aud` unless
you pass `--aud`. `bench.mjs --jank 25` keeps the page's thread busy 25 ms out of every 50, and
`--no-worker` decodes on it, for comparing the two; `--no-smooth` draws every frame the moment it
is decoded. With `--server-pid` (and `--x-pid` for the X server) it also reports CPU use. [PROTOCOL.md](PROTOCOL.md)
