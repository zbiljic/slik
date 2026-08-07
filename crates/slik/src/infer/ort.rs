use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
// Leading `::` selects the extern `ort` crate rather than this `ort` submodule.
use ::ort::session::Session;
use ::ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ::ort::value::Tensor;

use super::Detector;
use crate::nanodet::INPUT;

/// An ONNX Runtime model. `Session::run` requires `&mut self`, so the session
/// lives behind a `Mutex`, hidden from the `&self` [`Detector::infer`] interface.
pub(crate) struct OrtDetector {
    session: Mutex<Session>,
}

impl OrtDetector {
    pub(crate) fn load(path: &Path, threads: Option<usize>) -> Result<Self> {
        let session = builder(threads)?
            .commit_from_file(path)
            .with_context(|| format!("loading ONNX model from {}", path.display()))?;
        Ok(Self {
            session: Mutex::new(session),
        })
    }

    /// Load with the `CoreML` execution provider (Apple GPU/ANE) preferred,
    /// falling back to CPU for any op `CoreML` cannot run.
    #[cfg(feature = "coreml")]
    pub(crate) fn load_coreml(path: &Path, threads: Option<usize>) -> Result<Self> {
        use ::ort::ep::CoreML;

        let session = builder(threads)?
            .with_execution_providers([CoreML::default().build()])
            .map_err(|e| anyhow!("registering CoreML execution provider: {e}"))?
            .commit_from_file(path)
            .with_context(|| format!("loading ONNX model from {}", path.display()))?;
        Ok(Self {
            session: Mutex::new(session),
        })
    }
}

/// Session builder with Level3 graph optimization and, when set, `threads`
/// intra-op threads.
fn builder(threads: Option<usize>) -> Result<SessionBuilder> {
    // The builder-configuration methods return `ort::Error<SessionBuilder>`
    // (carrying the builder back for recovery), which is not `Send + Sync`, so
    // `anyhow::Context` does not apply — map their `Display` instead.
    let mut builder = Session::builder()
        .context("creating ort session builder")?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow!("setting ort optimization level: {e}"))?;
    if let Some(n) = threads {
        builder = builder
            .with_intra_threads(n)
            .map_err(|e| anyhow!("setting ort intra-op thread count: {e}"))?;
    }
    Ok(builder)
}

impl Detector for OrtDetector {
    fn infer(&self, input: &[f32]) -> Result<Vec<f32>> {
        let tensor = Tensor::from_array(([1_usize, 3, INPUT, INPUT], input.to_vec()))
            .context("shaping input tensor")?;
        let mut session = self
            .session
            .lock()
            .map_err(|_poison| anyhow!("ort session mutex poisoned"))?;
        let outputs = session
            .run(::ort::inputs![tensor])
            .context("running inference")?;
        let output = outputs
            .iter()
            .next()
            .ok_or_else(|| anyhow!("model produced no output"))?
            .1;
        let (_shape, data) = output
            .try_extract_tensor::<f32>()
            .context("extracting f32 output tensor")?;
        Ok(data.to_vec())
    }
}
