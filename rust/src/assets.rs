//! Embedded Vite frontend bundle per docs/rust-rewrite/api-contract.md §8 and
//! state-and-build.md §5.3. Served under /web with NO boundary checks (the
//! login page needs its assets), no cache headers, no SPA fallback — a miss
//! falls through to the global JSON 404.

use axum::body::Body;
use axum::http::{header, HeaderValue};
#[cfg(test)]
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::error;

#[derive(RustEmbed)]
#[folder = "../dist"]
struct Dist;

fn serve(path: &str) -> Option<Response> {
    let file = Dist::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // text types get an explicit charset, matching the Bun static handler
    let content_type = if mime.type_() == "text" {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    let mut resp = Response::new(Body::from(file.data.into_owned()));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    Some(resp)
}

/// `GET /web` and `GET /web/` → index.html; `GET /web/<path>` → the file,
/// with the Hono serveStatic default-document behavior for extension-less
/// paths; miss → JSON 404.
pub fn serve_web(rest: &str) -> Response {
    // Reject traversal outright; rust-embed keys are clean relative paths.
    if rest.split('/').any(|seg| seg == "..") {
        return error::not_found().into_response();
    }
    let path = rest.trim_start_matches('/');
    let candidates: Vec<String> = if path.is_empty() {
        vec!["index.html".to_string()]
    } else if path.ends_with('/') {
        vec![format!("{path}index.html")]
    } else {
        vec![path.to_string(), format!("{path}/index.html")]
    };
    for c in &candidates {
        if let Some(r) = serve(c) {
            return r;
        }
    }
    error::not_found().into_response()
}

/// True when the bundle contains an index.html — an absent bundle means a
/// broken build. Test-only today.
#[cfg(test)]
pub fn bundle_present() -> bool {
    Dist::get("index.html").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_served_at_web_root_variants() {
        // The build harness guarantees at least a placeholder dist/index.html.
        assert!(bundle_present());
        assert_eq!(serve_web("").status(), StatusCode::OK);
        assert_eq!(serve_web("/").status(), StatusCode::OK);
    }

    #[test]
    fn miss_is_json_404_and_traversal_rejected() {
        assert_eq!(serve_web("/definitely-missing.xyz").status(), StatusCode::NOT_FOUND);
        assert_eq!(serve_web("/../Cargo.toml").status(), StatusCode::NOT_FOUND);
    }
}
