# Dot-source before `cargo build` on Windows: . scripts/windows-env.ps1
# Static FFmpeg + static CRT (see .cargo/config.toml): remothing.exe is self-contained.
$root = Split-Path -Parent $PSScriptRoot
$env:LIBCLANG_PATH = Join-Path $root '.tools/libclang'
$env:VCPKG_ROOT = Join-Path $root '.tools/vcpkg'
$env:VCPKGRS_TRIPLET = 'x64-windows-static'
$env:FFMPEG_DIR = Join-Path $root '.tools/vcpkg/installed/x64-windows-static'
