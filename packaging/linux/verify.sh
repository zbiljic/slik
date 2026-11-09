#!/bin/sh
# Verify an unpacked Linux package: every non-host library resolves inside the
# package, the application starts in an empty environment without loading a
# library from outside the package, and (when encoders are available to make
# fixtures) H.264 and H.265 MP4 files decode.
set -eu

if [ "$(uname -s)" != "Linux" ]; then
    echo "The Linux package verification must run on Linux." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
if [ $# -ge 1 ]; then
    root=$1
else
    root=$(find "$repo_root/target/package/linux" -mindepth 1 -maxdepth 1 -type d -name 'slik-*-linux-*' | sort | tail -n 1)
fi
# ldd and /proc/<pid>/maps report resolved paths, so use the physical path.
root=$(CDPATH= cd -- "$root" && pwd -P)
launcher="$root/bin/slik"

. "$script_dir/host-libraries.sh"

for required_file in "$launcher" "$root/libexec/slik/slik" "$root/libexec/gstreamer-1.0/gst-plugin-scanner"; do
    if [ ! -f "$required_file" ]; then
        echo "Package file is missing: $required_file" >&2
        exit 1
    fi
done

verification_root=$(mktemp -d "${TMPDIR:-/tmp}/slik-verify.XXXXXX")
trap 'rm -rf "$verification_root"' EXIT HUP INT TERM

# Static check: with an empty LD_LIBRARY_PATH, everything that is not a host
# library must resolve to a file inside the package.
find "$root/bin" "$root/libexec" "$root/lib" -type f >"$verification_root/files"
while IFS= read -r package_file || [ -n "$package_file" ]; do
    # Only ELF files are checked; this avoids needing file(1), which minimal
    # images used for clean-room verification do not have.
    if [ "$(head -c 4 "$package_file" | od -An -c | tr -d ' ')" != '177ELF' ]; then
        continue
    fi
    env -i PATH=/usr/bin:/bin ldd "$package_file" >"$verification_root/ldd" 2>&1 || true
    while IFS= read -r ldd_line || [ -n "$ldd_line" ]; do
        soname=$(printf '%s' "${ldd_line%%=>*}" | tr -d '[:space:]')
        case "$ldd_line" in
            *"=> not found"*)
                if ! printf '%s\n' "$soname" | grep -Eq "$HOST_LIBRARIES"; then
                    echo "Unresolved dependency of $package_file: $soname" >&2
                    exit 1
                fi
                ;;
            *" => /"*)
                library_path=${ldd_line#*=> }
                library_path=${library_path%% (*}
                case "$library_path" in
                    "$root"/*) ;;
                    *)
                        if ! printf '%s\n' "$soname" | grep -Eq "$HOST_LIBRARIES"; then
                            echo "$package_file resolves $soname outside the package: $library_path" >&2
                            exit 1
                        fi
                        ;;
                esac
                ;;
        esac
    done <"$verification_root/ldd"
done <"$verification_root/files"

runtime_root="$verification_root/runtime"
mkdir -p "$runtime_root/cache" "$runtime_root/user"

# Report every shared object mapped by the process that is neither inside the
# package nor an allowed host library.
check_mappings() {
    process_id=$1
    awk '$6 ~ /\.so/ { print $6 }' "/proc/$process_id/maps" | sort -u >"$verification_root/maps"
    leaked=false
    while IFS= read -r mapped_file || [ -n "$mapped_file" ]; do
        case "$mapped_file" in
            "$root"/* | */gconv/*) continue ;;
        esac
        if ! basename "$mapped_file" | grep -Eq "$HOST_LIBRARIES"; then
            echo "Loaded from outside the package: $mapped_file" >&2
            leaked=true
        fi
    done <"$verification_root/maps"
    [ "$leaked" = false ]
}

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
    echo "Packaged application exited before the smoke-test interval completed." >&2
    sed -n '1,240p' "$runtime_log" >&2
    wait "$runtime_pid" || true
    exit 1
fi

if ! check_mappings "$runtime_pid"; then
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
    echo "Packaged application exited with status $runtime_exit." >&2
    sed -n '1,240p' "$runtime_log" >&2
    exit 1
fi

# Codec fixtures are produced with GStreamer tools from PATH in an isolated
# registry. SLIK_VERIFY_FIXTURES names a directory that keeps them, so a clean
# environment without GStreamer tools can reuse fixtures made elsewhere.
fixture_dir=${SLIK_VERIFY_FIXTURES:-$verification_root/fixtures}
mkdir -p "$fixture_dir"
if [ ! -f "$fixture_dir/h264.mp4" ] || [ ! -f "$fixture_dir/h265.mp4" ]; then
    if command -v gst-inspect-1.0 >/dev/null 2>&1 && command -v gst-launch-1.0 >/dev/null 2>&1; then
        GST_REGISTRY_1_0="$verification_root/host-registry.bin"
        export GST_REGISTRY_1_0
        codec_elements_available=true
        for codec_element in x264enc x265enc h264parse h265parse mp4mux; do
            if ! gst-inspect-1.0 --exists "$codec_element"; then
                codec_elements_available=false
            fi
        done

        if [ "$codec_elements_available" = true ]; then
            for codec_name in h264 h265; do
                gst-launch-1.0 -q \
                    videotestsrc num-buffers=4 \
                    ! video/x-raw,width=320,height=240,framerate=5/1 \
                    ! "x${codec_name#h}enc" ! "${codec_name}parse" ! mp4mux \
                    ! filesink location="$fixture_dir/$codec_name.mp4.part"
                mv "$fixture_dir/$codec_name.mp4.part" "$fixture_dir/$codec_name.mp4"
            done
        fi
    fi
fi

codec_tests_ran=false
if [ -f "$fixture_dir/h264.mp4" ] && [ -f "$fixture_dir/h265.mp4" ]; then
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
            echo "Packaged application could not decode the $codec_name fixture." >&2
            sed -n '1,240p' "$codec_log" >&2
            exit 1
        fi
    done
    codec_tests_ran=true
fi

if [ "$codec_tests_ran" = true ]; then
    echo "Verified relocatable dependencies, isolated runtime mappings, test source, and H.264/H.265 MP4 decoding."
else
    echo "Verified relocatable dependencies, isolated runtime mappings, and headless test-source startup."
    echo "Skipped codec fixtures: none in SLIK_VERIFY_FIXTURES and no x264enc/x265enc/h264parse/h265parse/mp4mux to make them."
fi
