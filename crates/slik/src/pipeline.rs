use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineBin, gst, make};
use tracing::info;

use crate::bins::source::Source;
use crate::infer::Detector;
use crate::nanodet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Pace {
    /// Per source: file = realtime, test/rtsp = fast.
    Auto,
    /// Leaky queue, sink sync off — sample the freshest frame, race a file to EOS.
    Fast,
    /// Pace to the source's real frame rate, still dropping to the freshest frame.
    Realtime,
    /// Process every frame; throttle the decoder to inference speed.
    Full,
}

impl Pace {
    pub(crate) fn resolve(self, source: &Source) -> Self {
        match self {
            Self::Auto if source.is_file() => Self::Realtime,
            Self::Auto => Self::Fast,
            other => other,
        }
    }
}

pub(crate) fn build(source: &Source, pace: Pace) -> Result<(gst::Pipeline, Option<gst::Pad>)> {
    let crop = make("videocrop", "crop")?;

    let scale = make("videoscale", "scale")?;
    scale.set_property("add-borders", true);

    let convert = make("videoconvert", "convert")?;

    let capsfilter = make("capsfilter", "caps")?;
    let input_dim =
        i32::try_from(nanodet::INPUT).context("NanoDet input dimension does not fit in i32")?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGR")
        .field("width", input_dim)
        .field("height", input_dim)
        .build();
    capsfilter.set_property("caps", &caps);

    let queue = make("queue", "throttle")?;
    let leaky = if pace == Pace::Full {
        "no"
    } else {
        "downstream"
    };
    queue.set_property_from_str("leaky", leaky);
    queue.set_property("max-size-buffers", 1u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property("max-size-bytes", 0u32);

    let identity = make("identity", "infer")?;

    let sink = make("fakesink", "sink")?;
    sink.set_property("sync", false);

    let pipeline = gst::Pipeline::with_name("slik");

    let source_bin = source.build()?;
    let watch = source.watch_pad(&source_bin)?;
    pipeline.add(&source_bin).context("adding source bin")?;
    pipeline
        .add_many([
            &crop,
            &scale,
            &convert,
            &capsfilter,
            &queue,
            &identity,
            &sink,
        ])
        .context("adding pipeline elements")?;

    if pace == Pace::Realtime {
        let paced = make("identity", "pace")?;
        paced.set_property("sync", true);
        pipeline.add(&paced).context("adding pace identity")?;
        source_bin
            .link(&paced)
            .context("linking source bin -> pace")?;
        paced.link(&crop).context("linking pace -> videocrop")?;
    } else {
        source_bin
            .link(&crop)
            .context("linking source bin -> videocrop")?;
    }
    gst::Element::link_many([
        &crop,
        &scale,
        &convert,
        &capsfilter,
        &queue,
        &identity,
        &sink,
    ])
    .context("linking the detection preprocessing chain")?;

    Ok((pipeline, watch))
}

/// Attach a buffer probe to `element`'s src pad that runs `NanoDet` detection on
/// each frame and prints the objects it sees.
pub(crate) fn install_frame_probe(
    pipeline: &gst::Pipeline,
    element_name: &str,
    model: Arc<dyn Detector>,
) -> Result<()> {
    let element = pipeline
        .by_name(element_name)
        .ok_or_else(|| anyhow!("pipeline has no element named '{element_name}'"))?;
    let src_pad = element
        .static_pad("src")
        .ok_or_else(|| anyhow!("element '{element_name}' has no static src pad"))?;

    let frames = Arc::new(AtomicU64::new(0));
    let detections = Arc::new(AtomicU64::new(0));
    let run_us = Arc::new(AtomicU64::new(0));
    let last_beat = Arc::new(Mutex::new(Instant::now()));

    src_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let t0 = Instant::now();
        let result = nanodet::run_inference(pad, info, model.as_ref());
        let dt = t0.elapsed();
        match result {
            Ok(n) => {
                let f = frames.fetch_add(1, Relaxed) + 1;
                detections.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Relaxed);
                let dt_us = u64::try_from(dt.as_micros()).unwrap_or(u64::MAX);
                let total_us = run_us.fetch_add(dt_us, Relaxed) + dt_us;
                if let Ok(mut beat) = last_beat.lock()
                    && (f == 1 || beat.elapsed() >= Duration::from_secs(1))
                {
                    let infer_ms = (dt.as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
                    // Integer average (µs → ms, no float cast) to satisfy clippy::cast_precision_loss.
                    let avg_infer_ms = total_us / 1000 / f;
                    info!(
                        frames = frames.load(Relaxed),
                        detections = detections.load(Relaxed),
                        infer_ms,
                        avg_infer_ms,
                        "pipeline running"
                    );
                    *beat = Instant::now();
                }
            }
            Err(err) => {
                gst::element_error!(
                    element,
                    gst::CoreError::Failed,
                    ("inference failed"),
                    ["{err:#}"]
                );
                return gst::PadProbeReturn::Remove;
            }
        }
        gst::PadProbeReturn::Ok
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use gstsmith_app::PipelineRunner;

    use super::*;

    struct FailingDetector;

    impl Detector for FailingDetector {
        fn infer(&self, _input: &[f32]) -> Result<Vec<f32>> {
            anyhow::bail!("fake detector failure")
        }
    }

    #[test]
    fn pace_auto_resolves_per_source() {
        assert_eq!(Pace::Auto.resolve(&Source::parse(None)), Pace::Fast);
        assert_eq!(
            Pace::Auto.resolve(&Source::parse(Some("/x.mp4"))),
            Pace::Realtime
        );
        assert_eq!(
            Pace::Auto.resolve(&Source::parse(Some("rtsp://x"))),
            Pace::Fast
        );
        assert_eq!(
            Pace::Full.resolve(&Source::parse(Some("/x.mp4"))),
            Pace::Full
        );
    }

    #[test]
    fn builds_test_source_pipeline() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (pipeline, watch) = build(&Source::parse(None), Pace::Fast).expect("pipeline builds");
        assert!(pipeline.by_name("source").is_some());
        assert!(pipeline.by_name("infer").is_some());
        assert!(watch.is_none(), "static source needs no watchdog");
    }

    #[test]
    fn realtime_inserts_pace_element() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (pipeline, _) =
            build(&Source::parse(Some("/x.mp4")), Pace::Realtime).expect("pipeline builds");
        assert!(
            pipeline.by_name("pace").is_some(),
            "realtime inserts a pace identity"
        );
        let (pipeline, _) =
            build(&Source::parse(Some("/x.mp4")), Pace::Fast).expect("pipeline builds");
        assert!(
            pipeline.by_name("pace").is_none(),
            "fast has no pace element"
        );
    }

    #[tokio::test]
    async fn inference_failure_reaches_pipeline_bus() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (pipeline, watch) = build(&Source::parse(None), Pace::Fast).expect("pipeline builds");
        assert!(watch.is_none(), "static source needs no watchdog");
        install_frame_probe(&pipeline, "infer", Arc::new(FailingDetector))
            .expect("frame probe installs");
        let observed = pipeline.clone();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            PipelineRunner::new(pipeline).run(std::future::pending()),
        )
        .await
        .expect("pipeline runner should receive inference failure before timeout");
        let err = result.expect_err("inference failure should stop the pipeline");
        let detail = format!("{err:#}");

        assert!(detail.contains("running inference"), "error was: {detail}");
        assert!(
            detail.contains("fake detector failure"),
            "error was: {detail}"
        );
        assert_eq!(observed.current_state(), gst::State::Null);
    }
}
