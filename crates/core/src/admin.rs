//! Admin API (JSON) behind token/password login. Served on its own listener.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::app::{App, VirtualRequest};
use crate::config::{constant_time_eq, hash_password, random_token, verify_password};
use crate::platform::DisplayMode;
use crate::shared::Tuning;

const COOKIE: &str = "rt_admin";
/// Values of the codec setting (see `EncoderSettings::codec`).
const CODECS: [&str; 4] = ["auto", "h264", "hevc", "av1"];

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/status", get(status))
        .route("/api/displays", get(displays))
        .route("/api/displays/select", post(select_display))
        .route("/api/virtual", post(create_virtual))
        .route("/api/virtual/install", post(install_virtual))
        .route("/api/virtual/{id}", put(set_virtual).delete(destroy_virtual))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/trace", get(traces))
        .route("/api/trace/{id}", get(trace))
}

type ApiResult = Result<Response, (StatusCode, String)>;

fn bad(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("{e:#}"))
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
}

/// Accepts a login session cookie or `Authorization: Bearer <admin_token>`.
fn check(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    if let Some(s) = session_cookie(headers) {
        if app.admin_sessions.lock().unwrap().contains(&s) {
            return Ok(());
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let token = app.settings.read().unwrap().auth.admin_token.clone();
    match bearer {
        Some(b) if !token.is_empty() && constant_time_eq(b.as_bytes(), token.as_bytes()) => Ok(()),
        _ => Err((StatusCode::UNAUTHORIZED, "login required".into())),
    }
}

#[derive(Deserialize)]
struct Login {
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
}

async fn login(State(app): State<Arc<App>>, Json(l): Json<Login>) -> ApiResult {
    let auth = app.settings.read().unwrap().auth.clone();
    let ok = (!l.token.is_empty() && constant_time_eq(l.token.as_bytes(), auth.admin_token.as_bytes()))
        || (!auth.admin_password.is_empty() && verify_password(&auth.admin_password, &l.password));
    if !ok {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".into()));
    }
    let session = random_token();
    app.admin_sessions.lock().unwrap().insert(session.clone());
    let secure = if app.tls_enabled { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/{secure}");
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response())
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    if let Some(s) = session_cookie(&headers) {
        app.admin_sessions.lock().unwrap().remove(&s);
    }
    let clear = format!("{COOKIE}=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/");
    Ok(([(header::SET_COOKIE, clear)], Json(json!({"ok": true}))).into_response())
}

async fn status(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    check(&app, &headers)?;
    let p = &app.platform;
    let (gpus, cpu) = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || (app.platform.monitor().gpus(), app.telemetry.cpu_usage()))
            .await
            .map_err(bad)?
    };
    Ok(Json(json!({
        "platform": p.name(),
        "provider": p.displays().provider_name(),
        // Every display being captured: who watches it, whether it moves as
        // a whole (motion mode), and above 0 an HDR picture up to that many
        // times SDR white.
        "streams": app.streams.active().iter().map(|s| json!({
            "display": s.control.current(),
            "clients": s.watchers(),
            "motion": s.shared.motion(),
            "hdr_max": s.shared.hdr_max(),
            // New pictures the capture delivers: what any client could get at most.
            "capture_fps": (s.capture_fps() * 10.0).round() / 10.0,
        })).collect::<Vec<_>>(),
        "scheduling": app.streaming.lock().unwrap().notes,
        "encoders": p.encoders().backends(),
        "encoders_unavailable": p.encoders().unavailable(),
        "encoder_preferred": app.settings.read().unwrap().encoder.preferred,
        "gpus": gpus,
        "cpu_percent": cpu,
        "sound": app.audio.status(),
        "microphone": app.mic.status(),
        "clients": app.telemetry.clients(),
        "errors": app.telemetry.errors(),
        "events": app.telemetry.events(),
    }))
    .into_response())
}

async fn displays(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    check(&app, &headers)?;
    Ok(Json(app.displays_json(None).map_err(bad)?).into_response())
}

#[derive(Deserialize)]
struct Select {
    id: String,
}

async fn select_display(State(app): State<Arc<App>>, headers: HeaderMap, Json(s): Json<Select>) -> ApiResult {
    check(&app, &headers)?;
    app.select_display(&s.id).map_err(bad)?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn create_virtual(State(app): State<Arc<App>>, headers: HeaderMap, Json(mode): Json<DisplayMode>) -> ApiResult {
    check(&app, &headers)?;
    let v = app.platform.displays().virtual_displays().ok_or_else(|| bad("virtual displays unsupported here"))?;
    mode.check(&v.capabilities()).map_err(bad)?;
    let info = v.create(mode).map_err(bad)?;
    app.streams.list_changed();
    Ok(Json(info).into_response())
}

async fn set_virtual(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mode): Json<DisplayMode>,
) -> ApiResult {
    check(&app, &headers)?;
    app.virtual_request(VirtualRequest::Set { id, mode }).map_err(bad)?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn install_virtual(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    check(&app, &headers)?;
    app.virtual_request(VirtualRequest::Install).map_err(bad)?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn destroy_virtual(State(app): State<Arc<App>>, headers: HeaderMap, Path(id): Path<String>) -> ApiResult {
    check(&app, &headers)?;
    // Its clients move to the default display at once.
    app.virtual_request(VirtualRequest::Destroy { id }).map_err(bad)?;
    Ok(Json(json!({"ok": true})).into_response())
}

/// Clients with a frame trace kept, and the batches kept of each.
async fn traces(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    check(&app, &headers)?;
    let clients: Vec<_> = app.telemetry.traced().into_iter().map(|(id, batches)| json!({"client": id, "batches": batches})).collect();
    Ok(Json(json!({"clients": clients})).into_response())
}

/// One client's frame trace: the batches it sent (see `C_TRACE`), oldest first.
async fn trace(State(app): State<Arc<App>>, headers: HeaderMap, Path(id): Path<u64>) -> ApiResult {
    check(&app, &headers)?;
    let batches = app.telemetry.trace(id).ok_or((StatusCode::NOT_FOUND, format!("no trace of client {id}")))?;
    Ok(Json(json!({"client": id, "batches": batches})).into_response())
}

async fn get_settings(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    check(&app, &headers)?;
    let s = app.settings.read().unwrap().clone();
    Ok(Json(json!({
        "tuning": s.tuning,
        "codec": s.encoder.codec,
        "codecs": CODECS,
        "encoder_preferred": s.encoder.preferred,
        "encoders": app.platform.encoders().backends(),
        "keyboard_layout": s.input.keyboard_layout,
        "frame_trace": s.debug.frame_trace,
        "stream_token": s.auth.stream_token,
        "admin_password_set": !s.auth.admin_password.is_empty(),
        "tls": app.tls_enabled,
        "config_path": app.config.path,
        "stream_listen": s.server.listen,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SettingsPatch {
    tuning: Option<Tuning>,
    codec: Option<String>,
    encoder_preferred: Option<String>,
    keyboard_layout: Option<String>,
    stream_token: Option<String>,
    admin_password: Option<String>,
    frame_trace: Option<bool>,
}

async fn put_settings(State(app): State<Arc<App>>, headers: HeaderMap, Json(p): Json<SettingsPatch>) -> ApiResult {
    check(&app, &headers)?;
    if let Some(t) = &p.tuning {
        if t.bandwidth_mbps <= 0.0 || t.max_lossless_frame_kb == 0 {
            return Err(bad("tuning values must be positive"));
        }
    }
    if let Some(c) = &p.codec {
        if !CODECS.contains(&c.as_str()) {
            return Err(bad(format!("unsupported codec {c}")));
        }
    }
    if let Some(e) = &p.encoder_preferred {
        if e != "auto" && !app.platform.encoders().backends().contains(&e.as_str()) {
            return Err(bad(format!("unknown encoder {e}")));
        }
    }
    let mut warnings = Vec::new();
    if let Some(layout) = &p.keyboard_layout {
        *app.keyboard_layout.write().unwrap() = layout.clone();
        warnings.extend(app.streams.set_keyboard_layout(layout).into_iter().map(|e| format!("keyboard layout: {e}")));
    }
    // Tuning and encoder preference are picked up live by the client threads.
    app.update_settings(|s| {
        if let Some(t) = p.tuning {
            s.tuning = t;
        }
        if let Some(c) = p.codec {
            s.encoder.codec = c;
        }
        if let Some(e) = p.encoder_preferred {
            s.encoder.preferred = e;
        }
        if let Some(l) = p.keyboard_layout {
            s.input.keyboard_layout = l;
        }
        if let Some(t) = p.frame_trace {
            s.debug.frame_trace = t;
        }
        if let Some(t) = p.stream_token {
            s.auth.stream_token = t;
        }
        if let Some(pw) = p.admin_password.filter(|pw| !pw.is_empty()) {
            s.auth.admin_password = hash_password(&pw);
        }
    })
    .map_err(bad)?;
    Ok(Json(json!({"ok": true, "warnings": warnings})).into_response())
}
