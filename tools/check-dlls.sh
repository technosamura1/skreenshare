#!/usr/bin/env bash
# CI-style check of the DLL dependencies of the release exe. Fails (exit 1) when
#   - the exe imports a bundled FFmpeg DLL statically (it must be delay-loaded: the DLLs are unpacked at run time,
#     a static import makes Windows refuse to start the exe: "не обнаружила avcodec-63.dll"),
#   - the exe delay-loads a DLL that is not in the bundle (except avformat, linked but never called),
#   - the exe or a bundled DLL imports something that is neither bundled nor a known Windows system DLL
#     (e.g. libx264-*.dll, libwinpthread-1.dll, vcruntime140.dll, which a clean Windows does not have).
# Usage: tools/check-dlls.sh [exe] [ffmpeg-dir]
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
EXE=${1:-$ROOT/target/x86_64-pc-windows-msvc/release/screenshare.exe}
FF=${2:-$ROOT/third_party/ffmpeg}
NEEDED='avcodec- avutil- swscale- swresample-'     # keep in sync with build.rs
# DLLs present on every Windows 10/11 (api-ms-win-* handled by pattern below).
SYSTEM=" kernel32 kernelbase user32 gdi32 advapi32 shell32 ole32 oleaut32 combase ws2_32 winmm imm32 dwmapi dxgi d3d11 \
mmdevapi avrt bcrypt bcryptprimitives ntdll msvcrt ucrtbase opengl32 uxtheme comctl32 comdlg32 crypt32 secur32 shlwapi \
userenv version setupapi iphlpapi dbghelp d3dcompiler_47 windowscodecs propsys dnsapi mswsock rpcrt4 sechost win32u \
dcomp dwrite d2d1 mf mfplat mfreadwrite mfuuid wtsapi32 cfgmgr32 powrprof netapi32 "
fail=0
imports() { # kind(Import|DelayImport) file
    llvm-readobj --coff-imports "$2" | awk -v k="$1" '$0 ~ "^"k" \\{"{f=1} f && /Name:/ {print $2; f=0}' ; }
[ -f "$EXE" ] || { echo "нет $EXE"; exit 1; }
bundle=()
for p in $NEEDED; do
    f=$(ls "$FF"/bin/${p}*.dll 2>/dev/null | head -1) || true
    [ -n "$f" ] && bundle+=("$(basename "$f")") || { echo "FAIL: в $FF/bin нет $p*.dll"; fail=1; }
done
inbundle() { local n=${1,,}; for b in "${bundle[@]}"; do [ "${b,,}" = "$n" ] && return 0; done; return 1; }
issystem() { local n=${1,,}; n=${n%.dll}; [[ $n == api-ms-win-* || $n == ext-ms-win-* ]] && return 0; [[ "$SYSTEM" == *" $n "* ]]; }
echo "в комплекте: ${bundle[*]}"
for d in $(imports Import "$EXE" | sort -u); do
    if inbundle "$d"; then echo "FAIL: exe импортирует $d статически (нужен /DELAYLOAD, MSVC-библиотеки: tools/make-import-libs.sh)"; fail=1
    elif ! issystem "$d"; then echo "FAIL: exe импортирует неизвестную DLL $d"; fail=1; fi
done
delayed=$(imports DelayImport "$EXE" | sort -u)
for d in $delayed; do
    inbundle "$d" || issystem "$d" || [[ ${d,,} == avformat-* ]] || { echo "FAIL: exe загружает по требованию $d, которой нет в комплекте"; fail=1; }
done
for need in "${bundle[@]}"; do
    case ${need,,} in swresample-*) continue;; esac     # only avcodec uses it
    grep -qix "$need" <<<"$delayed" || { echo "FAIL: $need не в списке отложенных импортов exe"; fail=1; }
done
for b in "${bundle[@]}"; do
    for d in $(imports Import "$FF/bin/$b" | sort -u); do
        inbundle "$d" || issystem "$d" || { echo "FAIL: $b импортирует $d: нет ни в комплекте, ни в системе"; fail=1; }
    done
done
[ $fail = 0 ] && echo "OK: все импорты exe и DLL найдутся (комплект или Windows)."
exit $fail
