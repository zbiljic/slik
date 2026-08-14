use std::{fmt, path::PathBuf};

use anyhow::{Context as _, Result, anyhow};
use gstsmith_app::gst;
use gstsmith_app::gst::prelude::*;
use gstsmith_app::{BinBase, PipelineBin, connect_dynamic, ghost_src};

const DECODED_TAIL: &str = "convert";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum RtspTransport {
    /// Force RTP/RTCP over the RTSP TCP connection.
    Tcp,
    /// Use unicast UDP for RTP/RTCP.
    Udp,
    /// Allow TCP, UDP multicast, and UDP, matching `GStreamer`'s default.
    Auto,
}

impl RtspTransport {
    fn protocols(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Auto => "tcp+udp-mcast+udp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RtspOptions {
    pub(crate) transport: RtspTransport,
    pub(crate) latency_ms: u32,
    pub(crate) drop_on_latency: bool,
}

impl Default for RtspOptions {
    fn default() -> Self {
        Self {
            transport: RtspTransport::Tcp,
            latency_ms: 2000,
            drop_on_latency: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum SourceKind {
    Test,
    File(PathBuf),
    Rtsp(String),
}

impl fmt::Debug for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Test => f.write_str("Test"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Rtsp(_) => f.write_str("Rtsp(<redacted>)"),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Source {
    base: BinBase,
    kind: SourceKind,
    rtsp: RtspOptions,
}

impl Source {
    #[cfg(test)]
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        Self::parse_with_options(raw, RtspOptions::default())
    }

    pub(crate) fn parse_with_options(raw: Option<&str>, rtsp: RtspOptions) -> Self {
        let kind = match raw.map(str::trim) {
            None | Some("" | "test") => SourceKind::Test,
            Some(uri) if uri.starts_with("rtsp://") => SourceKind::Rtsp(uri.to_owned()),
            Some(path) => SourceKind::File(PathBuf::from(path)),
        };
        Self {
            base: BinBase::new("source"),
            kind,
            rtsp,
        }
    }

    pub(crate) fn is_file(&self) -> bool {
        matches!(self.kind, SourceKind::File(_))
    }

    pub(crate) fn is_rtsp(&self) -> bool {
        matches!(self.kind, SourceKind::Rtsp(_))
    }

    pub(crate) fn watch_pad(&self, bin: &gst::Bin) -> Result<Option<gst::Pad>> {
        if self.kind == SourceKind::Test {
            return Ok(None);
        }
        let name = self.base.child(DECODED_TAIL);
        let elem = bin
            .by_name(&name)
            .ok_or_else(|| anyhow!("source bin missing element '{name}' to watch"))?;
        let pad = elem
            .static_pad("sink")
            .ok_or_else(|| anyhow!("element '{name}' has no sink pad to watch"))?;
        Ok(Some(pad))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RtspVideoCodec {
    H264,
    H265,
}

fn rtsp_video_codec(caps: &gst::Caps) -> Option<RtspVideoCodec> {
    caps.iter().find_map(|structure| {
        // rtspsrc normally reports application/x-rtp here, but GStreamer 1.28
        // can expose SDP-derived select-stream caps as application/x-unknown.
        // The eventual source pad is still restricted to supported RTP caps.
        if !matches!(
            structure.name().as_str(),
            "application/x-rtp" | "application/x-unknown"
        ) || structure.get::<String>("media").ok().as_deref() != Some("video")
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

impl Source {
    /// Build the source, controlling whether the synthetic test source follows
    /// the clock. The setting is ignored for file and RTSP sources.
    pub(crate) fn build_with_test_live(&self, test_live: bool) -> Result<gst::Bin> {
        let bin = self.base.bin();
        match &self.kind {
            SourceKind::Test => {
                let src = self.base.make("videotestsrc", "input")?;
                src.set_property("is-live", test_live);
                bin.add(&src).context("adding videotestsrc")?;
                ghost_src(&bin, &src)?;
                Ok(bin)
            }
            SourceKind::File(path) => {
                let src = self.base.make("filesrc", "input")?;
                let location = path
                    .to_str()
                    .ok_or_else(|| anyhow!("file path is not valid UTF-8: {}", path.display()))?;
                src.set_property("location", location);
                let dec = self.base.make("decodebin", "decode")?;
                // The videoconvert tail provides a stable decoded-video boundary. Its
                // raw-ANY input accepts any memory feature; the downstream bare BGR caps
                // constrain inference frames to default/system memory.
                let convert = self.base.make("videoconvert", DECODED_TAIL)?;
                bin.add_many([&src, &dec, &convert])
                    .context("adding filesrc ! decodebin ! videoconvert")?;
                gst::Element::link_many([&src, &dec]).context("linking filesrc ! decodebin")?;
                let convert_sink = convert
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("videoconvert has no sink pad"))?;
                // Match a decoded video pad regardless of memory feature (system vs GLMemory).
                let want = gst::Caps::builder("video/x-raw").any_features().build();
                connect_dynamic(&dec, convert_sink, Some(want), "decodebin -> convert")?;
                ghost_src(&bin, &convert)?;
                Ok(bin)
            }
            SourceKind::Rtsp(url) => {
                let src = self.base.make("rtspsrc", "input")?;
                src.set_property("location", url.as_str());
                src.set_property_from_str("protocols", self.rtsp.transport.protocols());
                src.set_property("latency", self.rtsp.latency_ms);
                src.set_property("drop-on-latency", self.rtsp.drop_on_latency);
                src.connect("select-stream", false, |args| {
                    let is_supported = args
                        .get(2)
                        .and_then(|v| v.get::<gst::Caps>().ok())
                        .is_some_and(|caps| rtsp_video_codec(&caps).is_some());
                    Some(is_supported.to_value())
                });

                let dec = self.base.make("decodebin", "decode")?;
                let convert = self.base.make("videoconvert", DECODED_TAIL)?;
                bin.add_many([&src, &dec, &convert])
                    .context("adding rtspsrc ! decodebin ! videoconvert")?;

                let decode_sink = dec
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("decodebin has no sink pad"))?;
                connect_dynamic(
                    &src,
                    decode_sink,
                    Some(supported_rtsp_video_caps()),
                    "rtspsrc -> decodebin",
                )?;

                let convert_sink = convert
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("videoconvert has no sink pad"))?;
                let want = gst::Caps::builder("video/x-raw").any_features().build();
                connect_dynamic(&dec, convert_sink, Some(want), "decodebin -> convert")?;
                ghost_src(&bin, &convert)?;
                Ok(bin)
            }
        }
    }
}

impl PipelineBin for Source {
    fn build(&self) -> Result<gst::Bin> {
        self.build_with_test_live(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_caps(name: &str, media: &str, encoding: Option<&str>) -> gst::Caps {
        gstsmith_app::init().expect("GStreamer should initialize");
        let builder = gst::Caps::builder(name).field("media", media);
        match encoding {
            Some(encoding) => builder.field("encoding-name", encoding).build(),
            None => builder.build(),
        }
    }

    fn rtp_caps(media: &str, encoding: Option<&str>) -> gst::Caps {
        stream_caps("application/x-rtp", media, encoding)
    }

    fn protocol_nicks(src: &gst::Element) -> Vec<String> {
        let value = src.property_value("protocols");
        let (_, flags) = gst::glib::FlagsValue::from_value(&value)
            .expect("rtspsrc protocols has a registered flags type");
        let mut nicks = flags
            .into_iter()
            .map(|flag| flag.nick().to_owned())
            .collect::<Vec<_>>();
        nicks.sort_unstable();
        nicks
    }

    fn build_rtsp(options: RtspOptions) -> gst::Bin {
        gstsmith_app::init().expect("GStreamer should initialize");
        Source::parse_with_options(Some("rtsp://camera.example/stream"), options)
            .build()
            .expect("source bin builds")
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
    fn recognizes_gstreamer_unknown_rtsp_stream_caps() {
        assert_eq!(
            rtsp_video_codec(&stream_caps("application/x-unknown", "video", Some("H264"))),
            Some(RtspVideoCodec::H264)
        );
        assert_eq!(
            rtsp_video_codec(&stream_caps("application/x-unknown", "video", Some("H265"))),
            Some(RtspVideoCodec::H265)
        );
    }

    #[test]
    fn rejects_unsupported_rtp_caps() {
        assert_eq!(rtsp_video_codec(&rtp_caps("video", Some("VP9"))), None);
        assert_eq!(rtsp_video_codec(&rtp_caps("audio", Some("H264"))), None);
        assert_eq!(rtsp_video_codec(&rtp_caps("video", None)), None);
        assert_eq!(
            rtsp_video_codec(&stream_caps("video/x-raw", "video", Some("H264"))),
            None
        );
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
        assert_eq!(Source::parse(None).kind, SourceKind::Test);
        assert_eq!(Source::parse(Some("")).kind, SourceKind::Test);
        assert_eq!(Source::parse(Some("test")).kind, SourceKind::Test);
        assert!(Source::parse(Some("rtsp://cam/stream")).is_rtsp());
        assert!(Source::parse(Some("/tmp/clip.mp4")).is_file());
    }

    #[test]
    fn debug_redacts_rtsp_uri_credentials_and_query_token() {
        let uri = "rtsp://camera-user:fake-password@camera.example/stream?token=fake-token";
        let source = Source::parse_with_options(Some(uri), RtspOptions::default());
        let debug = format!("{source:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(uri));
        assert!(!debug.contains("camera-user"));
        assert!(!debug.contains("fake-password"));
        assert!(!debug.contains("fake-token"));
    }

    #[test]
    fn rtsp_bin_builds_with_ghost_src_and_watch_pad() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let source = Source::parse(Some("rtsp://example/stream"));
        let bin = source.build().expect("source bin builds");
        assert!(
            bin.static_pad("src").is_some(),
            "bin exposes a ghost src pad"
        );
        assert!(
            bin.by_name("source-input").is_some(),
            "bin contains rtspsrc"
        );
        assert!(
            bin.by_name("source-decode").is_some(),
            "bin contains decodebin"
        );
        let tail = bin
            .by_name("source-convert")
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
    fn rtsp_transport_maps_to_registered_protocol_flags() {
        for (transport, expected) in [
            (RtspTransport::Tcp, vec!["tcp".to_owned()]),
            (RtspTransport::Udp, vec!["udp".to_owned()]),
            (
                RtspTransport::Auto,
                vec!["tcp".to_owned(), "udp".to_owned(), "udp-mcast".to_owned()],
            ),
        ] {
            let bin = build_rtsp(RtspOptions {
                transport,
                ..RtspOptions::default()
            });
            let src = bin.by_name("source-input").expect("rtspsrc exists");
            assert_eq!(protocol_nicks(&src), expected);
        }
    }

    #[test]
    fn rtsp_latency_and_drop_policy_round_trip() {
        for latency_ms in [0, 2000, u32::MAX] {
            for drop_on_latency in [false, true] {
                let bin = build_rtsp(RtspOptions {
                    latency_ms,
                    drop_on_latency,
                    ..RtspOptions::default()
                });
                let src = bin.by_name("source-input").expect("rtspsrc exists");
                assert_eq!(src.property::<u32>("latency"), latency_ms);
                assert_eq!(src.property::<bool>("drop-on-latency"), drop_on_latency);
            }
        }
    }

    #[test]
    fn file_and_test_source_topology_remains_unchanged() {
        gstsmith_app::init().expect("GStreamer should initialize");

        let test = Source::parse_with_options(
            None,
            RtspOptions {
                transport: RtspTransport::Udp,
                latency_ms: 0,
                drop_on_latency: true,
            },
        );
        let test_bin = test.build().expect("test source builds");
        assert!(test_bin.by_name("source-input").is_some());
        assert!(test_bin.by_name("source-convert").is_none());
        assert!(test.watch_pad(&test_bin).expect("watch_pad ok").is_none());

        let file = Source::parse_with_options(
            Some("/tmp/clip.mp4"),
            RtspOptions {
                transport: RtspTransport::Auto,
                latency_ms: u32::MAX,
                drop_on_latency: true,
            },
        );
        let file_bin = file.build().expect("file source builds");
        assert!(file_bin.by_name("source-input").is_some());
        assert!(file_bin.by_name("source-decode").is_some());
        assert!(file_bin.by_name("source-convert").is_some());
        assert_eq!(
            file_bin
                .by_name("source-input")
                .expect("filesrc exists")
                .property::<String>("location"),
            "/tmp/clip.mp4"
        );
    }

    #[test]
    fn test_source_has_no_watch_pad() {
        gstsmith_app::init().expect("GStreamer should initialize");
        let source = Source::parse(None);
        let bin = source.build().expect("source bin builds");
        assert!(
            source.watch_pad(&bin).expect("watch_pad ok").is_none(),
            "test source: nothing to watch"
        );
    }
}
