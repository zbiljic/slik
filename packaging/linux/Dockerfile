# Build environment for distributable Linux packages. slik's gstsmith plugins
# require GStreamer 1.28, which Ubuntu first ships in 26.04; its glibc 2.43 is
# therefore the package's minimum glibc.
FROM ubuntu:26.04@sha256:6a06111c3b0a8600feadb270aec771dc239bf407b25625f93f44ab00e11f4d3c

ARG RUST_VERSION

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        file \
        git \
        gstreamer1.0-libav \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-ugly \
        gstreamer1.0-tools \
        gstreamer1.0-x \
        libgstreamer-plugins-bad1.0-dev \
        libgstreamer-plugins-base1.0-dev \
        libgstreamer1.0-dev \
        libssl-dev \
        patchelf \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Installed world-readable so the build can run as the invoking user's UID.
ENV RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH
RUN test -n "$RUST_VERSION" \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | CARGO_HOME=/usr/local/cargo sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION" \
    && chmod -R a+rX /usr/local/rustup /usr/local/cargo
