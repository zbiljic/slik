#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "The macOS bundle verification must run on macOS." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
bundle=${1:-$repo_root/target/package/macos/Slik.app}
# lsof reports resolved paths, so compare against the bundle's physical path.
bundle=$(CDPATH= cd -- "$bundle" && pwd -P)
frameworks_dir="$bundle/Contents/Frameworks"
libraries_dir="$bundle/Contents/MacOS"
launcher="$bundle/Contents/MacOS/slik"

for required_file in "$bundle/Contents/Info.plist" "$launcher" "$bundle/Contents/MacOS/slik-real" "$bundle/Contents/Helpers/gst-plugin-scanner"; do
    if [ ! -f "$required_file" ]; then
        echo "Bundle file is missing: $required_file" >&2
        exit 1
    fi
done

verification_root=$(mktemp -d "${TMPDIR:-/tmp}/slik-verify.XXXXXX")
trap 'rm -rf "$verification_root"' EXIT HUP INT TERM
macho_list="$verification_root/macho-files"
find "$bundle" -type f -print | while IFS= read -r bundle_file; do
    if file "$bundle_file" | grep -q 'Mach-O'; then
        printf '%s\n' "$bundle_file"
    fi
done >"$macho_list"

while IFS= read -r macho_file || [ -n "$macho_file" ]; do
    dependency_file="$verification_root/dependencies"
    otool -L "$macho_file" | awk 'NR > 1 { print $1 }' >"$dependency_file"
    while IFS= read -r dependency_name || [ -n "$dependency_name" ]; do
        case "$dependency_name" in
            /System/Library/* | /usr/lib/*) ;;
            @rpath/*)
                dependency_basename=$(basename "$dependency_name")
                if [ ! -f "$libraries_dir/$dependency_basename" ] \
                    && [ ! -f "$frameworks_dir/$dependency_basename" ] \
                    && [ "$dependency_basename" != "$(basename "$macho_file")" ]; then
                    echo "Bundled dependency is missing for $macho_file: $dependency_name" >&2
                    exit 1
                fi
                ;;
            *)
                echo "Non-relocatable dependency in $macho_file: $dependency_name" >&2
                exit 1
                ;;
        esac
    done <"$dependency_file"

    rpath_file="$verification_root/rpaths"
    otool -l "$macho_file" | awk '$1 == "path" { print $2 }' >"$rpath_file"
    while IFS= read -r existing_rpath || [ -n "$existing_rpath" ]; do
        case "$existing_rpath" in
            /*)
                echo "Absolute runtime search path in $macho_file: $existing_rpath" >&2
                exit 1
                ;;
        esac
    done <"$rpath_file"
done <"$macho_list"

codesign --verify --deep --strict "$bundle"

runtime_root="$verification_root/runtime"
mkdir -p "$runtime_root/cache" "$runtime_root/user"
runtime_log="$verification_root/runtime.log"
env -i \
    HOME="$runtime_root/user" \
    PATH=/usr/bin:/bin \
    RUST_LOG=info \
    XDG_CACHE_HOME="$runtime_root/cache" \
    "$launcher" --source test >"$runtime_log" 2>&1 &
runtime_pid=$!

sleep 5
if ! kill -0 "$runtime_pid" 2>/dev/null; then
    echo "Bundled application exited before the smoke-test interval completed." >&2
    sed -n '1,240p' "$runtime_log" >&2
    wait "$runtime_pid" || true
    exit 1
fi

# Inventory the running process's mapped files with lsof rather than
# DYLD_PRINT_LIBRARIES: the hardened runtime used for Developer ID builds
# strips DYLD_* variables, which would silently empty that log. Any Mach-O
# file outside the bundle and the OS locations is a leak.
lsof -p "$runtime_pid" -a -d txt -Fn | sed -n 's/^n//p' | sort -u >"$verification_root/mapped-files"
if ! grep -qF "$bundle/Contents/MacOS/slik-real" "$verification_root/mapped-files"; then
    echo "Could not inventory the running application's mapped files." >&2
    kill -INT "$runtime_pid" || true
    wait "$runtime_pid" || true
    exit 1
fi
leaked=false
while IFS= read -r mapped_file || [ -n "$mapped_file" ]; do
    case "$mapped_file" in
        "$bundle"/* | /System/* | /usr/lib/*) continue ;;
    esac
    if file -b "$mapped_file" 2>/dev/null | grep -q '^Mach-O'; then
        echo "Loaded from outside the bundle: $mapped_file" >&2
        leaked=true
    fi
done <"$verification_root/mapped-files"
if [ "$leaked" = true ]; then
    kill -INT "$runtime_pid" || true
    wait "$runtime_pid" || true
    exit 1
fi

kill -INT "$runtime_pid"
set +e
wait "$runtime_pid"
runtime_exit=$?
set -e
if [ "$runtime_exit" -ne 0 ]; then
    echo "Bundled application exited with status $runtime_exit." >&2
    sed -n '1,240p' "$runtime_log" >&2
    exit 1
fi

# Fixtures are generated with the SDK's tools when packaging provides it, in an
# isolated registry so the host's GStreamer cache is left alone; otherwise fall
# back to whatever GStreamer tools are on PATH.
gst_inspect=gst-inspect-1.0
gst_launch=gst-launch-1.0
if [ -n "${SLIK_GSTREAMER_SDK:-}" ]; then
    gst_inspect="$SLIK_GSTREAMER_SDK/bin/gst-inspect-1.0"
    gst_launch="$SLIK_GSTREAMER_SDK/bin/gst-launch-1.0"
    GST_PLUGIN_PATH_1_0=
    GST_PLUGIN_SYSTEM_PATH_1_0="$SLIK_GSTREAMER_SDK/lib/gstreamer-1.0"
    GST_PLUGIN_SCANNER_1_0="$SLIK_GSTREAMER_SDK/libexec/gstreamer-1.0/gst-plugin-scanner"
    GST_REGISTRY_1_0="$verification_root/sdk-registry.bin"
    export GST_PLUGIN_PATH_1_0 GST_PLUGIN_SYSTEM_PATH_1_0 GST_PLUGIN_SCANNER_1_0 GST_REGISTRY_1_0
fi

codec_tests_ran=false
if command -v "$gst_inspect" >/dev/null 2>&1 \
    && command -v "$gst_launch" >/dev/null 2>&1; then
    codec_elements_available=true
    for codec_element in vtenc_h264 vtenc_h265 h264parse h265parse mp4mux; do
        if ! "$gst_inspect" --exists "$codec_element"; then
            codec_elements_available=false
        fi
    done

    if [ "$codec_elements_available" = true ]; then
        fixture_dir="$verification_root/fixtures"
        mkdir -p "$fixture_dir"
        "$gst_launch" -q \
            videotestsrc num-buffers=4 \
            ! video/x-raw,width=320,height=240,framerate=5/1 \
            ! vtenc_h264 ! h264parse ! mp4mux \
            ! filesink location="$fixture_dir/h264.mp4"
        "$gst_launch" -q \
            videotestsrc num-buffers=4 \
            ! video/x-raw,width=320,height=240,framerate=5/1 \
            ! vtenc_h265 ! h265parse ! mp4mux \
            ! filesink location="$fixture_dir/h265.mp4"

        for codec_name in h264 h265; do
            codec_log="$verification_root/$codec_name.log"
            if ! env -i \
                HOME="$runtime_root/user" \
                PATH=/usr/bin:/bin \
                RUST_LOG=info \
                XDG_CACHE_HOME="$runtime_root/cache" \
                "$launcher" \
                --source "$fixture_dir/$codec_name.mp4" \
                --pace full >"$codec_log" 2>&1; then
                echo "Bundled application could not decode the $codec_name fixture." >&2
                sed -n '1,240p' "$codec_log" >&2
                exit 1
            fi

            if grep -E 'GStreamer-GL-WARNING|NSApplication needs to be running' "$codec_log" >/dev/null; then
                echo "Bundled $codec_name decoding did not run cleanly on Cocoa's application main thread." >&2
                sed -n '1,240p' "$codec_log" >&2
                exit 1
            fi
        done
        codec_tests_ran=true
    fi
fi

if [ "$codec_tests_ran" = true ]; then
    echo "Verified relocatable dependencies, code signature, test source, and H.264/H.265 MP4 decoding."
else
    echo "Verified relocatable dependencies, code signature, and headless test-source startup."
    echo "Skipped codec fixtures because the build host lacks the required VideoToolbox encoder elements."
fi
