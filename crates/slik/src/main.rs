use anyhow::{Context as _, Result};
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
    let convert = make("videoconvert", "convert")?;
    let sink = make("fakesink", "sink")?;

    let pipeline = gst::Pipeline::with_name("slik");
    pipeline
        .add_many([&source, &convert, &sink])
        .context("adding pipeline elements")?;
    gst::Element::link_many([&source, &convert, &sink])
        .context("linking source ! convert ! sink")?;

    Ok(pipeline)
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
    fn builds_the_pipeline() {
        gstsmith_app::init().expect("GStreamer should initialize");

        let pipeline = build_pipeline().expect("pipeline should build");

        // videotestsrc + videoconvert + fakesink
        assert_eq!(pipeline.children().len(), 3);
        assert!(pipeline.by_name("source").is_some());
        assert!(pipeline.by_name("sink").is_some());
    }
}
