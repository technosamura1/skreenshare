#!/usr/bin/env bash
# Builds the FFmpeg that the exe bundles (third_party/ffmpeg) on Linux, cross-compiled with mingw-w64:
#   - FFmpeg n9.0.2, only what screenshare uses: HEVC/H.264 decoders (+D3D11VA/DXVA2), hevc_nvenc, h264_nvenc, libx264;
#   - nv-codec-headers n11.1.5.3 => NVENC API 11.1 => works with NVIDIA drivers from 471.41 (the newest headers
#     demanded a 2026 driver and failed on older ones with "Function not implemented");
#   - x264 linked statically (GPL: the resulting DLLs are GPL, see third_party/ffmpeg/LICENSE.txt).
# Needs: gcc-mingw-w64-x86-64-win32 binutils-mingw-w64-x86-64 nasm make git pkg-config.
# Usage: tools/build-ffmpeg.sh [workdir]   -> result in <workdir>/out/{bin,lib,include}; copy to third_party/ffmpeg.
set -euo pipefail
W=${1:-$PWD/ffbuild}; P=$W/prefix; mkdir -p "$W" "$P"; cd "$W"
git clone -q --depth 1 --branch n11.1.5.3 https://github.com/FFmpeg/nv-codec-headers.git || true
git clone -q --depth 1 --branch n9.0.2 https://github.com/FFmpeg/FFmpeg.git ffmpeg || true
git clone -q --depth 1 --branch stable https://code.videolan.org/videolan/x264.git || true
make -C nv-codec-headers PREFIX="$P" install
( cd x264 && ./configure --host=x86_64-w64-mingw32 --cross-prefix=x86_64-w64-mingw32- --prefix="$P" --enable-static \
    --disable-cli --disable-opencl --disable-avs --disable-swscale --disable-lavf --disable-ffms --disable-gpac \
    --disable-lsmash --bit-depth=8 --chroma-format=420 && make -j"$(nproc)" && make install )
export PKG_CONFIG_PATH=$P/lib/pkgconfig
( cd ffmpeg && ./configure --prefix="$W/out" --arch=x86_64 --target-os=mingw32 --cross-prefix=x86_64-w64-mingw32- \
    --pkg-config=pkg-config --pkg-config-flags=--static --enable-shared --disable-static --disable-programs \
    --disable-doc --disable-debug --disable-everything --disable-avdevice --disable-avfilter --disable-network \
    --disable-pthreads --enable-w32threads --enable-gpl --enable-libx264 --enable-ffnvcodec --enable-nvenc \
    --enable-d3d11va --enable-dxva2 --enable-decoder=hevc,h264 --enable-parser=hevc,h264 \
    --enable-encoder=hevc_nvenc,h264_nvenc,libx264 \
    --enable-hwaccel=hevc_d3d11va,hevc_d3d11va2,hevc_dxva2,h264_d3d11va,h264_d3d11va2,h264_dxva2 \
    --extra-cflags="-I$P/include" --extra-ldflags="-L$P/lib -static-libgcc" \
  && make -j"$(nproc)" && make install )
echo "готово: $W/out (скопируй bin, lib, include в third_party/ffmpeg, затем tools/make-import-libs.sh — иначе exe не запустится)"
