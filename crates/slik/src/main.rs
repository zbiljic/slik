use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineRunner, gst};

#[derive(Debug, Parser)]
#[command(version, about = "Run a GStreamer launch-style pipeline")]
struct Cli {
    /// `GStreamer` launch-style pipeline description.
    #[arg(
        value_name = "PIPELINE",
        default_value = "videotestsrc is-live=true ! fakesink"
    )]
    pipeline: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    gstsmith_app::init().context("initializing GStreamer")?;
    register_plugins()?;

    let pipeline = parse_pipeline(&cli.pipeline)?;
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

fn parse_pipeline(description: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(description)
        .with_context(|| format!("parsing GStreamer pipeline: {description}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|element| {
            anyhow!(
                "pipeline description did not produce a GStreamer pipeline (got {})",
                element.type_().name()
            )
        })
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for Ctrl-C: {err}");
    }
}
