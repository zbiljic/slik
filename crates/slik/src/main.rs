use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use clap::Parser;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineBin, PipelineRunner, gst, make};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod bins;
mod infer;
mod nanodet;

use crate::bins::source::Source;
use crate::infer::{Detector, Runtime};

const DEFAULT_LOG_FILTER: &str = "warn,slik=info,ort=error";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Pace {
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
    fn resolve(self, source: &Source) -> Pace {
        match self {
            Pace::Auto if source.is_file() => Pace::Realtime,
            Pace::Auto => Pace::Fast,
            other => other,
        }
    }
}

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

    /// Video source: empty/"test" = test pattern, a file path, or an rtsp:// URL carrying H.264 or H.265 video over RTP.
    #[arg(long, value_name = "URI")]
    source: Option<String>,

    /// Frame pacing: auto (file=realtime, live=fast), fast, realtime, or full.
    #[arg(long, value_enum, default_value_t = Pace::Auto)]
    pace: Pace,

    /// Inference backend: tract (pure Rust) or ort (ONNX Runtime).
    #[arg(long, value_enum, default_value_t = Runtime::Tract)]
    runtime: Runtime,

    /// Intra-op thread count for the ort backend (ignored by tract). Default: ort's own.
    #[arg(long, value_name = "N")]
    threads: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
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
    info!(model = %cli.model.display(), runtime = ?cli.runtime, threads = ?cli.threads, "loading NanoDet model");
    let model = infer::load(cli.runtime, &cli.model, cli.threads)?;
    info!("model loaded");

    info!("initializing GStreamer");
    gstsmith_app::init().context("initializing GStreamer")?;
    info!("registering plugins");
    register_plugins()?;

    let source = Source::parse(cli.source.as_deref());
    let watchdog_label = if source.is_rtsp() {
        "rtsp source did not produce decodable H.264 or H.265 video over RTP"
    } else {
        "source did not connect"
    };
    let pace = cli.pace.resolve(&source);
    info!(?pace, "frame pacing");
    info!("building pipeline");
    let (pipeline, watch) = build_pipeline(&source, pace)?;
    info!("installing frame probe");
    install_frame_probe(&pipeline, "infer", Arc::clone(&model))?;
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

fn register_plugins() -> Result<()> {
    gstconsole::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith console plugin")?;
    gstlines::plugin_register_static()
        .map(|_| ())
        .context("registering the gstsmith lines plugin")?;
    Ok(())
}

fn build_pipeline(source: &Source, pace: Pace) -> Result<(gst::Pipeline, Option<gst::Pad>)> {
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
fn install_frame_probe(
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
    use tracing::Level;
    use tracing_subscriber::prelude::*;

    use super::*;

    struct FailingDetector;

    impl Detector for FailingDetector {
        fn infer(&self, _input: &[f32]) -> Result<Vec<f32>> {
            anyhow::bail!("fake detector failure")
        }
    }

    #[test]
    fn default_log_filter_focuses_on_application_events() {
        let subscriber = tracing_subscriber::registry().with(EnvFilter::new(DEFAULT_LOG_FILTER));

        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: "slik", Level::INFO));
            assert!(!tracing::enabled!(target: "ort::logging", Level::INFO));
            assert!(!tracing::enabled!(target: "ort::logging", Level::WARN));
        });
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
        let (pipeline, watch) =
            build_pipeline(&Source::parse(None), Pace::Fast).expect("pipeline builds");
        assert!(pipeline.by_name("source").is_some());
        assert!(pipeline.by_name("infer").is_some());
        assert!(watch.is_none(), "static source needs no watchdog");
    }

    #[test]
    fn realtime_inserts_pace_element() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (pipeline, _) = build_pipeline(&Source::parse(Some("/x.mp4")), Pace::Realtime)
            .expect("pipeline builds");
        assert!(
            pipeline.by_name("pace").is_some(),
            "realtime inserts a pace identity"
        );
        let (pipeline, _) =
            build_pipeline(&Source::parse(Some("/x.mp4")), Pace::Fast).expect("pipeline builds");
        assert!(
            pipeline.by_name("pace").is_none(),
            "fast has no pace element"
        );
    }

    #[tokio::test]
    async fn inference_failure_reaches_pipeline_bus() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (pipeline, watch) =
            build_pipeline(&Source::parse(None), Pace::Fast).expect("pipeline builds");
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
