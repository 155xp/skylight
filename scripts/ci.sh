#!/bin/sh
# What CI runs (see ci/Dockerfile): format and lint checks, build, the Rust and Node tests, and a
# check that nothing ties Skylight to one GPU vendor. Needs no GPU; on a machine without one, the
# encoder must come up as software.
#
#   docker build -t skylight-ci ci && docker run --rm -v "$PWD:/src:ro" -e CARGO_TARGET_DIR=/tmp/t skylight-ci scripts/ci.sh
set -eu
cd "$(dirname "$0")/.."
if dpkg -s libffmpeg-nvenc-dev >/dev/null 2>&1; then
  echo "note: nv-codec-headers are installed; this build does not prove it builds without them"
fi
cargo fmt --check
cargo clippy --release --locked --all-targets -- -D warnings
cargo build --release --locked
# The X11 clipboard test needs an X server of its own (it overwrites the clipboard).
if [ -z "${SKYLIGHT_TEST_DISPLAY:-}" ] && command -v Xvfb >/dev/null; then
  # On a display number nobody uses: Xvfb picks it and writes it to fd 3.
  d=$(mktemp)
  Xvfb -displayfd 3 -nolisten tcp 3>"$d" 2>/dev/null &
  xvfb=$!
  trap 'kill $xvfb; rm -f "$d"' EXIT
  for _ in 1 2 3 4 5 6 7 8 9 10; do [ -s "$d" ] && break; sleep 0.2; done
  [ -s "$d" ] || { echo "error: Xvfb did not start" >&2; exit 1; }
  export SKYLIGHT_TEST_DISPLAY=":$(cat "$d")"
fi
cargo test --release --locked
node --test clients/node/test/
node --check web/skylight.js
node --check web/viewer.js
bin=${CARGO_TARGET_DIR:-target}/release/skylight
if ldd "$bin" | grep -iE 'cuda|nvidia|nvenc|nvcuvid'; then
  echo "error: the binary links a GPU vendor library; those must be loaded at runtime" >&2
  exit 1
fi
out=$("$bin" bench --backend synthetic --seconds 2 --mbps 10 2>&1)
echo "$out"
if ! ldconfig -p | grep -q libnvidia-encode; then
  echo "$out" | grep -q "encoder libx264" || { echo "error: expected the software fallback" >&2; exit 1; }
fi
echo "ci ok"
