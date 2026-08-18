//! Router assembly + handlers per docs/rust-rewrite/api-contract.md.
//! Boundary ladders run inside each handler in the contract's textual order
//! (host → auth → origin → json); the global body limit and the 405→404 and
//! debug-log layers wrap everything.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State as AxState;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use serde::Serialize;

use crate::boundary::{self, AuthMethod};
use crate::config::Config;
use crate::error::{self, AppError, Issue};
use crate::logger;
use crate::scheduler::SchedulerHandle;
use crate::session::{self, SessionStore};
use crate::sse::SseRegistry;
use crate::state::{classify, PublicState};
use crate::{assets, timefmt};

const BODY_LIMIT: usize = 8 * 1024;

#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<Config>,
    pub sessions: SessionStore,
    pub scheduler: SchedulerHandle,
    pub sse: SseRegistry,
}

/// Login/logout envelope — NOT the error envelope.
#[derive(Serialize)]
struct OkBody {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}

fn ok_body(status: StatusCode, ok: bool, message: Option<&'static str>) -> Response {
    (status, axum::Json(OkBody { ok, message })).into_response()
}

fn host_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn build_public_state(
    ctx: &AppContext,
    state: Option<crate::state::State>,
) -> PublicState {
    PublicState {
        has_cookie: state
            .as_ref()
            .and_then(|s| s.effective_cookie())
            .is_some(),
        has_auth: ctx.config.auth.is_configured(),
        next_contact_at: ctx
            .scheduler
            .next_contact_at()
            .map(|z| timefmt::to_wire(&z)),
        last_mam_contact: state.and_then(|s| s.last_mam_contact.map(|c| c.body)),
    }
}

// ---- route handlers ----

/// §4.1 GET / — Accept negotiation, 302.
async fn root(headers: HeaderMap) -> Response {
    let accept = header_str(&headers, header::ACCEPT).unwrap_or("");
    let json_pos = accept.find("application/json");
    let html_pos = accept.find("text/html");
    let prefer_json = match (json_pos, html_pos) {
        (Some(j), Some(h)) => j < h,
        (Some(_), None) => true,
        _ => false,
    };
    // 302 Found — Hono's redirect default; axum's `Redirect::to` would be 303
    // and `Redirect::found` no longer exists, so build it by hand.
    let target = if prefer_json { "/health" } else { "/web" };
    (
        StatusCode::FOUND,
        [(header::LOCATION, HeaderValue::from_static(target))],
    )
        .into_response()
}

/// §4.2 POST /login — ladder: host → origin → json → auth-mode → parse →
/// schema → password.
async fn login(
    AxState(ctx): AxState<AppContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let host = host_of(&headers);
    if let Err(e) = boundary::host_allowed(host, &ctx.config) {
        return e.into_response();
    }
    if let Err(e) = boundary::origin_allowed(
        AuthMethod::None,
        header_str(&headers, header::ORIGIN),
        host,
        &ctx.config,
    ) {
        return e.into_response();
    }
    if let Err(e) =
        boundary::require_json_body(header_str(&headers, header::CONTENT_TYPE))
    {
        return e.into_response();
    }
    let Some(password) = ctx.config.auth.password() else {
        return ok_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            Some("Browser login is unavailable: MOUSEHOLE_AUTH_PASSWORD is not set"),
        );
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return ok_body(
            StatusCode::BAD_REQUEST,
            false,
            Some("Malformed JSON in request body"),
        );
    };
    let Some(presented) = value
        .as_object()
        .and_then(|o| o.get("password"))
        .and_then(|p| p.as_str())
    else {
        return ok_body(
            StatusCode::BAD_REQUEST,
            false,
            Some("Request body must match expected schema"),
        );
    };
    if !session::safe_equal(presented.as_bytes(), password.as_bytes()) {
        return ok_body(StatusCode::UNAUTHORIZED, false, Some("Incorrect password"));
    }
    let id = ctx.sessions.create();
    let cookie = session::login_cookie(
        &id,
        ctx.config.session_duration_seconds,
        ctx.config.https_only_cookies,
    );
    let mut resp = ok_body(StatusCode::OK, true, None);
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie is ASCII"),
    );
    resp
}

/// §4.3 POST /logout — host → origin; always 200 + clearing cookie.
async fn logout(AxState(ctx): AxState<AppContext>, headers: HeaderMap) -> Response {
    let host = host_of(&headers);
    if let Err(e) = boundary::host_allowed(host, &ctx.config) {
        return e.into_response();
    }
    if let Err(e) = boundary::origin_allowed(
        AuthMethod::None,
        header_str(&headers, header::ORIGIN),
        host,
        &ctx.config,
    ) {
        return e.into_response();
    }
    if let Some(id) =
        session::extract_session_id(header_str(&headers, header::COOKIE))
    {
        ctx.sessions.delete(&id); // fires SSE close only for known sessions
    }
    let mut resp = ok_body(StatusCode::OK, true, None);
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session::logout_cookie()).expect("ASCII"),
    );
    resp
}

fn run_auth(ctx: &AppContext, headers: &HeaderMap) -> Result<AuthMethod, AppError> {
    boundary::require_auth(
        &ctx.config,
        &ctx.sessions,
        header_str(headers, header::AUTHORIZATION),
        header_str(headers, header::COOKIE),
    )
}

/// §4.4 POST /updates — host → auth → origin; contact now; 200 PublicState.
async fn updates(AxState(ctx): AxState<AppContext>, headers: HeaderMap) -> Response {
    let host = host_of(&headers);
    if let Err(e) = boundary::host_allowed(host, &ctx.config) {
        return e.into_response();
    }
    let method = match run_auth(&ctx, &headers) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = boundary::origin_allowed(
        method,
        header_str(&headers, header::ORIGIN),
        host,
        &ctx.config,
    ) {
        return e.into_response();
    }
    match ctx.scheduler.commit_contact(None).await {
        Ok(_) => match ctx.scheduler.store().read_if_exists() {
            Ok(state) => axum::Json(build_public_state(&ctx, state)).into_response(),
            Err(e) => e.into_response(),
        },
        Err(e) => e.into_response(),
    }
}

/// §4.5 GET /state — host → auth only (no origin: reads are CSRF-safe).
async fn get_state(AxState(ctx): AxState<AppContext>, headers: HeaderMap) -> Response {
    if let Err(e) = boundary::host_allowed(host_of(&headers), &ctx.config) {
        return e.into_response();
    }
    if let Err(e) = run_auth(&ctx, &headers) {
        return e.into_response();
    }
    match ctx.scheduler.store().read_if_exists() {
        Ok(state) => axum::Json(build_public_state(&ctx, state)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// §4.6 PUT /cookie — host → auth → origin → json; empty body is a
/// schema-error with NO path prefix; non-empty unparseable is a
/// json-parse-error naming method+URL.
async fn put_cookie(
    AxState(ctx): AxState<AppContext>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let host = host_of(&headers);
    if let Err(e) = boundary::host_allowed(host, &ctx.config) {
        return e.into_response();
    }
    let method = match run_auth(&ctx, &headers) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = boundary::origin_allowed(
        method,
        header_str(&headers, header::ORIGIN),
        host,
        &ctx.config,
    ) {
        return e.into_response();
    }
    if let Err(e) =
        boundary::require_json_body(header_str(&headers, header::CONTENT_TYPE))
    {
        return e.into_response();
    }

    let value: Option<serde_json::Value> = if body.is_empty() {
        None // empty text ⇒ undefined ⇒ schema failure, not parse failure
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => Some(v),
            Err(e) => {
                let url = format!("http://{}{}", host.unwrap_or(""), uri.path());
                return error::json_parse_error_request("PUT", &url, &e.to_string())
                    .into_response();
            }
        }
    };
    // Validation failures use zod's exact wording so the error bodies stay
    // byte-identical with the Bun backend.
    let new_cookie = match value.as_ref() {
        None => {
            return schema_400("", "Invalid input: expected object, received undefined")
        }
        Some(v) if !v.is_object() => {
            return schema_400(
                "",
                &format!("Invalid input: expected object, received {}", zod_type(v)),
            )
        }
        Some(v) => {
            let obj = v.as_object().expect("checked above");
            match obj.get("value") {
                None => {
                    return schema_400(
                        "value",
                        "Invalid input: expected string, received undefined",
                    )
                }
                Some(serde_json::Value::String(s)) if s.is_empty() => {
                    return schema_400(
                        "value",
                        "Too small: expected string to have >=1 characters",
                    )
                }
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => {
                    return schema_400(
                        "value",
                        &format!(
                            "Invalid input: expected string, received {}",
                            zod_type(other)
                        ),
                    )
                }
            }
        }
    };

    match ctx.scheduler.commit_contact(Some(new_cookie)).await {
        Ok(_) => match ctx.scheduler.store().read_if_exists() {
            Ok(state) => axum::Json(build_public_state(&ctx, state)).into_response(),
            Err(e) => e.into_response(),
        },
        Err(e) => e.into_response(),
    }
}

/// §4.7 GET /health — no boundary checks at all.
async fn health(AxState(ctx): AxState<AppContext>) -> Response {
    match ctx.scheduler.store().read_if_exists() {
        Ok(state) => {
            let status = classify(
                state
                    .as_ref()
                    .and_then(|s| s.last_mam_contact.as_ref())
                    .map(|c| &c.body),
            );
            axum::Json(serde_json::json!({ "lastMamContactResult": status })).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// §7 GET /events — host → auth → origin; manual SSE response (no hello, no
/// keep-alive pings, exact headers).
async fn events(AxState(ctx): AxState<AppContext>, headers: HeaderMap) -> Response {
    let host = host_of(&headers);
    if let Err(e) = boundary::host_allowed(host, &ctx.config) {
        return e.into_response();
    }
    let method = match run_auth(&ctx, &headers) {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = boundary::origin_allowed(
        method,
        header_str(&headers, header::ORIGIN),
        host,
        &ctx.config,
    ) {
        return e.into_response();
    }
    let session_id = if method == AuthMethod::Session {
        session::extract_session_id(header_str(&headers, header::COOKIE)).unwrap_or_default()
    } else {
        String::new()
    };
    let guard = ctx.sse.register(&session_id);
    let stream = futures::stream::unfold(guard, |mut g| async move {
        g.rx.recv()
            .await
            .map(|frame| (Ok::<_, std::convert::Infallible>(Bytes::from_static(frame)), g))
    });
    let mut resp = Response::new(Body::from_stream(stream));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    resp
}

/// zod's names for JSON types, for byte-identical validation messages.
fn zod_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn schema_400(path: &str, message: &str) -> Response {
    error::schema_error(
        StatusCode::BAD_REQUEST,
        "request body",
        vec![Issue {
            path: path.into(),
            message: message.into(),
        }],
    )
    .into_response()
}

async fn web_root() -> Response {
    assets::serve_web("")
}

async fn web_path(axum::extract::Path(rest): axum::extract::Path<String>) -> Response {
    assets::serve_web(&rest)
}

// ---- router assembly + global layers ----

pub fn router(ctx: AppContext) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/updates", post(updates))
        .route("/state", get(get_state))
        .route("/cookie", put(put_cookie))
        .route("/health", get(health))
        .route("/events", get(events))
        .route("/web", get(web_root))
        .route("/web/", get(web_root))
        .route("/web/{*rest}", get(web_path))
        .fallback(|| async { error::not_found().into_response() })
        .layer(axum::middleware::from_fn(normalize_status_layer))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .layer(axum::middleware::from_fn(body_limit_and_log_layer))
        .with_state(ctx)
}

/// Outermost: up-front declared-length check (413 precedes every boundary
/// check) + the per-request debug log line.
async fn body_limit_and_log_layer(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let declared_over = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n > BODY_LIMIT)
        .unwrap_or(false);
    let resp = if declared_over {
        error::payload_too_large().into_response()
    } else {
        next.run(req).await
    };
    logger::debug(&format!("{method} {path} → {}", resp.status().as_u16()));
    resp
}

/// Inner normalization: axum's bare 405 (matched path, wrong method) becomes
/// the contract's JSON 404; a bare 413 from the streaming body limit becomes
/// the contract's JSON 413.
async fn normalize_status_layer(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let resp = next.run(req).await;
    match resp.status() {
        StatusCode::METHOD_NOT_ALLOWED => error::not_found().into_response(),
        StatusCode::PAYLOAD_TOO_LARGE if !is_json(&resp) => {
            error::payload_too_large().into_response()
        }
        _ => resp,
    }
}

fn is_json(resp: &Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false)
}
