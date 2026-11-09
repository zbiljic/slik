# Shared by package.sh and verify.sh: libraries that must come from the host
# rather than the package. These are the C runtime and dynamic loader (tied to
# the loader that runs the package), the GCC and C++ runtimes (shared with
# every other library in the process), and the GL/EGL vendor-dispatch and
# NVIDIA libraries, which must match the installed graphics driver. Client
# libraries and loaders such as libdrm, X11/xcb, libva, and the Vulkan loader
# are bundled: distribution FFmpeg builds link them directly, so leaving them
# out would break decoding on headless hosts.
#
# libnss_* are glibc modules loaded for name lookups (e.g. an RTSP host name).
# They are never DT_NEEDED, so this only matters when checking what a running
# process mapped; glibc's gconv modules are matched by directory in verify.sh.
# shellcheck disable=SC2034 # used by the scripts that source this file
HOST_LIBRARIES='^(ld-linux.*|libc|libm|libmvec|libdl|libpthread|librt|libutil|libresolv|libanl|libnsl|libnss_.*|libgcc_s|libstdc\+\+|libGL|libGLX|libGLdispatch|libEGL|libGLESv2|libOpenGL|libgbm|libnvidia.*|libcuda)\.so'
