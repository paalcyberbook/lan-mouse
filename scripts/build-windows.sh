#!/usr/bin/env bash
#
# Cross-compile lan-mouse for Windows using Docker.
#
# Usage:
#   ./scripts/build-windows.sh              # Build (creates toolchain image if needed)
#   ./scripts/build-windows.sh --rebuild    # Force rebuild the toolchain image
#   ./scripts/build-windows.sh --clean      # Remove toolchain image
#
# The toolchain image (GTK4, libadwaita, Rust for mingw) is built once and reused.
# Only the lan-mouse source is mounted and compiled on each run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
IMAGE_NAME="lan-mouse-windows-toolchain"

cd "$PROJECT_DIR"

case "${1:-}" in
    --clean)
        echo "==> Removing toolchain image..."
        docker rmi "$IMAGE_NAME" 2>/dev/null || true
        echo "Done."
        exit 0
        ;;
    --rebuild)
        echo "==> Forcing toolchain image rebuild..."
        docker rmi "$IMAGE_NAME" 2>/dev/null || true
        ;;
esac

# Build toolchain image if it doesn't exist
if ! docker image inspect "$IMAGE_NAME" &>/dev/null; then
    echo "==> Building Windows cross-compilation toolchain image (one-time)..."
    echo "    This builds GTK4 + libadwaita + Rust for mingw. Takes a while."
    docker build -f Dockerfile.windows -t "$IMAGE_NAME" .
    echo "==> Toolchain image ready."
else
    echo "==> Using cached toolchain image: $IMAGE_NAME"
fi

# Run the build by mounting source and cargo cache into the container
echo "==> Compiling lan-mouse for Windows..."
docker run --rm \
    -v "${PROJECT_DIR}:/build:z" \
    -v "lan-mouse-cargo-cache:/root/.cargo/registry" \
    "$IMAGE_NAME"

echo "==> Done!"
ls -lh lan-mouse-windows-x86_64.zip lan-mouse-windows-x86_64-installer.exe 2>/dev/null \
    || echo "Check output above for errors."
