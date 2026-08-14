# Inference benchmarks

This document records end-to-end inference benchmarks and the procedure used
to produce them. Keep old results when adding a new run: add a separate section,
record the exact source revisions and model hash, and use the same measurement
method when comparing results.

The current pipeline performs inference exclusively through reusable gstsmith
elements:

```text
video -> tractinference/ortinference -> nanodettensordec -> sink
```

The custom pad-probe implementation appears below only as repository history.
Future benchmarks should measure the plugin pipeline and should not restore or
maintain the probe implementation.

## Procedure for future runs

1. Record the machine, OS, GStreamer/Rust versions, source revisions, provider
   features, model hash, ModelInfo, caps, and thread settings.
2. Build each tested revision or feature combination from a clean or disposable
   source tree with the same release profile. Do not benchmark a debug target.
3. Use the same model and input pattern. Confirm `--pace full` makes the test
   source non-live; otherwise every capable runtime will appear capped near 30
   FPS.
4. Confirm `model-channel-order=bgr` is set and caps still describe the actual
   buffer layout.
5. Warm each provider once before recording results. Hardware-provider first
   use can include graph compilation and cache setup.
6. Prefer at least three 30-second recorded runs per mode. Report the median
   and spread, not only the best run.
7. For precise steady state, connect to `fpsdisplaysink`'s
   `fps-measurements` signal or use a measurement-only downstream buffer
   counter that resets after a fixed warm-up interval.
8. Check logs for startup/provider errors. Where supported, use strict provider
   assignment separately to determine whether ORT CPU fallback is required.
9. Note thermal state, power mode, and competing workloads. Run variants in an
   interleaved order if differences are small.
10. Restore or discard every measurement-only source change and rebuild the
    production release binary before handing off.

## Historical probe implementation

Commit [`13f92b5`](https://github.com/zbiljic/slik/commit/13f92b5) is the last
clean revision used as the probe baseline. At that revision, `slik` owned the
Tract and ONNX Runtime backends directly in `src/infer/` and the model-specific
preprocessing and decoding in `src/nanodet.rs`.

The GStreamer pipeline placed an `identity` element named `infer` immediately
before its terminal sink. During startup, the application attached a buffer
probe to that element's source pad. The callback mapped each BGR video frame,
prepared the model tensor, invoked the selected runtime, decoded NanoDet
results, and emitted progress metrics. Inference errors were posted to the
pipeline bus from the callback.

That application-owned inference path was removed in favor of reusable
`tractinference` or `ortinference` and `nanodettensordec` elements. It is not a
supported alternative architecture; the commit is the source of record when
the old implementation needs to be inspected.

## Migration comparison

This section records the one-time migration comparison between gstsmith
elements and the previous custom inference probe. It is retained to explain
the performance trade-off observed during the migration, not as an architecture
that `slik` continues to support.

### Purpose

This run compared the reusable gstsmith pipeline

```text
video -> tractinference/ortinference -> nanodettensordec -> sink
```

with the previous slik implementation, which ran preprocessing, inference, and
NanoDet decoding from a custom GStreamer pad probe.

The test measured the complete steady streaming path rather than an isolated
model invocation. Results therefore include video preprocessing, inference,
output handling, NanoDet decoding/NMS, analytics metadata creation, GStreamer
element boundaries, and the measurement sink.

### Revisions and environment

| Item | Value |
|---|---|
| Machine | Mac Studio, Apple M1 Max |
| CPU cores | 10: 8 performance, 2 efficiency |
| Memory | 32 GB |
| GStreamer | 1.28.5 from Homebrew |
| Rust | 1.97.1 |
| slik probe baseline | clean commit `13f92b5` |
| slik plugin version | migration working tree based on `13f92b5` |
| gstsmith-rs | `main` at `c989860` (`feat(inference): add model channel order`) |
| Model | NanoDet-Plus-m 320 ONNX |
| Model SHA-256 | `4f12723cce3d48e47ca92cb925ba74d97a965c069208edca660bbb9f7ce2c610` |
| Input | synthetic `videotestsrc`, 320x320 BGR |
| Pacing | full/unpaced; `videotestsrc is-live=false` |
| ORT threads | default ONNX Runtime policy; no `--threads` override |
| Measurement | one recorded 15-second run per runtime |

The plugin pipeline used truthful BGR video caps and
`model-channel-order=bgr`. Its ModelInfo normalization ranges were in semantic
R, G, B order. The model and companion were located at:

```text
crates/slik/models/nanodet-plus-m-320.onnx
crates/slik/models/nanodet-plus-m-320.onnx.modelinfo
```

The `models/` directory was ignored by Git, so the model and companion had to
be present locally in both benchmark copies.

### Release builds

The current plugin implementation was built in every supported feature
combination:

```sh
mise exec -- cargo build --release -p slik
mise exec -- cargo build --release -p slik --features metal
mise exec -- cargo build --release -p slik --features coreml
mise exec -- cargo build --release -p slik --features coreml,metal
```

All four builds completed successfully. Release settings came from the root
manifest: optimization level 3, one codegen unit, LTO enabled, and symbols
stripped. The combined-feature binary was used for every timed plugin runtime
so compiler settings and executable composition remained identical.

The baseline was exported from the clean commit rather than produced by
reverting the active working tree:

```sh
git archive 13f92b5 | tar -x -C /path/to/disposable-probe-copy
```

It was then built with the same release profile and both provider features:

```sh
cargo build --release -p slik --features coreml,metal
```

### Measurement harness

All measurement-only changes were made in disposable copies outside the
repository. No FPS instrumentation was left in the application source.

Both implementations replaced their terminal `fakesink` with a
`fpsdisplaysink` wrapping a `fakesink`:

```rust
let video_sink = make("fakesink", "benchmark-video-sink")?;
let sink = make("fpsdisplaysink", "sink")?;
sink.set_property("video-sink", &video_sink);
sink.set_property("text-overlay", false);
sink.set_property("fps-update-interval", 1_000i32);
sink.set_property("sync", false);
```

The plugin implementation already makes the synthetic source non-live for
`--pace full`. The disposable probe baseline was changed to
`videotestsrc is-live=false` as well. Without this adjustment, the old test
source is clock-limited to 30 FPS and cannot reveal inference throughput.

Each runtime was executed for 15 wall-clock seconds and stopped cleanly with
SIGINT. For example:

```sh
GST_DEBUG_NO_COLOR=1 GST_DEBUG=fpsdisplaysink:6 \
  ./target/release/slik --pace full --runtime tract \
  > bench-tract.log 2>&1 &
bench_pid=$!
sleep 15
kill -INT "$bench_pid"
wait "$bench_pid"
rg "Average-fps|pipeline stopped|ERROR" bench-tract.log
```

Repeat the command with these runtime values:

```text
tract
tract-metal
ort
ort-coreml
```

The reported value is `fpsdisplaysink`'s final `Average-fps` from its stop
message. The FPS timer covers frames processed after the pipeline starts; model
loading before pipeline startup does not count as zero-FPS benchmark time.

### Results

Higher FPS and lower milliseconds per frame are better. The change percentage
is `(plugin FPS / probe FPS - 1) * 100`.

| Runtime | Plugin pipeline | Probe baseline | Change | Plugin ms/frame | Probe ms/frame |
|---|---:|---:|---:|---:|---:|
| Tract CPU | 26.90 FPS | 30.12 FPS | -10.69% | 37.17 | 33.20 |
| Tract Metal | 33.18 FPS | 37.08 FPS | -10.52% | 30.14 | 26.97 |
| ORT CPU | 103.23 FPS | 108.93 FPS | -5.23% | 9.69 | 9.18 |
| ORT CoreML | 41.83 FPS | 43.07 FPS | -2.88% | 23.91 | 23.22 |

The one-second steady ranges printed by `fpsdisplaysink` were:

| Runtime | Plugin min-max | Probe min-max |
|---|---:|---:|
| Tract CPU | 25.91-26.94 FPS | 28.24-29.38 FPS |
| Tract Metal | 30.99-33.84 FPS | 32.95-38.08 FPS |
| ORT CPU | 96.32-98.50 FPS | 100.85-103.61 FPS |
| ORT CoreML | 36.90-40.28 FPS | 39.94-40.99 FPS |

ORT's final average is higher than its recorded steady min-max because the FPS
sink includes an initial queued-frame burst in the cumulative average. The
same method was used for both implementations, but future runs should prefer
multiple longer samples or signal-based post-warm-up frame counting when small
differences matter.

### Interpretation

- The reusable plugin pipeline was slower in all four modes in this run.
- The gap was about 10.5-10.7% for Tract, 5.2% for ORT CPU, and 2.9% for ORT
  CoreML.
- The plugin path pays for a generic tensor contract, tensor metadata/output
  ownership, a separate decoder element, and additional GStreamer boundaries.
  The old probe called model-specific code directly.
- ORT CPU was the fastest runtime for this model by a wide margin.
- Metal improved Tract throughput but did not approach ORT CPU.
- CoreML did not beat ORT CPU. Provider graph partitioning, unsupported nodes,
  host transfers, and dispatch overhead can outweigh acceleration for this
  model. Selecting CoreML does not prove complete Neural Engine execution.

The performance cost should be weighed against the standardized path's
benefits: backend interchangeability, normal GStreamer composition, tensor
decoder reuse, explicit caps/contracts, and removal of application-specific
inference probes.

Release builds emitted one unrelated future-compatibility warning for
`block v0.1.6`; it did not fail compilation or the benchmark runs.
