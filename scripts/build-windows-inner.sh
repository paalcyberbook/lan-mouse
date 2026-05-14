#!/usr/bin/env bash
#
# Inner script invoked by the Windows cross-compile container.
# Lives in the project (mounted at /build inside the container) so edits
# don't require rebuilding the toolchain image — only `Dockerfile.windows`
# changes need `make windows-rebuild`.
#
# Container expectations: WORKDIR is /build, the mingw toolchain is on
# PATH, unix2dos + zip + makensis are installed.

set -euo pipefail

export PKG_CONFIG_PATH=""
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH_x86_64_pc_windows_gnu=/usr/x86_64-w64-mingw32/sys-root/mingw/lib/pkgconfig/:/usr/x86_64-w64-mingw32/lib/pkgconfig/

cargo build --release --target x86_64-pc-windows-gnu -p lan-mouse -p lan-mouse-launcher

rm -rf /output
mkdir -p /output/bin

cp target/x86_64-pc-windows-gnu/release/lan-mouse.exe /output/bin/
cp /usr/x86_64-w64-mingw32/sys-root/mingw/bin/*.dll /output/bin/ 2>/dev/null || true
cp target/x86_64-pc-windows-gnu/release/launch-lan-mouse.exe /output/
cp scripts/register-logger.cmd /output/
unix2dos /output/register-logger.cmd

cat > /output/README.txt <<'EOF'
Lan Mouse — portable build

Double-click launch-lan-mouse.exe to start.
The main binary and its DLLs live under the bin\ folder.

Debugging across hosts? Double-click register-logger.cmd to ship
logs to cb-logger so they can be merged with logs from your Linux
and macOS hosts.
EOF
unix2dos /output/README.txt

rm -f /build/lan-mouse-windows-x86_64.zip /build/lan-mouse-windows-x86_64-installer.exe
(cd /output && zip -r /build/lan-mouse-windows-x86_64.zip .)
echo '==> Portable zip built: /build/lan-mouse-windows-x86_64.zip'

makensis \
    -DSTAGE_DIR=/output \
    -DOUT_FILE=/build/lan-mouse-windows-x86_64-installer.exe \
    scripts/lan-mouse.nsi
echo '==> Installer built: /build/lan-mouse-windows-x86_64-installer.exe'
