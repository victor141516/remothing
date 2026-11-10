//! Web clients embedded in the binary (built into client/dist by `npm run build`).

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../client/dist"]
struct Dist;

fn serve(path: &str) -> Response {
    match Dist::get(path) {
        Some(file) => ([(header::CONTENT_TYPE, file.metadata.mimetype().to_string())], file.data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Streaming client: everything except the admin UI.
pub async fn stream_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.starts_with("admin") {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve(path)
}

/// Admin UI: `admin.html` at the root plus its bundle.
pub async fn admin_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "admin.html" } else { path };
    if !path.starts_with("admin") {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve(path)
}
