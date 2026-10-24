#!/usr/bin/env bash
# Builds skreenshare-proektor.exe on Linux (cross-compile with cargo-xwin). One-time setup:
#   rustup target add x86_64-pc-windows-msvc && cargo install cargo-xwin
#   apt install clang lld llvm libclang-dev nasm unzip curl
set -euo pipefail
cd "$(dirname "$0")/.."
if [ ! -d third_party/ffmpeg/bin ]; then
  mkdir -p third_party && cd third_party
  curl -L -o ffmpeg.zip https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip
  unzip -q ffmpeg.zip && mv ffmpeg-n9.0-latest-win64-lgpl-shared-9.0 ffmpeg && rm ffmpeg.zip
  cd ..
  tools/make-import-libs.sh
fi
export LIBCLANG_PATH="${LIBCLANG_PATH:-$(dirname "$(ls /usr/lib/llvm-*/lib/libclang*.so* | head -1)")}"
export XWIN_ACCEPT_LICENSE=1
# cargo-xwin ignores [target] rustflags from .cargo/config.toml, so pass the static CRT explicitly
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static"
cargo xwin build --release --target x86_64-pc-windows-msvc
mkdir -p dist && cp target/x86_64-pc-windows-msvc/release/skreenshare-proektor.exe dist/
echo "dist/skreenshare-proektor.exe"
