use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineRunner, gst};

#[derive(Debug, Parser)]
#[command(version, about = "Run a GStreamer video pipeline")]
struct Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let _cli = Cli::parse();

    gstsmith_app::init().context("initializing GStreamer")?;
    register_plugins()?;

    let pipeline = build_pipeline()?;
    install_frame_probe(&pipeline, "infer")?;
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
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGB")
        .field("width", 320i32)
        .field("height", 320i32)
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

/// Attach a buffer probe to `element`'s src pad. For now it only logs the first
/// frame's caps and a periodic buffer count. Plan 003 runs inference here.
fn install_frame_probe(pipeline: &gst::Pipeline, element_name: &str) -> Result<()> {
    let element = pipeline
        .by_name(element_name)
        .ok_or_else(|| anyhow!("pipeline has no element named '{element_name}'"))?;
    let src_pad = element
        .static_pad("src")
        .ok_or_else(|| anyhow!("element '{element_name}' has no static src pad"))?;

    let count = Arc::new(AtomicU64::new(0));

    src_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(_buffer) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };

        let n = count.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            // Log the negotiated caps once, so we can confirm RGB 320x320.
            match pad.current_caps() {
                Some(caps) => println!("first frame caps: {caps}"),
                None => println!("first frame: caps not yet negotiated"),
            }
        } else if n.is_multiple_of(100) {
            println!("frames seen: {n}");
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

        install_frame_probe(&pipeline, "infer").expect("probe should install");
    }
}
