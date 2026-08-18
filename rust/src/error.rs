//! Error model per docs/rust-rewrite/api-contract.md §2.5 + §9.
//! Every non-2xx JSON response (except POST /login's {ok,message} envelope)
//! serializes an `ErrorBody`. Message templates are wire contract.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorBody {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issues: Option<Vec<Issue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ErrorBody>>,
}

impl ErrorBody {
    pub fn new(error_type: &str, message: impl Into<String>) -> Self {
        Self {
            error_type: error_type.into(),
            message: message.into(),
            issues: None,
            cause: None,
        }
    }
}

/// A fully-formed application error: wire body + status (+ extra headers).
#[derive(Debug, Clone)]
pub struct AppError {
    pub status: StatusCode,
    pub body: ErrorBody,
    pub headers: HeaderMap,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.body.error_type, self.body.message)
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.headers, axum::Json(self.body)).into_response()
    }
}

fn plain(status: StatusCode, error_type: &str, message: impl Into<String>) -> AppError {
    AppError {
        status,
        body: ErrorBody::new(error_type, message),
        headers: HeaderMap::new(),
    }
}

// ---- catalog constructors (templates are contract; do not reword) ----

pub fn payload_too_large() -> AppError {
    plain(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload-too-large",
        "Request body must not exceed 8192 bytes.",
    )
}

pub fn not_found() -> AppError {
    plain(StatusCode::NOT_FOUND, "not-found", "Not Found")
}

pub fn host_not_allowed(message: impl Into<String>) -> AppError {
    plain(StatusCode::FORBIDDEN, "host-not-allowed", message)
}

pub fn origin_not_allowed(normalized_origin: &str) -> AppError {
    plain(
        StatusCode::FORBIDDEN,
        "origin-not-allowed",
        format!(
            "Origin \"{normalized_origin}\" is not allowed. (Add to MOUSEHOLE_ALLOWED_ORIGINS to permit it.)"
        ),
    )
}

pub fn authentication_required(message: &str) -> AppError {
    let mut headers = HeaderMap::new();
    headers.insert(
        "WWW-Authenticate",
        HeaderValue::from_static("Bearer realm=\"Mousehole\""),
    );
    AppError {
        status: StatusCode::UNAUTHORIZED,
        body: ErrorBody::new("authentication-required", message),
        headers,
    }
}

pub fn auth_not_configured() -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "auth-not-configured",
        "Mousehole authentication is not configured. Set MOUSEHOLE_AUTH_PASSWORD or MOUSEHOLE_AUTH_TOKEN to enable.",
    )
}

pub fn unsupported_media_type(raw_content_type: &str) -> AppError {
    plain(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported-media-type",
        format!(
            "Unsupported content type \"{raw_content_type}\", must be \"application/json\""
        ),
    )
}

/// `source` is e.g. "request body" or a file path/url. Message carries the
/// first issue, path-prefixed only when the path is non-empty.
pub fn schema_error(status: StatusCode, source: &str, issues: Vec<Issue>) -> AppError {
    let first = issues.first();
    let detail = match first {
        Some(i) if !i.path.is_empty() => format!("{}: {}", i.path, i.message),
        Some(i) => i.message.clone(),
        None => String::new(),
    };
    let mut e = plain(
        status,
        "schema-error",
        format!("Schema validation failed for data from {source}: {detail}"),
    );
    e.body.issues = Some(issues);
    e
}

pub fn json_parse_error_request(method: &str, url: &str, cause_message: &str) -> AppError {
    // The parse error rides along as a cause, like the original. The wording
    // inside is the parser's own (serde here, JavaScriptCore there) — that
    // difference is declared in the impact itinerary.
    let mut e = plain(
        StatusCode::BAD_REQUEST,
        "json-parse-error",
        format!("Error parsing JSON from request with method {method} and URL {url}"),
    );
    e.body.cause = Some(Box::new(ErrorBody::new("unhandled-error", cause_message)));
    e
}

pub fn json_parse_error_response(status: u16, url: &str) -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "json-parse-error",
        format!("Error parsing JSON from response with status {status} and URL {url}"),
    )
}

pub fn json_parse_error_file(path: &str) -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "json-parse-error",
        format!("Error parsing JSON from file at {path}"),
    )
}

pub fn file_read_error(path: &str) -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "file-read-error",
        format!("Error reading file: {path}. Check that it is readable and is not a directory."),
    )
}

pub fn file_write_error(path: &str) -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "file-write-error",
        format!("Error writing file: {path}. Check that the parent directory exists and is writable."),
    )
}

pub fn directory_create_error(path: &str) -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "directory-create-error",
        format!("Error creating directory: {path}. Check that the parent directory exists and you have write permissions."),
    )
}

pub fn network_error(url: &str) -> AppError {
    plain(
        StatusCode::INTERNAL_SERVER_ERROR,
        "network-error",
        format!("Network request to {url} failed"),
    )
}

pub fn timeout_error(url: &str, seconds: f64) -> AppError {
    plain(
        StatusCode::GATEWAY_TIMEOUT,
        "timeout-error",
        format!("Request to {url} timed out after {seconds}s. Is the network up?"),
    )
}

pub fn unhandled(message: impl Into<String>) -> AppError {
    plain(StatusCode::INTERNAL_SERVER_ERROR, "unhandled-error", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape() {
        let e = schema_error(
            StatusCode::BAD_REQUEST,
            "request body",
            vec![Issue {
                path: "value".into(),
                message: "must not be empty".into(),
            }],
        );
        let json = serde_json::to_value(&e.body).unwrap();
        assert_eq!(json["type"], "schema-error");
        assert_eq!(
            json["message"],
            "Schema validation failed for data from request body: value: must not be empty"
        );
        assert_eq!(json["issues"][0]["path"], "value");
        assert!(json.get("cause").is_none());
    }

    #[test]
    fn schema_error_without_path_has_no_prefix() {
        let e = schema_error(
            StatusCode::BAD_REQUEST,
            "request body",
            vec![Issue {
                path: String::new(),
                message: "expected object".into(),
            }],
        );
        assert_eq!(
            e.body.message,
            "Schema validation failed for data from request body: expected object"
        );
    }

    #[test]
    fn cause_chain_nests() {
        let mut e = unhandled("outer");
        e.body.cause = Some(Box::new(ErrorBody::new(
            "unhandled-error",
            "Unhandled error: boom",
        )));
        let json = serde_json::to_value(&e.body).unwrap();
        assert_eq!(json["cause"]["type"], "unhandled-error");
        assert!(json["cause"].get("cause").is_none());
    }

    #[test]
    fn timeout_message_formats_float_like_js() {
        assert_eq!(
            timeout_error("https://x", 10.0).body.message,
            "Request to https://x timed out after 10s. Is the network up?"
        );
        assert_eq!(
            timeout_error("https://x", 0.5).body.message,
            "Request to https://x timed out after 0.5s. Is the network up?"
        );
    }

    #[test]
    fn auth_required_carries_www_authenticate() {
        let e = authentication_required("No credentials presented");
        assert_eq!(
            e.headers.get("WWW-Authenticate").unwrap(),
            "Bearer realm=\"Mousehole\""
        );
        assert_eq!(e.status, StatusCode::UNAUTHORIZED);
    }
}
