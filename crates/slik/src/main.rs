use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineRunner, gst};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod bins;
mod nanodet;

use crate::bins::source::Source;

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

    /// Video source: empty/"test" = test pattern, a file path, or an rtsp:// URL.
    #[arg(long, value_name = "URI")]
    source: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if !cli.model.exists() {
        anyhow::bail!(
            "model file not found: {} — download the NanoDet-Plus-m 320x320 ONNX and pass it with --model",
            cli.model.display()
        );
    }
    info!(model = %cli.model.display(), "loading NanoDet model");
    let model = nanodet::load_model(&cli.model)?;
    info!("model loaded");

    info!("initializing GStreamer");
    gstsmith_app::init().context("initializing GStreamer")?;
    info!("registering plugins");
    register_plugins()?;

    let source = Source::parse(cli.source.as_deref());
    info!("building pipeline");
    let (pipeline, watch) = build_pipeline(&source)?;
    info!("installing frame probe");
    install_frame_probe(&pipeline, "infer", Arc::clone(&model))?;
    info!(build = env!("SLIK_GIT_HASH"), "starting pipeline");

    let shutdown = async move {
        match watch {
            Some(pad) => {
                tokio::select! {
                    _ = shutdown_signal() => {}
                    _ = link_watchdog(pad, Duration::from_secs(10)) => {}
                }
            }
            None => shutdown_signal().await,
        }
    };

    let exit = PipelineRunner::new(pipeline).run(shutdown).await?;

    info!(?exit, build = env!("SLIK_GIT_HASH"), "pipeline stopped");
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

pub(crate) fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("creating element '{factory}' (named '{name}')"))
}

fn build_pipeline(source: &Source) -> Result<(gst::Pipeline, Option<gst::Pad>)> {
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

    let (source_bin, watch) = source.build_watched()?;
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

    source_bin
        .link(&crop)
        .context("linking source bin -> videocrop")?;
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

    let frames = Arc::new(AtomicU64::new(0));
    let detections = Arc::new(AtomicU64::new(0));
    let last_beat = Arc::new(Mutex::new(Instant::now()));

    src_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        match nanodet::run_inference(pad, info, &model) {
            Ok(n) => {
                let f = frames.fetch_add(1, Relaxed) + 1;
                detections.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Relaxed);
                if let Ok(mut beat) = last_beat.lock()
                    && (f == 1 || beat.elapsed() >= Duration::from_secs(1))
                {
                    info!(
                        frames = frames.load(Relaxed),
                        detections = detections.load(Relaxed),
                        "pipeline running"
                    );
                    *beat = Instant::now();
                }
            }
            Err(err) => {
                warn!(error = %format!("{err:#}"), "inference error");
            }
        }
        gst::PadProbeReturn::Ok
    });

    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(%err, "failed to listen for Ctrl-C");
    }
}

/// Completes (triggering shutdown) if `pad` is still unlinked after `timeout`.
async fn link_watchdog(pad: gst::Pad, timeout: Duration) {
    tokio::time::sleep(timeout).await;
    if pad.is_linked() {
        std::future::pending::<()>().await;
    } else {
        eprintln!("source did not connect within {timeout:?}; shutting down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_test_source_pipeline() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (pipeline, watch) = build_pipeline(&Source::Test).expect("pipeline builds");
        assert!(pipeline.by_name("source-bin").is_some());
        assert!(pipeline.by_name("infer").is_some());
        assert!(watch.is_none(), "static source needs no watchdog");
    }
}
