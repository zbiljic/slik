use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use tract_onnx::prelude::*;

use super::Detector;
use crate::nanodet::INPUT;

/// A ready-to-run tract model with a fixed 1x3x320x320 f32 input.
pub(crate) struct TractDetector {
    model: Arc<TypedRunnableModel>,
}

impl TractDetector {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let model = typed_model(path)?
            .into_optimized()
            .context("optimizing the model graph")?
            .into_runnable()
            .context("making the model runnable")?;
        Ok(Self { model })
    }

    /// Load with tract's Metal GPU backend (Apple). Metal kernels dispatch through
    /// a thread-local stream initialized lazily on first use.
    #[cfg(feature = "metal")]
    pub(crate) fn load_metal(path: &Path) -> Result<Self> {
        use tract_metal::MetalTransform;
        use tract_onnx::tract_core::transform::ModelTransform;

        let mut model = typed_model(path)?;
        MetalTransform::default()
            .transform(&mut model)
            .context("applying the Metal transform")?;
        let model = model
            .into_optimized()
            .context("optimizing the Metal model graph")?
            .into_runnable()
            .context("making the Metal model runnable")?;
        Ok(Self { model })
    }
}

/// Parse `path` into an unoptimized typed model with a fixed 1x3x320x320 input.
fn typed_model(path: &Path) -> Result<TypedModel> {
    tract_onnx::onnx()
        .model_for_path(path)
        .with_context(|| format!("loading ONNX model from {}", path.display()))?
        .with_input_fact(0, f32::fact([1, 3, INPUT, INPUT]).into())
        .context("setting model input shape to 1x3x320x320")?
        .into_typed()
        .context("converting to a typed model graph")
}

impl Detector for TractDetector {
    fn infer(&self, input: &[f32]) -> Result<Vec<f32>> {
        let tensor: Tensor =
            tract_ndarray::Array4::from_shape_vec((1, 3, INPUT, INPUT), input.to_vec())
                .context("shaping input tensor")?
                .into();
        let outputs = self
            .model
            .run(tvec!(tensor.into()))
            .context("running inference")?;
        let out = outputs
            .first()
            .ok_or_else(|| anyhow!("model produced no output"))?;
        let view = out
            .to_plain_array_view::<f32>()
            .context("output tensor is not f32")?;
        Ok(view.iter().copied().collect())
    }
}
