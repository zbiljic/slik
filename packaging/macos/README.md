# Self-contained macOS bundle

The macOS packaging prototype creates a relocatable `Slik.app` containing the
application, model assets, a deliberately selected set of GStreamer plugins,
the GStreamer plugin scanner, and every non-system dynamic library those files
need. A user does not need to install GStreamer to run the resulting bundle.

Build and verify it with:

```sh
mise run package:macos          # from the host's GStreamer (e.g. Homebrew)
mise run package:macos --dist   # from the pinned official GStreamer SDK
```

The output is `target/package/macos/Slik.app`. The build machine needs the
Rust toolchain, Xcode command-line tools, `pkg-config` (for example
`brew install pkgconf`), and the model installed by `mise run setup:model`.

Without `--dist` the bundle is assembled from the host's GStreamer. That is
quick and needs no download, but the bundle inherits the host libraries'
minimum macOS version; Homebrew bottles target the build host's own release.
Use `--dist` for anything given to other people. It does not need GStreamer
installed on the build machine.

## GStreamer SDK

The bundle is built and linked against the official GStreamer macOS packages
from gstreamer.freedesktop.org, not a package manager's copy. Those libraries
already use relocatable `@rpath` install names and target macOS 11.0; Homebrew
bottles, by contrast, target the build host's macOS release, which would make
the bundle refuse to start on anything older.

`package.sh --dist` (or `mise run setup:gstreamer:macos-sdk` on its own) runs
[`gstreamer-sdk.sh`](gstreamer-sdk.sh), which:

- downloads the pinned runtime and development `.pkg` files into
  `.cache/gstreamer/downloads`, resuming interrupted transfers, and checks
  their pinned SHA-256 digests (the upstream packages are not signed);
- expands them with `pkgutil` instead of installing them, so no administrator
  rights are needed and `/Library/Frameworks` is untouched;
- unpacks only the SDK components that provide allowlisted plugins or their
  libraries, drops static archives, and rewrites the `.pc` files to the
  private prefix `.cache/gstreamer/macos-<version>`.

`package.sh` points `pkg-config` at that prefix, builds in a separate
`target/package/macos/cargo` directory so SDK-linked artifacts do not
invalidate everyday builds against the host GStreamer, sets
`MACOSX_DEPLOYMENT_TARGET=11.0`, and thins the SDK's universal binaries to the
host architecture. Set `SLIK_GSTREAMER_SDK` to use a different, already
prepared prefix. To upgrade GStreamer, change the version and both digests in
`gstreamer-sdk.sh`; keep the component list in step with `plugins.txt`.

## Runtime layout

The bundle uses a small, standard-library-only launcher at
`Contents/MacOS/slik`. It finds the bundle relative to its own executable
(following symlinks, so `ln -s /Applications/Slik.app/Contents/MacOS/slik
/usr/local/bin/slik` works) and starts `slik-real` with private runtime paths
for:

- plugins in `Contents/Frameworks`;
- `gst-plugin-scanner` in `Contents/Helpers`;
- the model, model-info contract, and labels in `Contents/Resources`;
- an empty `Contents/Resources/gio/modules`, so the bundled GLib never loads
  GIO modules installed on the host; and
- a versioned, architecture-specific GStreamer registry in the user's cache.

`slik` runs every pipeline inside GStreamer's macOS application loop, because
VideoToolbox decoders and video sinks can need Cocoa's main thread. The bundle
sets `LSUIElement`, so headless runs do not show a Dock icon. A run over SSH
without a GUI login session has not been tested.

The runtime itself is loaded directly from the application bundle. It is not
extracted on first run: loading in place avoids duplicate files, cache cleanup,
concurrent extraction, and invalidating the bundle's code signature. Only
GStreamer's generated plugin registry is written to the user cache.

The packaging script recursively follows Mach-O dependencies, copies
non-system libraries, rewrites their install names to relative `@rpath` names,
and signs nested code before signing the app. Verification rejects absolute
build-machine paths, checks the code signature, launches the app in an empty
environment while checking (with `lsof`, which also works under the hardened
runtime) that every mapped Mach-O file comes from the bundle or the OS, and—when
VideoToolbox encoder elements are available on the build host—decodes generated
H.264 and H.265 MP4 fixtures with the private runtime.

## Included media support

[`plugins.txt`](plugins.txt) is the auditable plugin allowlist. The initial
bundle includes the pipeline's test source, file and RTSP support, H.264/H.265
RTP and parsing, Apple VideoToolbox decoding, MP4, Matroska and MPEG-TS
demuxers, and the optional native preview path.

`gst-libav` is intentionally excluded. It substantially increases bundle size
and brings additional codec and redistribution obligations; H.264/H.265 use
Apple's system decoder instead. Add a plugin to the allowlist only when a
supported input requires it, then rerun the complete verifier.

## Signing and release considerations

Local builds use an ad-hoc signature. Supply a Developer ID Application
identity for a distributable build:

```sh
SLIK_CODESIGN_IDENTITY='Developer ID Application: Example (TEAMID)' \
  mise run package:macos
```

Signing is implemented, but notarization and stapling are not. A public release
also needs reviewed third-party license notices for every library listed in
`Contents/Resources/runtime-manifest.txt`.

The application inherits the highest minimum-macOS version among the Rust and
GStreamer binaries it packages; with the official SDK this is currently 11.0.
The bundle contains the host architecture only. Build once per architecture
(or add an explicit universal binary merge step) before publishing; the
verifier tests the host architecture only.
