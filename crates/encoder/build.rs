//! Static FFmpeg on Windows: ffmpeg-sys-next links only the FFmpeg libraries
//! themselves, not what they were built against (x264, oneVPL, Media
//! Foundation/COM system libraries). Those are listed in the `Libs:` lines of
//! FFmpeg's pkg-config files, which vcpkg installs next to the libraries, so
//! link exactly what this FFmpeg build needs.

use std::env;
use std::fs;
use std::path::PathBuf;

const FFMPEG_LIBS: [&str; 7] = ["avcodec", "avdevice", "avfilter", "avformat", "avutil", "swresample", "swscale"];

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    println!("cargo:rerun-if-env-changed=VCPKGRS_TRIPLET");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") || env::var_os("CARGO_FEATURE_STATIC").is_none() {
        return;
    }
    let prefix = match (env::var_os("FFMPEG_DIR"), env::var_os("VCPKG_ROOT")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(root)) => {
            let triplet = env::var("VCPKGRS_TRIPLET").unwrap_or_else(|_| "x64-windows-static-md".into());
            PathBuf::from(root).join("installed").join(triplet)
        }
        (None, None) => panic!("static FFmpeg on Windows needs FFMPEG_DIR or VCPKG_ROOT (see scripts/windows-env.ps1)"),
    };
    let lib_dir = prefix.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    let mut linked: Vec<String> = Vec::new();
    for pc in ["libavcodec", "libavformat", "libavutil", "libswscale"] {
        let path = lib_dir.join("pkgconfig").join(format!("{pc}.pc"));
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let libs = text.lines().filter(|l| l.starts_with("Libs:") || l.starts_with("Libs.private:"));
        for token in libs.flat_map(|l| l.split_once(':').unwrap().1.split_whitespace()) {
            let Some(name) = token.trim_matches('"').strip_prefix("-l") else { continue };
            if FFMPEG_LIBS.contains(&name) || linked.iter().any(|l| l == name) {
                continue;
            }
            println!("cargo:rustc-link-lib={name}");
            linked.push(name.to_string());
        }
    }
}
