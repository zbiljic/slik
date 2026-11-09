#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "The macOS application bundle must be built on macOS." >&2
    exit 1
fi

for required_command in cargo codesign file install_name_tool lipo otool pkg-config; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "Required packaging command not found: $required_command" >&2
        exit 1
    fi
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

usage() {
    echo "Usage: $0 [--dist]" >&2
    echo "  --dist  build against the pinned official GStreamer SDK for distribution" >&2
}

dist=false
for argument in "$@"; do
    case "$argument" in
        --dist) dist=true ;;
        -h | --help) usage; exit 0 ;;
        *) usage; exit 2 ;;
    esac
done

if [ "$dist" = true ]; then
    # Build and bundle against the official GStreamer macOS SDK rather than a
    # package manager's copy: its libraries are relocatable and target an old
    # macOS release, whereas e.g. Homebrew bottles require the build host's
    # macOS.
    sdk_prefix=${SLIK_GSTREAMER_SDK:-$("$script_dir/gstreamer-sdk.sh")}
    if [ ! -f "$sdk_prefix/lib/pkgconfig/gstreamer-1.0.pc" ]; then
        echo "GStreamer SDK not found at $sdk_prefix" >&2
        exit 1
    fi
    SLIK_GSTREAMER_SDK=$sdk_prefix
    PKG_CONFIG_LIBDIR="$sdk_prefix/lib/pkgconfig"
    unset PKG_CONFIG_PATH
    export SLIK_GSTREAMER_SDK PKG_CONFIG_LIBDIR

    # Match the SDK's deployment target so the Rust binary does not raise the
    # bundle's minimum macOS version.
    MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
    export MACOSX_DEPLOYMENT_TARGET

    # A separate target directory keeps SDK-linked artifacts from
    # invalidating day-to-day builds that link the host's GStreamer.
    CARGO_TARGET_DIR="$repo_root/target/package/macos/cargo"
    gstreamer_source="official SDK"
else
    # Local bundle from the host's GStreamer: quick and needs no download, but
    # it inherits the host libraries' minimum macOS version.
    unset SLIK_GSTREAMER_SDK
    CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo_root/target}
    gstreamer_source="host ($(pkg-config --variable=prefix gstreamer-1.0))"
fi
export CARGO_TARGET_DIR

bundle_arch=$(uname -m)

plugin_source_dir=$(pkg-config --variable=pluginsdir gstreamer-1.0)
scanner_source_dir=$(pkg-config --variable=pluginscannerdir gstreamer-1.0)
scanner_source="$scanner_source_dir/gst-plugin-scanner"
model_source="$repo_root/crates/slik/models/nanodet-plus-m-320.onnx"
model_info_source="$model_source.modelinfo"
labels_source="$repo_root/crates/slik/src/coco.names"

for required_file in "$scanner_source" "$model_source" "$model_info_source" "$labels_source"; do
    if [ ! -f "$required_file" ]; then
        echo "Required bundle input not found: $required_file" >&2
        echo "Run 'mise run setup:model' before packaging." >&2
        exit 1
    fi
done

echo "Building release binaries..."
cargo build --locked --release -p slik --bins

launcher_source="$CARGO_TARGET_DIR/release/slik-launcher"
executable_source="$CARGO_TARGET_DIR/release/slik"
# Read the version from Cargo: the SDK-linked binary cannot run until it is
# inside the bundle with its runtime search path in place.
app_version=$(cargo pkgid --locked -p slik | sed 's/.*[#@]//')
case "$app_version" in
    '' | *[!0-9A-Za-z._-]*)
        echo "Could not determine a safe application version from '$app_version'." >&2
        exit 1
        ;;
esac

staging_root=$(mktemp -d "${TMPDIR:-/tmp}/slik-package.XXXXXX")
trap 'rm -rf "$staging_root"' EXIT HUP INT TERM

bundle="$staging_root/Slik.app"
contents="$bundle/Contents"
macos_dir="$contents/MacOS"
frameworks_dir="$contents/Frameworks"
plugins_dir="$frameworks_dir"
libraries_dir="$macos_dir"
helpers_dir="$contents/Helpers"
resources_dir="$contents/Resources"
# The launcher points GIO_MODULE_DIR at the empty gio/modules directory so
# the bundled GLib never loads the host's GIO modules.
mkdir -p "$macos_dir" "$frameworks_dir" "$plugins_dir" "$helpers_dir" "$resources_dir/gio/modules"

cp "$script_dir/Info.plist" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $app_version" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $app_version" "$contents/Info.plist"

# Copy a Mach-O file, keeping only the architecture being bundled. The SDK
# ships universal binaries, which would otherwise double the bundle size.
copy_macho() {
    cp -L "$1" "$2"
    chmod u+w "$2"
    if [ "$(lipo -archs "$2" | wc -w)" -gt 1 ]; then
        lipo "$2" -thin "$bundle_arch" -output "$2"
    fi
}

cp "$launcher_source" "$macos_dir/slik"
cp "$executable_source" "$macos_dir/slik-real"
copy_macho "$scanner_source" "$helpers_dir/gst-plugin-scanner"
cp "$model_source" "$resources_dir/"
cp "$model_info_source" "$resources_dir/"
cp "$labels_source" "$resources_dir/"
cp "$script_dir/plugins.txt" "$resources_dir/gstreamer-plugins.txt"
chmod u+w "$macos_dir/slik" "$macos_dir/slik-real" "$helpers_dir/gst-plugin-scanner"

queue_file="$staging_root/dependency-queue"
processed_file="$staging_root/dependency-processed"
: >"$queue_file"
: >"$processed_file"
printf '%s\n' "$launcher_source" "$executable_source" "$scanner_source" >>"$queue_file"

while IFS= read -r plugin_name || [ -n "$plugin_name" ]; do
    case "$plugin_name" in
        '' | \#*) continue ;;
    esac

    plugin_source="$plugin_source_dir/libgst${plugin_name}.dylib"
    if [ ! -f "$plugin_source" ]; then
        echo "Selected GStreamer plugin is unavailable: $plugin_name ($plugin_source)" >&2
        exit 1
    fi
    copy_macho "$plugin_source" "$plugins_dir/$(basename "$plugin_source")"
    printf '%s\n' "$plugin_source" >>"$queue_file"
done <"$script_dir/plugins.txt"

resolve_dependency() {
    dependency_name=$1
    source_object=$2
    dependency_suffix=${dependency_name#@loader_path/}
    dependency_basename=$(basename "$dependency_name")

    case "$dependency_name" in
        /*)
            printf '%s\n' "$dependency_name"
            return
            ;;
        @loader_path/*)
            candidate="$(dirname "$source_object")/$dependency_suffix"
            if [ -f "$candidate" ]; then
                printf '%s\n' "$candidate"
                return
            fi
            ;;
        @rpath/* | @executable_path/*)
            candidate="$(dirname "$source_object")/$dependency_basename"
            if [ -f "$candidate" ]; then
                printf '%s\n' "$candidate"
                return
            fi
            candidate="$(pkg-config --variable=libdir gstreamer-1.0)/$dependency_basename"
            if [ -f "$candidate" ]; then
                printf '%s\n' "$candidate"
                return
            fi
            ;;
    esac

    printf '%s\n' ""
}

queue_index=1
while :; do
    source_object=$(sed -n "${queue_index}p" "$queue_file")
    if [ -z "$source_object" ]; then
        break
    fi
    queue_index=$((queue_index + 1))

    if grep -Fqx "$source_object" "$processed_file"; then
        continue
    fi
    printf '%s\n' "$source_object" >>"$processed_file"

    dependency_file="$staging_root/dependencies"
    otool -L "$source_object" | awk 'NR > 1 { print $1 }' >"$dependency_file"
    while IFS= read -r dependency_name || [ -n "$dependency_name" ]; do
        case "$dependency_name" in
            /System/Library/* | /usr/lib/*) continue ;;
        esac

        dependency_source=$(resolve_dependency "$dependency_name" "$source_object")
        if [ -z "$dependency_source" ]; then
            echo "Could not resolve dependency '$dependency_name' from '$source_object'." >&2
            exit 1
        fi
        if [ ! -f "$dependency_source" ]; then
            echo "Resolved dependency does not exist: $dependency_source" >&2
            exit 1
        fi

        dependency_basename=$(basename "$dependency_source")
        if [ "$dependency_basename" = "$(basename "$source_object")" ]; then
            continue
        fi

        dependency_target="$libraries_dir/$dependency_basename"
        if [ ! -f "$dependency_target" ]; then
            copy_macho "$dependency_source" "$dependency_target"
            printf '%s\n' "$dependency_source" >>"$queue_file"
        fi
    done <"$dependency_file"
done

patch_macho() {
    macho_file=$1
    relative_rpath=$2
    dependency_file="$staging_root/patch-dependencies"
    rpath_file="$staging_root/patch-rpaths"

    chmod u+w "$macho_file"
    codesign --remove-signature "$macho_file" 2>/dev/null || true
    otool -L "$macho_file" | awk 'NR > 1 { print $1 }' >"$dependency_file"
    while IFS= read -r dependency_name || [ -n "$dependency_name" ]; do
        case "$dependency_name" in
            /System/Library/* | /usr/lib/* | @rpath/*) continue ;;
        esac
        dependency_basename=$(basename "$dependency_name")
        if [ -f "$libraries_dir/$dependency_basename" ]; then
            install_name_tool -change "$dependency_name" "@rpath/$dependency_basename" "$macho_file"
        fi
    done <"$dependency_file"

    otool -l "$macho_file" | awk '$1 == "path" { print $2 }' >"$rpath_file"
    while IFS= read -r existing_rpath || [ -n "$existing_rpath" ]; do
        case "$existing_rpath" in
            /*) install_name_tool -delete_rpath "$existing_rpath" "$macho_file" ;;
        esac
    done <"$rpath_file"

    if ! otool -l "$macho_file" | awk -v wanted="$relative_rpath" '$1 == "path" && $2 == wanted { found = 1 } END { exit !found }'; then
        install_name_tool -add_rpath "$relative_rpath" "$macho_file"
    fi

    install_id=$(otool -D "$macho_file" 2>/dev/null | sed -n '2p')
    if [ -n "$install_id" ]; then
        install_name_tool -id "@rpath/$(basename "$macho_file")" "$macho_file"
    fi
}

macho_list="$staging_root/macho-files"
: >"$macho_list"
find "$bundle" -type f -print | while IFS= read -r bundle_file; do
    if file "$bundle_file" | grep -q 'Mach-O'; then
        printf '%s\n' "$bundle_file"
    fi
done >"$macho_list"

while IFS= read -r macho_file || [ -n "$macho_file" ]; do
    case "$macho_file" in
        "$macos_dir"/*) relative_rpath='@loader_path' ;;
        "$helpers_dir"/*) relative_rpath='@loader_path/../MacOS' ;;
        "$frameworks_dir"/*) relative_rpath='@loader_path/../MacOS' ;;
        *)
            echo "Unexpected Mach-O location: $macho_file" >&2
            exit 1
            ;;
    esac
    patch_macho "$macho_file" "$relative_rpath"
done <"$macho_list"

minimum_system_version=$(
    while IFS= read -r macho_file || [ -n "$macho_file" ]; do
        otool -l "$macho_file" | awk '
            $1 == "cmd" {
                in_version = ($2 == "LC_BUILD_VERSION" || $2 == "LC_VERSION_MIN_MACOSX")
            }
            in_version && ($1 == "minos" || $1 == "version") {
                print $2
                exit
            }
        '
    done <"$macho_list" | awk -F. '
        {
            major = $1 + 0
            minor = $2 + 0
            patch = $3 + 0
            if (!found || major > max_major \
                || (major == max_major && minor > max_minor) \
                || (major == max_major && minor == max_minor && patch > max_patch)) {
                found = 1
                max_major = major
                max_minor = minor
                max_patch = patch
                selected = $0
            }
        }
        END {
            if (found) print selected
        }
    '
)
if [ -n "$minimum_system_version" ]; then
    /usr/libexec/PlistBuddy \
        -c "Set :LSMinimumSystemVersion $minimum_system_version" \
        "$contents/Info.plist"
fi

runtime_manifest="$resources_dir/runtime-manifest.txt"
{
    printf 'Slik %s\n' "$app_version"
    printf 'GStreamer %s\n' "$(pkg-config --modversion gstreamer-1.0)"
    printf 'GStreamer source: %s\n' "$gstreamer_source"
    printf 'Minimum macOS %s\n' "${minimum_system_version:-unknown}"
    printf '\nBundled Mach-O files:\n'
    while IFS= read -r macho_file || [ -n "$macho_file" ]; do
        printf '%s\n' "${macho_file#"$contents/"}"
    done <"$macho_list"
} >"$runtime_manifest"

signing_identity=${SLIK_CODESIGN_IDENTITY:--}
sign_macho() {
    macho_file=$1
    if [ "$signing_identity" = "-" ]; then
        codesign --force --sign - --timestamp=none "$macho_file"
    else
        codesign --force --sign "$signing_identity" --options runtime --timestamp "$macho_file"
    fi
}

while IFS= read -r macho_file || [ -n "$macho_file" ]; do
    case "$macho_file" in
        "$macos_dir/slik" | "$macos_dir/slik-real") continue ;;
    esac
    sign_macho "$macho_file"
done <"$macho_list"
sign_macho "$macos_dir/slik-real"
sign_macho "$macos_dir/slik"

if [ "$signing_identity" = "-" ]; then
    codesign --force --sign - --timestamp=none "$bundle"
else
    codesign --force --sign "$signing_identity" --options runtime --timestamp "$bundle"
fi

package_root="$repo_root/target/package/macos"
output_bundle="$package_root/Slik.app"
mkdir -p "$package_root"
rm -rf "$output_bundle"
mv "$bundle" "$output_bundle"

echo "Created $output_bundle"
"$script_dir/verify.sh" "$output_bundle"
