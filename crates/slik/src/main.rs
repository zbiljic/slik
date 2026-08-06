use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineRunner, gst};

mod nanodet;

#[derive(Debug, Parser)]
#[command(version, about = "Run a GStreamer video detection pipeline")]
struct Cli {
    /// Path to the NanoDet-Plus-m 320x320 ONNX model.
    #[arg(
        long,
        value_name = "PATH",
        default_value = "crates/slik/models/nanodet-plus-m-320.onnx"
    )]
    model: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if !cli.model.exists() {
        anyhow::bail!(
            "model file not found: {} — download the NanoDet-Plus-m 320x320 ONNX and pass it with --model",
            cli.model.display()
        );
    }
    let model = nanodet::load_model(&cli.model)?;

    gstsmith_app::init().context("initializing GStreamer")?;
    register_plugins()?;

    let pipeline = build_pipeline()?;
    install_frame_probe(&pipeline, "infer", Arc::clone(&model))?;
    let exit = PipelineRunner::new(pipeline).run(shutdown_signal()).await?;

    println!(
        "pipeline stopped: {exit:?} (build {})",
        env!("SLIK_GIT_HASH")
    );
    Ok(())
}

fn register_plugins() -> Result<()> {
    gstconsole::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith console plugin")?;
    gstlines::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith lines plugin")?;
    Ok(())
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("creating element '{factory}' (named '{name}')"))
}

fn build_pipeline() -> Result<gst::Pipeline> {
    let source = make("videotestsrc", "source")?;
    source.set_property("is-live", true);

    let crop = make("videocrop", "crop")?;

    let scale = make("videoscale", "scale")?;
    scale.set_property("add-borders", true);

    let convert = make("videoconvert", "convert")?;

    let capsfilter = make("capsfilter", "caps")?;
    let input_dim =
        i32::try_from(nanodet::INPUT).context("NanoDet input dimension does not fit in i32")?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGB")
        .field("width", input_dim)
        .field("height", input_dim)
        .build();
    capsfilter.set_property("caps", &caps);

    let queue = make("queue", "throttle")?;
    // Drop the oldest buffers so the probe always sees the freshest frame.
    queue.set_property_from_str("leaky", "downstream");
    queue.set_property("max-size-buffers", 1u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property("max-size-bytes", 0u32);

    let identity = make("identity", "infer")?;

    let sink = make("fakesink", "sink")?;
    sink.set_property("sync", false);

    let pipeline = gst::Pipeline::with_name("slik");
    pipeline
        .add_many([
            &source,
            &crop,
            &scale,
            &convert,
            &capsfilter,
            &queue,
            &identity,
            &sink,
        ])
        .context("adding pipeline elements")?;
    gst::Element::link_many([
        &source,
        &crop,
        &scale,
        &convert,
        &capsfilter,
        &queue,
        &identity,
        &sink,
    ])
    .context("linking the detection preprocessing chain")?;

    Ok(pipeline)
}

/// Attach a buffer probe to `element`'s src pad that runs `NanoDet` detection on
/// each frame and prints the objects it sees.
fn install_frame_probe(
    pipeline: &gst::Pipeline,
    element_name: &str,
    model: Arc<nanodet::Model>,
) -> Result<()> {
    let element = pipeline
        .by_name(element_name)
        .ok_or_else(|| anyhow!("pipeline has no element named '{element_name}'"))?;
    let src_pad = element
        .static_pad("src")
        .ok_or_else(|| anyhow!("element '{element_name}' has no static src pad"))?;

    src_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Err(err) = nanodet::run_inference(pad, info, &model) {
            eprintln!("inference error: {err:#}");
        }
        gst::PadProbeReturn::Ok
    });

    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for Ctrl-C: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_detection_pipeline() {
        gstsmith_app::init().expect("GStreamer should initialize");

        let pipeline = build_pipeline().expect("pipeline should build");

        assert!(pipeline.by_name("source").is_some());
        assert!(pipeline.by_name("infer").is_some());
        assert!(pipeline.by_name("sink").is_some());
    }
}
