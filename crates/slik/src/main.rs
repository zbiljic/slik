use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineRunner, gst};
use tracing::{error, info};

mod bins;
mod logging;
mod pipeline;

use crate::bins::sink::Output;
use crate::bins::source::{RtspOptions, RtspTransport, Source};
use crate::pipeline::{Pace, Runtime};

#[derive(Debug, Parser)]
#[command(version, about = "Run a GStreamer video detection pipeline")]
struct Cli {
    /// Path to a `NanoDet` ONNX model; defaults to the bundled 320x320 model.
    #[arg(long, value_name = "PATH")]
    model: Option<PathBuf>,

    /// Model-info contract; defaults to the contract bundled with the selected default model.
    #[arg(long, value_name = "PATH")]
    model_info: Option<PathBuf>,

    /// COCO labels passed to the `NanoDet` tensor decoder; defaults to the bundled labels.
    #[arg(long, value_name = "PATH")]
    labels: Option<PathBuf>,

    /// Video source: empty/"test" = test pattern, a file path, or an rtsp:// URL carrying H.264 or H.265 video over RTP.
    #[arg(long, value_name = "URI")]
    source: Option<String>,

    /// Frame pacing: auto (file=realtime, live=fast), fast, realtime, or full.
    #[arg(long, value_enum, default_value_t = Pace::Auto)]
    pace: Pace,

    /// Inference backend: tract (pure Rust) or ort (ONNX Runtime).
    #[arg(long, value_enum, default_value_t = Runtime::Tract)]
    runtime: Runtime,

    /// Output mode: discard, or preview the model-sized annotated video (requires optional `GStreamer` plugins).
    #[arg(long, value_enum, default_value_t = Output::Discard)]
    output: Output,

    /// Intra-op thread count for the ort backend (ignored by tract). Default: ort's own.
    #[arg(long, value_name = "N")]
    threads: Option<usize>,

    /// RTSP lower transport: tcp, udp (unicast), or auto (tcp+udp-mcast+udp). Ignored for file/test sources.
    #[arg(long, value_enum, default_value_t = RtspTransport::Tcp)]
    rtsp_transport: RtspTransport,

    /// RTSP jitterbuffer latency in milliseconds. Ignored for file/test sources.
    #[arg(long, value_name = "N", default_value_t = 2000)]
    rtsp_latency_ms: u32,

    /// Bound RTSP jitterbuffer latency by dropping late data. Ignored for file/test sources. Default: false.
    #[arg(long, default_value_t = false)]
    rtsp_drop_on_latency: bool,
}

#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    let cli = Cli::parse();
    // Both macOS video sinks and AppleMedia/VideoToolbox decoders can require
    // Cocoa's application main thread, including for otherwise headless runs.
    // Host every pipeline inside GStreamer's macOS application loop.
    gst::macos_main(|| run_with_tokio(cli))
}

#[cfg(not(target_os = "macos"))]
#[tokio::main]
async fn main() -> Result<()> {
    run(Cli::parse()).await
}

#[cfg(target_os = "macos")]
fn run_with_tokio(cli: Cli) -> Result<()> {
    tokio::runtime::Runtime::new()
        .context("creating Tokio runtime")?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    logging::init();

    let resource_dir = std::env::var_os("SLIK_RESOURCE_DIR").map(PathBuf::from);
    let model = resolve_asset(
        cli.model.as_deref(),
        resource_dir.as_deref(),
        "nanodet-plus-m-320.onnx",
        Path::new("crates/slik/models/nanodet-plus-m-320.onnx"),
    );
    let model_info = resolve_asset(
        cli.model_info.as_deref(),
        resource_dir.as_deref(),
        "nanodet-plus-m-320.onnx.modelinfo",
        Path::new("crates/slik/models/nanodet-plus-m-320.onnx.modelinfo"),
    );
    let labels = resolve_asset(
        cli.labels.as_deref(),
        resource_dir.as_deref(),
        "coco.names",
        Path::new("crates/slik/src/coco.names"),
    );

    if !model.exists() {
        anyhow::bail!(
            "model file not found: {} — download a supported NanoDet 320x320 or 416x416 ONNX model and pass it with --model",
            model.display()
        );
    }
    if !model_info.exists() {
        anyhow::bail!("model-info file not found: {}", model_info.display());
    }
    if !labels.exists() {
        anyhow::bail!("label file not found: {}", labels.display());
    }

    info!("initializing GStreamer");
    gstsmith_app::init().context("initializing GStreamer")?;
    info!("registering plugins");
    register_plugins()?;

    let source = Source::parse_with_options(
        cli.source.as_deref(),
        RtspOptions {
            transport: cli.rtsp_transport,
            latency_ms: cli.rtsp_latency_ms,
            drop_on_latency: cli.rtsp_drop_on_latency,
        },
    );
    let watchdog_label = if source.is_rtsp() {
        "rtsp source did not produce decodable H.264 or H.265 video over RTP"
    } else {
        "source did not connect"
    };
    let pace = cli.pace.resolve(&source);
    info!(?pace, "frame pacing");
    info!("building pipeline");
    let (pipeline, watch) = pipeline::build(
        &source,
        pace,
        cli.runtime,
        cli.output,
        &model,
        &model_info,
        &labels,
        cli.threads,
    )?;
    info!(build = env!("SLIK_GIT_HASH"), "starting pipeline");

    let shutdown = async move {
        match watch {
            Some(pad) => {
                tokio::select! {
                    _ = shutdown_signal() => {}
                    _ = link_watchdog(pad, Duration::from_secs(10), watchdog_label) => {}
                }
            }
            None => shutdown_signal().await,
        }
    };

    let exit = PipelineRunner::new(pipeline).run(shutdown).await?;

    info!(?exit, build = env!("SLIK_GIT_HASH"), "pipeline stopped");
    Ok(())
}

fn resolve_asset(
    explicit: Option<&Path>,
    resource_dir: Option<&Path>,
    bundled_name: &str,
    development_default: &Path,
) -> PathBuf {
    explicit.map_or_else(
        || {
            resource_dir.map_or_else(
                || development_default.to_owned(),
                |dir| dir.join(bundled_name),
            )
        },
        Path::to_owned,
    )
}

fn register_plugins() -> Result<()> {
    gstconsole::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith console plugin")?;
    gstlines::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith lines plugin")?;
    gstnanodet::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith NanoDet tensor decoder plugin")?;
    gsttractinference::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith Tract inference plugin")?;
    gstortinference::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith ORT inference plugin")?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(%err, "failed to listen for Ctrl-C");
    }
}

/// Completes (triggering shutdown) if `pad` is still unlinked after `timeout`.
async fn link_watchdog(pad: gst::Pad, timeout: Duration, diagnostic: &str) {
    tokio::time::sleep(timeout).await;
    if pad.is_linked() {
        std::future::pending::<()>().await;
    } else {
        eprintln!("{diagnostic} within {timeout:?}; shutting down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_asset_path_wins_over_bundle_resources() {
        assert_eq!(
            resolve_asset(
                Some(Path::new("custom/model.onnx")),
                Some(Path::new("/bundle/resources")),
                "default.onnx",
                Path::new("development/default.onnx"),
            ),
            Path::new("custom/model.onnx")
        );
    }

    #[test]
    fn bundle_resource_path_wins_over_development_default() {
        assert_eq!(
            resolve_asset(
                None,
                Some(Path::new("/bundle/resources")),
                "default.onnx",
                Path::new("development/default.onnx"),
            ),
            Path::new("/bundle/resources/default.onnx")
        );
    }

    #[test]
    fn development_default_remains_available_outside_bundle() {
        assert_eq!(
            resolve_asset(
                None,
                None,
                "default.onnx",
                Path::new("development/default.onnx"),
            ),
            Path::new("development/default.onnx")
        );
    }
}
