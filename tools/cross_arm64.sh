#!/usr/bin/env bash
# Build Debian 12 aarch64 release on a glibc builder. Do not run this on the target device.
set -euo pipefail
# shellcheck disable=SC1091
if [ -f "$HOME/.cargo/env" ]; then
  . "$HOME/.cargo/env"
fi
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
export CFLAGS_aarch64_unknown_linux_gnu="-O2 -fPIC"
export CARGO_TERM_COLOR=never
echo "host=$(uname -m) $(uname -s)"
echo "rustc=$(rustc -vV | awk '/^release:/{print $2}')"
echo "cc=$(aarch64-linux-gnu-gcc --version | head -n1)"
echo "glibc_cross=2.35 (Ubuntu 22.04; expected OK on Debian 12 glibc 2.36)"
echo "root=$ROOT"
cargo build --release --locked --target aarch64-unknown-linux-gnu
BIN="$ROOT/target/aarch64-unknown-linux-gnu/release/aegis-observatory"
file "$BIN" || true
ls -l "$BIN"
sha256sum "$BIN"
aarch64-linux-gnu-readelf -d "$BIN" | awk '/NEEDED|RPATH|RUNPATH/'
echo BUILD_OK
