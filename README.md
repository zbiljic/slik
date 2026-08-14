# slik

`slik` is a Rust CLI for running a GStreamer video inference pipeline. It
accepts a synthetic test pattern, a video file, or an H.264/H.265 RTSP stream
and performs inference with either Tract or ONNX Runtime.

The default configuration is headless: it uses the NanoDet-Plus-m 320x320
model, runs Tract on the CPU, and discards processed frames. An optional preview
displays the model-sized frame with detection metadata overlaid.

## Requirements

- Rust, configured by [`mise`](https://mise.jdx.dev/)
- GStreamer 1.24 or newer, including development files, command-line tools, and
  the base and good plugin collections
- `curl` and `sha256sum` or `shasum` to provision model assets

On macOS with Homebrew:

```sh
brew install gstreamer
```

On Ubuntu or Debian:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  pkg-config \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-tools \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-libav
```

Additional plugins may be required for codecs used by a particular file or
RTSP stream. Preview output also requires `objectdetectionoverlay` and
`autovideosink`; on Ubuntu or Debian, install the bad and good plugin
collections.

## Quick start

Install the pinned toolchain, check GStreamer, and download the default model:

```sh
mise install
mise run setup
```

Run the live test pattern in optimized release mode and stop it with Ctrl-C:

```sh
mise exec -- cargo run --locked --release -p slik
```

Release builds are recommended for normal operation because inference is
substantially slower without compiler optimizations. Use the default debug
build only for development and quick validation.

`mise run setup` verifies the development libraries and required runtime
elements before downloading the model. Downloads are checked against pinned
SHA-256 hashes; model binaries are kept out of Git.

The setup steps can also be run independently:

```sh
mise run check:gstreamer
mise run check:gstreamer:plugins
mise run setup:model
```

See [Models](docs/models.md) for supported contracts, the alternative fixture,
and custom model paths.

## Sources and output

With no `--source`, or with `--source test`, `slik` uses a synthetic test
pattern. A file path is decoded through GStreamer, while an `rtsp://` URL must
provide H.264 or H.265 video over RTP.

```sh
# Headless test pattern
mise exec -- cargo run --locked --release -p slik

# Video file
mise exec -- cargo run --locked --release -p slik -- \
  --source /path/to/video.mp4

# RTSP camera
mise exec -- cargo run --locked --release -p slik -- \
  --source rtsp://camera.example/stream
```

The default `discard` output is suitable for CI and servers. To display
annotated detections, first verify the optional elements and then select
`preview`:

```sh
mise run check:gstreamer:preview
mise exec -- cargo run --locked --release -p slik -- \
  --source test \
  --output preview
```

Preview shows the 320x320 or 416x416 inference frame, not the source's original
resolution. Press Ctrl-C to stop; closing the preview window is not a portable
shutdown mechanism.

## Inference backends

The CPU backends are available in the default build:

```sh
mise exec -- cargo run --locked --release -p slik -- --runtime tract
mise exec -- cargo run --locked --release -p slik -- --runtime ort --threads 2
```

Apple hardware providers are opt-in Cargo features:

```sh
mise exec -- cargo run --locked --release -p slik --features metal -- \
  --runtime tract-metal
mise exec -- cargo run --locked --release -p slik --features coreml -- \
  --runtime ort-coreml
```

`--threads` sets the intra-op thread count for the ORT backends and is ignored
by Tract. See [Inference benchmarks](docs/inference-benchmarks.md) for measured
release performance and the reproducible comparison procedure.

## Frame pacing

Use `--pace` to choose how the pipeline behaves when inference and input rates
differ:

| Mode | Behavior |
| --- | --- |
| `auto` | Default. Uses `realtime` for files and `fast` for test and RTSP sources. |
| `fast` | Keeps the freshest frame by dropping queued frames; a file can race to EOS. |
| `realtime` | Follows the source frame rate while still keeping only the freshest queued frame. |
| `full` | Processes every frame and lets inference throttle the decoder. With the test source, this is an unpaced throughput mode. |

For example, process every frame in a file:

```sh
mise exec -- cargo run --locked --release -p slik -- \
  --source /path/to/video.mp4 \
  --pace full
```

## RTSP controls

RTSP defaults to TCP with a 2000 ms jitterbuffer. The RTSP flags are accepted
for every source type but ignored for test patterns and files.

| Option | Default | Behavior |
| --- | --- | --- |
| `--rtsp-transport tcp\|udp\|auto` | `tcp` | Use interleaved TCP, unicast UDP, or GStreamer's full TCP/UDP selection. |
| `--rtsp-latency-ms N` | `2000` | Set jitterbuffer latency. More buffering tolerates jitter at the cost of responsiveness. |
| `--rtsp-drop-on-latency` | disabled | Bound accumulated latency by dropping data that arrives too late. |

For a bounded UDP-unicast profile:

```sh
mise exec -- cargo run --locked --release -p slik -- \
  --source rtsp://camera.example/stream \
  --rtsp-transport udp \
  --rtsp-latency-ms 500 \
  --rtsp-drop-on-latency
```

These options tune the source jitterbuffer; UDP does not guarantee lower
latency, and TCP does not prevent all forms of loss. The current application
runs the pipeline once and exits on a pipeline error; it does not reconnect
automatically.

Run `mise exec -- cargo run --locked -p slik -- --help` for the complete CLI
reference.

## Pipeline

The pipeline is assembled directly through the GStreamer Rust API. Source
frames are scaled to the model contract, converted to BGR, passed through a
[`gstsmith-rs`](https://github.com/zbiljic/gstsmith-rs) inference element, and
decoded into standard GStreamer analytics metadata by `nanodettensordec`.
[`gstsmith-app`](https://github.com/zbiljic/gstsmith-app-rs) provides GStreamer
initialization, pipeline lifecycle, shutdown handling, and the asynchronous
runner.

## Development

Run the complete local validation gate before submitting a change:

```sh
mise run pre-commit
```

For a faster iteration loop, run individual tasks:

```sh
mise run fmt:check
mise run lint
mise run deps:check
mise run check
mise run test
```

Build an optimized binary with:

```sh
mise run build:release
```
