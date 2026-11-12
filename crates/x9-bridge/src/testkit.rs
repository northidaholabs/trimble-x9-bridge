//! Test harness: a bridge on an in-process mock scanner, called through its
//! router with no network between them except the mock's local port.
//!
//! ```ignore
//! let b = Bridge::new().await;
//! let (code, body) = b.call("GET", "/v1/whoami", Some(OPERATOR), None).await;
//! ```

use crate::model::{Caller, Role, Rules};
use crate::state::AppState;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

pub const VIEWER: &str = "viewer-token-0123456789abcdef";
pub const AGENT: &str = "agent-token-0123456789abcdef";
pub const OPERATOR: &str = "operator-token-0123456789abcdef";
pub const ADMIN: &str = "admin-token-0123456789abcdef";

pub struct Bridge {
    pub st: Arc<AppState>,
    pub app: Router,
    /// Holds `data/` and `recipes/`; removed when the bridge is dropped.
    pub dir: PathBuf,
    /// The mock scanner's API base.
    pub base: String,
}

/// A mock that runs fast: a full dome takes well under a second.
pub fn fast_mock() -> x9_mock::Config {
    x9_mock::Config { speed: 400.0, cancel_linger: 1.0, ..Default::default() }
}

impl Bridge {
    pub async fn new() -> Self {
        Self::with_mock(fast_mock()).await
    }

    pub async fn with_mock(cfg: x9_mock::Config) -> Self {
        let base = x9_mock::spawn(cfg).await;
        Self::on_scanner(&base)
    }

    /// A bridge on any scanner base, such as an unreachable one.
    pub fn on_scanner(base: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("x9-bridge-test-{}", uuid::Uuid::new_v4()));
        let (data, recipes) = (dir.join("data"), dir.join("recipes"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&recipes).unwrap();
        let tokens: HashMap<String, Caller> = [(VIEWER, "viewer", Role::Viewer), (AGENT, "agent", Role::Agent), (OPERATOR, "crew", Role::Operator), (ADMIN, "admin", Role::Admin)]
            .into_iter()
            .map(|(t, n, r)| (t.to_string(), Caller { name: n.to_string(), role: r }))
            .collect();
        let scanner = x9_client::Client::connect(base).unwrap();
        let st = Arc::new(AppState::new(scanner, tokens, Rules::default(), Duration::from_millis(20), data, recipes));
        Bridge { app: crate::routes::router(st.clone()), st, dir, base: base.to_string() }
    }

    /// Start the job worker, as `main` does. Without it, scans stay queued.
    pub fn start_worker(&self) {
        tokio::spawn(crate::worker::run(self.st.clone()));
    }

    /// Put a recipe file in the recipe directory.
    pub fn add_recipe(&self, file: &str, toml: &str) {
        std::fs::write(self.dir.join("recipes").join(file), toml).unwrap();
    }

    /// A request with an optional bearer token and JSON body.
    pub async fn request(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> Response {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap()
    }

    /// A request whose JSON reply (or `Null` for an empty or non-JSON body) comes back with its status.
    pub async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let res = self.request(method, path, token, body).await;
        let code = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 28).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// A request with a raw body (a CSV, say), whose JSON reply comes back with its status.
    pub async fn send(&self, method: &str, path: &str, token: Option<&str>, content_type: &str, body: &str) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", content_type);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = self.app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let code = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 28).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// A PUT of raw bytes; the reply's status, content type and body.
    pub async fn put_bytes(&self, path: &str, token: Option<&str>, body: Vec<u8>) -> (StatusCode, String, Vec<u8>) {
        let mut req = Request::builder().method("PUT").uri(path).header("content-type", "application/octet-stream");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = self.app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
        let code = res.status();
        let ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        (code, ct, axum::body::to_bytes(res.into_body(), 1 << 28).await.unwrap().to_vec())
    }

    /// A request whose raw reply body comes back with its status and content type.
    pub async fn bytes(&self, method: &str, path: &str, token: Option<&str>) -> (StatusCode, String, Vec<u8>) {
        let res = self.request(method, path, token, None).await;
        let code = res.status();
        let ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 28).await.unwrap();
        (code, ct, bytes.to_vec())
    }

    /// Poll `GET path` until `pred` holds, up to `secs` seconds.
    pub async fn wait_for(&self, path: &str, token: &str, secs: f64, pred: impl Fn(&Value) -> bool) -> Value {
        let end = std::time::Instant::now() + Duration::from_secs_f64(secs);
        loop {
            let (_, v) = self.call("GET", path, Some(token), None).await;
            if pred(&v) {
                return v;
            }
            assert!(std::time::Instant::now() < end, "timed out waiting on {path}: last {v}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn harness_reaches_the_mock_through_the_bridge() {
        let b = Bridge::new().await;
        let (code, me) = b.call("GET", "/v1/whoami", Some(OPERATOR), None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(me["role"], "operator");
        let (code, _) = b.call("GET", "/v1/whoami", None, None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, projects) = b.call("GET", "/v1/projects", Some(VIEWER), None).await;
        assert_eq!(code, StatusCode::OK, "{projects}");
    }
}
