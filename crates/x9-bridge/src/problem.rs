//! Errors as RFC 9457 problem documents with a machine-readable `code`.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub title: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, title: impl Into<String>) -> Self {
        ApiError { status, code, title: title.into(), detail: None, hint: None }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "type": format!("https://x9-bridge.local/problems/{}", self.code),
            "title": self.title,
            "status": self.status.as_u16(),
            "code": self.code,
        });
        if let Some(d) = &self.detail {
            v["detail"] = json!(d);
        }
        if let Some(h) = &self.hint {
            v["hint"] = json!(h);
        }
        v
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "Missing or unknown bearer token")
    }
    pub fn forbidden(what: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", format!("Your role can't {what}"))
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", format!("No such {what}"))
    }
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", "Invalid request").detail(detail)
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", "Conflict").detail(detail)
    }
    pub fn disconnected(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "scanner_disconnected", "The scanner is not reachable")
            .detail(detail)
            .hint("Plug in and power the X9; the bridge brings the USB link up.")
    }
    /// The bridge's own disk (`BRIDGE_DATA`) failed: the cache, a download, an export file.
    pub fn cache(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "cache_error", "The bridge could not read or write its files")
            .detail(detail)
            .hint("Check that BRIDGE_DATA is writable and its disk has space.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, [(header::CONTENT_TYPE, "application/problem+json")], self.to_json().to_string())
            .into_response()
    }
}

impl From<x9_client::X9Error> for ApiError {
    fn from(e: x9_client::X9Error) -> Self {
        match &e {
            x9_client::X9Error::Api { status: 409, .. } => {
                Self::new(StatusCode::CONFLICT, "scanner_busy", "The scanner is busy").detail(e.to_string())
            }
            x9_client::X9Error::Api { status, .. } => {
                Self::new(StatusCode::BAD_GATEWAY, "scanner_error", format!("The scanner refused the request ({status})"))
                    .detail(e.to_string())
            }
            // Only downloads touch the disk: writing the file the scanner sent.
            x9_client::X9Error::Io(io) => Self::cache(format!("writing a download: {io}")),
            _ => Self::disconnected(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x9_client::X9Error;

    fn api(status: u16) -> X9Error {
        X9Error::Api { status, path: "/api/v1/tzfs/3/preview".into(), body: String::new() }
    }

    #[test]
    fn scanner_errors_map_to_their_own_codes() {
        let busy = ApiError::from(api(409));
        assert_eq!((busy.status, busy.code), (StatusCode::CONFLICT, "scanner_busy"));
        for s in [400, 404, 500] {
            let e = ApiError::from(api(s));
            assert_eq!((e.status, e.code), (StatusCode::BAD_GATEWAY, "scanner_error"), "{s}");
            assert!(e.title.contains(&s.to_string()));
        }
        let disk = ApiError::from(X9Error::Io(std::io::Error::new(std::io::ErrorKind::StorageFull, "no space left")));
        assert_eq!((disk.status, disk.code), (StatusCode::INTERNAL_SERVER_ERROR, "cache_error"));
        let garbled = ApiError::from(X9Error::Json(serde_json::from_str::<u8>("x").unwrap_err()));
        assert_eq!(garbled.code, "scanner_disconnected");
    }

    #[test]
    fn a_problem_document_carries_its_detail_and_hint_only_when_set() {
        let bare = ApiError::not_found("scan").to_json();
        assert_eq!(bare, json!({"type": "https://x9-bridge.local/problems/not_found", "title": "No such scan", "status": 404, "code": "not_found"}));
        let full = ApiError::disconnected("connection refused").to_json();
        assert_eq!(full["status"], 503);
        assert_eq!(full["detail"], "connection refused");
        assert!(full["hint"].as_str().unwrap().contains("X9"));
        let hinted = ApiError::invalid("x").hint("try y").to_json();
        assert_eq!((hinted["code"].as_str(), hinted["hint"].as_str()), (Some("invalid_request"), Some("try y")));
    }

    #[test]
    fn each_constructor_has_its_status_and_code() {
        for (e, status, code) in [
            (ApiError::unauthorized(), StatusCode::UNAUTHORIZED, "unauthorized"),
            (ApiError::forbidden("start scans"), StatusCode::FORBIDDEN, "forbidden"),
            (ApiError::not_found("job"), StatusCode::NOT_FOUND, "not_found"),
            (ApiError::invalid("bad"), StatusCode::BAD_REQUEST, "invalid_request"),
            (ApiError::conflict("busy"), StatusCode::CONFLICT, "conflict"),
            (ApiError::disconnected("gone"), StatusCode::SERVICE_UNAVAILABLE, "scanner_disconnected"),
            (ApiError::cache("disk full"), StatusCode::INTERNAL_SERVER_ERROR, "cache_error"),
        ] {
            assert_eq!((e.status, e.code), (status, code));
        }
        assert_eq!(ApiError::forbidden("start scans").title, "Your role can't start scans");
        assert!(ApiError::cache("disk full").hint.unwrap().contains("BRIDGE_DATA"));
    }

    #[tokio::test]
    async fn a_problem_responds_as_problem_json_with_its_status() {
        let res = ApiError::conflict("the queue is full").into_response();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "application/problem+json");
        let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!((v["code"].as_str(), v["detail"].as_str()), (Some("conflict"), Some("the queue is full")));
    }
}
