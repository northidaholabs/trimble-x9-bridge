//! Unit tests for the HTTP routes: every route in `router()` called through
//! the router, on a bridge whose scanner is the in-process mock (`testkit`).
//!
//! Scan files can't come from the mock (it serves files only from a fixtures
//! directory), so the routes that read one are tested through the bridge's own
//! caches: a synthetic preview JPEG, placeholder scan files and views written
//! where the bridge keeps them, and a depth view built from synthetic points.

use crate::model::{Caller, Role};
use crate::testkit::*;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// The mock's own project and its one scan (a full dome, station 1).
const MOCK_PROJECT: &str = "39187455-8278-428c-89ad-8a4835f95be6";
const MOCK_SCAN: &str = "5b5314cf-76b8-4f59-a445-ceee1e6df4c3";
/// A well-formed scan id the scanner doesn't list.
const NO_SCAN: &str = "0000aaaa-0000-4000-8000-000000000000";
const FUTURE: &str = "2099-01-01T00:00:00Z";

// ------------------------------------------------------------------ helpers ---

/// Any request: raw body bytes and extra headers. Returns status, headers and body.
async fn send(b: &Bridge, method: &str, path: &str, token: Option<&str>, headers: &[(&str, &str)], body: Vec<u8>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = b.app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let (code, hs) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 28).await.unwrap();
    (code, hs, bytes.to_vec())
}

/// A request with a raw body, its JSON reply (or `Null`).
async fn send_json(b: &Bridge, method: &str, path: &str, token: Option<&str>, headers: &[(&str, &str)], body: &[u8]) -> (StatusCode, Value) {
    let (code, _, bytes) = send(b, method, path, token, headers, body.to_vec()).await;
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The reply is a problem document with this status and code.
#[track_caller]
fn problem(got: (StatusCode, Value), status: StatusCode, code: &str) -> Value {
    let (s, v) = got;
    assert_eq!(s, status, "{v}");
    assert_eq!(v["code"], code, "{v}");
    assert_eq!(v["status"], status.as_u16(), "{v}");
    assert!(v["type"].as_str().is_some_and(|t| t.ends_with(code)), "{v}");
    v
}

/// The reply is 200 (or `status`) with a JSON body.
#[track_caller]
fn ok(got: (StatusCode, Value), status: StatusCode) -> Value {
    assert_eq!(got.0, status, "{}", got.1);
    got.1
}

fn data(b: &Bridge) -> std::path::PathBuf {
    b.dir.join("data")
}

/// Write a file under the bridge's data directory.
fn plant(b: &Bridge, rel: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = data(b).join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p
}

/// A flat mid-grey baseline JPEG, `w × h`: every 8×8 block has only a zero DC
/// difference and an end-of-block, each coded as the one 1-bit Huffman code.
fn grey_jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    j.extend([0xFF, 0xDB, 0x00, 0x43, 0x00]);
    j.extend([1u8; 64]);
    j.extend([0xFF, 0xC0, 0x00, 0x0B, 0x08]);
    j.extend(h.to_be_bytes());
    j.extend(w.to_be_bytes());
    j.extend([0x01, 0x01, 0x11, 0x00]);
    for class in [0x00u8, 0x10] {
        j.extend([0xFF, 0xC4, 0x00, 0x14, class, 1]);
        j.extend([0u8; 15]);
        j.push(0x00);
    }
    j.extend([0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
    let bits = (w as usize).div_ceil(8) * (h as usize).div_ceil(8) * 2;
    j.extend(vec![0u8; bits / 8]);
    if bits % 8 != 0 {
        j.push(0xFF >> (bits % 8));
    }
    j.extend([0xFF, 0xD9]);
    j
}

const PREVIEW: (u16, u16) = (400, 200);

/// The mock scan's preview (a synthetic JPEG) and a placeholder scan file that
/// doesn't decode, in the bridge's cache.
fn plant_scan_files(b: &Bridge, scan_id: &str) {
    plant(b, &format!("scans/{scan_id}.jpg"), &grey_jpeg(PREVIEW.0, PREVIEW.1));
    plant(b, &format!("scans/{scan_id}.tzf"), b"not a tzf file");
}

/// A dome inside a round room: walls 5 m out, the floor 1.5 m down.
fn room_depth(preview: (usize, usize)) -> crate::view3d::DepthView {
    use std::f64::consts::{FRAC_PI_3, TAU};
    use trimble_tzf::{GridPoint, Point};
    let (cols, rows) = (360u32, 120u32);
    let mut pts = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let az = c as f64 / cols as f64 * TAU;
            let el = FRAC_PI_3 - r as f64 / rows as f64 * 2.0 * FRAC_PI_3;
            let dir = [el.cos() * az.cos(), el.cos() * az.sin(), el.sin()];
            let wall = 5.0 / el.cos();
            let t = if el < -1e-6 { wall.min(-1.5 / el.sin()) } else { wall };
            let point = Point { x: (t * dir[0]) as f32, y: (t * dir[1]) as f32, z: (t * dir[2]) as f32, intensity: 0.5 };
            pts.push(GridPoint { point, row: r, col: c });
        }
    }
    crate::view3d::DepthView::build(&pts, (cols, rows), preview).expect("the synthetic room maps onto its preview")
}

/// A registration of the mock's project: its one scan as the frame station,
/// one wall from (0, 0) to (5, 0), and the merged points that saw it.
fn register_mock_project(b: &Bridge) {
    let reg: crate::register::Registration = serde_json::from_value(json!({
        "project_id": MOCK_PROJECT,
        "frame_scan_id": MOCK_SCAN,
        "stations": [{"scan_id": MOCK_SCAN, "station": 1, "pose": {"turn_deg": 90.0, "xyz_m": [1.0, 2.0, 0.0]},
                      "audit": {"verdict": "frame", "ties": [], "reasons": []}}],
        "pairs": [],
        "walls": [{"wall_id": "w1", "from": [0.0, 0.0], "to": [5.0, 0.0], "length_m": 5.0, "z_m": [0.0, 2.5],
                   "seen_by": [1], "kind": "building", "flatness": 1.0}],
        "summary": "One station: it is the project frame.",
        "updated_at": "2026-01-01T00:00:00Z",
    }))
    .unwrap();
    let mut m = crate::register::Merged::default();
    for i in 0..=50 {
        for k in 0..=25 {
            let (x, z) = (i as f32 * 0.1, k as f32 * 0.1);
            m.pts.push([x, 0.0, z]);
            m.spacing_mm.push(5.0);
            m.station.push(1);
            m.plan.push([x, 0.0]);
        }
    }
    m.ground.insert((1, 1), -1.5);
    let mut inner = b.st.inner.lock().unwrap();
    inner.registrations.insert(MOCK_PROJECT.into(), reg);
    inner.merged.insert(MOCK_PROJECT.into(), Arc::new(m));
}

/// Poll until `pred` holds on a job (up to `secs`).
async fn wait_job(b: &Bridge, job_id: &str, secs: f64, pred: impl Fn(&Value) -> bool) -> Value {
    b.wait_for(&format!("/v1/jobs/{job_id}"), VIEWER, secs, pred).await
}

fn finished(j: &Value) -> bool {
    matches!(j["state"].as_str(), Some("succeeded" | "failed" | "cancelled"))
}

async fn new_project(b: &Bridge, name: &str) -> String {
    let p = ok(b.call("POST", "/v1/projects", Some(AGENT), Some(json!({"name": name}))).await, StatusCode::CREATED);
    p["project_id"].as_str().unwrap().to_string()
}

fn dome(project_id: &str) -> Value {
    json!({"project_id": project_id, "kind": "full_dome", "mode": "STD_L0", "note": "station 1"})
}

// --------------------------------------------------------------------- auth ---

/// Who a route lets in.
#[derive(Clone, Copy, Debug)]
enum Need {
    /// Any token.
    Any,
    /// Any role but viewer.
    Agent,
    /// Operator or admin.
    Operator,
}

/// Every route in `router()` but the UI and health, with the role it needs.
const ROUTES: &[(&str, &str, Need)] = &[
    ("GET", "/v1/whoami", Need::Any),
    ("GET", "/v1/status", Need::Any),
    ("GET", "/v1/rules", Need::Any),
    ("GET", "/v1/scan-modes", Need::Any),
    ("GET", "/v1/projects", Need::Any),
    ("POST", "/v1/projects", Need::Agent),
    ("GET", "/v1/scans", Need::Any),
    ("POST", "/v1/scans", Need::Agent),
    ("GET", "/v1/scans/S", Need::Any),
    ("DELETE", "/v1/scans/S", Need::Operator),
    ("GET", "/v1/scans/S/preview", Need::Any),
    ("GET", "/v1/scans/S/tzf", Need::Any),
    ("GET", "/v1/scans/S/view", Need::Any),
    ("GET", "/v1/scans/S/coverage", Need::Any),
    ("GET", "/v1/scans/S/coverage/image", Need::Any),
    ("GET", "/v1/scans/S/next-station", Need::Any),
    ("PUT", "/v1/scans/S/next-station", Need::Agent),
    ("GET", "/v1/scans/S/next-station/image", Need::Any),
    ("GET", "/v1/scans/S/next-station/plan", Need::Any),
    ("POST", "/v1/scans/S/project", Need::Any),
    ("GET", "/v1/next-stations", Need::Any),
    ("GET", "/v1/missions", Need::Any),
    ("GET", "/v1/projects/P/registration", Need::Any),
    ("POST", "/v1/projects/P/registration", Need::Agent),
    ("GET", "/v1/projects/P/registration/plan", Need::Any),
    ("POST", "/v1/projects/P/registration/face", Need::Any),
    ("GET", "/v1/projects/P/minimap", Need::Any),
    ("GET", "/v1/workers", Need::Any),
    ("POST", "/v1/work/claim", Need::Agent),
    ("POST", "/v1/work/J/progress", Need::Agent),
    ("PUT", "/v1/work/J/merged", Need::Agent),
    ("POST", "/v1/work/J/result", Need::Agent),
    ("POST", "/v1/work/J/fail", Need::Agent),
    ("GET", "/v1/projects/P/mission", Need::Any),
    ("PUT", "/v1/projects/P/mission", Need::Agent),
    ("POST", "/v1/projects/P/mission/targets", Need::Agent),
    ("POST", "/v1/projects/P/mission/targets/T/ask", Need::Agent),
    ("GET", "/v1/projects/P/mission/targets/T/ask/image", Need::Any),
    ("POST", "/v1/projects/P/mission/ready", Need::Agent),
    ("POST", "/v1/projects/P/mission/sign-off", Need::Operator),
    ("POST", "/v1/scans/S/exports", Need::Agent),
    ("GET", "/v1/exports/E", Need::Any),
    ("GET", "/v1/exports/E/file", Need::Any),
    ("POST", "/v1/scans/S/measurements", Need::Agent),
    ("GET", "/v1/measurements/M", Need::Any),
    ("GET", "/v1/measurements/M/evidence", Need::Any),
    ("POST", "/v1/aim", Need::Any),
    ("GET", "/v1/recipes", Need::Any),
    ("GET", "/v1/recipes/R", Need::Any),
    ("GET", "/v1/runs", Need::Any),
    ("POST", "/v1/runs", Need::Agent),
    ("GET", "/v1/runs/R", Need::Any),
    ("POST", "/v1/runs/R/advance", Need::Agent),
    ("POST", "/v1/runs/R/propose", Need::Agent),
    ("POST", "/v1/runs/R/cancel", Need::Agent),
    ("GET", "/v1/jobs", Need::Any),
    ("GET", "/v1/jobs/J", Need::Any),
    ("POST", "/v1/jobs/J/cancel", Need::Agent),
    ("POST", "/v1/jobs/J/approve", Need::Operator),
    ("GET", "/v1/grants", Need::Any),
    ("POST", "/v1/grants", Need::Operator),
    ("DELETE", "/v1/grants/G", Need::Operator),
    ("GET", "/v1/log", Need::Any),
    ("POST", "/v1/log", Need::Agent),
    ("GET", "/v1/events", Need::Any),
];

fn concrete(path: &str) -> String {
    path.replace("/S/", &format!("/{MOCK_SCAN}/"))
        .replace("/S", &format!("/{MOCK_SCAN}"))
        .replace("/P/", &format!("/{MOCK_PROJECT}/"))
        .replace("/J/", "/job-x/")
        .replace("/J", "/job-x")
        .replace("/T/", "/target-x/")
        .replace("/E", "/export-x")
        .replace("/M", "/measurement-x")
        .replace("/R/", "/run-x/")
        .replace("/R", "/run-x")
        .replace("/G", "/grant-x")
}

#[tokio::test]
async fn every_route_without_a_token_is_401_unauthorized() {
    let b = Bridge::new().await;
    for (method, path, _) in ROUTES {
        let path = concrete(path);
        let got = send_json(&b, method, &path, None, &[("content-type", "application/json")], b"{}").await;
        assert_eq!(got.0, StatusCode::UNAUTHORIZED, "{method} {path}: {}", got.1);
        assert_eq!(got.1["code"], "unauthorized", "{method} {path}");
    }
}

#[tokio::test]
async fn an_unknown_token_or_another_scheme_is_401() {
    let b = Bridge::new().await;
    let got = b.call("GET", "/v1/whoami", Some("not-a-token"), None).await;
    problem(got, StatusCode::UNAUTHORIZED, "unauthorized");
    let got = send_json(&b, "GET", "/v1/whoami", None, &[("authorization", &format!("Basic {VIEWER}"))], b"").await;
    problem(got, StatusCode::UNAUTHORIZED, "unauthorized");
    // Errors are problem documents.
    let (_, hs, _) = send(&b, "GET", "/v1/status", None, &[], vec![]).await;
    assert_eq!(hs["content-type"], "application/problem+json");
}

#[tokio::test]
async fn a_role_below_the_routes_need_is_403_forbidden_before_the_body_is_read() {
    let b = Bridge::new().await;
    for (method, path, need) in ROUTES {
        let path = concrete(path);
        let below: &[&str] = match need {
            Need::Any => continue,
            Need::Agent => &[VIEWER],
            Need::Operator => &[VIEWER, AGENT],
        };
        for tok in below {
            // A malformed body still gets 403: the role is checked first.
            let got = send_json(&b, method, &path, Some(tok), &[("content-type", "application/json")], b"{not json").await;
            assert_eq!(got.0, StatusCode::FORBIDDEN, "{method} {path} as {tok}: {}", got.1);
            assert_eq!(got.1["code"], "forbidden", "{method} {path}");
            assert!(got.1["title"].as_str().unwrap().starts_with("Your role can't"), "{}", got.1);
        }
    }
}

// ------------------------------------------------------------- system & ui ---

#[tokio::test]
async fn health_needs_no_token_and_names_the_version() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/health", None, None).await, StatusCode::OK);
    assert_eq!(v, json!({"ok": true, "version": crate::routes::VERSION}));
}

#[tokio::test]
async fn whoami_names_each_tokens_caller_and_role() {
    let b = Bridge::new().await;
    for (tok, name, role) in [(VIEWER, "viewer", "viewer"), (AGENT, "agent", "agent"), (OPERATOR, "crew", "operator"), (ADMIN, "admin", "admin")] {
        let v = ok(b.call("GET", "/v1/whoami", Some(tok), None).await, StatusCode::OK);
        assert_eq!(v, json!({"name": name, "role": role}));
    }
}

#[tokio::test]
async fn status_maps_the_scanner_and_the_bridge() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/status", Some(VIEWER), None).await, StatusCode::OK);
    let s = &v["scanner"];
    assert_eq!(s["connected"], true);
    assert_eq!(s["battery_pct"], 87);
    assert_eq!(s["busy"], false);
    assert_eq!(s["current_job_id"], Value::Null);
    assert_eq!(s["leveling"]["state"], "ok");
    assert_eq!(v["bridge"]["version"], crate::routes::VERSION);
    assert_eq!(v["bridge"]["queue_length"], 0);
    // A queued scan counts in the queue.
    ok(b.call("POST", "/v1/scans", Some(OPERATOR), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    ok(b.call("POST", "/v1/scans", Some(AGENT), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    let v = ok(b.call("GET", "/v1/status", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["bridge"]["queue_length"], 2);
}

#[tokio::test]
async fn rules_are_the_bridges_settings() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/rules", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["require_approval_for_scans"], true);
    assert_eq!(v["max_queue_length"], 20);
    assert_eq!(v["min_battery_pct_to_scan"], 15);
}

#[tokio::test]
async fn scan_modes_are_translated() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/scan-modes", Some(VIEWER), None).await, StatusCode::OK);
    let modes: Vec<&str> = v.as_array().unwrap().iter().filter_map(|m| m["mode"].as_str()).collect();
    assert!(modes.contains(&"STD_L0") && modes.contains(&"STD_L2"), "{v}");
    assert!(v[0].get("range_m").is_some() && v[0].get("full_dome_seconds").is_some(), "{v}");
}

#[tokio::test]
async fn an_unreachable_scanner_reads_disconnected_and_refuses_scanner_work_with_503() {
    // Port 9 (discard) on this machine: nothing listens, the connection is refused.
    let b = Bridge::on_scanner("http://127.0.0.1:9/api/v1");
    let v = ok(b.call("GET", "/v1/status", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["scanner"], json!({"connected": false}));
    for path in ["/v1/scan-modes", "/v1/projects", "/v1/scans", &format!("/v1/scans/{MOCK_SCAN}"), "/v1/missions"] {
        let got = b.call("GET", path, Some(VIEWER), None).await;
        if path == "/v1/missions" {
            assert_eq!(got.0, StatusCode::OK, "missions need no scanner");
            continue;
        }
        let v = problem(got, StatusCode::SERVICE_UNAVAILABLE, "scanner_disconnected");
        assert!(v["hint"].is_string(), "{v}");
    }
    let got = b.call("POST", "/v1/scans", Some(OPERATOR), Some(dome(MOCK_PROJECT))).await;
    problem(got, StatusCode::SERVICE_UNAVAILABLE, "scanner_disconnected");
    let got = b.call("POST", "/v1/projects", Some(AGENT), Some(json!({"name": "x"}))).await;
    problem(got, StatusCode::SERVICE_UNAVAILABLE, "scanner_disconnected");
}

#[tokio::test]
async fn the_root_and_ui_lead_to_the_web_ui_without_a_token() {
    let b = Bridge::new().await;
    for path in ["/", "/ui"] {
        let (code, hs, _) = send(&b, "GET", path, None, &[], vec![]).await;
        assert!(code.is_redirection(), "{path}: {code}");
        assert_eq!(hs["location"], "/ui/");
    }
    let (code, ct, body) = b.bytes("GET", "/ui/", None).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "text/html; charset=utf-8"));
    assert!(String::from_utf8_lossy(&body).contains("app.js"));
    let (code, ct, body) = b.bytes("GET", "/ui/app.js", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(ct.contains("javascript"), "{ct}");
    assert!(String::from_utf8_lossy(&body).contains("/whoami"));
    let (code, ct, _) = b.bytes("GET", "/ui/icon.svg", None).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/svg+xml"));
    let (code, _, _) = b.bytes("GET", "/ui/nope.js", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_event_stream_is_server_sent_events() {
    let b = Bridge::new().await;
    let res = b.request("GET", "/v1/events", Some(VIEWER), None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    drop(res);
}

// ----------------------------------------------------------------- projects ---

#[tokio::test]
async fn projects_are_created_listed_and_logged() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/projects", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v, json!([{"project_id": MOCK_PROJECT, "name": "mock_project", "scan_count": 1}]));

    let p = ok(b.call("POST", "/v1/projects", Some(AGENT), Some(json!({"name": "  north-house  "}))).await, StatusCode::CREATED);
    assert_eq!(p["name"], "north-house", "the name is trimmed");
    let pid = p["project_id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(pid).is_ok(), "the bridge makes a UUID: {pid}");

    let v = ok(b.call("GET", "/v1/projects", Some(VIEWER), None).await, StatusCode::OK);
    assert!(v.as_array().unwrap().iter().any(|x| x["project_id"] == pid && x["name"] == "north-house"), "{v}");
    let log = ok(b.call("GET", &format!("/v1/log?project_id={pid}"), Some(VIEWER), None).await, StatusCode::OK);
    assert!(log[0]["text"].as_str().unwrap().contains("created"), "{log}");
    assert_eq!(log[0]["author"], "agent");

    let got = b.call("POST", "/v1/projects", Some(OPERATOR), Some(json!({"name": "north-house"}))).await;
    let v = problem(got, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("already exists"), "{v}");
}

#[tokio::test]
async fn a_project_name_must_be_1_to_64_bytes_without_a_slash() {
    let b = Bridge::new().await;
    for name in ["", "   ", "a/b", &"x".repeat(65)] {
        let got = b.call("POST", "/v1/projects", Some(AGENT), Some(json!({"name": name}))).await;
        problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    }
    let got = b.call("POST", "/v1/projects", Some(AGENT), Some(json!({"title": "no name"}))).await;
    problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    let got = send_json(&b, "POST", "/v1/projects", Some(AGENT), &[], b"not json").await;
    problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    ok(b.call("POST", "/v1/projects", Some(AGENT), Some(json!({"name": "x".repeat(64)}))).await, StatusCode::CREATED);
}

// -------------------------------------------------------------------- scans ---

#[tokio::test]
async fn scans_are_listed_filtered_and_got_by_id() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/scans", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let s = &v[0];
    assert_eq!((s["scan_id"].as_str(), s["project_id"].as_str(), s["kind"].as_str(), s["station"].as_i64()),
               (Some(MOCK_SCAN), Some(MOCK_PROJECT), Some("full_dome"), Some(1)));
    let v = ok(b.call("GET", &format!("/v1/scans?project_id={MOCK_PROJECT}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);
    let v = ok(b.call("GET", "/v1/scans?project_id=elsewhere", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v, json!([]));
    let v = ok(b.call("GET", &format!("/v1/scans/{MOCK_SCAN}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["scan_id"], MOCK_SCAN);
    assert_eq!(v["point_count"], 24844238u64);
    problem(b.call("GET", &format!("/v1/scans/{NO_SCAN}"), Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn a_scan_request_is_validated_before_it_is_admitted() {
    let b = Bridge::new().await;
    let pid = MOCK_PROJECT;
    let got = b.call("POST", "/v1/scans", Some(AGENT), Some(json!({"project_id": pid, "kind": "area", "mode": "STD_L0"}))).await;
    let v = problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("window"), "{v}");
    let got = b.call("POST", "/v1/scans", Some(AGENT), Some(json!({"project_id": pid, "kind": "full_dome", "mode": "STD_L0",
                                                                     "window": {"start_ha": 0, "delta_ha": 1, "start_va": 1, "delta_va": 1}}))).await;
    problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    let got = b.call("POST", "/v1/scans", Some(OPERATOR), Some(json!({"project_id": pid, "kind": "full_dome", "mode": "STD_L9"}))).await;
    let v = problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("STD_L0"), "the valid modes are listed: {v}");
    let got = b.call("POST", "/v1/scans", Some(OPERATOR), Some(json!({"project_id": pid, "kind": "full_dome", "mode": " "}))).await;
    problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    let got = b.call("POST", "/v1/scans", Some(OPERATOR), Some(json!({"project_id": "nope", "kind": "full_dome", "mode": "STD_L0"}))).await;
    problem(got, StatusCode::NOT_FOUND, "not_found");
    let got = b.call("POST", "/v1/scans", Some(OPERATOR), Some(json!({"bogus": true}))).await;
    problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    assert_eq!(ok(b.call("GET", "/v1/jobs", Some(VIEWER), None).await, StatusCode::OK), json!([]), "nothing was queued");
}

#[tokio::test]
async fn an_operators_scan_is_queued_and_a_full_queue_is_a_conflict() {
    let b = Bridge::new().await;
    for i in 0..20 {
        let j = ok(b.call("POST", "/v1/scans", Some(OPERATOR), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
        assert_eq!(j["state"], "queued", "scan {i}: {j}");
        assert_eq!(j["kind"], "scan");
        assert_eq!(j["requested_by"], "crew");
        assert_eq!(j["project_name"], "mock_project");
        assert_eq!(j["request"]["mode"], "STD_L0");
    }
    let v = problem(b.call("POST", "/v1/scans", Some(OPERATOR), Some(dome(MOCK_PROJECT))).await, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("queue is full"), "{v}");
    let jobs = ok(b.call("GET", "/v1/jobs", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(jobs.as_array().unwrap().len(), 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_scan_waits_for_approval_then_runs_on_the_scanner_and_finishes() {
    let b = Bridge::new().await;
    b.start_worker();
    let pid = new_project(&b, "approval-flow").await;
    let job = ok(b.call("POST", "/v1/scans", Some(AGENT), Some(dome(&pid))).await, StatusCode::ACCEPTED);
    assert_eq!(job["state"], "awaiting_approval", "{job}");
    assert_eq!(job["project_name"], "approval-flow");
    let id = job["job_id"].as_str().unwrap().to_string();
    // The worker leaves it alone until a person approves it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let j = ok(b.call("GET", &format!("/v1/jobs/{id}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(j["state"], "awaiting_approval");

    problem(b.call("POST", &format!("/v1/jobs/{id}/approve"), Some(AGENT), None).await, StatusCode::FORBIDDEN, "forbidden");
    let a = ok(b.call("POST", &format!("/v1/jobs/{id}/approve"), Some(OPERATOR), None).await, StatusCode::OK);
    assert_eq!(a["state"], "queued");
    let again = problem(b.call("POST", &format!("/v1/jobs/{id}/approve"), Some(OPERATOR), None).await, StatusCode::CONFLICT, "conflict");
    assert!(again["detail"].as_str().unwrap().ends_with("not awaiting approval"), "{again}");

    let done = wait_job(&b, &id, 10.0, finished).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let scan_id = done["result"]["scan_id"].as_str().unwrap();
    let sc = ok(b.call("GET", &format!("/v1/scans/{scan_id}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!((sc["project_id"].as_str(), sc["kind"].as_str()), (Some(pid.as_str()), Some("full_dome")));
    assert!(sc["timings"].is_object(), "the bridge keeps the scan's timings: {sc}");
    let log = ok(b.call("GET", &format!("/v1/log?project_id={pid}"), Some(VIEWER), None).await, StatusCode::OK);
    let texts: Vec<&str> = log.as_array().unwrap().iter().filter_map(|e| e["text"].as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("awaiting approval") && t.contains("station 1")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("approved")), "{texts:?}");
    // Every job, newest first.
    let jobs = ok(b.call("GET", "/v1/jobs", Some(VIEWER), None).await, StatusCode::OK);
    assert!(jobs.as_array().unwrap().iter().any(|j| j["job_id"] == id.as_str()), "{jobs}");
}

#[tokio::test]
async fn approve_refuses_an_unknown_job_and_a_malformed_body() {
    let b = Bridge::new().await;
    problem(b.call("POST", "/v1/jobs/job-x/approve", Some(OPERATOR), None).await, StatusCode::NOT_FOUND, "not_found");
    let job = ok(b.call("POST", "/v1/scans", Some(AGENT), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    let path = format!("/v1/jobs/{}/approve", job["job_id"].as_str().unwrap());
    problem(send_json(&b, "POST", &path, Some(OPERATOR), &[], b"{\"run\": 3}").await, StatusCode::BAD_REQUEST, "invalid_request");
    // A body of only whitespace is the same as none.
    let v = ok(send_json(&b, "POST", &path, Some(ADMIN), &[], b"  \n").await, StatusCode::OK);
    assert_eq!(v["state"], "queued");
}

#[tokio::test]
async fn jobs_and_their_404s() {
    let b = Bridge::new().await;
    problem(b.call("GET", "/v1/jobs/job-x", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("POST", "/v1/jobs/job-x/cancel", Some(AGENT), None).await, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn an_agent_cancels_only_its_own_jobs_and_an_operator_any() {
    let b = Bridge::new().await;
    let theirs = ok(b.call("POST", "/v1/scans", Some(OPERATOR), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    let theirs = theirs["job_id"].as_str().unwrap();
    let got = b.call("POST", &format!("/v1/jobs/{theirs}/cancel"), Some(AGENT), None).await;
    problem(got, StatusCode::FORBIDDEN, "forbidden");

    let mine = ok(b.call("POST", "/v1/scans", Some(AGENT), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    let mine = mine["job_id"].as_str().unwrap();
    let c = ok(b.call("POST", &format!("/v1/jobs/{mine}/cancel"), Some(AGENT), None).await, StatusCode::OK);
    assert_eq!(c["state"], "cancelled");
    assert!(c["finished_at"].is_string(), "{c}");
    let c = ok(b.call("POST", &format!("/v1/jobs/{theirs}/cancel"), Some(OPERATOR), None).await, StatusCode::OK);
    assert_eq!(c["state"], "cancelled");
    // Cancelling a finished job leaves it as it was.
    let c = ok(b.call("POST", &format!("/v1/jobs/{mine}/cancel"), Some(ADMIN), None).await, StatusCode::OK);
    assert_eq!(c["state"], "cancelled");
    // A cancelled job can't be approved.
    problem(b.call("POST", &format!("/v1/jobs/{mine}/approve"), Some(OPERATOR), None).await, StatusCode::CONFLICT, "conflict");
    let log = ok(b.call("GET", "/v1/log", Some(VIEWER), None).await, StatusCode::OK);
    assert!(log.as_array().unwrap().iter().any(|e| e["text"].as_str().unwrap().starts_with("Cancel requested")), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_running_scan_asks_the_worker_to_stop_it() {
    let b = Bridge::with_mock(x9_mock::Config { speed: 20.0, cancel_linger: 1.0, ..Default::default() }).await;
    b.start_worker();
    let job = ok(b.call("POST", "/v1/scans", Some(OPERATOR), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    let id = job["job_id"].as_str().unwrap().to_string();
    wait_job(&b, &id, 5.0, |j| j["state"] != "queued").await;
    let st = ok(b.call("GET", "/v1/status", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(st["scanner"]["current_job_id"], id.as_str(), "{st}");
    ok(b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(OPERATOR), None).await, StatusCode::OK);
    let j = wait_job(&b, &id, 5.0, finished).await;
    assert_eq!(j["state"], "cancelled", "{j}");
}

#[tokio::test]
async fn delete_needs_an_operator_and_the_scan_id_repeated_in_x_confirm() {
    let b = Bridge::new().await;
    plant_scan_files(&b, MOCK_SCAN);
    let path = format!("/v1/scans/{MOCK_SCAN}");
    let v = problem(send_json(&b, "DELETE", &path, Some(OPERATOR), &[], b"").await, StatusCode::PRECONDITION_FAILED, "confirm_mismatch");
    assert!(v["hint"].as_str().unwrap().contains("X-Confirm"), "{v}");
    problem(send_json(&b, "DELETE", &path, Some(OPERATOR), &[("x-confirm", NO_SCAN)], b"").await, StatusCode::PRECONDITION_FAILED, "confirm_mismatch");
    let other = format!("/v1/scans/{NO_SCAN}");
    problem(send_json(&b, "DELETE", &other, Some(OPERATOR), &[("x-confirm", NO_SCAN)], b"").await, StatusCode::NOT_FOUND, "not_found");

    let (code, _, body) = send(&b, "DELETE", &path, Some(ADMIN), &[("x-confirm", MOCK_SCAN)], vec![]).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    problem(b.call("GET", &path, Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    assert!(!data(&b).join(format!("scans/{MOCK_SCAN}.jpg")).exists(), "the cached preview is evicted");
    assert!(!data(&b).join(format!("scans/{MOCK_SCAN}.tzf")).exists(), "the cached file is evicted");
    let log = ok(b.call("GET", "/v1/log", Some(VIEWER), None).await, StatusCode::OK);
    assert!(log.as_array().unwrap().iter().any(|e| e["text"] == format!("Scan {MOCK_SCAN} deleted")), "{log}");
}

// ------------------------------------------------------------------- grants ---

#[tokio::test]
async fn a_grant_covers_an_agents_scan_and_a_cancel_before_it_starts_gives_the_use_back() {
    let b = Bridge::new().await;
    let req = json!({"project_id": MOCK_PROJECT, "expires_at": FUTURE, "max_scans": 1, "allowed_modes": ["STD_L0"], "note": "asked in the chat"});
    let g = ok(b.call("POST", "/v1/grants", Some(OPERATOR), Some(req)).await, StatusCode::CREATED);
    assert_eq!((g["scans_used"].as_u64(), g["granted_by"].as_str(), g["max_scans"].as_u64()), (Some(0), Some("crew"), Some(1)));
    assert!(g["grant_id"].as_str().unwrap().starts_with("grant"), "{g}");
    let gid = g["grant_id"].as_str().unwrap().to_string();
    assert!(data(&b).join("grants.json").exists(), "grants are saved for a restart");

    let job = ok(b.call("POST", "/v1/scans", Some(AGENT), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    assert_eq!(job["state"], "queued", "the grant covers it: {job}");
    let used = |gl: &Value| gl.as_array().unwrap().iter().find(|x| x["grant_id"] == gid.as_str()).map(|x| x["scans_used"].clone());
    let gl = ok(b.call("GET", "/v1/grants", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(used(&gl), Some(json!(1)));

    // Used up: the next one waits.
    let next = ok(b.call("POST", "/v1/scans", Some(AGENT), Some(dome(MOCK_PROJECT))).await, StatusCode::ACCEPTED);
    assert_eq!(next["state"], "awaiting_approval");

    let c = ok(b.call("POST", &format!("/v1/jobs/{}/cancel", job["job_id"].as_str().unwrap()), Some(AGENT), None).await, StatusCode::OK);
    assert_eq!(c["state"], "cancelled");
    let gl = ok(b.call("GET", "/v1/grants", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(used(&gl), Some(json!(0)), "the use is given back");

    let (code, _, _) = send(&b, "DELETE", &format!("/v1/grants/{gid}"), Some(OPERATOR), &[], vec![]).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let gl = ok(b.call("GET", "/v1/grants", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(gl, json!([]));
    problem(b.call("DELETE", &format!("/v1/grants/{gid}"), Some(OPERATOR), None).await, StatusCode::NOT_FOUND, "not_found");
    let log = ok(b.call("GET", "/v1/log", Some(VIEWER), None).await, StatusCode::OK);
    let texts: Vec<&str> = log.as_array().unwrap().iter().filter_map(|e| e["text"].as_str()).collect();
    assert!(texts.iter().any(|t| t.starts_with("Grant: up to 1 scans until") && t.ends_with("(asked in the chat)")), "{texts:?}");
    assert!(texts.iter().any(|t| *t == format!("Grant {gid} revoked")), "{texts:?}");
}

#[tokio::test]
async fn a_grant_needs_a_future_rfc3339_expiry() {
    let b = Bridge::new().await;
    for when in ["tomorrow", "2001-01-01T00:00:00Z"] {
        let got = b.call("POST", "/v1/grants", Some(OPERATOR), Some(json!({"project_id": MOCK_PROJECT, "expires_at": when}))).await;
        problem(got, StatusCode::BAD_REQUEST, "invalid_request");
    }
    problem(b.call("POST", "/v1/grants", Some(OPERATOR), Some(json!({"project_id": MOCK_PROJECT}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let g = ok(b.call("POST", "/v1/grants", Some(ADMIN), Some(json!({"project_id": MOCK_PROJECT, "expires_at": FUTURE}))).await, StatusCode::CREATED);
    assert_eq!(g["max_scans"], Value::Null);
    let log = ok(b.call("GET", "/v1/log", Some(VIEWER), None).await, StatusCode::OK);
    assert!(log[0]["text"].as_str().unwrap().starts_with("Grant: scans until"), "{log}");
}

// ---------------------------------------------------------------------- log ---

#[tokio::test]
async fn the_log_takes_notes_and_filters_by_project_and_time() {
    let b = Bridge::new().await;
    let e = ok(b.call("POST", "/v1/log", Some(AGENT), Some(json!({"text": "Plant moved out of the way", "project_id": "p1"}))).await, StatusCode::CREATED);
    assert_eq!((e["author"].as_str(), e["kind"].as_str(), e["project_id"].as_str()), (Some("agent"), Some("note"), Some("p1")));
    assert!(e["entry_id"].is_string() && e["at"].is_string() && e["refs"].is_object(), "{e}");
    ok(b.call("POST", "/v1/log", Some(OPERATOR), Some(json!({"text": "Lunch"}))).await, StatusCode::CREATED);

    let all = ok(b.call("GET", "/v1/log", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 2);
    let p1 = ok(b.call("GET", "/v1/log?project_id=p1", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(p1.as_array().unwrap().len(), 1);
    let since = ok(b.call("GET", "/v1/log?since=2000-01-01T00:00:00Z", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(since.as_array().unwrap().len(), 2);
    let later = ok(b.call("GET", &format!("/v1/log?since={FUTURE}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(later, json!([]));
    problem(b.call("GET", "/v1/log?since=yesterday", Some(VIEWER), None).await, StatusCode::BAD_REQUEST, "invalid_request");
}

#[tokio::test]
async fn a_log_note_is_1_to_4000_characters_not_only_spaces() {
    let b = Bridge::new().await;
    for text in ["", "   ", &"x".repeat(4001)] {
        let v = problem(b.call("POST", "/v1/log", Some(AGENT), Some(json!({"text": text}))).await, StatusCode::BAD_REQUEST, "invalid_request");
        assert!(v["detail"].as_str().unwrap().contains("1–4000"), "{v}");
    }
    problem(b.call("POST", "/v1/log", Some(AGENT), Some(json!({"note": "x"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
}

// ---------------------------------------------------------------- missions ---

#[tokio::test]
async fn a_mission_is_set_its_targets_settled_ready_and_signed_off() {
    let b = Bridge::new().await;
    let path = format!("/v1/projects/{MOCK_PROJECT}/mission");
    problem(b.call("GET", &path, Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    let m = ok(b.call("PUT", &path, Some(AGENT), Some(json!({
        "goal": "Scan the exterior of the house", "kind": "exterior", "done_when": "Every facade seen",
        "targets": [{"target_id": "north", "name": "North facade", "kind": "facade"}]}))).await, StatusCode::OK);
    assert_eq!((m["state"].as_str(), m["created_by"].as_str(), m["project_id"].as_str()), (Some("active"), Some("agent"), Some(MOCK_PROJECT)));
    assert_eq!(m["targets"][0]["status"], "open");
    assert!(data(&b).join("missions.json").exists());
    let got = ok(b.call("GET", &path, Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(got["mission_id"], m["mission_id"]);
    let all = ok(b.call("GET", "/v1/missions", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(all.as_array().unwrap().len(), 1);

    let ready = format!("{path}/ready");
    let v = problem(b.call("POST", &ready, Some(AGENT), Some(json!({"summary": "done"}))).await, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("North facade"), "the open target is named: {v}");
    problem(b.call("POST", &format!("{path}/sign-off"), Some(OPERATOR), None).await, StatusCode::CONFLICT, "conflict");

    let targets = format!("{path}/targets");
    let v = problem(b.call("POST", &targets, Some(AGENT), Some(json!({"targets": [{"target_id": "north", "status": "covered", "stations": ["nope"]}]}))).await,
                    StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("not in this project"), "{v}");
    problem(b.call("POST", &targets, Some(AGENT), Some(json!({"targets": [{"target_id": "bad id!"}]}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &targets, Some(AGENT), Some(json!({"target": []}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let m = ok(b.call("POST", &targets, Some(AGENT), Some(json!({"targets": [
        {"target_id": "north", "status": "covered", "stations": [MOCK_SCAN]},
        {"target_id": "south", "name": "South facade", "status": "declined", "note": "the yard is locked"}]}))).await, StatusCode::OK);
    assert_eq!(m["targets"][0]["status"], "covered");
    assert_eq!(m["targets"][1]["status"], "declined");

    problem(b.call("POST", &ready, Some(AGENT), Some(json!({"words": "x"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let m = ok(b.call("POST", &ready, Some(AGENT), Some(json!({"summary": "North seen; south declined."}))).await, StatusCode::OK);
    assert_eq!(m["state"], "ready");
    problem(send_json(&b, "POST", &format!("{path}/sign-off"), Some(OPERATOR), &[], b"[1]").await, StatusCode::BAD_REQUEST, "invalid_request");
    let m = ok(b.call("POST", &format!("{path}/sign-off"), Some(OPERATOR), Some(json!({"confirmed_via": "web UI"}))).await, StatusCode::OK);
    assert_eq!((m["state"].as_str(), m["signed_off_by"].as_str(), m["confirmed_via"].as_str()), (Some("signed_off"), Some("crew"), Some("web UI")));

    let v = problem(b.call("POST", &targets, Some(AGENT), Some(json!({"targets": [{"target_id": "west", "name": "West facade"}]}))).await,
                    StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("signed off"), "{v}");
    problem(b.call("POST", &ready, Some(AGENT), Some(json!({"summary": "again"}))).await, StatusCode::CONFLICT, "conflict");

    let log = ok(b.call("GET", &format!("/v1/log?project_id={MOCK_PROJECT}"), Some(VIEWER), None).await, StatusCode::OK);
    let texts: Vec<&str> = log.as_array().unwrap().iter().filter_map(|e| e["text"].as_str()).collect();
    assert!(texts.iter().any(|t| t.starts_with("Mission: Scan the exterior")), "{texts:?}");
    assert!(texts.iter().any(|t| t.starts_with("Target changed: North facade (status, stations)")), "{texts:?}");
    assert!(texts.iter().any(|t| *t == "Target set: South facade (no kind)"), "{texts:?}");
    assert!(texts.iter().any(|t| t.starts_with("Ready to leave: North seen")), "{texts:?}");
    assert!(texts.iter().any(|t| *t == "Mission signed off: Scan the exterior of the house (1 covered, 1 declined) (web UI)"), "{texts:?}");
}

#[tokio::test]
async fn a_mission_needs_a_known_project_a_goal_and_a_kind() {
    let b = Bridge::new().await;
    let good = json!({"goal": "Measure the windows", "kind": "windows", "done_when": "Each window measured"});
    problem(b.call("PUT", "/v1/projects/nope/mission", Some(AGENT), Some(good.clone())).await, StatusCode::NOT_FOUND, "not_found");
    let path = format!("/v1/projects/{MOCK_PROJECT}/mission");
    problem(b.call("PUT", &path, Some(AGENT), Some(json!({"goal": "x", "kind": "castle", "done_when": "y"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("PUT", &path, Some(AGENT), Some(json!({"goal": " ", "kind": "room", "done_when": "y"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let bad_target = json!({"goal": "g", "kind": "room", "done_when": "d", "targets": [{"target_id": "t", "status": "covered", "stations": ["nope"]}]});
    problem(b.call("PUT", &path, Some(AGENT), Some(bad_target)).await, StatusCode::BAD_REQUEST, "invalid_request");
    // Targets, ready and sign-off need a mission.
    for (method, sub, tok) in [("POST", "targets", AGENT), ("POST", "ready", AGENT), ("POST", "sign-off", OPERATOR)] {
        let body = json!({"targets": [], "summary": "s"});
        problem(b.call(method, &format!("{path}/{sub}"), Some(tok), Some(body)).await, StatusCode::NOT_FOUND, "not_found");
    }
    // An empty ledger can't be ready.
    ok(b.call("PUT", &path, Some(OPERATOR), Some(good)).await, StatusCode::OK);
    let v = problem(b.call("POST", &format!("{path}/ready"), Some(AGENT), Some(json!({"summary": "s"}))).await, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("ledger is empty"), "{v}");
    // A sign-off with no body at all.
    ok(b.call("POST", &format!("{path}/targets"), Some(AGENT), Some(json!({"targets": [{"target_id": "w1", "name": "Kitchen window", "status": "declined", "note": "blinds down"}]}))).await, StatusCode::OK);
    ok(b.call("POST", &format!("{path}/ready"), Some(AGENT), Some(json!({"summary": "s"}))).await, StatusCode::OK);
    let m = ok(send_json(&b, "POST", &format!("{path}/sign-off"), Some(ADMIN), &[], b"").await, StatusCode::OK);
    assert_eq!(m["state"], "signed_off");
    assert!(m.get("confirmed_via").is_none(), "{m}");
}

#[tokio::test]
async fn a_wall_target_is_judged_from_the_registration_and_its_question_asked() {
    let b = Bridge::new().await;
    register_mock_project(&b);
    let path = format!("/v1/projects/{MOCK_PROJECT}/mission");
    ok(b.call("PUT", &path, Some(AGENT), Some(json!({"goal": "Check the walls", "kind": "room", "done_when": "Each wall seen"}))).await, StatusCode::OK);
    let m = ok(b.call("POST", &format!("{path}/targets"), Some(AGENT), Some(json!({"targets": [
        {"target_id": "long", "name": "Long wall", "kind": "wall", "wall_id": "w1"},
        {"target_id": "far", "name": "Far wall", "kind": "wall", "face": {"from": [100.0, 100.0], "to": [110.0, 100.0]}, "question": "plumb"},
        {"target_id": "plain", "name": "Door"}]}))).await, StatusCode::OK);
    let t = |id: &str| m["targets"].as_array().unwrap().iter().find(|t| t["target_id"] == id).unwrap().clone();
    assert_eq!(t("long")["status"], "covered", "{}", t("long"));
    assert_eq!(t("long")["face"]["from"], json!([0.0, 0.0]));
    assert_eq!(t("far")["status"], "open");
    // Changing a known target's face and question is logged with both.
    ok(b.call("POST", &format!("{path}/targets"), Some(AGENT), Some(json!({"targets": [
        {"target_id": "far", "face": {"from": [100.0, 101.0], "to": [110.0, 101.0]}, "question": "plumb"}]}))).await, StatusCode::OK);
    let log = ok(b.call("GET", &format!("/v1/log?project_id={MOCK_PROJECT}"), Some(VIEWER), None).await, StatusCode::OK);
    let texts: Vec<&str> = log.as_array().unwrap().iter().filter_map(|e| e["text"].as_str()).collect();
    assert!(texts.iter().any(|t| t.starts_with("Target changed: Far wall (face, question)") && t.contains("face (100.00, 101.00)") && t.contains("asks plumb")), "{texts:?}");

    // Asking: the far face nobody saw has no answer yet, and no evidence.
    let ask = format!("{path}/targets/far/ask");
    let v = ok(b.call("POST", &ask, Some(AGENT), None).await, StatusCode::OK);
    assert_eq!(v["target_id"], "far", "{v}");
    assert!(v["verdict"].is_string(), "{v}");
    problem(b.call("GET", &format!("{ask}/image"), Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("POST", &format!("{path}/targets/plain/ask"), Some(AGENT), None).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("{path}/targets/nope/ask"), Some(AGENT), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("POST", "/v1/projects/nope/mission/targets/far/ask", Some(AGENT), None).await, StatusCode::NOT_FOUND, "not_found");
    // The evidence image, once there is one.
    b.st.inner.lock().unwrap().evidence.insert((MOCK_PROJECT.into(), "far".into()), b"\x89PNG evidence".to_vec());
    let (code, ct, body) = b.bytes("GET", &format!("{ask}/image"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str(), body.as_slice()), (StatusCode::OK, "image/png", &b"\x89PNG evidence"[..]));
}

// ------------------------------------------------------------ registration ---

#[tokio::test]
async fn registration_reads_empty_before_the_first_full_dome() {
    let b = Bridge::new().await;
    let v = ok(b.call("GET", "/v1/projects/p-new/registration", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!((v["project_id"].as_str(), v["stations"].clone(), v["registering_job_id"].clone()), (Some("p-new"), json!([]), Value::Null));
    assert!(v["summary"].as_str().unwrap().starts_with("Not registered yet"), "{v}");
    problem(b.call("GET", "/v1/projects/p-new/registration/plan", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("POST", "/v1/projects/nope/registration", Some(AGENT), None).await, StatusCode::NOT_FOUND, "not_found");
    let got = b.call("POST", "/v1/projects/p-new/registration/face", Some(VIEWER), Some(json!({"from": [0, 0], "to": [1, 0]}))).await;
    let v = problem(got, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("not registered"), "{v}");
}

#[tokio::test]
async fn a_registered_project_shows_its_stations_plan_faces_and_minimap() {
    let b = Bridge::new().await;
    register_mock_project(&b);
    let base = format!("/v1/projects/{MOCK_PROJECT}");
    let v = ok(b.call("GET", &format!("{base}/registration"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["stations"][0]["scan_id"], MOCK_SCAN, "{v}");
    assert_eq!(v["stations"][0]["placed"], true);
    assert_eq!(v["walls"][0]["wall_id"], "w1");
    assert_eq!(v["plan_url"], format!("{base}/registration/plan"));
    assert_eq!(v["registering_job_id"], Value::Null);

    // A wall target shows on the plan and the minimap.
    ok(b.call("PUT", &format!("{base}/mission"), Some(AGENT), Some(json!({"goal": "g", "kind": "room", "done_when": "d",
        "targets": [{"target_id": "a", "name": "A", "wall_id": "w1"}, {"target_id": "b", "name": "B", "face": {"from": [0, 3], "to": [4, 3]}},
                    {"target_id": "c", "name": "C", "face": {"from": [0, 6], "to": [4, 6]}, "status": "declined", "note": "fenced off"}]}))).await, StatusCode::OK);
    let (code, ct, png) = b.bytes("GET", &format!("{base}/registration/plan"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/png"));
    assert_eq!(&png[1..4], b"PNG");

    let f = ok(b.call("POST", &format!("{base}/registration/face"), Some(VIEWER), Some(json!({"wall_id": "w1"}))).await, StatusCode::OK);
    assert_eq!(f["face"], json!({"from": [0.0, 0.0], "to": [5.0, 0.0]}));
    assert!(f["seen_share"].as_f64().unwrap() > 0.8, "{f}");
    let f = ok(b.call("POST", &format!("{base}/registration/face"), Some(VIEWER), Some(json!({"from": [0, 9], "to": [5, 9]}))).await, StatusCode::OK);
    assert_eq!(f["seen_share"], 0.0, "{f}");
    let v = problem(b.call("POST", &format!("{base}/registration/face"), Some(VIEWER), Some(json!({"wall_id": "w999"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("w999"), "{v}");
    problem(b.call("POST", &format!("{base}/registration/face"), Some(VIEWER), Some(json!({"from": [0, 0]}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("{base}/registration/face"), Some(VIEWER), Some(json!({"from": "x"}))).await, StatusCode::BAD_REQUEST, "invalid_request");

    let (code, ct, png) = b.bytes("GET", &format!("{base}/minimap"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/png"));
    assert_eq!(&png[1..4], b"PNG");
    let (code, ct, svg) = b.bytes("GET", &format!("{base}/minimap?format=svg"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/svg+xml"));
    assert!(svg.starts_with(b"<svg"), "{}", String::from_utf8_lossy(&svg[..40.min(svg.len())]));
}

#[tokio::test]
async fn the_minimap_is_drawn_before_any_registration_listing_waiting_domes() {
    let b = Bridge::new().await;
    let (code, ct, svg) = b.bytes("GET", &format!("/v1/projects/{MOCK_PROJECT}/minimap?format=svg"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/svg+xml"));
    assert!(svg.ends_with(b"</svg>") || String::from_utf8_lossy(&svg).contains("</svg>"));
    let (code, ct, _) = b.bytes("GET", "/v1/projects/elsewhere/minimap", Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/png"));
}

// ------------------------------------------------------------------ workers ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_claims_reports_progress_uploads_and_hands_in_a_registration() {
    let b = Bridge::new().await;
    let pid = new_project(&b, "worker-flow").await;
    problem(b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "bad name!"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"name": "laptop"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let (code, _, body) = send(&b, "POST", "/v1/work/claim", Some(AGENT), &[("content-type", "application/json")], br#"{"worker": "laptop-1"}"#.to_vec()).await;
    assert_eq!(code, StatusCode::NO_CONTENT, "nothing to do yet");
    assert!(body.is_empty());
    let ws = ok(b.call("GET", "/v1/workers", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(ws[0]["worker"], "laptop-1", "{ws}");
    assert_eq!(ws[0]["token"], "agent");
    assert_eq!(ws[0]["job_id"], Value::Null);

    let rj = ok(b.call("POST", &format!("/v1/projects/{pid}/registration"), Some(AGENT), None).await, StatusCode::ACCEPTED);
    assert_eq!(rj["kind"], "register", "{rj}");
    let jid = rj["job_id"].as_str().unwrap().to_string();
    // Asked again while it runs: the same job.
    let again = ok(b.call("POST", &format!("/v1/projects/{pid}/registration"), Some(AGENT), None).await, StatusCode::ACCEPTED);
    assert_eq!(again["job_id"], jid.as_str());
    let reg = ok(b.call("GET", &format!("/v1/projects/{pid}/registration"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(reg["registering_job_id"], jid.as_str());

    let job = loop {
        let got = b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "laptop-1"}))).await;
        if got.0 == StatusCode::OK {
            break got.1;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!((job["job_id"].as_str(), job["kind"].as_str(), job["project_id"].as_str()), (Some(jid.as_str()), Some("register"), Some(pid.as_str())));
    assert_eq!((job["stations"].clone(), job["pairs"].clone()), (json!([]), json!([])));
    let ws = ok(b.call("GET", "/v1/workers", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(ws[0]["job_id"], jid.as_str());

    let work = format!("/v1/work/{jid}");
    // Only the claimer's token may report.
    let v = problem(b.call("POST", &format!("{work}/progress"), Some(ADMIN), Some(json!({"phase": "x"}))).await, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("not claimed"), "{v}");
    problem(b.call("POST", "/v1/work/job-x/progress", Some(AGENT), Some(json!({"phase": "x"}))).await, StatusCode::CONFLICT, "conflict");
    problem(b.call("POST", &format!("{work}/progress"), Some(AGENT), Some(json!({"progress": 0.5}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let (code, _, _) = send(&b, "POST", &format!("{work}/progress"), Some(AGENT), &[], br#"{"phase": "decoding on laptop-1", "progress": 7}"#.to_vec()).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let j = ok(b.call("GET", &format!("/v1/jobs/{jid}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!((j["phase"].as_str(), j["progress"].as_f64()), (Some("decoding on laptop-1"), Some(1.0)), "progress is clamped: {j}");

    let result = json!({"pairs": [], "poses": [], "audits": [], "walls": []});
    let v = problem(b.call("POST", &format!("{work}/result"), Some(AGENT), Some(result.clone())).await, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("merged"), "the merged points come first: {v}");
    problem(send_json(&b, "PUT", &format!("{work}/merged"), Some(AGENT), &[], b"not merged points").await, StatusCode::BAD_REQUEST, "invalid_request");
    let merged = crate::register::Merged::default().to_bytes();
    problem(send_json(&b, "PUT", &format!("{work}/merged"), Some(ADMIN), &[], &merged).await, StatusCode::CONFLICT, "conflict");
    let (code, _, _) = send(&b, "PUT", &format!("{work}/merged"), Some(AGENT), &[], merged).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let v = problem(b.call("POST", &format!("{work}/result"), Some(AGENT), Some(json!({"pairs": [], "poses": [null], "audits": [], "walls": []}))).await,
                    StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("poses and audits"), "{v}");
    problem(b.call("POST", &format!("{work}/result"), Some(AGENT), Some(json!({"pairs": []}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let (code, _, _) = send(&b, "POST", &format!("{work}/result"), Some(AGENT), &[("content-type", "application/json")], result.to_string().into_bytes()).await;
    assert_eq!(code, StatusCode::NO_CONTENT);

    // The second ask made the job run once more: the same job is offered again.
    let job = loop {
        let got = b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "laptop-1"}))).await;
        if got.0 == StatusCode::OK {
            break got.1;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(job["job_id"], jid.as_str());
    let merged = crate::register::Merged::default().to_bytes();
    assert_eq!(send(&b, "PUT", &format!("{work}/merged"), Some(AGENT), &[], merged).await.0, StatusCode::NO_CONTENT);
    assert_eq!(send(&b, "POST", &format!("{work}/result"), Some(AGENT), &[], result.to_string().into_bytes()).await.0, StatusCode::NO_CONTENT);

    let j = wait_job(&b, &jid, 10.0, finished).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    let reg = ok(b.call("GET", &format!("/v1/projects/{pid}/registration"), Some(VIEWER), None).await, StatusCode::OK);
    assert!(reg["updated_at"].is_string(), "{reg}");
    assert_eq!(reg["registering_job_id"], Value::Null);
    // The worker's job is gone: nothing more to hand in.
    problem(b.call("POST", &format!("{work}/fail"), Some(AGENT), Some(json!({"error": "late"}))).await, StatusCode::CONFLICT, "conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_worker_hands_the_registration_back_to_the_bridge() {
    let b = Bridge::new().await;
    let pid = new_project(&b, "worker-fails").await;
    send(&b, "POST", "/v1/work/claim", Some(OPERATOR), &[], br#"{"worker": "desk.pc"}"#.to_vec()).await;
    let rj = ok(b.call("POST", &format!("/v1/projects/{pid}/registration"), Some(OPERATOR), None).await, StatusCode::ACCEPTED);
    let jid = rj["job_id"].as_str().unwrap().to_string();
    loop {
        let got = b.call("POST", "/v1/work/claim", Some(OPERATOR), Some(json!({"worker": "desk.pc"}))).await;
        if got.0 == StatusCode::OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    problem(b.call("POST", &format!("/v1/work/{jid}/fail"), Some(OPERATOR), Some(json!({"reason": "x"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let (code, _, _) = send(&b, "POST", &format!("/v1/work/{jid}/fail"), Some(OPERATOR), &[], br#"{"error": "laptop lid closed"}"#.to_vec()).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let j = wait_job(&b, &jid, 10.0, finished).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    let log = ok(b.call("GET", &format!("/v1/log?project_id={pid}"), Some(VIEWER), None).await, StatusCode::OK);
    assert!(log.as_array().unwrap().iter().any(|e| e["text"].as_str().unwrap().contains("came back to the bridge") && e["text"].as_str().unwrap().contains("laptop lid closed")), "{log}");
}

// ------------------------------------------------- scan files (via caches) ---

#[tokio::test]
async fn preview_and_scan_file_are_served_from_the_cache_while_the_scanner_lists_the_scan() {
    let b = Bridge::new().await;
    plant_scan_files(&b, MOCK_SCAN);
    let (code, ct, body) = b.bytes("GET", &format!("/v1/scans/{MOCK_SCAN}/preview"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/jpeg"));
    assert_eq!(&body[..2], b"\xff\xd8");
    let res = b.request("GET", &format!("/v1/scans/{MOCK_SCAN}/tzf"), Some(VIEWER), None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "application/octet-stream");
    assert_eq!(res.headers()["content-disposition"], format!("attachment; filename=\"{MOCK_SCAN}.tzf\""));
    // A cached file for a scan the scanner doesn't list is refused and evicted.
    let ghost = plant(&b, &format!("scans/{NO_SCAN}.jpg"), b"\xff\xd8 ghost");
    problem(b.call("GET", &format!("/v1/scans/{NO_SCAN}/preview"), Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    assert!(!ghost.exists());
    // An id that is not a scan UUID never reaches the disk.
    problem(b.call("GET", "/v1/scans/not..a-uuid/tzf", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn a_scan_file_the_scanner_cant_send_is_502_scanner_error() {
    let b = Bridge::new().await;
    for part in ["preview", "tzf", "view", "coverage", "coverage/image", "next-station/plan"] {
        let v = problem(b.call("GET", &format!("/v1/scans/{MOCK_SCAN}/{part}"), Some(VIEWER), None).await, StatusCode::BAD_GATEWAY, "scanner_error");
        assert!(v["title"].as_str().unwrap().contains("404"), "{part}: {v}");
    }
    assert!(!data(&b).join("scans").read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".part-")), "no partial download is left");
}

#[tokio::test]
async fn a_cache_the_bridge_cant_write_is_500_cache_error() {
    let b = Bridge::new().await;
    std::fs::write(data(&b).join("scans"), b"not a directory").unwrap();
    let v = problem(b.call("GET", &format!("/v1/scans/{MOCK_SCAN}/preview"), Some(VIEWER), None).await, StatusCode::INTERNAL_SERVER_ERROR, "cache_error");
    assert!(v["hint"].as_str().unwrap().contains("BRIDGE_DATA"), "{v}");
}

#[tokio::test]
async fn the_viewer_points_and_coverage_come_from_the_view_cache() {
    let b = Bridge::new().await;
    plant_scan_files(&b, MOCK_SCAN);
    plant(&b, &format!("views/{MOCK_SCAN}-2000000.x9v"), b"X9V1 points");
    plant(&b, &format!("views/{MOCK_SCAN}.coverage.json"), br#"{"point_count": 5, "classes": []}"#);
    plant(&b, &format!("views/{MOCK_SCAN}.coverage.png"), b"\x89PNG coverage");
    let (code, ct, body) = b.bytes("GET", &format!("/v1/scans/{MOCK_SCAN}/view"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str(), body.as_slice()), (StatusCode::OK, "application/octet-stream", &b"X9V1 points"[..]));
    let v = ok(b.call("GET", &format!("/v1/scans/{MOCK_SCAN}/coverage"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v, json!({"point_count": 5, "classes": [], "image_url": format!("/v1/scans/{MOCK_SCAN}/coverage/image")}));
    let (code, ct, body) = b.bytes("GET", &format!("/v1/scans/{MOCK_SCAN}/coverage/image"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str(), body.as_slice()), (StatusCode::OK, "image/png", &b"\x89PNG coverage"[..]));

    // Another size isn't cached: built from the scan file, which doesn't decode.
    let v = problem(b.call("GET", &format!("/v1/scans/{MOCK_SCAN}/view?max_points=5"), Some(VIEWER), None).await, StatusCode::UNPROCESSABLE_ENTITY, "decode_failed");
    assert!(v["detail"].as_str().unwrap().starts_with("open"), "{v}");
    // A coverage summary that isn't JSON is the cache's fault.
    plant(&b, &format!("views/{MOCK_SCAN}.coverage.json"), b"{broken");
    problem(b.call("GET", &format!("/v1/scans/{MOCK_SCAN}/coverage"), Some(VIEWER), None).await, StatusCode::INTERNAL_SERVER_ERROR, "cache_error");
    for part in ["view", "coverage", "coverage/image"] {
        problem(b.call("GET", &format!("/v1/scans/{NO_SCAN}/{part}"), Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    }
}

#[tokio::test]
async fn the_next_station_plan_comes_from_its_cache_and_a_subject_is_validated() {
    let b = Bridge::new().await;
    plant_scan_files(&b, MOCK_SCAN);
    plant(&b, &format!("views/{MOCK_SCAN}.plan.json"), br#"{"cell_m": 0.5, "radius_m": 25, "candidates": [], "reason": "cached"}"#);
    let plan = format!("/v1/scans/{MOCK_SCAN}/next-station/plan");
    let v = ok(b.call("GET", &plan, Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v, json!({"cell_m": 0.5, "radius_m": 25, "candidates": [], "reason": "cached", "scan_id": MOCK_SCAN}));
    for bad in ["523", "a,b", "-1,5", "1,2,3", "7,7", "0,400", "390,500"] {
        let v = problem(b.call("GET", &format!("{plan}?subject={bad}"), Some(VIEWER), None).await, StatusCode::BAD_REQUEST, "invalid_request");
        assert!(v["detail"].as_str().unwrap().starts_with("subject"), "{bad}: {v}");
    }
    // A subject is one question: never cached, planned from the scan file.
    let v = problem(b.call("GET", &format!("{plan}?subject=10,20"), Some(VIEWER), None).await, StatusCode::UNPROCESSABLE_ENTITY, "decode_failed");
    assert!(v["detail"].is_string(), "{v}");
    // Without a cached plan, the build fails the same way.
    std::fs::remove_file(data(&b).join(format!("views/{MOCK_SCAN}.plan.json"))).unwrap();
    problem(b.call("GET", &plan, Some(VIEWER), None).await, StatusCode::UNPROCESSABLE_ENTITY, "decode_failed");
    problem(b.call("GET", "/v1/scans/zz-not-hex/next-station/plan", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("GET", &format!("/v1/scans/{NO_SCAN}/next-station/plan"), Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
}

fn proposal() -> Value {
    json!({
        "marker": {"x": 200, "y": 150, "label": "on the rug by the door"},
        "note": "Carry the tripod past the lamp to the rug by the door.",
        "labels": [{"x": 220, "y": 100, "text": "lamp", "kind": "landmark"}],
        "boxes": [{"x0": 150, "y0": 20, "x1": 250, "y1": 80, "text": "Seen: the window", "kind": "seen"}],
    })
}

#[tokio::test]
async fn a_next_station_proposal_is_checked_drawn_kept_and_listed() {
    let b = Bridge::new().await;
    plant_scan_files(&b, MOCK_SCAN);
    register_mock_project(&b);
    let path = format!("/v1/scans/{MOCK_SCAN}/next-station");
    problem(b.call("GET", &path, Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("GET", &format!("{path}/image"), Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    assert_eq!(ok(b.call("GET", "/v1/next-stations", Some(VIEWER), None).await, StatusCode::OK), json!([]));

    let mut off = proposal();
    off["marker"]["x"] = json!(99999);
    let v = problem(b.call("PUT", &path, Some(AGENT), Some(off)).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("marker"), "{v}");
    let mut turn = proposal();
    turn["note"] = json!("Turn right at the lamp.");
    let v = problem(b.call("PUT", &path, Some(AGENT), Some(turn)).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("\"right\""), "{v}");
    problem(b.call("PUT", &path, Some(AGENT), Some(json!({"note": "no marker"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("PUT", &format!("/v1/scans/{NO_SCAN}/next-station"), Some(AGENT), Some(proposal())).await, StatusCode::NOT_FOUND, "not_found");

    let v = ok(b.call("PUT", &path, Some(AGENT), Some(proposal())).await, StatusCode::OK);
    assert_eq!((v["scan_id"].as_str(), v["project_id"].as_str(), v["proposed_by"].as_str()), (Some(MOCK_SCAN), Some(MOCK_PROJECT), Some("agent")));
    assert_eq!(v["preview_size"], json!([PREVIEW.0, PREVIEW.1]));
    assert_eq!(v["marker"]["x"], 200);
    assert_eq!(v["station"], 1);
    assert_eq!(v["image_url"], format!("{path}/image"));
    assert_eq!(ok(b.call("GET", &path, Some(VIEWER), None).await, StatusCode::OK), v);
    let (code, ct, png) = b.bytes("GET", &format!("{path}/image"), Some(VIEWER)).await;
    assert_eq!((code, ct.as_str()), (StatusCode::OK, "image/png"));
    assert_eq!(&png[1..4], b"PNG");
    let all = ok(b.call("GET", "/v1/next-stations", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(all, json!([v]));
    let log = ok(b.call("GET", &format!("/v1/log?project_id={MOCK_PROJECT}"), Some(VIEWER), None).await, StatusCode::OK);
    assert!(log.as_array().unwrap().iter().any(|e| e["text"].as_str().unwrap().starts_with("Next station proposed from station 1")), "{log}");
}

#[tokio::test]
async fn a_marker_must_be_on_ground_a_tripod_can_stand_on() {
    // A room 11 m by 10 m, its floor 1.5 m below the scanner, cast at 1° a
    // cell: 154 rows from 0.4 rad off the zenith, on a 400 × 200 preview.
    let b = Bridge::new().await;
    plant(&b, &format!("scans/{MOCK_SCAN}.jpg"), &grey_jpeg(PREVIEW.0, PREVIEW.1));
    plant(&b, &format!("scans/{MOCK_SCAN}.tzf"), &crate::export::tests::room_dome(360, [-6.0, -4.0, -1.5], [5.0, 6.0, 3.0]));
    register_mock_project(&b);
    let path = format!("/v1/scans/{MOCK_SCAN}/next-station");
    let at = |y: i64| {
        let mut p = proposal();
        p["marker"]["y"] = json!(y);
        p
    };
    // Pixel rows 39, 104 and 127 are grid rows 30 (over the walls: nothing
    // seen), 80 (the wall below the scanner) and 98 (the floor 2.5 m out).
    let v = problem(b.call("PUT", &path, Some(AGENT), Some(at(39))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("saw nothing at that pixel"), "{v}");
    let v = problem(b.call("PUT", &path, Some(AGENT), Some(at(104))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("a tripod can't stand there: something stands"), "{v}");
    problem(b.call("GET", &path, Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    let v = ok(b.call("PUT", &path, Some(AGENT), Some(at(127))).await, StatusCode::OK);
    assert!(v["marker"]["distance_m"].as_f64().is_some_and(|d| (d - 2.5).abs() < 0.3), "{v}");
    assert!(v["marker"]["slope_deg"].as_f64().is_some_and(|s| s < 1.0), "{v}");
}

#[tokio::test]
async fn a_marker_on_a_counter_top_is_refused() {
    // The same room with a counter all round, 2 to 3 m out and 0.75 m up:
    // its top is flat and wide enough, but it isn't the floor.
    use std::f64::consts::TAU;
    let room = crate::export::tests::room([-6.0, -4.0, -1.5], [5.0, 6.0, 3.0]);
    let hit = move |va: f64, ha: f64| {
        let (h, dz) = (va.sin(), va.cos());
        if dz < 0.0 {
            let face = 2.0 / h;
            if (-1.5..=-0.75).contains(&(face * dz)) {
                return Some(face);
            }
            let top = -0.75 / dz;
            if (2.0..=3.0).contains(&(top * h)) {
                return Some(top);
            }
        }
        room(va, ha)
    };
    let step = TAU / 360.0;
    let tzf = crate::export::tests::tzf_bytes(((std::f64::consts::PI - 0.45) / step) as usize, 360, 0.4, 0.0, step, hit);
    let b = Bridge::new().await;
    plant(&b, &format!("scans/{MOCK_SCAN}.jpg"), &grey_jpeg(PREVIEW.0, PREVIEW.1));
    plant(&b, &format!("scans/{MOCK_SCAN}.tzf"), &tzf);
    register_mock_project(&b);
    // Pixel row 108 holds grid row 83: the counter top 2.6 m out, its face
    // 0.6 m nearer.
    let mut p = proposal();
    p["marker"]["y"] = json!(108);
    let v = problem(b.call("PUT", &format!("/v1/scans/{MOCK_SCAN}/next-station"), Some(AGENT), Some(p)).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("raised"), "{v}");
}

#[tokio::test]
async fn a_marker_up_a_hill_above_the_scanner_is_judged_where_the_scan_saw_it() {
    // Ground rising 10° all round from 1.5 m below the scanner: 15 m out it
    // is 1.1 m above it. A 60° wedge at 0.1° a cell, near the horizon.
    let step = 0.1f64.to_radians();
    let k = 10f64.to_radians().tan();
    let hill = move |va: f64, _ha: f64| {
        let (h, dz) = (va.sin(), va.cos());
        (dz - k * h < 0.0).then(|| -1.5 / (dz - k * h))
    };
    let tzf = crate::export::tests::tzf_bytes(230, 600, 1.3, 0.0, step, hill);
    let b = Bridge::new().await;
    plant(&b, &format!("scans/{MOCK_SCAN}.jpg"), &grey_jpeg(PREVIEW.0, PREVIEW.1));
    plant(&b, &format!("scans/{MOCK_SCAN}.tzf"), &tzf);
    register_mock_project(&b);
    // Grid row 112 looks 4.4° up, at the ground 15 m out: pixel row 97.
    let mut p = proposal();
    p["marker"]["y"] = json!(97);
    let v = ok(b.call("PUT", &format!("/v1/scans/{MOCK_SCAN}/next-station"), Some(AGENT), Some(p)).await, StatusCode::OK);
    assert!(v["marker"]["distance_m"].as_f64().is_some_and(|d| (d - 15.0).abs() < 1.5), "{v}");
    assert!(v["marker"]["slope_deg"].as_f64().is_some_and(|s| (s - 10.0).abs() < 2.0), "{v}");
}

#[tokio::test]
async fn a_proposal_on_a_preview_that_isnt_a_jpeg_is_refused() {
    let b = Bridge::new().await;
    plant(&b, &format!("scans/{MOCK_SCAN}.jpg"), b"\xff\xd8 not really");
    let v = problem(b.call("PUT", &format!("/v1/scans/{MOCK_SCAN}/next-station"), Some(AGENT), Some(proposal())).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().starts_with("preview"), "{v}");
}

#[tokio::test]
async fn points_are_projected_onto_the_scans_preview_in_its_frame_or_the_projects() {
    let b = Bridge::new().await;
    b.st.depth.lock().unwrap().push((MOCK_SCAN.to_string(), Arc::new(room_depth((PREVIEW.0 as usize, PREVIEW.1 as usize)))));
    let path = format!("/v1/scans/{MOCK_SCAN}/project");
    let v = ok(b.call("POST", &path, Some(VIEWER), Some(json!({"points": [[3.0, 0.0, 0.0], {"xy": [2.0, 1.0], "up": 1.0}, {"xy": [-2.0, 0.5]}, [0.0, 0.0, 0.0]]}))).await, StatusCode::OK);
    assert_eq!((v["scan_id"].as_str(), v["preview_size"].clone()), (Some(MOCK_SCAN), json!([PREVIEW.0, PREVIEW.1])));
    assert!(v["scanner_height_m"].as_f64().is_some_and(|h| (h - 1.5).abs() < 0.1), "{v}");
    let pts = v["points"].as_array().unwrap();
    assert_eq!(pts.len(), 4);
    assert_eq!(pts[0]["seen"], "visible", "{v}");
    assert_eq!(pts[0]["range_m"], 3.0);
    assert_eq!(pts[0]["z_m"], 0.0);
    assert!(pts[1]["z_m"].as_f64().is_some_and(|z| (z + 0.5).abs() < 0.1), "1 m above the floor: {v}");
    assert!(pts[2]["px"].is_array(), "{v}");
    assert_eq!(pts[3], Value::Null, "a point at the scanner has no place on the preview");

    for bad in [json!([[1.0, 2.0]]), json!([{"xy": [1.0]}]), json!([{"xy": [1.0, 2.0], "up": "high"}]), json!(["x"])] {
        let v = problem(b.call("POST", &path, Some(VIEWER), Some(json!({"points": bad}))).await, StatusCode::BAD_REQUEST, "invalid_request");
        assert!(v["detail"].as_str().unwrap().starts_with("points[0]"), "{v}");
    }
    let many: Vec<Value> = (0..20_001).map(|_| json!([1.0, 0.0, 0.0])).collect();
    problem(b.call("POST", &path, Some(VIEWER), Some(json!({"points": many}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &path, Some(VIEWER), Some(json!({"frame": "world", "points": []}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &path, Some(VIEWER), Some(json!({"pts": []}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    let v = problem(b.call("POST", &path, Some(VIEWER), Some(json!({"frame": "project", "points": []}))).await, StatusCode::CONFLICT, "conflict");
    assert!(v["detail"].as_str().unwrap().contains("not placed"), "{v}");

    // Placed at (1, 2), turned 90°: the project's (1, 5) is 3 m out along the scan's -x.
    register_mock_project(&b);
    let v = ok(b.call("POST", &path, Some(VIEWER), Some(json!({"frame": "project", "points": [[1.0, 5.0, 0.0]]}))).await, StatusCode::OK);
    assert_eq!(v["points"][0]["range_m"], 3.0, "{v}");
    problem(b.call("POST", "/v1/scans/zz-not-hex/project", Some(VIEWER), Some(json!({"points": []}))).await, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn projecting_onto_a_scan_without_a_depth_view_builds_one_from_its_file() {
    let b = Bridge::new().await;
    let path = format!("/v1/scans/{MOCK_SCAN}/project");
    // No preview on the scanner.
    problem(b.call("POST", &path, Some(VIEWER), Some(json!({"points": [[1.0, 0.0, 0.0]]}))).await, StatusCode::BAD_GATEWAY, "scanner_error");
    plant_scan_files(&b, MOCK_SCAN);
    problem(b.call("POST", &path, Some(VIEWER), Some(json!({"points": [[1.0, 0.0, 0.0]]}))).await, StatusCode::UNPROCESSABLE_ENTITY, "decode_failed");
    plant(&b, &format!("scans/{MOCK_SCAN}.jpg"), b"not a jpeg");
    problem(b.call("POST", &path, Some(VIEWER), Some(json!({"points": [[1.0, 0.0, 0.0]]}))).await, StatusCode::INTERNAL_SERVER_ERROR, "cache_error");
}

#[tokio::test]
async fn aim_validates_its_box_request_and_needs_a_decodable_reference_scan() {
    let b = Bridge::new().await;
    let req = |scan: &str| json!({"reference_scan_id": scan, "box": {"x0": 10, "y0": 10, "x1": 50, "y1": 40}});
    problem(b.call("POST", "/v1/aim", Some(VIEWER), Some(json!({"box": {}}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", "/v1/aim", Some(VIEWER), Some(req(NO_SCAN))).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("POST", "/v1/aim", Some(VIEWER), Some(req(MOCK_SCAN))).await, StatusCode::BAD_GATEWAY, "scanner_error");
    plant_scan_files(&b, MOCK_SCAN);
    let v = problem(b.call("POST", "/v1/aim", Some(AGENT), Some(req(MOCK_SCAN))).await, StatusCode::UNPROCESSABLE_ENTITY, "decode_failed");
    assert_eq!(v["title"], "Could not read the reference scan");
}

#[tokio::test]
async fn aim_is_refused_while_the_scanner_says_it_moved() {
    // A replay with no stations: the mock reports the instrument moved.
    let rec = std::env::temp_dir().join(format!("x9-replay-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&rec).unwrap();
    std::fs::write(rec.join("manifest.json"), b"[]").unwrap();
    let b = Bridge::with_mock(x9_mock::Config { replay: Some(rec.to_string_lossy().into()), ..fast_mock() }).await;
    let got = b.call("POST", "/v1/aim", Some(AGENT), Some(json!({"reference_scan_id": MOCK_SCAN, "box": {"x0": 1, "y0": 1, "x1": 2, "y1": 2}}))).await;
    let v = problem(got, StatusCode::CONFLICT, "instrument_moved");
    assert!(v["hint"].as_str().unwrap().contains("overview"), "{v}");
    let st = ok(b.call("GET", "/v1/status", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(st["scanner"]["moved_since_last_scan"], true);
    std::fs::remove_dir_all(rec).unwrap();
}

// -------------------------------------------------- exports & measurements ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_export_starts_as_a_running_job_and_fails_without_the_scan_file() {
    let b = Bridge::new().await;
    let path = format!("/v1/scans/{MOCK_SCAN}/exports");
    problem(b.call("POST", &path, Some(AGENT), Some(json!({"format": "obj"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("/v1/scans/{NO_SCAN}/exports"), Some(AGENT), Some(json!({"format": "ply"}))).await, StatusCode::NOT_FOUND, "not_found");
    let j = ok(b.call("POST", &path, Some(AGENT), Some(json!({"format": "las", "intensity": "none"}))).await, StatusCode::ACCEPTED);
    assert_eq!((j["kind"].as_str(), j["state"].as_str(), j["phase"].as_str()), (Some("export"), Some("running"), Some("downloading")));
    let done = wait_job(&b, j["job_id"].as_str().unwrap(), 10.0, finished).await;
    assert_eq!(done["state"], "failed", "{done}");
    assert_eq!(done["problem"]["code"], "scanner_error");
    let st = ok(b.call("GET", "/v1/status", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(st["scanner"]["current_job_id"], Value::Null, "an export is not the scanner's job");
}

#[tokio::test]
async fn exports_are_read_by_id_with_their_file() {
    let b = Bridge::new().await;
    problem(b.call("GET", "/v1/exports/export-x", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("GET", "/v1/exports/export-x/file", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    let file = plant(&b, "exports/exp-1.ply", b"ply\nend_header\n");
    {
        let mut inner = b.st.inner.lock().unwrap();
        inner.exports.insert("exp-1".into(), json!({"export_id": "exp-1", "scan_id": MOCK_SCAN, "format": "ply", "point_count": 0}));
        inner.export_files.insert("exp-1".into(), file);
        inner.export_files.insert("exp-gone".into(), data(&b).join("exports/gone.las"));
    }
    let v = ok(b.call("GET", "/v1/exports/exp-1", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["format"], "ply");
    let res = b.request("GET", "/v1/exports/exp-1/file", Some(VIEWER), None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-disposition"], "attachment; filename=\"exp-1.ply\"");
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&body[..], b"ply\nend_header\n");
    problem(b.call("GET", "/v1/exports/exp-gone/file", Some(VIEWER), None).await, StatusCode::INTERNAL_SERVER_ERROR, "cache_error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_measurement_of_an_opening_starts_as_a_job_and_fails_without_the_scan_file() {
    let b = Bridge::new().await;
    let path = format!("/v1/scans/{MOCK_SCAN}/measurements");
    let v = problem(b.call("POST", &path, Some(AGENT), Some(json!({"kind": "door"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("opening"), "{v}");
    problem(b.call("POST", &path, Some(AGENT), Some(json!({"box": {}}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("/v1/scans/{NO_SCAN}/measurements"), Some(AGENT), Some(json!({"kind": "opening"}))).await, StatusCode::NOT_FOUND, "not_found");
    let j = ok(b.call("POST", &path, Some(AGENT), Some(json!({"kind": "opening", "box": {"x0": 1, "y0": 1, "x1": 9, "y1": 9}, "margin_rad": -1.0}))).await, StatusCode::ACCEPTED);
    assert_eq!((j["kind"].as_str(), j["state"].as_str()), (Some("measure"), Some("running")));
    let done = wait_job(&b, j["job_id"].as_str().unwrap(), 10.0, finished).await;
    assert_eq!(done["state"], "failed", "{done}");
}

#[tokio::test]
async fn measurements_are_read_by_id_with_their_evidence() {
    let b = Bridge::new().await;
    problem(b.call("GET", "/v1/measurements/m-x", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("GET", "/v1/measurements/m-x/evidence", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
    let png = plant(&b, "measurements/m-1.png", b"\x89PNG evidence");
    {
        let mut inner = b.st.inner.lock().unwrap();
        inner.measurements.insert("m-1".into(), json!({"measurement_id": "m-1", "opening_width_mm": 1185}));
        inner.measurement_files.insert("m-1".into(), png);
        inner.measurement_files.insert("m-gone".into(), data(&b).join("measurements/gone.png"));
    }
    let v = ok(b.call("GET", "/v1/measurements/m-1", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(v["opening_width_mm"], 1185);
    let (code, ct, body) = b.bytes("GET", "/v1/measurements/m-1/evidence", Some(VIEWER)).await;
    assert_eq!((code, ct.as_str(), body.as_slice()), (StatusCode::OK, "image/png", &b"\x89PNG evidence"[..]));
    problem(b.call("GET", "/v1/measurements/m-gone/evidence", Some(VIEWER), None).await, StatusCode::INTERNAL_SERVER_ERROR, "cache_error");
}

// --------------------------------------------------------- recipes & runs ---

const NOTE_RECIPE: &str = r#"
id = "log-note"
title = "Note"
version = "1"
summary = "Writes a note."
[[inputs]]
name = "text"
type = "string"
[[steps]]
id = "note"
kind = "action"
operation = "addLogNote"
arguments = { text = "{{input.text}}" }
"#;

const BOX_RECIPE: &str = r#"
id = "box-then-note"
title = "Box, then a note"
version = "2"
[[inputs]]
name = "purpose"
type = "choice"
choices = ["blinds", "trim"]
[[steps]]
id = "box"
kind = "person"
instructions = "Draw a box around the window."
needs = ["box"]
[[steps]]
id = "note"
kind = "action"
operation = "addLogNote"
arguments = { text = "Boxed for {{input.purpose}}" }
"#;

const TWO_SCANS: &str = r#"
id = "two-domes"
title = "Two domes"
version = "1"
[[steps]]
id = "first"
kind = "action"
operation = "startScan"
arguments = { kind = "full_dome", mode = "STD_L0" }
[[steps]]
id = "second"
kind = "action"
operation = "startScan"
arguments = { kind = "full_dome", mode = "STD_L0" }
"#;

fn with_recipes(b: &Bridge) {
    b.add_recipe("log-note.toml", NOTE_RECIPE);
    b.add_recipe("box.toml", BOX_RECIPE);
    b.add_recipe("two.toml", TWO_SCANS);
    b.add_recipe("broken.toml", "id = \"broken\"\ntitle = \"x\"\nversion = \"0\"\n[[steps]]\nid = \"a\"\nkind = \"action\"\noperation = \"rmRf\"\n");
}

async fn wait_run(b: &Bridge, run_id: &str, pred: impl Fn(&Value) -> bool) -> Value {
    b.wait_for(&format!("/v1/runs/{run_id}"), VIEWER, 10.0, pred).await
}

fn run_over(r: &Value) -> bool {
    matches!(r["state"].as_str(), Some("succeeded" | "failed" | "cancelled"))
}

#[tokio::test]
async fn recipes_are_listed_without_the_broken_one_and_served_as_data() {
    let b = Bridge::new().await;
    with_recipes(&b);
    let v = ok(b.call("GET", "/v1/recipes", Some(VIEWER), None).await, StatusCode::OK);
    let mut ids: Vec<&str> = v.as_array().unwrap().iter().filter_map(|r| r["recipe_id"].as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["box-then-note", "log-note", "two-domes"]);
    let note = v.as_array().unwrap().iter().find(|r| r["recipe_id"] == "log-note").unwrap();
    assert_eq!(note, &json!({"recipe_id": "log-note", "title": "Note", "version": "1", "summary": "Writes a note."}));
    let r = ok(b.call("GET", "/v1/recipes/box-then-note", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(r["steps"][0]["step_id"], "box");
    assert_eq!(r["steps"][0]["needs"], json!(["box"]));
    problem(b.call("GET", "/v1/recipes/broken", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_is_validated_started_and_runs_its_steps() {
    let b = Bridge::new().await;
    with_recipes(&b);
    let start = |recipe: &str, project: &str, inputs: Value| json!({"recipe_id": recipe, "project_id": project, "inputs": inputs});
    problem(b.call("POST", "/v1/runs", Some(AGENT), Some(start("nope", MOCK_PROJECT, json!({})))).await, StatusCode::NOT_FOUND, "not_found");
    problem(b.call("POST", "/v1/runs", Some(AGENT), Some(start("log-note", "nope", json!({"text": "x"})))).await, StatusCode::NOT_FOUND, "not_found");
    let v = problem(b.call("POST", "/v1/runs", Some(AGENT), Some(start("log-note", MOCK_PROJECT, json!({})))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("\"text\" is required"), "{v}");
    let v = problem(b.call("POST", "/v1/runs", Some(AGENT), Some(start("box-then-note", MOCK_PROJECT, json!({"purpose": "nonsense"})))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert!(v["detail"].as_str().unwrap().contains("blinds"), "{v}");
    problem(b.call("POST", "/v1/runs", Some(AGENT), Some(json!({"recipe_id": "log-note"}))).await, StatusCode::BAD_REQUEST, "invalid_request");

    let r = ok(b.call("POST", "/v1/runs", Some(AGENT), Some(start("log-note", MOCK_PROJECT, json!({"text": "Door wedged open"})))).await, StatusCode::CREATED);
    assert_eq!((r["recipe_id"].as_str(), r["recipe_version"].as_str(), r["started_by"].as_str()), (Some("log-note"), Some("1"), Some("agent")));
    assert_eq!(r["steps"][0]["state"], "pending");
    let rid = r["run_id"].as_str().unwrap().to_string();
    let r = wait_run(&b, &rid, run_over).await;
    assert_eq!(r["state"], "succeeded", "{r}");
    assert_eq!(r["interventions"]["total"], 0);
    let log = ok(b.call("GET", &format!("/v1/log?project_id={MOCK_PROJECT}"), Some(VIEWER), None).await, StatusCode::OK);
    assert!(log.as_array().unwrap().iter().any(|e| e["text"] == "Door wedged open" && e["refs"]["run_id"] == rid.as_str()), "{log}");

    let runs = ok(b.call("GET", "/v1/runs", Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(runs[0]["run_id"], rid.as_str());
    problem(b.call("GET", "/v1/runs/run-x", Some(VIEWER), None).await, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_proposes_a_person_step_and_advancing_it_finishes_the_run() {
    let b = Bridge::new().await;
    with_recipes(&b);
    let r = ok(b.call("POST", "/v1/runs", Some(AGENT), Some(json!({"recipe_id": "box-then-note", "project_id": MOCK_PROJECT, "inputs": {"purpose": "blinds"}}))).await, StatusCode::CREATED);
    let rid = r["run_id"].as_str().unwrap().to_string();
    let r = wait_run(&b, &rid, |r| r["waiting_for"] == "input").await;
    assert_eq!((r["state"].as_str(), r["current_step_id"].as_str()), (Some("waiting_for_person"), Some("box")));

    let base = format!("/v1/runs/{rid}");
    let bx = json!({"x0": 1, "y0": 2, "x1": 30, "y1": 40});
    let v = problem(b.call("POST", &format!("{base}/propose"), Some(AGENT), Some(json!({"step_id": "box", "input": {}}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    assert_eq!(v["detail"], "missing: box");
    let long = "x".repeat(2001);
    problem(b.call("POST", &format!("{base}/propose"), Some(AGENT), Some(json!({"step_id": "box", "input": {"box": bx}, "note": long}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("{base}/propose"), Some(AGENT), Some(json!({"step_id": "box"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("{base}/propose"), Some(AGENT), Some(json!({"step_id": "nope", "input": {}}))).await, StatusCode::NOT_FOUND, "not_found");
    let r = ok(b.call("POST", &format!("{base}/propose"), Some(AGENT), Some(json!({"step_id": "box", "input": {"box": bx}, "note": "  the window by the door  "}))).await, StatusCode::OK);
    let p = &r["steps"][0]["proposal"];
    assert_eq!((p["by"].as_str(), p["note"].as_str(), p["input"]["box"].clone()), (Some("agent"), Some("the window by the door"), bx.clone()));
    assert_eq!(r["waiting_for"], "input", "the step keeps waiting");

    problem(b.call("POST", &format!("{base}/advance"), Some(OPERATOR), Some(json!({"step_id": "box", "input": "box"}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("{base}/advance"), Some(OPERATOR), Some(json!({"input": {}}))).await, StatusCode::BAD_REQUEST, "invalid_request");
    problem(b.call("POST", &format!("{base}/advance"), Some(OPERATOR), Some(json!({"step_id": "note", "input": {}}))).await, StatusCode::CONFLICT, "conflict");
    let r = ok(b.call("POST", &format!("{base}/advance"), Some(OPERATOR), Some(json!({"step_id": "box", "input": {"box": bx}}))).await, StatusCode::OK);
    assert_eq!(r["interventions"]["confirmed"], 1, "{r}");
    let r = wait_run(&b, &rid, run_over).await;
    assert_eq!(r["state"], "succeeded", "{r}");
    assert_eq!(r["steps"][0]["output"]["proposal_accepted"], true);
    assert_eq!(r["interventions"]["total"], 1);
    problem(b.call("POST", &format!("{base}/advance"), Some(AGENT), Some(json!({"step_id": "box", "input": {"box": bx}}))).await, StatusCode::CONFLICT, "conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_runs_starter_or_an_operator_may_steer_it() {
    let b = Bridge::new().await;
    with_recipes(&b);
    let (recipes, _) = crate::recipe::load_dir(&b.st.recipes_dir);
    let recipe = recipes.into_iter().find(|r| r.recipe_id == "box-then-note").unwrap();
    let other = Caller { name: "agent2".into(), role: Role::Agent };
    let run = crate::runs::start(&b.st, other, recipe, MOCK_PROJECT.into(), json!({"purpose": "trim"})).unwrap();
    let base = format!("/v1/runs/{}", run.run_id);
    wait_run(&b, &run.run_id, |r| r["waiting_for"] == "input").await;
    // Another agent: 403 before the body is read.
    for sub in ["advance", "propose"] {
        let got = send_json(&b, "POST", &format!("{base}/{sub}"), Some(AGENT), &[], b"{not json").await;
        problem(got, StatusCode::FORBIDDEN, "forbidden");
    }
    problem(b.call("POST", &format!("{base}/cancel"), Some(AGENT), None).await, StatusCode::FORBIDDEN, "forbidden");
    for sub in ["advance", "propose", "cancel"] {
        problem(b.call("POST", &format!("/v1/runs/run-x/{sub}"), Some(AGENT), Some(json!({"step_id": "box", "input": {}}))).await, StatusCode::NOT_FOUND, "not_found");
    }
    let r = ok(b.call("POST", &format!("{base}/cancel"), Some(OPERATOR), None).await, StatusCode::OK);
    assert_eq!(r["run_id"], run.run_id.as_str());
    let r = wait_run(&b, &run.run_id, run_over).await;
    assert_eq!(r["state"], "cancelled", "{r}");
    assert_eq!(r["steps"][0]["state"], "skipped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approving_a_runs_scan_for_the_whole_run_grants_its_later_scans_and_a_cancel_stops_it() {
    let b = Bridge::new().await;
    with_recipes(&b);
    let r = ok(b.call("POST", "/v1/runs", Some(AGENT), Some(json!({"recipe_id": "two-domes", "project_id": MOCK_PROJECT}))).await, StatusCode::CREATED);
    let rid = r["run_id"].as_str().unwrap().to_string();
    let r = wait_run(&b, &rid, |r| r["waiting_for"] == "approval").await;
    let jid = r["steps"][0]["job_id"].as_str().unwrap().to_string();
    let j = ok(b.call("GET", &format!("/v1/jobs/{jid}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!((j["state"].as_str(), j["run_id"].as_str()), (Some("awaiting_approval"), Some(rid.as_str())));

    let a = ok(b.call("POST", &format!("/v1/jobs/{jid}/approve"), Some(OPERATOR), Some(json!({"run": true}))).await, StatusCode::OK);
    assert_eq!(a["state"], "queued");
    let gl = ok(b.call("GET", "/v1/grants", Some(VIEWER), None).await, StatusCode::OK);
    let g = gl.as_array().unwrap().iter().find(|g| g["run_id"] == rid.as_str()).expect("a grant for the run's later scans");
    assert_eq!((g["max_scans"].as_u64(), g["granted_by"].as_str(), g["project_id"].as_str()), (Some(1), Some("crew"), Some(MOCK_PROJECT)));
    let r = ok(b.call("GET", &format!("/v1/runs/{rid}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(r["interventions"]["approvals"], 1);
    let log = ok(b.call("GET", "/v1/log", Some(VIEWER), None).await, StatusCode::OK);
    assert!(log.as_array().unwrap().iter().any(|e| e["text"] == format!("Run {rid} approved: its next 1 scan need no approval")), "{log}");

    // No worker: the scan stays queued until the run is cancelled.
    ok(b.call("POST", &format!("/v1/runs/{rid}/cancel"), Some(AGENT), None).await, StatusCode::OK);
    let r = wait_run(&b, &rid, run_over).await;
    assert_eq!(r["state"], "cancelled", "{r}");
    let j = ok(b.call("GET", &format!("/v1/jobs/{jid}"), Some(VIEWER), None).await, StatusCode::OK);
    assert_eq!(j["state"], "cancelled");
}
