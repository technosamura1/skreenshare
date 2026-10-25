#!/usr/bin/env bash
# Linux counterpart of make-import-libs.ps1: (re)creates MSVC-format import libraries for the FFmpeg DLLs in
# third_party/ffmpeg from the .def files of the mingw build (llvm-dlltool writes "short import" libraries).
#
# Why this matters: mingw's libav*.dll.a are GNU import libs; the linker ignores /DELAYLOAD for them, so the exe
# imported avcodec-63.dll statically and Windows refused to start it ("система не обнаружила avcodec-63.dll")
# before our code could unpack the bundled DLLs. With these libs the imports are delay-loaded (see build.rs).
# Run after every new FFmpeg build (tools/build-ffmpeg.sh), then check with tools/check-dlls.sh.
set -euo pipefail
FF=${1:-$(cd "$(dirname "$0")/.." && pwd)/third_party/ffmpeg}
cd "$FF"
mkdir -p lib/gnu-import-libs
for f in lib/lib*.dll.a; do [ -e "$f" ] && mv "$f" lib/gnu-import-libs/; done   # must not shadow the .lib files
rm -f bin/*.lib                                                                 # stale copies (other format)
for dll in bin/*.dll; do
    name=$(basename "$dll"); base=${name%%-*}
    def=lib/${name%.dll}.def
    [ -f "$def" ] || { echo "нет $def для $name" >&2; exit 1; }
    llvm-dlltool -m i386:x86-64 -D "$name" -d "$def" -l "lib/$base.lib"
    echo "$name -> lib/$base.lib ($(grep -c '^ ' "$def") экспортов)"
done
