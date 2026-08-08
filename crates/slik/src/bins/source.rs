use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;

use super::{PipelineBin, connect_dynamic, ghost_src};
use crate::make;

// Element names used to build and re-find watch pads — single-sourced.
const FILE_TAIL: &str = "source-convert"; // videoconvert; deferred-link target for File
const RTSP_DEPAY: &str = "depay"; // rtph264depay; deferred-link target for Rtsp

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
}

fn is_h264_video_rtp(caps: &gst::Caps) -> bool {
    caps.iter().any(|structure| {
        structure.name() == "application/x-rtp"
            && structure
                .get::<String>("media")
                .is_ok_and(|media| media == "video")
            && structure
                .get::<String>("encoding-name")
                .is_ok_and(|encoding| encoding.eq_ignore_ascii_case("H264"))
    })
}

impl PipelineBin for Source {
    fn build(&self) -> Result<gst::Bin> {
        let bin = gst::Bin::with_name("source-bin");
        match self {
            Source::Test => {
                let src = make("videotestsrc", "source")?;
                src.set_property("is-live", true);
                bin.add(&src).context("adding videotestsrc")?;
                ghost_src(&bin, &src)?;
                Ok(bin)
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
                let convert = make("videoconvert", FILE_TAIL)?;
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
                    convert_sink,
                    Some(want),
                    "decodebin -> convert".to_owned(),
                )?;
                ghost_src(&bin, &convert)?;
                Ok(bin)
            }
            Source::Rtsp(url) => {
                let src = make("rtspsrc", "source")?;
                src.set_property("location", url.as_str());
                src.set_property_from_str("protocols", "tcp");
                src.connect("select-stream", false, |args| {
                    let is_supported = args
                        .get(2)
                        .and_then(|v| v.get::<gst::Caps>().ok())
                        .is_some_and(|caps| is_h264_video_rtp(&caps));
                    Some(is_supported.to_value())
                });

                let depay = make("rtph264depay", RTSP_DEPAY)?;
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
                    .field("encoding-name", "H264")
                    .build();
                connect_dynamic(&src, depay_sink, Some(want), "rtspsrc -> depay".to_owned())?;
                ghost_src(&bin, &dec)?;
                Ok(bin)
            }
        }
    }

    fn watch_pad(&self, bin: &gst::Bin) -> Result<Option<gst::Pad>> {
        let name = match self {
            Source::Test => return Ok(None),
            Source::File(_) => FILE_TAIL,
            Source::Rtsp(_) => RTSP_DEPAY,
        };
        let elem = bin
            .by_name(name)
            .ok_or_else(|| anyhow!("source bin missing element '{name}' to watch"))?;
        let pad = elem
            .static_pad("sink")
            .ok_or_else(|| anyhow!("element '{name}' has no sink pad to watch"))?;
        Ok(Some(pad))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp_caps(media: &str, encoding: Option<&str>) -> gst::Caps {
        gstsmith_app::init().expect("GStreamer should initialize");
        let builder = gst::Caps::builder("application/x-rtp").field("media", media);
        match encoding {
            Some(encoding) => builder.field("encoding-name", encoding).build(),
            None => builder.build(),
        }
    }

    #[test]
    fn accepts_h264_video_rtp_caps() {
        assert!(is_h264_video_rtp(&rtp_caps("video", Some("h264"))));
    }

    #[test]
    fn rejects_h265_video_rtp_caps() {
        assert!(!is_h264_video_rtp(&rtp_caps("video", Some("H265"))));
    }

    #[test]
    fn rejects_audio_rtp_caps() {
        assert!(!is_h264_video_rtp(&rtp_caps("audio", Some("H264"))));
    }

    #[test]
    fn rejects_rtp_caps_without_encoding_name() {
        assert!(!is_h264_video_rtp(&rtp_caps("video", None)));
    }

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
    fn rtsp_bin_builds_with_ghost_src_and_watch_pad() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let source = Source::Rtsp("rtsp://example/stream".to_owned());
        let bin = source.build().expect("source bin builds");
        assert!(
            bin.static_pad("src").is_some(),
            "bin exposes a ghost src pad"
        );
        let watch = source
            .watch_pad(&bin)
            .expect("watch_pad ok")
            .expect("rtsp has a watch pad");
        assert!(!watch.is_linked(), "watch pad starts unlinked");
    }

    #[test]
    fn test_source_has_no_watch_pad() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let source = Source::Test;
        let bin = source.build().expect("source bin builds");
        assert!(
            source.watch_pad(&bin).expect("watch_pad ok").is_none(),
            "test source: nothing to watch"
        );
    }
}
