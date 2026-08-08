//! Reusable pipeline fragments ("bins"), composed like elements.
//!
//! A bin builds a `gst::Bin` whose children are linked internally and whose
//! boundary pads are exposed as ghost pads. The caller adds and links the bin
//! exactly as it would an element. Kept deliberately small: plain Rust types, no
//! `GObject` registration.

use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;

pub mod source;

/// A reusable pipeline fragment.
pub trait PipelineBin {
    /// Build the bin: children added + linked, ghost pads created. The returned
    /// bin is **not** yet added to a pipeline.
    ///
    /// # Errors
    /// Returns an error if any child element cannot be created, linked, or ghosted.
    fn build(&self) -> Result<gst::Bin>;

    /// The internal pad whose linking indicates the bin connected, for bins that
    /// defer a runtime (sometimes-pad) link. `bin` is the just-built bin. Default:
    /// nothing to watch.
    ///
    /// # Errors
    /// Returns an error if the bin is missing an element this bin expects to have
    /// created in `build`.
    fn watch_pad(&self, _bin: &gst::Bin) -> Result<Option<gst::Pad>> {
        Ok(None)
    }
}

pub(crate) fn connect_dynamic(
    src: &gst::Element,
    sink_pad: gst::Pad,
    want_caps: Option<gst::Caps>,
    label: String,
) -> Result<()> {
    let emits_sometimes = src.factory().is_some_and(|f| {
        f.static_pad_templates().iter().any(|t| {
            t.direction() == gst::PadDirection::Src && t.presence() == gst::PadPresence::Sometimes
        })
    });
    if !emits_sometimes {
        anyhow::bail!(
            "element '{}' has no dynamic src pad; pad-added would never fire",
            src.name()
        );
    }

    src.connect_pad_added(move |src, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        if let Some(want) = &want_caps {
            let have = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
            if !have.can_intersect(want) {
                return;
            }
        }
        if sink_pad.is_linked() {
            return;
        }
        if let Err(err) = pad.link(&sink_pad) {
            gst::element_error!(
                src,
                gst::CoreError::Negotiation,
                ["failed to link dynamic pad for {label}: {err}"]
            );
        }
    });

    Ok(())
}

pub(crate) fn ghost_src(bin: &gst::Bin, elem: &gst::Element) -> Result<()> {
    let src_pad = elem
        .static_pad("src")
        .ok_or_else(|| anyhow!("element '{}' has no static src pad to ghost", elem.name()))?;
    let ghost = gst::GhostPad::builder_with_target(&src_pad)
        .context("creating ghost src pad")?
        .name("src")
        .build();
    bin.add_pad(&ghost)
        .context("adding ghost src pad to source bin")?;
    Ok(())
}
