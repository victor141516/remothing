# Fetches the native build dependencies for the Windows binary into .tools/:
#   - libclang.dll (bindgen, used by ffmpeg-sys-next), taken from the `libclang` wheel
#   - vcpkg + static FFmpeg (NVENC, AMF, QuickSync, x264) linked into remothing.exe
# Afterwards build with scripts/windows-env.ps1 loaded.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$tools = Join-Path $root '.tools'
New-Item -ItemType Directory -Force $tools | Out-Null

$clangDir = Join-Path $tools 'libclang'
if (-not (Test-Path (Join-Path $clangDir 'libclang.dll'))) {
    $tmp = Join-Path $tools 'wheel'
    python -m pip download libclang==18.1.1 --only-binary=:all: --platform win_amd64 --no-deps -d $tmp
    $whl = Get-ChildItem $tmp -Filter 'libclang-*.whl' | Select-Object -First 1
    New-Item -ItemType Directory -Force (Join-Path $tmp 'x') | Out-Null
    tar -xf $whl.FullName -C (Join-Path $tmp 'x')
    New-Item -ItemType Directory -Force $clangDir | Out-Null
    $dll = Get-ChildItem (Join-Path $tmp 'x') -Recurse -Filter 'libclang.dll' | Select-Object -First 1
    Copy-Item $dll.FullName $clangDir
    Remove-Item -Recurse -Force $tmp
}

$vcpkg = Join-Path $tools 'vcpkg'
if (-not (Test-Path (Join-Path $vcpkg 'vcpkg.exe'))) {
    if (-not (Test-Path $vcpkg)) { git clone --depth 1 https://github.com/microsoft/vcpkg $vcpkg }
    & (Join-Path $vcpkg 'bootstrap-vcpkg.bat') -disableMetrics
}
& (Join-Path $vcpkg 'vcpkg.exe') install `
    'ffmpeg[core,avcodec,avformat,swscale,nvcodec,amf,qsv,x264,gpl]:x64-windows-static-md' `
    --clean-after-build
