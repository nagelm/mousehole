//! MAM external API client per docs/rust-rewrite/mam-behavior.md §1–§3.
//! Manual cookie handling only — no reqwest cookie store, ever (§3).

use std::net::Ipv4Addr;

use axum::http::StatusCode;
use percent_encoding::percent_decode_str;
use serde::Deserialize;

use crate::error::{self, AppError, Issue};

pub const DEFAULT_BASE_URL: &str = "https://t.myanonamouse.net";

#[derive(Debug, Clone)]
pub struct MamClient {
    client: reqwest::Client,
    base_url: String,
    timeout_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MamUpdateResult {
    pub ip: String,
    pub asn: i64,
    pub as_name: String,
    pub success: bool,
    pub msg: String,
    pub http_status: u16,
    pub rotated_cookie: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostInfo {
    pub ip: String,
    pub asn: i64,
    pub as_name: String,
}

// NOTE: no deny_unknown_fields — MAM may add keys (contract).
#[derive(Deserialize)]
struct DynamicSeedboxBody {
    #[serde(rename = "Success")]
    success: bool,
    msg: String,
    ip: String,
    #[serde(rename = "ASN")]
    asn: i64,
    #[serde(rename = "AS")]
    as_name: String,
}

#[derive(Deserialize)]
struct JsonIpBody {
    ip: String,
    #[serde(rename = "ASN")]
    asn: i64,
    #[serde(rename = "AS")]
    as_name: String,
}

impl MamClient {
    pub fn new(timeout_seconds: f64, base_url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("mousehole-by-timtimtim/", "0.5.0"))
            // Frugality: drop pooled connections between the widely-spaced
            // contacts instead of holding buffers for the <5 MB RSS target.
            .pool_max_idle_per_host(0)
            .build()
            .expect("reqwest client builds");
        Self {
            client,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            timeout_seconds,
        }
    }

    fn transport_error(&self, e: &reqwest::Error, url: &str) -> AppError {
        if e.is_timeout() {
            error::timeout_error(url, self.timeout_seconds)
        } else {
            error::network_error(url)
        }
    }

    /// §1.2 dynamicSeedbox.php — cookie-driven IP update. Body is parsed for
    /// ANY HTTP status; the status is recorded, never branched on here.
    pub async fn update_mam_ip(&self, cookie: &str) -> Result<MamUpdateResult, AppError> {
        let url = format!("{}/json/dynamicSeedbox.php", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::COOKIE, format!("mam_id={cookie}"))
            .timeout(std::time::Duration::from_secs_f64(self.timeout_seconds))
            .send()
            .await
            .map_err(|e| self.transport_error(&e, &url))?;

        let http_status = resp.status().as_u16();
        let rotated_cookie = rotated_cookie_from_headers(
            resp.headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok()),
        );
        let text = resp
            .text()
            .await
            .map_err(|e| self.transport_error(&e, &url))?;

        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| error::json_parse_error_response(http_status, &url))?;
        let body: DynamicSeedboxBody = serde_json::from_value(value).map_err(|e| {
            error::schema_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &url,
                vec![Issue {
                    path: String::new(),
                    message: e.to_string(),
                }],
            )
        })?;
        if body.ip.parse::<Ipv4Addr>().is_err() {
            return Err(error::schema_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &url,
                vec![Issue {
                    path: "ip".into(),
                    message: "must be an IPv4 address".into(),
                }],
            ));
        }

        Ok(MamUpdateResult {
            ip: body.ip,
            asn: body.asn,
            as_name: body.as_name,
            success: body.success,
            msg: body.msg,
            http_status,
            rotated_cookie,
        })
    }

    /// §1.3 jsonIp.php — cookie-less lookup; requires 2xx (plain error text
    /// per contract: surfaces as unhandled-error in stored contacts).
    pub async fn get_host_info(&self) -> Result<HostInfo, AppError> {
        let url = format!("{}/json/jsonIp.php", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs_f64(self.timeout_seconds))
            .send()
            .await
            .map_err(|e| self.transport_error(&e, &url))?;

        let http_status = resp.status();
        if !http_status.is_success() {
            return Err(error::unhandled(format!(
                "Failed to fetch host IP from {url}: {}",
                http_status.as_u16()
            )));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| self.transport_error(&e, &url))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| error::json_parse_error_response(http_status.as_u16(), &url))?;
        let body: JsonIpBody = serde_json::from_value(value).map_err(|e| {
            error::schema_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &url,
                vec![Issue {
                    path: String::new(),
                    message: e.to_string(),
                }],
            )
        })?;
        if body.ip.parse::<Ipv4Addr>().is_err() {
            return Err(error::schema_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &url,
                vec![Issue {
                    path: "ip".into(),
                    message: "must be an IPv4 address".into(),
                }],
            ));
        }
        Ok(HostInfo {
            ip: body.ip,
            asn: body.asn,
            as_name: body.as_name,
        })
    }
}

/// §3: first Set-Cookie named `mam_id` wins; value percent-decoded on receipt
/// (matches set-cookie-parser's decodeValues default), sent back verbatim.
pub fn rotated_cookie_from_headers<'a>(
    headers: impl Iterator<Item = &'a str>,
) -> Option<String> {
    for h in headers {
        if let Ok(c) = cookie::Cookie::parse(h.to_string()) {
            if c.name() == "mam_id" {
                let decoded = percent_decode_str(c.value())
                    .decode_utf8()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| c.value().to_string());
                return Some(decoded);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_mam_id_wins_and_is_percent_decoded() {
        let headers = vec![
            "other=x; Path=/",
            "mam_id=abc%3D%3D123; Path=/; HttpOnly",
            "mam_id=second",
        ];
        assert_eq!(
            rotated_cookie_from_headers(headers.into_iter()),
            Some("abc==123".to_string())
        );
    }

    #[test]
    fn no_mam_id_means_no_rotation() {
        let headers = vec!["session=x; Path=/"];
        assert_eq!(rotated_cookie_from_headers(headers.into_iter()), None);
    }
}
