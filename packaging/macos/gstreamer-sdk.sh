#!/bin/sh
# Download, verify, and unpack the official GStreamer macOS runtime and
# development packages into a private, relocatable prefix.
#
# The packages are expanded with pkgutil rather than installed, so no
# administrator rights are needed and the system's /Library/Frameworks is left
# untouched. Progress is reported on stderr; the prefix path is the only output
# on stdout so callers can capture it.
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "The GStreamer macOS SDK can only be prepared on macOS." >&2
    exit 1
fi

gstreamer_version=1.28.8
runtime_package="gstreamer-1.0-${gstreamer_version}-universal.pkg"
runtime_sha256=d371ab04a7632713626836c2fc234e863cc98484c74c473eba99b0fbd9b3a3fe
devel_package="gstreamer-1.0-devel-${gstreamer_version}-universal.pkg"
devel_sha256=e6d380bc17be7eccf13e0154567d4718fc9321da822408c7ca0cd808b3d4a3f7
download_base="https://gstreamer.freedesktop.org/data/pkg/osx/${gstreamer_version}"
framework_location=/Library/Frameworks/GStreamer.framework/Versions/1.0

# SDK components to unpack. Each must cover a plugin in plugins.txt or a
# library those plugins link; everything else (libav, GPL codecs, Python,
# GTK, ...) is left out to keep the prefix small and its contents auditable.
components="base-system base-crypto core playback net effects codecs codecs-restricted system analytics"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
cache_dir=${SLIK_GSTREAMER_CACHE_DIR:-$repo_root/.cache/gstreamer}
download_dir="$cache_dir/downloads"
prefix="$cache_dir/macos-${gstreamer_version}"

stamp="$gstreamer_version $components"
if [ -f "$prefix/.complete" ] && [ "$(cat "$prefix/.complete")" = "$stamp" ]; then
    echo "GStreamer $gstreamer_version macOS SDK is ready: $prefix" >&2
    printf '%s\n' "$prefix"
    exit 0
fi

for required_command in curl ditto pkgutil shasum; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "Required command not found: $required_command" >&2
        exit 1
    fi
done

download() {
    package_name=$1
    expected_sha256=$2
    package_path="$download_dir/$package_name"

    if [ -f "$package_path" ] \
        && [ "$(shasum -a 256 "$package_path" | awk '{print $1}')" = "$expected_sha256" ]; then
        echo "Using verified download: $package_path" >&2
        return
    fi

    mkdir -p "$download_dir"
    partial_path="$package_path.part"
    echo "Downloading $package_name..." >&2
    # The upstream server frequently drops long transfers; resume the partial
    # file instead of restarting from zero.
    attempt=1
    until curl --fail --location --http1.1 --silent --show-error \
        --continue-at - --output "$partial_path" "$download_base/$package_name"; do
        if [ "$attempt" -ge 50 ]; then
            echo "Giving up on $package_name after $attempt attempts." >&2
            exit 1
        fi
        attempt=$((attempt + 1))
        echo "Download interrupted; resuming (attempt $attempt)..." >&2
        sleep 2
    done

    actual_sha256=$(shasum -a 256 "$partial_path" | awk '{print $1}')
    if [ "$actual_sha256" != "$expected_sha256" ]; then
        rm -f "$partial_path"
        echo "$package_name checksum mismatch." >&2
        echo "Expected: $expected_sha256" >&2
        echo "Actual:   $actual_sha256" >&2
        exit 1
    fi
    mv "$partial_path" "$package_path"
}

download "$runtime_package" "$runtime_sha256"
download "$devel_package" "$devel_sha256"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/slik-gstreamer-sdk.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM
staging_prefix="$work_dir/prefix"
mkdir -p "$staging_prefix"

for package_name in "$runtime_package" "$devel_package"; do
    echo "Expanding $package_name..." >&2
    expanded_dir="$work_dir/${package_name%.pkg}"
    pkgutil --expand-full "$download_dir/$package_name" "$expanded_dir"

    for component in "$expanded_dir"/*.pkg; do
        [ -d "$component/Payload" ] || continue
        component_name=$(basename "$component" "-${gstreamer_version}-universal.pkg")
        component_name=${component_name#gstreamer-1.0-}
        component_name=${component_name%-devel}
        component_name=${component_name%-1.0}
        case " $components " in
            *" $component_name "*) ;;
            *) continue ;;
        esac
        install_location=$(sed -n 's/.*install-location="\([^"]*\)".*/\1/p' "$component/PackageInfo" | head -n 1)
        # Only the versioned framework contents form the prefix; the outer
        # framework wrapper is just symlinks and a merged convenience dylib.
        case "${install_location%/}" in
            "$framework_location") ditto "$component/Payload" "$staging_prefix" ;;
            *) ;;
        esac
    done
done

# slik links GStreamer dynamically; the static archives are most of the
# development package's size.
find "$staging_prefix" -type f \( -name '*.a' -o -name '*.la' \) -delete

if [ ! -f "$staging_prefix/lib/pkgconfig/gstreamer-1.0.pc" ]; then
    echo "Expanded packages did not provide gstreamer-1.0.pc." >&2
    exit 1
fi

# The .pc files hard-code the framework install location; point them at the
# private prefix instead.
find "$staging_prefix/lib/pkgconfig" -name '*.pc' -type f | while IFS= read -r pc_file; do
    sed -i '' "s|^prefix=.*|prefix=$prefix|" "$pc_file"
done

rm -rf "$prefix"
mkdir -p "$cache_dir"
mv "$staging_prefix" "$prefix"
printf '%s\n' "$stamp" >"$prefix/.complete"

echo "GStreamer $gstreamer_version macOS SDK is ready: $prefix" >&2
printf '%s\n' "$prefix"
