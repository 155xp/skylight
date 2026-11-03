#!/bin/sh
# What CI runs (see ci/Dockerfile): build, test, and check that nothing ties Skylight to one GPU
# vendor. Needs no GPU; on a machine without one, the encoder must come up as software.
#
#   docker build -t skylight-ci ci && docker run --rm -v "$PWD:/src:ro" -e CARGO_TARGET_DIR=/tmp/t skylight-ci scripts/ci.sh
set -eu
cd "$(dirname "$0")/.."
if dpkg -s libffmpeg-nvenc-dev >/dev/null 2>&1; then
  echo "note: nv-codec-headers are installed; this build does not prove it builds without them"
fi
cargo build --release --locked
cargo test --release --locked
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
