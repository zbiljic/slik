#!/bin/sh
# Build a relocatable Linux tarball containing slik, its default model, and a
# private GStreamer runtime.
#
# Without --dist the package is assembled from the host's GStreamer
# installation. With --dist the same script runs inside a pinned Ubuntu 26.04
# container (see package-in-container.sh) so the result has a known glibc
# baseline and GStreamer version.
set -eu

usage() {
    echo "Usage: $0 [--dist]" >&2
    echo "  --dist  build inside the pinned Ubuntu 26.04 container for distribution" >&2
}

dist=false
for argument in "$@"; do
    case "$argument" in
        --dist) dist=true ;;
        -h | --help) usage; exit 0 ;;
        *) usage; exit 2 ;;
    esac
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

# --dist only needs a container tool on the host, so it can be started from
# macOS too; everything below runs on Linux.
if [ "$dist" = true ] && [ -z "${SLIK_IN_PACKAGE_CONTAINER:-}" ]; then
    exec "$script_dir/package-in-container.sh"
fi

if [ "$(uname -s)" != "Linux" ]; then
    echo "The Linux package must be built on Linux (or use --dist with Docker)." >&2
    exit 1
fi

for required_command in cargo file ldd patchelf pkg-config tar; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "Required packaging command not found: $required_command" >&2
        exit 1
    fi
done

. "$script_dir/host-libraries.sh"

plugin_source_dir=$(pkg-config --variable=pluginsdir gstreamer-1.0)
scanner_source="$(pkg-config --variable=pluginscannerdir gstreamer-1.0)/gst-plugin-scanner"
model_source="$repo_root/crates/slik/models/nanodet-plus-m-320.onnx"
model_info_source="$model_source.modelinfo"
labels_source="$repo_root/crates/slik/src/coco.names"

for required_file in "$scanner_source" "$model_source" "$model_info_source" "$labels_source"; do
    if [ ! -f "$required_file" ]; then
        echo "Required package input not found: $required_file" >&2
        echo "Run 'mise run setup:model' before packaging." >&2
        exit 1
    fi
done

if [ "$dist" = true ]; then
    gstreamer_source="container ($(sed -n 's/^PRETTY_NAME="\(.*\)"$/\1/p' /etc/os-release))"
else
    gstreamer_source="host ($(pkg-config --variable=prefix gstreamer-1.0))"
fi

echo "Building release binaries..."
cargo build --locked --release -p slik --bins

target_dir=${CARGO_TARGET_DIR:-$repo_root/target}
app_version=$(cargo pkgid --locked -p slik | sed 's/.*[#@]//')
case "$app_version" in
    '' | *[!0-9A-Za-z._-]*)
        echo "Could not determine a safe application version from '$app_version'." >&2
        exit 1
        ;;
esac
package_arch=$(uname -m)
package_name="slik-$app_version-linux-$package_arch"

staging_root=$(mktemp -d "${TMPDIR:-/tmp}/slik-package.XXXXXX")
trap 'rm -rf "$staging_root"' EXIT HUP INT TERM

root="$staging_root/$package_name"
mkdir -p \
    "$root/bin" \
    "$root/lib/gstreamer-1.0" \
    "$root/lib/gio/modules" \
    "$root/libexec/slik" \
    "$root/libexec/gstreamer-1.0" \
    "$root/share/slik" \
    "$root/share/doc/slik/third-party"

cp "$target_dir/release/slik-launcher" "$root/bin/slik"
cp "$target_dir/release/slik" "$root/libexec/slik/slik"
cp -L "$scanner_source" "$root/libexec/gstreamer-1.0/gst-plugin-scanner"
cp "$model_source" "$model_info_source" "$labels_source" "$root/share/slik/"
cp "$script_dir/plugins.txt" "$root/share/slik/gstreamer-plugins.txt"
cp "$repo_root/README.md" "$root/share/doc/slik/"

while IFS= read -r plugin_name || [ -n "$plugin_name" ]; do
    case "$plugin_name" in
        '' | \#*) continue ;;
    esac
    plugin_source="$plugin_source_dir/libgst${plugin_name}.so"
    if [ ! -f "$plugin_source" ]; then
        echo "Selected GStreamer plugin is unavailable: $plugin_name ($plugin_source)" >&2
        exit 1
    fi
    cp -L "$plugin_source" "$root/lib/gstreamer-1.0/"
done <"$script_dir/plugins.txt"

# ldd reports each object's complete transitive closure, so one pass over the
# executables and plugins finds every library the package needs.
library_sources="$staging_root/library-sources"
ldd_output="$staging_root/ldd-output"
: >"$library_sources.unsorted"
for elf_file in \
    "$root/libexec/slik/slik" \
    "$root/libexec/gstreamer-1.0/gst-plugin-scanner" \
    "$root"/lib/gstreamer-1.0/*.so; do
    ldd "$elf_file" >"$ldd_output"
    while IFS= read -r ldd_line || [ -n "$ldd_line" ]; do
        soname=$(printf '%s' "${ldd_line%%=>*}" | tr -d '[:space:]')
        case "$ldd_line" in
            *"=> not found"*)
                if ! printf '%s\n' "$soname" | grep -Eq "$HOST_LIBRARIES"; then
                    echo "Unresolved dependency of $elf_file: $soname" >&2
                    exit 1
                fi
                ;;
            *" => /"*)
                library_path=${ldd_line#*=> }
                library_path=${library_path%% (*}
                if ! printf '%s\n' "$soname" | grep -Eq "$HOST_LIBRARIES"; then
                    printf '%s %s\n' "$soname" "$library_path" >>"$library_sources.unsorted"
                fi
                ;;
        esac
    done <"$ldd_output"
done
sort -u "$library_sources.unsorted" >"$library_sources"

while read -r soname library_path; do
    cp -L "$library_path" "$root/lib/$soname"
done <"$library_sources"

# Relative search paths make the tree relocatable. DT_RPATH (rather than
# DT_RUNPATH) is used so a caller's LD_LIBRARY_PATH cannot substitute a
# different GStreamer or GLib for the bundled one.
set_rpath() {
    patchelf --force-rpath --set-rpath "$2" "$1"
}
set_rpath "$root/libexec/slik/slik" '$ORIGIN/../../lib'
set_rpath "$root/libexec/gstreamer-1.0/gst-plugin-scanner" '$ORIGIN/../../lib'
for library in "$root"/lib/*.so*; do
    set_rpath "$library" '$ORIGIN'
done
for plugin in "$root"/lib/gstreamer-1.0/*.so; do
    set_rpath "$plugin" '$ORIGIN/..'
done

# Third-party license notices, from the Debian packages that provided each
# bundled library and plugin.
if command -v dpkg >/dev/null 2>&1; then
    {
        cut -d' ' -f2 "$library_sources"
        for plugin in "$root"/lib/gstreamer-1.0/*.so; do
            printf '%s\n' "$plugin_source_dir/$(basename "$plugin")"
        done
        printf '%s\n' "$scanner_source"
    } | while IFS= read -r source_file; do
        dpkg -S "$(readlink -f "$source_file")" 2>/dev/null | sed -n '1s/:.*//p' | cut -d: -f1
    done | sort -u >"$staging_root/debian-packages"

    while IFS= read -r debian_package; do
        copyright="/usr/share/doc/$debian_package/copyright"
        if [ -f "$copyright" ]; then
            cp "$copyright" "$root/share/doc/slik/third-party/$debian_package.copyright"
        fi
    done <"$staging_root/debian-packages"
fi

{
    printf 'Slik %s\n' "$app_version"
    printf 'GStreamer %s\n' "$(pkg-config --modversion gstreamer-1.0)"
    printf 'GStreamer source: %s\n' "$gstreamer_source"
    printf 'glibc %s\n' "$(ldd --version | sed -n '1s/.* //p')"
    printf '\nBundled ELF files:\n'
    (cd "$root" && find . -type f \( -name '*.so*' -o -path './bin/*' -o -path './libexec/*' \) | sed 's|^\./||' | sort)
    if [ -f "$staging_root/debian-packages" ]; then
        printf '\nSource Debian packages:\n'
        cat "$staging_root/debian-packages"
    fi
} >"$root/share/slik/runtime-manifest.txt"

package_root=${SLIK_PACKAGE_DIR:-$repo_root/target/package/linux}
mkdir -p "$package_root"
rm -rf "${package_root:?}/$package_name" "$package_root/$package_name.tar.gz"
tar -C "$staging_root" -czf "$package_root/$package_name.tar.gz" "$package_name"
mv "$root" "$package_root/$package_name"
# Lets callers outside the build environment (package-in-container.sh) find
# the package without recomputing its name.
printf '%s\n' "$package_name" >"$package_root/latest"

echo "Created $package_root/$package_name.tar.gz"
"$script_dir/verify.sh" "$package_root/$package_name"
