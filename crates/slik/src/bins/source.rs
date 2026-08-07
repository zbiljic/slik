use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;

use super::{connect_dynamic, ghost_src};
use crate::make;

#[derive(Debug, Clone)]
pub(crate) enum Source {
    Test,
    File(PathBuf),
    Rtsp(String),
}

impl Source {
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            None | Some("" | "test") => Source::Test,
            Some(uri) if uri.starts_with("rtsp://") => Source::Rtsp(uri.to_owned()),
            Some(path) => Source::File(PathBuf::from(path)),
        }
    }

    pub(crate) fn build_watched(&self) -> Result<(gst::Bin, Option<gst::Pad>)> {
        let bin = gst::Bin::with_name("source-bin");
        match self {
            Source::Test => {
                let src = make("videotestsrc", "source")?;
                src.set_property("is-live", true);
                bin.add(&src).context("adding videotestsrc")?;
                ghost_src(&bin, &src)?;
                Ok((bin, None))
            }
            Source::File(path) => {
                let src = make("filesrc", "source")?;
                let location = path
                    .to_str()
                    .ok_or_else(|| anyhow!("file path is not valid UTF-8: {}", path.display()))?;
                src.set_property("location", location);
                let dec = make("decodebin", "decode")?;
                // videoconvert tail: it accepts only system-memory video/x-raw, which
                // forces a hardware decoder (e.g. macOS vtdechw, which otherwise emits
                // video/x-raw(memory:GLMemory)) to negotiate CPU-accessible buffers the
                // downstream videocrop/videoscale chain can consume.
                let convert = make("videoconvert", "source-convert")?;
                bin.add_many([&src, &dec, &convert])
                    .context("adding filesrc ! decodebin ! videoconvert")?;
                gst::Element::link_many([&src, &dec]).context("linking filesrc ! decodebin")?;
                let convert_sink = convert
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("videoconvert has no sink pad"))?;
                // Match a decoded video pad regardless of memory feature (system vs GLMemory).
                let want = gst::Caps::builder("video/x-raw").any_features().build();
                connect_dynamic(
                    &dec,
                    convert_sink.clone(),
                    Some(want),
                    "decodebin -> convert".to_owned(),
                )?;
                ghost_src(&bin, &convert)?;
                Ok((bin, Some(convert_sink)))
            }
            Source::Rtsp(url) => {
                let src = make("rtspsrc", "source")?;
                src.set_property("location", url.as_str());
                src.set_property_from_str("protocols", "tcp");
                src.connect("select-stream", false, |args| {
                    let is_video = args
                        .get(2)
                        .and_then(|v| v.get::<gst::Caps>().ok())
                        .and_then(|caps| {
                            caps.structure(0)
                                .and_then(|s| s.get::<String>("media").ok())
                        })
                        .is_some_and(|media| media == "video");
                    Some(is_video.to_value())
                });

                let depay = make("rtph264depay", "depay")?;
                let parse = make("h264parse", "parse")?;
                let dec = make("avdec_h264", "decode")?;
                bin.add_many([&src, &depay, &parse, &dec])
                    .context("adding rtsp source chain")?;
                gst::Element::link_many([&depay, &parse, &dec])
                    .context("linking rtph264depay ! h264parse ! avdec_h264")?;

                let depay_sink = depay
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("rtph264depay has no sink pad"))?;
                let want = gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .build();
                connect_dynamic(
                    &src,
                    depay_sink.clone(),
                    Some(want),
                    "rtspsrc -> depay".to_owned(),
                )?;
                ghost_src(&bin, &dec)?;
                Ok((bin, Some(depay_sink)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_uris() {
        assert!(matches!(Source::parse(None), Source::Test));
        assert!(matches!(Source::parse(Some("")), Source::Test));
        assert!(matches!(Source::parse(Some("test")), Source::Test));
        assert!(matches!(
            Source::parse(Some("rtsp://cam/stream")),
            Source::Rtsp(_)
        ));
        assert!(matches!(
            Source::parse(Some("/tmp/clip.mp4")),
            Source::File(_)
        ));
    }

    #[test]
    fn rtsp_source_bin_has_ghost_src_and_unlinked_watch_pad() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let (bin, watch) = Source::Rtsp("rtsp://example/stream".to_owned())
            .build_watched()
            .expect("source bin builds");
        assert!(
            bin.static_pad("src").is_some(),
            "bin exposes a ghost src pad"
        );
        let watch = watch.expect("dynamic source has a watch pad");
        assert!(!watch.is_linked(), "watch pad starts unlinked");
    }
}
