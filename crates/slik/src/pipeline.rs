use std::path::Path;

use anyhow::{Context as _, Result};
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineBin, gst, make};

use crate::bins::sink::{Output, Sink};
use crate::bins::source::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Runtime {
    /// Pure-Rust `tract` runtime on the CPU.
    Tract,
    /// `tract` with its Metal execution provider.
    TractMetal,
    /// ONNX Runtime on the CPU.
    Ort,
    /// ONNX Runtime with its `CoreML` execution provider.
    OrtCoreml,
}

impl Runtime {
    fn factory(self) -> &'static str {
        match self {
            Self::Tract | Self::TractMetal => "tractinference",
            Self::Ort | Self::OrtCoreml => "ortinference",
        }
    }

    fn execution_provider(self) -> &'static str {
        match self {
            Self::Tract | Self::Ort => "cpu",
            Self::TractMetal => "metal",
            Self::OrtCoreml => "coreml",
        }
    }
}

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

#[expect(
    clippy::too_many_arguments,
    reason = "pipeline builder mirrors the CLI's independent options"
)]
pub(crate) fn build(
    source: &Source,
    pace: Pace,
    runtime: Runtime,
    output: Output,
    model: &Path,
    model_info: &Path,
    labels: &Path,
    threads: Option<usize>,
) -> Result<(gst::Pipeline, Option<gst::Pad>)> {
    let crop = make("videocrop", "crop")?;

    let scale = make("videoscale", "scale")?;
    scale.set_property("add-borders", true);

    let convert = make("videoconvert", "convert")?;

    let capsfilter = make("capsfilter", "caps")?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGR")
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

    let inference = make(runtime.factory(), "infer")?;
    inference.set_property("model-file", model.to_string_lossy().as_ref());
    inference.set_property("model-info-file", model_info.to_string_lossy().as_ref());
    inference.set_property_from_str("execution-provider", runtime.execution_provider());
    // Keep video caps truthful and independently pack the model tensor in the
    // BGR order expected by the published NanoDet model.
    inference.set_property_from_str("model-channel-order", "bgr");
    if matches!(runtime, Runtime::Ort | Runtime::OrtCoreml)
        && let Some(threads) = threads
    {
        let threads = u32::try_from(threads).context("ORT thread count does not fit in u32")?;
        inference.set_property("intra-op-threads", threads);
    }

    let decoder = make("nanodettensordec", "decode")?;
    decoder.set_property("label-file", labels.to_string_lossy().as_ref());

    let pipeline = gst::Pipeline::with_name("slik");

    // A full-paced synthetic source is a throughput benchmark: do not make it
    // wait on the clock. Other pace modes retain the live 30 FPS behavior.
    let source_bin = source.build_with_test_live(pace != Pace::Full)?;
    let watch = source.watch_pad(&source_bin)?;
    pipeline.add(&source_bin).context("adding source bin")?;
    pipeline
        .add_many([
            &crop,
            &scale,
            &convert,
            &capsfilter,
            &queue,
            &inference,
            &decoder,
        ])
        .context("adding pipeline elements")?;

    let sink_bin = Sink::new(output).build()?;
    pipeline.add(&sink_bin).context("adding output bin")?;
    decoder
        .link(&sink_bin)
        .context("linking decoder to output bin")?;

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
        &inference,
        &decoder,
    ])
    .context("linking the detection preprocessing chain")?;

    Ok((pipeline, watch))
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use super::*;

    static INIT: Once = Once::new();

    fn init_plugins() {
        INIT.call_once(|| {
            gstsmith_app::init().expect("GStreamer should initialize");
            gstnanodet::plugin_register_static().expect("NanoDet plugin should register");
            gsttractinference::plugin_register_static().expect("Tract plugin should register");
            gstortinference::plugin_register_static().expect("ORT plugin should register");
        });
    }

    fn build_test_pipeline(
        pace: Pace,
        runtime: Runtime,
        output: Output,
    ) -> Result<(gst::Pipeline, Option<gst::Pad>)> {
        build(
            &Source::parse(None),
            pace,
            runtime,
            output,
            Path::new("model.onnx"),
            Path::new("model.onnx.modelinfo"),
            Path::new("coco.names"),
            None,
        )
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
        init_plugins();
        let (pipeline, watch) = build_test_pipeline(Pace::Fast, Runtime::Tract, Output::Discard)
            .expect("pipeline builds");
        assert!(pipeline.by_name("source").is_some());
        let inference = pipeline.by_name("infer").expect("inference element exists");
        assert_eq!(inference.type_().name(), "GstSmithTractInference");
        let channel_order_value = inference.property_value("model-channel-order");
        let (_, channel_order) = gst::glib::EnumValue::from_value(&channel_order_value)
            .expect("model-channel-order has an enum value");
        assert_eq!(channel_order.nick(), "bgr");
        assert!(pipeline.by_name("model-color-order").is_none());
        let decoder = pipeline.by_name("decode").expect("decoder element exists");
        assert_eq!(decoder.type_().name(), "GstSmithNanoDetTensorDec");
        assert!(watch.is_none(), "static source needs no watchdog");
    }

    #[test]
    fn model_caps_delegate_spatial_dimensions_to_inference() {
        init_plugins();
        for (runtime, expected_type) in [
            (Runtime::Tract, "GstSmithTractInference"),
            (Runtime::Ort, "GstSmithOrtInference"),
        ] {
            let (pipeline, _) =
                build_test_pipeline(Pace::Fast, runtime, Output::Discard).expect("pipeline builds");
            let capsfilter = pipeline.by_name("caps").expect("capsfilter exists");
            let caps = capsfilter.property::<gst::Caps>("caps");
            let structure = caps.structure(0).expect("caps structure exists");
            assert_eq!(
                structure.get::<&str>("format").expect("format exists"),
                "BGR"
            );
            structure.get::<i32>("width").unwrap_err();
            structure.get::<i32>("height").unwrap_err();
            let inference = pipeline.by_name("infer").expect("inference element exists");
            assert_eq!(inference.type_().name(), expected_type);
        }
    }

    #[test]
    fn full_pace_uncaps_test_source() {
        init_plugins();
        for (pace, expected_live) in [(Pace::Fast, true), (Pace::Full, false)] {
            let (pipeline, _) = build_test_pipeline(pace, Runtime::Tract, Output::Discard)
                .expect("pipeline builds");
            let source = pipeline
                .by_name("source-input")
                .expect("test source element exists");
            assert_eq!(source.property::<bool>("is-live"), expected_live);
        }
    }

    #[test]
    fn realtime_inserts_pace_element() {
        init_plugins();
        let (pipeline, _) = build(
            &Source::parse(Some("/x.mp4")),
            Pace::Realtime,
            Runtime::Ort,
            Output::Discard,
            Path::new("model.onnx"),
            Path::new("model.onnx.modelinfo"),
            Path::new("coco.names"),
            Some(2),
        )
        .expect("pipeline builds");
        assert!(
            pipeline.by_name("pace").is_some(),
            "realtime inserts a pace identity"
        );
        let (pipeline, _) = build(
            &Source::parse(Some("/x.mp4")),
            Pace::Fast,
            Runtime::Tract,
            Output::Discard,
            Path::new("model.onnx"),
            Path::new("model.onnx.modelinfo"),
            Path::new("coco.names"),
            None,
        )
        .expect("pipeline builds");
        assert!(
            pipeline.by_name("pace").is_none(),
            "fast has no pace element"
        );
    }

    #[test]
    fn runtime_selects_plugin_and_provider() {
        assert_eq!(Runtime::Tract.factory(), "tractinference");
        assert_eq!(Runtime::TractMetal.execution_provider(), "metal");
        assert_eq!(Runtime::Ort.factory(), "ortinference");
        assert_eq!(Runtime::OrtCoreml.execution_provider(), "coreml");
    }

    #[test]
    fn throttle_queue_preserves_pace_backpressure_policy() {
        init_plugins();
        for (pace, expected_leaky) in [
            (Pace::Fast, "downstream"),
            (Pace::Realtime, "downstream"),
            (Pace::Full, "no"),
        ] {
            let (pipeline, _) = build_test_pipeline(pace, Runtime::Tract, Output::Discard)
                .expect("pipeline builds");
            let queue = pipeline.by_name("throttle").expect("throttle queue exists");
            let leaky_value = queue.property_value("leaky");
            let (_, leaky) = gst::glib::EnumValue::from_value(&leaky_value)
                .expect("throttle queue leaky has an enum value");
            assert_eq!(leaky.nick(), expected_leaky);
            assert_eq!(queue.property::<u32>("max-size-buffers"), 1);
            assert_eq!(queue.property::<u64>("max-size-time"), 0);
            assert_eq!(queue.property::<u32>("max-size-bytes"), 0);
        }
    }
}
