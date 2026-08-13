use anyhow::{Context as _, Result};
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{PipelineBin, ghost_sink, gst, make};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Output {
    /// Discard decoded frames without displaying them.
    Discard,
    /// Display the model-sized frame with detection metadata overlaid.
    Preview,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Sink {
    output: Output,
}

impl Sink {
    pub(crate) fn new(output: Output) -> Self {
        Self { output }
    }
}

impl PipelineBin for Sink {
    fn build(&self) -> Result<gst::Bin> {
        let bin = gst::Bin::with_name("output");

        match self.output {
            Output::Discard => {
                let sink = make("fakesink", "sink")?;
                sink.set_property("sync", false);
                bin.add(&sink).context("adding discard output sink")?;
                ghost_sink(&bin, &sink)?;
            }
            Output::Preview => {
                let queue = make("queue", "preview-queue")?;
                queue.set_property_from_str("leaky", "downstream");
                queue.set_property("max-size-buffers", 1u32);
                queue.set_property("max-size-time", 0u64);
                queue.set_property("max-size-bytes", 0u32);

                let overlay = make("objectdetectionoverlay", "overlay")?;
                let convert = make("videoconvert", "preview-convert")?;
                let sink = make("autovideosink", "sink")?;
                sink.set_property("sync", false);

                bin.add_many([&queue, &overlay, &convert, &sink])
                    .context("adding preview output elements")?;
                gst::Element::link_many([&queue, &overlay, &convert, &sink])
                    .context("linking preview output")?;
                ghost_sink(&bin, &queue)?;
            }
        }

        Ok(bin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_uses_fake_sink_without_overlay() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let bin = Sink::new(Output::Discard)
            .build()
            .expect("discard sink builds");
        let sink = bin.by_name("sink").expect("output sink exists");
        assert_eq!(sink.type_().name(), "GstFakeSink");
        assert!(!sink.property::<bool>("sync"));
        assert!(bin.by_name("overlay").is_none());
        assert!(bin.static_pad("sink").is_some());
    }

    #[test]
    fn preview_uses_bounded_output_chain() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let bin = Sink::new(Output::Preview)
            .build()
            .expect("preview sink builds");
        for (name, factory) in [
            ("preview-queue", "GstQueue"),
            ("overlay", "GstObjectDetectionOverlay"),
            ("preview-convert", "GstVideoConvert"),
            ("sink", "GstAutoVideoSink"),
        ] {
            assert_eq!(
                bin.by_name(name)
                    .expect("preview element exists")
                    .type_()
                    .name(),
                factory
            );
        }
        let queue = bin.by_name("preview-queue").expect("preview queue exists");
        let leaky_value = queue.property_value("leaky");
        let (_, leaky) = gst::glib::EnumValue::from_value(&leaky_value)
            .expect("preview queue leaky has an enum value");
        assert_eq!(leaky.nick(), "downstream");
        assert_eq!(queue.property::<u32>("max-size-buffers"), 1);
        assert_eq!(queue.property::<u64>("max-size-time"), 0);
        assert_eq!(queue.property::<u32>("max-size-bytes"), 0);
        assert!(
            !bin.by_name("sink")
                .expect("preview sink exists")
                .property::<bool>("sync")
        );
        assert!(bin.static_pad("sink").is_some());
    }
}
