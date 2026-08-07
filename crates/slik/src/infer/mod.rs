//! Pluggable inference runtimes behind a common [`Detector`] trait.
//!
//! `NanoDet` preprocessing and decoding live in [`crate::nanodet`]; a backend
//! here only turns a flat NCHW f32 input into a flat output tensor. Keeping the
//! runtime behind a trait lets the same pipeline A/B-benchmark different ONNX
//! runtimes via `--runtime`.
//!
//! Note: this is a plain runtime abstraction, not a [`crate::bins::PipelineBin`]
//! — inference runs in a pad probe, not as a `GStreamer` element.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

mod ort;
mod tract;

/// A loaded, ready-to-run detection model with a fixed 1x3x320x320 f32 input.
pub trait Detector: Send + Sync {
    /// Run one forward pass on a flat NCHW f32 input, returning the flat output.
    fn infer(&self, input: &[f32]) -> Result<Vec<f32>>;
}

/// Inference backend selected on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Runtime {
    /// Pure-Rust `tract` runtime.
    Tract,
    /// ONNX Runtime via the `ort` crate.
    Ort,
}

/// Load `path` with the selected `runtime`.
pub fn load(runtime: Runtime, path: &Path) -> Result<Arc<dyn Detector>> {
    match runtime {
        Runtime::Tract => Ok(Arc::new(tract::TractDetector::load(path)?)),
        Runtime::Ort => Ok(Arc::new(ort::OrtDetector::load(path)?)),
    }
}
