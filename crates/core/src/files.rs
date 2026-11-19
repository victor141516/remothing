//! Files between the client's computer and the session. A file dropped on
//! or pasted into the page is saved in the session user's Downloads folder
//! (`POST /files`); a file copied on the session can be downloaded from the
//! page (`GET /files/clipboard`). Plain HTTP beside the stream sockets: a
//! file needs no low latency, must not hold up frames or input, and the
//! browser shows a download's progress itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::app::App;

/// Longest name kept for a received file (bytes): every file system takes it.
const MAX_NAME: usize = 200;
/// A download is read and sent in pieces of this size.
const READ_PIECE: usize = 256 * 1024;
/// Names Windows keeps for devices, with any extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Deserialize)]
pub struct FileQuery {
    #[serde(default)]
    token: String,
    /// The page's session key: the display its stream socket shows.
    #[serde(default)]
    session: String,
    /// Upload: the file's name. Download: the name expected at `index`.
    #[serde(default)]
    name: String,
    #[serde(default)]
    index: usize,
}

/// `POST /files?token&name`: the body is the file. Answers where it went.
pub async fn upload(State(app): State<Arc<App>>, Query(q): Query<FileQuery>, body: Body) -> Response {
    if !crate::stream::token_ok(&app, &q.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match save(&app, &q.name, body).await {
        Ok(path) => {
            eprintln!("file received from a client: {}", path.display());
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = path.parent().map(|d| d.display().to_string()).unwrap_or_default();
            Json(json!({ "name": name, "dir": dir })).into_response()
        }
        Err(e) => {
            app.telemetry.error("file from a client", &e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}

/// Writes the body under a name of its own (`name.part`), then gives it the
/// first free name: a half-received file never looks complete.
async fn save(app: &App, name: &str, body: Body) -> Result<PathBuf> {
    let name = safe_name(name);
    let platform = app.platform.clone();
    let dir = tokio::task::spawn_blocking(move || platform.downloads_dir()).await??;
    let (path, part) = reserve(&dir, &name).await?;
    let written = async {
        let mut file = tokio::fs::File::create(&part).await.with_context(|| format!("create {}", part.display()))?;
        let mut stream = body.into_data_stream();
        while let Some(piece) = stream.next().await {
            file.write_all(&piece.context("the upload broke off")?).await.context("write the file")?;
        }
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&part, &path).await.context("rename the received file")?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    let platform = app.platform.clone();
    let given = path.clone();
    tokio::task::spawn_blocking(move || platform.hand_over(&given)).await??;
    Ok(path)
}

/// Takes the first free name, "name", "name (1)", ..., by creating it empty
/// (so two uploads of the same name never pick the same one), and the
/// `.part` path to write into meanwhile.
async fn reserve(dir: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    let (stem, ext) = match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    for n in 0..1000 {
        let candidate = if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(&candidate);
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await {
            Ok(_) => return Ok((path, dir.join(format!("{candidate}.part")))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create a file in {}", dir.display())),
        }
    }
    bail!("{} already holds 1000 files named like {name}", dir.display())
}

/// A file name of its own from what the browser gave: no folders, nothing
/// Windows refuses, never empty.
fn safe_name(given: &str) -> String {
    let last = given.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut name: String = last
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    // Windows drops trailing dots and spaces, which would make another name.
    name.truncate(name.trim_end_matches(['.', ' ']).len());
    let name = name.trim_start().to_string();
    let mut name = if name.is_empty() { "file".to_string() } else { name };
    let base = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    if RESERVED.contains(&base.as_str()) {
        name.insert(0, '_');
    }
    if name.len() > MAX_NAME {
        let mut end = MAX_NAME;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
    }
    name
}

/// `GET /files/clipboard?token&session&index&name`: a file on the clipboard
/// of the display that page shows. Only what is on the clipboard right now
/// can be read this way, and only by index and name together, so a list
/// that changed meanwhile never sends another file.
pub async fn clipboard_file(State(app): State<Arc<App>>, Query(q): Query<FileQuery>) -> Response {
    if !crate::stream::token_ok(&app, &q.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let route = app.input_routes.lock().unwrap().get(&q.session).cloned();
    let Some(watching) = route else { return (StatusCode::NOT_FOUND, "no such page").into_response() };
    let files = watching.read().unwrap().control.clipboard_files().borrow().clone();
    let Some(path) = files.get(q.index).filter(|p| display_name(p) == q.name).cloned() else {
        return (StatusCode::NOT_FOUND, "that file is no longer on the clipboard").into_response();
    };
    match open(&path).await {
        Ok((file, size)) => {
            let body = Body::from_stream(futures_util::stream::unfold(Some(file), |file| async move {
                let mut file = file?;
                let mut piece = vec![0; READ_PIECE];
                match file.read(&mut piece).await {
                    Ok(0) => None,
                    Ok(n) => {
                        piece.truncate(n);
                        Some((Ok(Bytes::from(piece)), Some(file)))
                    }
                    Err(e) => Some((Err(e), None)),
                }
            }));
            let disposition = format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", ascii_name(&q.name), percent(&q.name));
            let mut response = body.into_response();
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            if let Ok(value) = HeaderValue::from_str(&disposition) {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(e) => {
            app.telemetry.error("file to a client", &e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}

async fn open(path: &Path) -> Result<(tokio::fs::File, u64)> {
    let file = tokio::fs::File::open(path).await.with_context(|| format!("open {}", path.display()))?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        bail!("{} is a folder", path.display());
    }
    Ok((file, meta.len()))
}

/// The clipboard's files as S_CLIPBOARD_FILES lists them.
pub fn listing(files: &[PathBuf]) -> serde_json::Value {
    let files: Vec<_> = files
        .iter()
        .map(|path| {
            let meta = std::fs::metadata(path).ok();
            json!({
                "name": display_name(path),
                "size": meta.as_ref().map_or(0, |m| m.len()),
                "folder": meta.is_some_and(|m| m.is_dir()),
            })
        })
        .collect();
    json!({ "files": files })
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The path of a `file://` URI (`text/uri-list`, macOS file URLs); None for
/// another scheme, another host or a path that is not UTF-8.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    let path = match rest.find('/')? {
        0 => rest,
        i if &rest[..i] == "localhost" => &rest[i..],
        _ => return None,
    };
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// For browsers that only read the plain `filename` parameter.
fn ascii_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect()
}

/// RFC 5987 encoding of `filename*`.
fn percent(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_in_the_folder() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Windows\\..\\evil.exe"), "evil.exe");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("dir/"), "file");
    }

    #[test]
    fn names_windows_accepts() {
        assert_eq!(safe_name("con.txt"), "_con.txt");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name("report. . "), "report");
        assert_eq!(safe_name("informe ñ.pdf"), "informe ñ.pdf");
        assert!(safe_name(&"é".repeat(300)).len() <= MAX_NAME);
    }

    #[test]
    fn file_uris() {
        assert_eq!(file_uri_path("file:///home/user/Mi%20archivo%C3%B1.txt"), Some(PathBuf::from("/home/user/Mi archivoñ.txt")));
        assert_eq!(file_uri_path("file://localhost/tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(file_uri_path("file://other/tmp/a"), None);
        assert_eq!(file_uri_path("http://x/a"), None);
    }
}
