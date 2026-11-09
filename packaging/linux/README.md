# Self-contained Linux package

The Linux packaging creates a relocatable tarball containing the application,
model assets, a deliberately selected set of GStreamer plugins, the GStreamer
plugin scanner, and every shared library those files need apart from a small
set that must come from the host. A user does not need to install GStreamer to
run it.

```sh
mise run package:linux          # from the host's GStreamer
mise run package:linux --dist   # inside the pinned Ubuntu 26.04 container
```

The output is `target/package/linux/slik-<version>-linux-<arch>.tar.gz`, plus
the unpacked tree next to it. Run `bin/slik` from anywhere after extracting.

## Host and distribution builds

Without `--dist`, [`package.sh`](package.sh) uses the host's GStreamer
development files and plugins, so the host needs the same packages as for
development plus `patchelf`. The package then requires a glibc at least as new
as the host's.

With `--dist`, [`package-in-container.sh`](package-in-container.sh) builds the
[`Dockerfile`](Dockerfile) image (Ubuntu 26.04 pinned by digest, the Rust
version from `mise.toml`), runs `package.sh` inside it as the invoking user, and
then verifies the result a second time in the bare base image, which has no
GStreamer, no build tools, and nothing else the package could silently rely
on. Only Docker is needed on the build host, which may also be a Mac (the
package is built for the Docker host's CPU architecture); set
`SLIK_CONTAINER_TOOL=podman` to use Podman. Output and Cargo state are kept
under `target/package/linux`.

Ubuntu 26.04 is used because slik's gstsmith plugins require GStreamer 1.28,
and it is the first Ubuntu release that ships it. Its glibc 2.43 is therefore
the package's minimum glibc: it runs on Ubuntu 26.04 and Fedora 44, for
example, but not on Ubuntu 24.04. Supporting older distributions means building
GStreamer 1.28 (and a minimal FFmpeg) from source on an older base image.

## Runtime layout

```text
slik-<version>-linux-<arch>/
  bin/slik                                   launcher
  libexec/slik/slik                          application
  libexec/gstreamer-1.0/gst-plugin-scanner
  lib/*.so*                                  bundled libraries
  lib/gstreamer-1.0/*.so                     allowlisted plugins
  share/slik/                                model, labels, runtime manifest
  share/doc/slik/third-party/                Debian copyright files
```

The launcher sets GStreamer's plugin path, scanner, and a versioned registry
under `$XDG_CACHE_HOME` (or `~/.cache`), points `GIO_MODULE_DIR` at the package
so the host's GIO modules are never loaded into the bundled GLib, and then
executes the application. Every bundled ELF file carries an `$ORIGIN`-relative
`DT_RPATH`, which also takes precedence over a caller's `LD_LIBRARY_PATH`.

## Host libraries

The package deliberately does not contain glibc and the dynamic loader, the GCC
and C++ runtimes, the GL/EGL vendor-dispatch libraries, or NVIDIA/CUDA
libraries: these must match the host's loader and graphics driver. Everything
else, including libdrm, the X11/xcb client libraries, and the VA-API, VDPAU,
and Vulkan loaders, is bundled because distribution FFmpeg builds link them
directly; leaving them to the host would break decoding on headless machines.
The list lives in [`host-libraries.sh`](host-libraries.sh), which both
`package.sh` and `verify.sh` source.

## Included media support

[`plugins.txt`](plugins.txt) is the auditable plugin allowlist. It matches the
macOS bundle, except that H.264/H.265 decoding uses gst-libav (Linux has no
system decoder comparable to VideoToolbox) and preview uses the X11 sinks.

gst-libav accounts for most of the package's size: Ubuntu's FFmpeg links a
large set of codec, filter, and hardware libraries. Ubuntu builds FFmpeg with
GPL components enabled, so review the bundled copyright files before
publishing; a minimal LGPL FFmpeg with only the H.264/H.265 decoders would
shrink the package substantially and simplify licensing.

## Verification

[`verify.sh`](verify.sh) checks that every bundled ELF file resolves its
non-host dependencies inside the package, launches the test source in an empty
environment while checking `/proc/<pid>/maps` for libraries loaded from outside
the package, and decodes H.264 and H.265 MP4 fixtures. Fixtures are made with
`x264enc`/`x265enc` when available, or reused from `SLIK_VERIFY_FIXTURES`.
