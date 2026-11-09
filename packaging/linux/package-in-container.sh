#!/bin/sh
# Run package.sh --dist inside the pinned Ubuntu 26.04 build container, then
# verify the result again in the bare base image.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
container_tool=${SLIK_CONTAINER_TOOL:-docker}

if ! command -v "$container_tool" >/dev/null 2>&1; then
    echo "Container tool not found: $container_tool (set SLIK_CONTAINER_TOOL to override)" >&2
    exit 1
fi

# Use the same Rust toolchain as local development.
rust_version=$(sed -n 's/^rust = { version = "\([^"]*\)".*/\1/p' "$repo_root/mise.toml")
if [ -z "$rust_version" ]; then
    echo "Could not read the Rust version from mise.toml." >&2
    exit 1
fi

image="slik-package-linux:rust-$rust_version"
echo "Preparing build image $image..."
"$container_tool" build \
    --build-arg "RUST_VERSION=$rust_version" \
    --tag "$image" \
    --file "$script_dir/Dockerfile" \
    "$script_dir"

# Output and build state live under target/package/linux so they are owned by
# the invoking user and survive between runs. The directory is mounted on its
# own at /out, by physical path, because target/ itself may be a symlink that
# does not resolve inside the container.
output_dir="$repo_root/target/package/linux"
mkdir -p "$output_dir/container/cargo-home" "$output_dir/container/home"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd -P)

rm -f "$output_dir/latest"
"$container_tool" run --rm \
    --user "$(id -u):$(id -g)" \
    --volume "$repo_root:/src" \
    --volume "$output_dir:/out" \
    --workdir /src \
    --env SLIK_IN_PACKAGE_CONTAINER=1 \
    --env SLIK_PACKAGE_DIR=/out \
    --env SLIK_VERIFY_FIXTURES=/out/container/fixtures \
    --env HOME=/out/container/home \
    --env CARGO_HOME=/out/container/cargo-home \
    --env CARGO_TARGET_DIR=/out/container/target \
    "$image" \
    ./packaging/linux/package.sh --dist

# Verify again in the bare base image, which has no GStreamer, no build tools,
# and nothing else the package could accidentally rely on. The fixtures made
# by the first verification are reused.
base_image=$(sed -n 's/^FROM \([^ ]*\).*/\1/p' "$script_dir/Dockerfile" | head -n 1)
package_name=$(cat "$output_dir/latest" 2>/dev/null || true)
if [ -z "$package_name" ] || [ ! -d "$output_dir/$package_name" ]; then
    echo "Could not find the package that was just built." >&2
    exit 1
fi
echo "Verifying $package_name in the bare base image..."
exec "$container_tool" run --rm \
    --user "$(id -u):$(id -g)" \
    --volume "$repo_root:/src:ro" \
    --volume "$output_dir:/out:ro" \
    --tmpfs /tmp:exec \
    --env SLIK_VERIFY_FIXTURES=/out/container/fixtures \
    "$base_image" \
    /src/packaging/linux/verify.sh "/out/$package_name"
