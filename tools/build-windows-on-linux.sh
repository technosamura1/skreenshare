#!/usr/bin/env bash
# Builds skreenshare-proektor.exe on Linux (cross-compile with cargo-xwin). One-time setup:
#   rustup target add x86_64-pc-windows-msvc && cargo install cargo-xwin
#   apt install clang lld llvm libclang-dev nasm unzip curl
set -euo pipefail
cd "$(dirname "$0")/.."
if [ ! -d third_party/ffmpeg/bin ]; then
  echo "нет third_party/ffmpeg: соберите его через tools/build-ffmpeg.sh (slim FFmpeg n9.0.2 с NVENC-заголовками n11.1.5.3, нужен NVIDIA-драйвер >= 471.41)" >&2
  exit 1
fi
export LIBCLANG_PATH="${LIBCLANG_PATH:-$(dirname "$(ls /usr/lib/llvm-*/lib/libclang*.so* | head -1)")}"
export XWIN_ACCEPT_LICENSE=1
# cargo-xwin ignores [target] rustflags from .cargo/config.toml, so pass the static CRT explicitly
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static"
cargo xwin build --release --target x86_64-pc-windows-msvc
mkdir -p dist && cp target/x86_64-pc-windows-msvc/release/skreenshare-proektor.exe dist/
echo "dist/skreenshare-proektor.exe"
