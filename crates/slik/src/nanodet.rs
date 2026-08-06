use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use gstreamer_video as gst_video;
use gstreamer_video::prelude::VideoFrameExt;
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;
use tract_onnx::prelude::*;

/// A ready-to-run tract model with a fixed 1x3x320x320 f32 input.
pub type Model = TypedRunnableModel;

pub const INPUT: usize = 320;

const NUM_CLASSES: usize = 80;
const REG_MAX: usize = 7; // 8 bins per side
const BINS: usize = REG_MAX + 1;
const STRIDES: [usize; 4] = [8, 16, 32, 64];
const SCORE_THRESH: f32 = 0.3;
const NMS_IOU: f32 = 0.6;

static COCO_NAMES: &str = include_str!("coco.names");

pub fn load_model(path: &Path) -> Result<Arc<Model>> {
    let model = tract_onnx::onnx()
        .model_for_path(path)
        .with_context(|| format!("loading ONNX model from {}", path.display()))?
        .with_input_fact(0, f32::fact([1, 3, INPUT, INPUT]).into())
        .context("setting model input shape to 1x3x320x320")?
        .into_optimized()
        .context("optimizing the model graph")?
        .into_runnable()
        .context("making the model runnable")?;
    Ok(model)
}

/// Runs detection on the frame carried by `info` and prints each detection.
/// Returns the number of detections remaining after NMS.
pub fn run_inference(
    pad: &gst::Pad,
    info: &gst::PadProbeInfo,
    model: &Arc<Model>,
) -> Result<usize> {
    let Some(buffer) = info.buffer() else {
        return Ok(0);
    };
    let caps = pad
        .current_caps()
        .ok_or_else(|| anyhow!("no caps negotiated on probe pad"))?;
    let vinfo = gst_video::VideoInfo::from_caps(&caps).context("parsing VideoInfo from caps")?;

    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &vinfo)
        .context("mapping video frame readable")?;

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    if width != INPUT || height != INPUT {
        return Err(anyhow!(
            "expected {INPUT}x{INPUT} frame, got {width}x{height}"
        ));
    }
    let stride_i32 = frame
        .plane_stride()
        .first()
        .copied()
        .ok_or_else(|| anyhow!("frame has no plane 0 stride"))?;
    #[expect(
        clippy::cast_sign_loss,
        reason = "a video plane stride is always non-negative"
    )]
    let stride = stride_i32 as usize;
    let data = frame.plane_data(0).context("reading plane 0 data")?;

    // NCHW f32 input with NanoDet BGR mean/std. Pipeline delivers RGB; NanoDet
    // channel order is BGR, so tensor channel 0=B,1=G,2=R.
    // mean(BGR) = [103.53, 116.28, 123.675], std(BGR) = [57.375, 57.12, 58.395].
    let mean = [103.53_f32, 116.28, 123.675];
    let std = [57.375_f32, 57.12, 58.395];
    let mut input = vec![0.0_f32; 3 * INPUT * INPUT];
    let (plane_r, plane_g, plane_b) = (0usize, INPUT * INPUT, 2 * INPUT * INPUT);
    #[expect(
        clippy::indexing_slicing,
        reason = "fixed-size tensor buffer, indices bounded by INPUT"
    )]
    for y in 0..INPUT {
        let row = data
            .get(y * stride..y * stride + INPUT * 3)
            .ok_or_else(|| anyhow!("row {y} out of bounds"))?;
        for x in 0..INPUT {
            let px = row
                .get(x * 3..x * 3 + 3)
                .ok_or_else(|| anyhow!("pixel oob"))?;
            let (r, g, b) = (f32::from(px[0]), f32::from(px[1]), f32::from(px[2]));
            let idx = y * INPUT + x;
            input[plane_b + idx] = (b - mean[0]) / std[0];
            input[plane_g + idx] = (g - mean[1]) / std[1];
            input[plane_r + idx] = (r - mean[2]) / std[2];
        }
    }

    let tensor: Tensor = tract_ndarray::Array4::from_shape_vec((1, 3, INPUT, INPUT), input)
        .context("shaping input tensor")?
        .into();

    let outputs = model
        .run(tvec!(tensor.into()))
        .context("running inference")?;
    let out = outputs
        .first()
        .ok_or_else(|| anyhow!("model produced no output"))?;

    let detections = decode_nanodet(out)?;
    for d in &detections {
        println!(
            "{} {:.2} @ [{:.0},{:.0},{:.0},{:.0}]",
            coco_label(d.class),
            d.score,
            d.x1,
            d.y1,
            d.x2,
            d.y2
        );
    }
    Ok(detections.len())
}

#[derive(Clone, Copy, Debug)]
struct Det {
    class: usize,
    score: f32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

/// Softmax over `v`, then expectation `E = sum(i * softmax_i)`.
fn integral(v: &[f32]) -> f32 {
    let max = v.iter().copied().fold(f32::MIN, f32::max);
    let exps: Vec<f32> = v.iter().map(|&x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    #[expect(
        clippy::cast_precision_loss,
        reason = "bin index is at most REG_MAX (7), exactly representable in f32"
    )]
    exps.iter()
        .enumerate()
        .map(|(i, &e)| i as f32 * (e / sum))
        .sum()
}

fn decode_nanodet(out: &Tensor) -> Result<Vec<Det>> {
    let arr = out
        .to_plain_array_view::<f32>()
        .context("output tensor is not f32")?;
    let shape = arr.shape();
    let points = *shape
        .get(1)
        .ok_or_else(|| anyhow!("output missing dim 1"))?;
    let channels = *shape
        .get(2)
        .ok_or_else(|| anyhow!("output missing dim 2"))?;
    let arr = arr
        .into_shape_with_order((points, channels))
        .context("reshape output")?;

    let mut dets: Vec<Det> = Vec::new();
    let mut point = 0usize;
    for &stride in &STRIDES {
        let grid = INPUT / stride; // 40, 20, 10, 5
        for gy in 0..grid {
            for gx in 0..grid {
                let row = arr.row(point);
                point += 1;

                let mut best_c = 0usize;
                let mut best_s = 0.0_f32;
                for c in 0..NUM_CLASSES {
                    // NanoDet-Plus' ONNX head already outputs class probabilities
                    // (post-sigmoid), so read the score raw.
                    let s = *row
                        .get(c)
                        .ok_or_else(|| anyhow!("class {c} out of bounds"))?;
                    if s > best_s {
                        best_s = s;
                        best_c = c;
                    }
                }
                if best_s < SCORE_THRESH {
                    continue;
                }

                let base = NUM_CLASSES;
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "stride is one of 8/16/32/64, exactly representable in f32"
                )]
                let dist = |side: usize| -> Result<f32> {
                    let start = base + side * BINS;
                    let mut bins = [0.0_f32; BINS];
                    for (k, slot) in bins.iter_mut().enumerate() {
                        *slot = *row
                            .get(start + k)
                            .ok_or_else(|| anyhow!("reg bin {k} out of bounds"))?;
                    }
                    Ok(integral(&bins) * stride as f32)
                };
                let (l, t, r, b) = (dist(0)?, dist(1)?, dist(2)?, dist(3)?);

                #[expect(
                    clippy::cast_precision_loss,
                    reason = "grid coordinates are small (< 40), exactly representable in f32"
                )]
                let (cx, cy) = (gx as f32 * stride as f32, gy as f32 * stride as f32);
                dets.push(Det {
                    class: best_c,
                    score: best_s,
                    x1: cx - l,
                    y1: cy - t,
                    x2: cx + r,
                    y2: cy + b,
                });
            }
        }
    }
    Ok(nms(dets))
}

fn iou(a: &Det, b: &Det) -> f32 {
    let xx1 = a.x1.max(b.x1);
    let yy1 = a.y1.max(b.y1);
    let xx2 = a.x2.min(b.x2);
    let yy2 = a.y2.min(b.y2);
    let w = (xx2 - xx1).max(0.0);
    let h = (yy2 - yy1).max(0.0);
    let inter = w * h;
    let area_a = (a.x2 - a.x1).max(0.0) * (a.y2 - a.y1).max(0.0);
    let area_b = (b.x2 - b.x1).max(0.0) * (b.y2 - b.y1).max(0.0);
    let union = area_a + area_b - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Per-class greedy NMS.
fn nms(mut dets: Vec<Det>) -> Vec<Det> {
    dets.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut keep: Vec<Det> = Vec::new();
    for d in dets {
        if keep
            .iter()
            .any(|k| k.class == d.class && iou(k, &d) > NMS_IOU)
        {
            continue;
        }
        keep.push(d);
    }
    keep
}

fn coco_label(i: usize) -> &'static str {
    COCO_NAMES.lines().nth(i).unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integral_of_delta_is_the_index() {
        let mut bins = [0.0_f32; BINS];
        bins[3] = 20.0;
        assert!((integral(&bins) - 3.0).abs() < 0.01);
    }

    #[test]
    fn iou_of_identical_boxes_is_one() {
        let d = Det {
            class: 0,
            score: 1.0,
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        assert!((iou(&d, &d) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn nms_suppresses_overlapping_same_class() {
        let a = Det {
            class: 0,
            score: 0.9,
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
        };
        let b = Det {
            class: 0,
            score: 0.5,
            x1: 1.0,
            y1: 1.0,
            x2: 11.0,
            y2: 11.0,
        };
        let kept = nms(vec![a, b]);
        assert_eq!(kept.len(), 1);
        assert!((kept[0].score - 0.9).abs() < 1e-6);
    }
}
