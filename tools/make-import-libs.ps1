# Regenerates MSVC-format import libraries for the FFmpeg DLLs in third_party/ffmpeg.
# BtbN builds ship GNU (dlltool) import libs; MSVC's /DELAYLOAD ignores those (LNK4199), and delay-loading
# is what lets screenshare.exe carry FFmpeg inside itself. Run once after unpacking a new FFmpeg build.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$ff = Join-Path $root "third_party\ffmpeg"
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$tools = Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\x64" | Sort-Object Name | Select-Object -Last 1
$dumpbin = Join-Path $tools.FullName "dumpbin.exe"
$lib = Join-Path $tools.FullName "lib.exe"

foreach ($dll in Get-ChildItem "$ff\bin\*.dll") {
    $base = ($dll.BaseName -split '-')[0]          # avcodec-62 -> avcodec
    $exports = & $dumpbin /nologo /exports $dll.FullName |
        Where-Object { $_ -match '^\s+\d+\s+[0-9A-F]+\s+[0-9A-F]{8}\s+(\S+)' } |
        ForEach-Object { $Matches[1] }
    $def = Join-Path $env:TEMP "$base.def"
    @("LIBRARY $($dll.Name)", "EXPORTS") + $exports | Set-Content $def -Encoding ascii
    & $lib /nologo /machine:x64 /def:$def /out:"$ff\lib\$base.lib" | Out-Null
    Remove-Item $def, "$ff\lib\$base.exp" -ErrorAction SilentlyContinue
    "{0,-16} {1,5} exports -> lib\{2}.lib" -f $dll.Name, $exports.Count, $base
}
