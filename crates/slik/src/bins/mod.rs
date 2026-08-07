use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;

pub mod source;

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

    src.connect_pad_added(move |_src, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        if let Some(want) = &want_caps {
            let Some(have) = pad.current_caps() else {
                return;
            };
            if !have.can_intersect(want) {
                return;
            }
        }
        if sink_pad.is_linked() {
            return;
        }
        if let Err(err) = pad.link(&sink_pad) {
            eprintln!("failed to link dynamic pad for {label}: {err}");
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
