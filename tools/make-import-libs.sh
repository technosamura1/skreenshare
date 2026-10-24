#!/usr/bin/env bash
# Linux/macOS: MSVC-style import libraries for the FFmpeg DLLs in third_party/ffmpeg (needs llvm-dlltool).
# BtbN ships GNU import libs, which MSVC's /DELAYLOAD ignores; delay-loading is what lets the exe carry FFmpeg inside.
set -euo pipefail
ff="$(cd "$(dirname "$0")/.." && pwd)/third_party/ffmpeg"
for def in "$ff"/lib/*.def; do
  name="$(basename "$def" .def)"          # avcodec-63
  base="${name%-*}"                        # avcodec
  llvm-dlltool -m i386:x86-64 -d "$def" -D "$name.dll" -l "$ff/lib/$base.lib"
  echo "$name.dll -> lib/$base.lib"
done
