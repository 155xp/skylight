# A clean build environment with no GPU vendor packages: no NVIDIA driver, no CUDA, no
# nv-codec-headers. Skylight must build, pass its tests and stream (on software encoding) here.
FROM rust:1-trixie
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev libswscale-dev \
      clang pkg-config nodejs xvfb \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy
WORKDIR /src
