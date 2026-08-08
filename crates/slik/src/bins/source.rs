use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;

use super::{PipelineBin, connect_dynamic, ghost_src};
use crate::make;

// Element names used to build and re-find watch pads — single-sourced.
const DECODED_TAIL: &str = "source-convert"; // videoconvert; deferred-link target

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RtspVideoCodec {
    H264,
    H265,
}

fn rtsp_video_codec(caps: &gst::Caps) -> Option<RtspVideoCodec> {
    caps.iter().find_map(|structure| {
        if structure.name() != "application/x-rtp"
            || structure.get::<String>("media").ok().as_deref() != Some("video")
        {
            return None;
        }

        let encoding = structure.get::<String>("encoding-name").ok()?;
        if encoding.eq_ignore_ascii_case("H264") {
            Some(RtspVideoCodec::H264)
        } else if encoding.eq_ignore_ascii_case("H265") {
            Some(RtspVideoCodec::H265)
        } else {
            None
        }
    })
}

fn supported_rtsp_video_caps() -> gst::Caps {
    gst::Caps::builder_full()
        .structure(
            gst::Structure::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "H264")
                .build(),
        )
        .structure(
            gst::Structure::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "H265")
                .build(),
        )
        .build()
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
                // The videoconvert tail provides a stable decoded-video boundary. Its
                // raw-ANY input accepts any memory feature; the downstream bare BGR caps
                // constrain inference frames to default/system memory.
                let convert = make("videoconvert", DECODED_TAIL)?;
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
                        .is_some_and(|caps| rtsp_video_codec(&caps).is_some());
                    Some(is_supported.to_value())
                });

                let dec = make("decodebin", "decode")?;
                let convert = make("videoconvert", DECODED_TAIL)?;
                bin.add_many([&src, &dec, &convert])
                    .context("adding rtspsrc ! decodebin ! videoconvert")?;

                let decode_sink = dec
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("decodebin has no sink pad"))?;
                connect_dynamic(
                    &src,
                    decode_sink,
                    Some(supported_rtsp_video_caps()),
                    "rtspsrc -> decodebin".to_owned(),
                )?;

                let convert_sink = convert
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("videoconvert has no sink pad"))?;
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
        }
    }

    fn watch_pad(&self, bin: &gst::Bin) -> Result<Option<gst::Pad>> {
        let name = match self {
            Source::Test => return Ok(None),
            Source::File(_) | Source::Rtsp(_) => DECODED_TAIL,
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
    fn recognizes_supported_video_rtp_codecs() {
        assert_eq!(
            rtsp_video_codec(&rtp_caps("video", Some("h264"))),
            Some(RtspVideoCodec::H264)
        );
        assert_eq!(
            rtsp_video_codec(&rtp_caps("video", Some("H265"))),
            Some(RtspVideoCodec::H265)
        );
    }

    #[test]
    fn rejects_unsupported_rtp_caps() {
        assert_eq!(rtsp_video_codec(&rtp_caps("video", Some("VP9"))), None);
        assert_eq!(rtsp_video_codec(&rtp_caps("audio", Some("H264"))), None);
        assert_eq!(rtsp_video_codec(&rtp_caps("video", None)), None);
    }

    #[test]
    fn supported_caps_intersect_only_supported_video_rtp() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let supported = supported_rtsp_video_caps();
        assert!(supported.can_intersect(&rtp_caps("video", Some("H264"))));
        assert!(supported.can_intersect(&rtp_caps("video", Some("H265"))));
        assert!(!supported.can_intersect(&rtp_caps("video", Some("VP9"))));
        assert!(!supported.can_intersect(&rtp_caps("audio", Some("H264"))));
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
        assert!(bin.by_name("source").is_some(), "bin contains rtspsrc");
        assert!(bin.by_name("decode").is_some(), "bin contains decodebin");
        let tail = bin
            .by_name(DECODED_TAIL)
            .expect("bin contains decoded videoconvert tail");
        assert!(
            bin.by_name("depay").is_none(),
            "bin has no fixed depayloader"
        );
        assert!(bin.by_name("parse").is_none(), "bin has no fixed parser");
        let watch = source
            .watch_pad(&bin)
            .expect("watch_pad ok")
            .expect("rtsp has a watch pad");
        assert_eq!(
            watch,
            tail.static_pad("sink").expect("tail has a sink pad"),
            "watch pad is the decoded tail sink"
        );
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
