//! Mock X9 scanner API — serves a subset of `/api/v1` with shapes matching live
//! captures (capture/live/*.json) so x9-client, the bridge and the GUI can be
//! developed offline. Bind defaults to 127.0.0.1:34567 (the device's real port).
//!
//! Stateful: `POST /scans` starts a simulated scan that moves through the
//! phases seen on a real X9 (initializing → running sweep → scan file appears →
//! color imaging if requested → task clears), adds the scan to `/scans`, and can
//! be stopped with `DELETE /tasks/{id}`. As on a real X9, a cancelled task
//! stays listed in `/tasks` for a while (exitResult still "none"), and a scan
//! cancelled during its sweep is still filed, with the points swept so far.
//!
//! Environment:
//! - `X9_MOCK_ADDR` (default `127.0.0.1:34567`)
//! - `X9_MOCK_SPEED` time compression, default 10 (a 54 s scan takes ~5 s)
//! - `X9_MOCK_BATTERY` battery percent, default 87
//! - `X9_MOCK_ON_AC` `1` to report AC power, default 0
//! - `X9_MOCK_FALLBACK_MODE` a scan mode whose requests are echoed (and run)
//!   as a default color full dome, to test clients' echo checks
//! - `X9_MOCK_CANCEL_LINGER` seconds a cancelled task stays listed, default 13
//!   (scaled by the speed factor like every other duration)
//! - `X9_MOCK_FIXTURES` directory with `dome.tzf`, `dome.jpg`, `area.tzf`,
//!   `area.jpg`: served for full-dome and area scans' files and previews
//! - `X9_MOCK_REPLAY` directory of a recorded survey: `station_01.tzf`,
//!   `station_01.jpg`, … and `manifest.json` (`[{"station": 1, "points": n},
//!   …]`). The n-th full dome in a project is station n, with its real file,
//!   preview and point count, as if the crew had moved the tripod there
//!   (`instrumentMovedSinceLastScan` reads true). After the last recorded
//!   station, and for area scans (a recording has none), scans are refused.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Mock {
    projects: Vec<Value>,
    scans: Vec<Value>,
    task: Option<Value>,
    /// Set while the current task is cancelled but still listed.
    cancelled: Option<i64>,
    next_task_id: i64,
    cancel_linger: f64,
    battery: u32,
    on_ac: bool,
    speed: f64,
    /// Replay: the recording's directory and each station's point count.
    replay: Option<(String, Vec<u64>)>,
    fallback_mode: Option<String>,
    fixtures: Option<String>,
}

/// How the mock behaves. `Config::from_env` reads the variables listed above;
/// tests build one directly.
#[derive(Clone, Debug)]
pub struct Config {
    /// Time compression: a 54 s scan takes 54 / speed seconds.
    pub speed: f64,
    pub battery: u32,
    pub on_ac: bool,
    /// Seconds (before compression) a cancelled task stays listed.
    pub cancel_linger: f64,
    /// A scan mode whose requests are echoed and run as a default color full dome.
    pub fallback_mode: Option<String>,
    /// Directory with dome.tzf, dome.jpg, area.tzf, area.jpg.
    pub fixtures: Option<String>,
    /// Directory of a recorded survey (see the module docs).
    pub replay: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { speed: 10.0, battery: 87, on_ac: false, cancel_linger: 13.0, fallback_mode: None, fixtures: None, replay: None }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let d = Config::default();
        Config {
            speed: env("X9_MOCK_SPEED", "10").parse().unwrap_or(d.speed),
            battery: env("X9_MOCK_BATTERY", "87").parse().unwrap_or(d.battery),
            on_ac: env("X9_MOCK_ON_AC", "0") == "1",
            cancel_linger: env("X9_MOCK_CANCEL_LINGER", "13").parse().unwrap_or(d.cancel_linger),
            fallback_mode: std::env::var("X9_MOCK_FALLBACK_MODE").ok(),
            fixtures: std::env::var("X9_MOCK_FIXTURES").ok(),
            replay: std::env::var("X9_MOCK_REPLAY").ok(),
        }
    }
}

/// Load a replay's manifest: point counts in station order.
fn load_replay(dir: &str) -> Vec<u64> {
    let m: Value = std::fs::read(format!("{dir}/manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| panic!("X9_MOCK_REPLAY: {dir}/manifest.json is missing or not JSON"));
    let mut st: Vec<(u64, u64)> = m
        .as_array()
        .map(|a| a.iter().filter_map(|s| Some((s["station"].as_u64()?, s["points"].as_u64()?))).collect())
        .unwrap_or_default();
    st.sort();
    st.into_iter().map(|(_, p)| p).collect()
}

type Shared = Arc<Mutex<Mock>>;

/// The mock's routes, with fresh state.
pub fn app(cfg: Config) -> Router {
    let mock = Mock {
        projects: vec![json!({"id": 0, "nextProjectScanID": 2, "projectName": "mock_project",
                              "projectUUID": "39187455-8278-428c-89ad-8a4835f95be6"})],
        scans: vec![json!({"id": 0, "fastMode": false, "scanMode": "STD_L1", "levelingStatus": "ok", "lockedScan": true,
             "scanUUID": "5b5314cf-76b8-4f59-a445-ceee1e6df4c3", "ppm": 0, "tzfId": 0,
             "parentScanUUID": "00000000-0000-0000-0000-000000000000",
             "image": {"colorMode": "OFF", "downloaded": false, "file": "", "fileSize": 0, "id": 0, "nbImages": 0, "whiteBalanceMode": "mode_1"},
             "tzf": {"downloaded": true, "file": "mock_project/1.tzf", "fileSize": 102108747u64, "id": 0, "nbPoints": 24844238u64}})],
        task: None,
        cancelled: None,
        next_task_id: 0,
        cancel_linger: cfg.cancel_linger,
        battery: cfg.battery,
        on_ac: cfg.on_ac,
        speed: cfg.speed,
        replay: cfg.replay.map(|d| {
            let pts = load_replay(&d);
            eprintln!("x9-mock: replaying {} recorded stations from {d}", pts.len());
            (d, pts)
        }),
        fallback_mode: cfg.fallback_mode,
        fixtures: cfg.fixtures,
    };
    let shared: Shared = Arc::new(Mutex::new(mock));

    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/scans", get(scans).post(launch_scan))
        .route("/api/v1/scans/modes", get(scan_modes))
        .route("/api/v1/scans/:id", get(scan).delete(delete_scan))
        .route("/api/v1/projects", get(projects).post(create_project))
        .route("/api/v1/tasks", get(tasks))
        .route("/api/v1/tzfs/:id", get(tzf))
        .route("/api/v1/tzfs/:id/file", get(tzf_file))
        .route("/api/v1/tzfs/:id/preview", get(tzf_preview))
        .route("/api/v1/tasks/:id", delete(cancel_task))
        .route("/api/v1/errors", get(errors))
        .route("/api/v1/diagnostics", get(|| async { Json(json!([])) }))
        .with_state(shared)
}

/// Serve the mock on a free local port; returns its API base,
/// `http://127.0.0.1:PORT/api/v1`. For tests.
pub async fn spawn(cfg: Config) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("binding a free port");
    let addr = listener.local_addr().expect("local address");
    let app = app(cfg);
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    format!("http://{addr}/api/v1")
}


async fn status(State(m): State<Shared>) -> Json<Value> {
    let m = m.lock().unwrap();
    let task_id = m.task.as_ref().and_then(|t| t["id"].as_i64()).unwrap_or(-1);
    Json(json!({
        "batteryLevel": m.battery,
        "currentParentUUID": "00000000-0000-0000-0000-000000000000",
        "currentTaskID": task_id,
        "errorList": [{"description": "", "errorLevel": "Information", "type": "NoError"}],
        "imuData": {"confidence": 1, "firmwareVersion": "", "orientation": 0, "timestamp": 0},
        "instrumentMovedSinceLastScan": m.replay.is_some(),
        "laserPointerState": {"laserEnabled": false},
        "leveling": {"compensatorStatus": "on", "levelingStatus": "ok", "sighting": 0.0155, "trunnion": 0.0230},
        "pluggedToAC": m.on_ac,
        "storage": {"availableSize": 24356585472u64, "fileSystem": "FAT32", "isPresent": true, "isReadOnly": false, "totalSize": 31902400512u64},
        "wifiConfiguration": {"SSID": "TRIMBLE-X9-MOCK", "securityMode": "wpa2_psk", "regionCode": "NL", "wifiBand": "both"}
    }))
}

async fn projects(State(m): State<Shared>) -> Json<Value> {
    Json(Value::Array(m.lock().unwrap().projects.clone()))
}

async fn create_project(State(m): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut m = m.lock().unwrap();
    let p = json!({"id": m.projects.len(), "nextProjectScanID": 1,
                   "projectName": body["projectName"], "projectUUID": body["projectUUID"]});
    m.projects.push(p.clone());
    (StatusCode::OK, Json(p))
}

async fn scans(State(m): State<Shared>) -> Json<Value> {
    Json(Value::Array(m.lock().unwrap().scans.clone()))
}

/// GET /scans/{id}: one scan by its (positional) id.
async fn scan(State(m): State<Shared>, Path(id): Path<i64>) -> Result<Json<Value>, StatusCode> {
    let g = m.lock().unwrap();
    g.scans.iter().find(|s| s["id"].as_i64() == Some(id)).cloned().map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// GET /tzfs/{id}: a scan file's metadata (the `tzf` object `/scans` embeds).
async fn tzf(State(m): State<Shared>, Path(id): Path<i64>) -> Result<Json<Value>, StatusCode> {
    let g = m.lock().unwrap();
    g.scans.iter().find(|s| s["tzfId"].as_i64() == Some(id)).map(|s| Json(s["tzf"].clone())).ok_or(StatusCode::NOT_FOUND)
}

async fn scan_modes() -> Json<Value> {
    Json(json!([
        {"scanMode": "STD_L0", "range": 30, "nbPoints": 6799768, "fileSize": 36700160, "scanTime": 50,
         "spacing": 0.0015079644737231006, "edmFrequency": 500000, "areaScanTime": {"acquisitionTime": 17, "constantTime": 38}},
        {"scanMode": "STD_L1", "range": 150, "nbPoints": 27199074, "fileSize": 104857600, "scanTime": 123,
         "spacing": 0.000_753_982_236_861_550_3, "edmFrequency": 500000, "areaScanTime": {"acquisitionTime": 69, "constantTime": 40}},
        {"scanMode": "STD_L2", "range": 150, "nbPoints": 61197916, "fileSize": 225443840, "scanTime": 213,
         "spacing": 0.000_502_654_824_574_366_9, "edmFrequency": 500000, "areaScanTime": {"acquisitionTime": 156, "constantTime": 42}},
        {"scanMode": "HS_L1", "range": 150, "nbPoints": 12088380, "fileSize": 41943040, "scanTime": 147,
         "spacing": 0.0011309778792038423, "edmFrequency": 166666, "areaScanTime": {"acquisitionTime": 51, "constantTime": 42}}
    ]))
}

async fn tasks(State(m): State<Shared>) -> Json<Value> {
    Json(match &m.lock().unwrap().task {
        Some(t) => json!([t]),
        None => json!([]),
    })
}

async fn cancel_task(State(m): State<Shared>, Path(id): Path<i64>) -> StatusCode {
    let linger = {
        let mut g = m.lock().unwrap();
        if g.task.as_ref().and_then(|t| t["id"].as_i64()) != Some(id) {
            return StatusCode::NOT_FOUND;
        }
        if g.cancelled == Some(id) {
            return StatusCode::OK;
        }
        g.cancelled = Some(id);
        g.cancel_linger / g.speed
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(linger)).await;
        let mut g = m.lock().unwrap();
        let Some(t) = g.task.clone().filter(|t| t["id"].as_i64() == Some(id)) else { return };
        // Cancelled mid-sweep: the scanner files what it swept so far.
        if t["state"] == "running" && t["file"] == "" {
            let full = full_points(&t["scanRequest"]) as f64;
            let frac = t["percentage"].as_f64().unwrap_or(0.0).max(0.01);
            file_scan(&mut g, &t["scanRequest"], (full * frac) as u64);
        }
        g.task = None;
        g.cancelled = None;
    });
    StatusCode::OK
}

fn full_points(req: &Value) -> u64 {
    if req["scanType"] == "AreaScan" { 777_580 } else { 6_281_859 }
}

/// Add a scan to `/scans` the way the device names it; returns (file, scan id).
fn file_scan(g: &mut Mock, req: &Value, points: u64) -> (String, usize) {
    let area = req["scanType"] == "AreaScan";
    let color = req["colorMode"] == "ON";
    let project_name = req["projectName"].as_str().unwrap_or("mock_project").to_string();
    let proj_idx = g.projects.iter().position(|p| p["projectName"] == project_name.as_str());
    let station = proj_idx.map(|i| g.projects[i]["nextProjectScanID"].as_i64().unwrap_or(1)).unwrap_or(1);
    let file = if area {
        let parent = (station - 1).max(1);
        let k = g.scans.iter()
            .filter(|s| s["tzf"]["file"].as_str().unwrap_or("").starts_with(&format!("{project_name}/{parent}_")))
            .count() + 1;
        format!("{project_name}/{parent}_{k}.tzf")
    } else {
        if let Some(i) = proj_idx {
            g.projects[i]["nextProjectScanID"] = json!(station + 1);
        }
        format!("{project_name}/{station}.tzf")
    };
    let n = g.scans.len();
    let mode = req["scanMode"].as_str().unwrap_or("STD_L0").to_string();
    let short = file.rsplit('/').next().unwrap_or("").to_string();
    g.scans.push(json!({"id": n, "fastMode": !area, "scanMode": mode, "levelingStatus": "ok", "lockedScan": false,
        "scanUUID": uuid::Uuid::new_v4().to_string(), "ppm": 0, "tzfId": n,
        "parentScanUUID": "00000000-0000-0000-0000-000000000000",
        "image": {"colorMode": if color { "ON" } else { "OFF" }, "downloaded": false, "file": "", "fileSize": 0, "id": n, "nbImages": if color { 30 } else { 0 }, "whiteBalanceMode": "mode_1"},
        "tzf": {"downloaded": false, "file": file, "fileSize": points * 9, "id": n, "nbPoints": points}}));
    (short, n)
}

async fn delete_scan(State(m): State<Shared>, Path(id): Path<i64>) -> StatusCode {
    let mut m = m.lock().unwrap();
    let Some(pos) = m.scans.iter().position(|s| s["id"].as_i64() == Some(id)) else {
        return StatusCode::NOT_FOUND;
    };
    m.scans.remove(pos);
    // Ids are positional on the real device: renumber.
    for (i, s) in m.scans.iter_mut().enumerate() {
        s["id"] = json!(i);
        s["tzfId"] = json!(i);
    }
    StatusCode::OK
}

async fn launch_scan(State(m): State<Shared>, Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
    let task = {
        let mut g = m.lock().unwrap();
        if g.task.is_some() {
            return (StatusCode::CONFLICT, Json(json!({"error": "taskManager is busy"})));
        }
        if let Some((_, pts)) = &g.replay {
            if req["scanType"] == "AreaScan" {
                return (StatusCode::CONFLICT, Json(json!({"error": "replay: the recording has no area scans"})));
            }
            let done = g.scans.iter().filter(|s| s["tzf"]["file"].as_str().is_some_and(|f| {
                f.starts_with(&format!("{}/", req["projectName"].as_str().unwrap_or(""))) && !f.rsplit('/').next().unwrap_or("").contains('_')
            })).count();
            if done >= pts.len() {
                return (StatusCode::CONFLICT, Json(json!({"error": format!("replay: all {} recorded stations are scanned", pts.len())})));
            }
        }
        let id = g.next_task_id;
        g.next_task_id += 1;
        let fallback = g.fallback_mode.clone().filter(|m| req["scanMode"] == m.as_str());
        let req = if fallback.is_some() {
            json!({"scanMode": "STD_L0", "scanType": "FullDome", "colorMode": "ON", "fastMode": false,
                   "projectName": req["projectName"], "projectUUID": req["projectUUID"],
                   "startHA": -1, "deltaHA": 0, "startVA": -1, "deltaVA": 0})
        } else {
            req
        };
        let t = json!({"id": id, "type": "scan", "state": "initializing", "percentage": 0.0,
                       "exitResult": "none", "file": "", "scanID": 0,
                       "taskUUID": uuid::Uuid::new_v4().to_string(), "scanRequest": req});
        g.task = Some(t.clone());
        t
    };
    tokio::spawn(simulate(m.clone(), task.clone()));
    (StatusCode::ACCEPTED, Json(task))
}

/// Walk one scan through its phases. Real-time durations (STD_L0, already
/// level) divided by the speed factor: leveling 18 s, sweep 22 s (area 8 s),
/// file ready 6 s later, color 150 s.
async fn simulate(m: Shared, task: Value) {
    let id = task["id"].as_i64().unwrap();
    let req = &task["scanRequest"];
    let area = req["scanType"] == "AreaScan";
    let color = req["colorMode"] == "ON";
    let speed = m.lock().unwrap().speed;
    let wait = |s: f64| tokio::time::sleep(Duration::from_secs_f64(s / speed));
    // Returns false once the task was cancelled.
    let set = |f: &dyn Fn(&mut Value)| -> bool {
        let mut g = m.lock().unwrap();
        if g.cancelled == Some(id) {
            return false;
        }
        match g.task.as_mut() {
            Some(t) if t["id"].as_i64() == Some(id) => {
                f(t);
                true
            }
            _ => false,
        }
    };

    wait(18.0).await;
    if !set(&|t| t["state"] = json!("running")) {
        return;
    }
    let sweep = if area { 8.0 } else { 22.0 };
    for i in 1..=10 {
        wait(sweep / 10.0).await;
        if !set(&|t| t["percentage"] = json!(i as f64 / 10.0)) {
            return;
        }
    }
    wait(6.0).await;
    {
        let mut g = m.lock().unwrap();
        if g.task.as_ref().and_then(|t| t["id"].as_i64()) != Some(id) || g.cancelled == Some(id) {
            return;
        }
        let points = match &g.replay {
            Some((_, pts)) if req["scanType"] != "AreaScan" => {
                let project = req["projectName"].as_str().unwrap_or("").to_string();
                let done = g.scans.iter().filter(|s| s["tzf"]["file"].as_str().is_some_and(|f| {
                    f.starts_with(&format!("{project}/")) && !f.rsplit('/').next().unwrap_or("").contains('_')
                })).count();
                pts.get(done).copied().unwrap_or_else(|| full_points(req))
            }
            _ => full_points(req),
        };
        let (short, n) = file_scan(&mut g, req, points);
        if let Some(t) = g.task.as_mut() {
            t["file"] = json!(short);
            t["scanID"] = json!(n);
        }
    }
    if color {
        for i in 0..=30 {
            if !set(&|t| t["percentage"] = json!(i as f64 / 30.0)) {
                return;
            }
            wait(5.0).await;
        }
    }
    wait(2.0).await;
    let mut g = m.lock().unwrap();
    if g.task.as_ref().and_then(|t| t["id"].as_i64()) == Some(id) && g.cancelled != Some(id) {
        g.task = None;
    }
}

/// Fixture for a scan: `area` if its file name has a `_` (an area scan), else `dome`.
async fn fixture(m: &Shared, id: i64, ext: &str) -> Result<Vec<u8>, StatusCode> {
    let (file, replay, fixtures) = {
        let g = m.lock().unwrap();
        let s = g.scans.iter().find(|s| s["tzfId"].as_i64() == Some(id)).ok_or(StatusCode::NOT_FOUND)?;
        (s["tzf"]["file"].as_str().unwrap_or("").to_string(), g.replay.as_ref().map(|(d, _)| d.clone()), g.fixtures.clone())
    };
    let base = file.rsplit('/').next().unwrap_or("");
    // A replayed full dome: `<project>/<station>.tzf` is the recording's station.
    if let (Some(dir), Some(st)) = (replay, base.strip_suffix(".tzf").and_then(|n| n.parse::<u32>().ok())) {
        return tokio::fs::read(format!("{dir}/station_{st:02}.{ext}")).await.map_err(|_| StatusCode::NOT_FOUND);
    }
    let dir = fixtures.ok_or(StatusCode::NOT_FOUND)?;
    let kind = if base.contains('_') { "area" } else { "dome" };
    tokio::fs::read(format!("{dir}/{kind}.{ext}")).await.map_err(|_| StatusCode::NOT_FOUND)
}

async fn tzf_file(State(m): State<Shared>, Path(id): Path<i64>) -> Result<Vec<u8>, StatusCode> {
    fixture(&m, id, "tzf").await
}

async fn tzf_preview(State(m): State<Shared>, Path(id): Path<i64>) -> Result<([(axum::http::HeaderName, &'static str); 1], Vec<u8>), StatusCode> {
    Ok(([(axum::http::header::CONTENT_TYPE, "image/jpeg")], fixture(&m, id, "jpg").await?))
}

async fn errors() -> Json<Value> {
    Json(json!([{"description": "", "errorLevel": "Information", "type": "NoError"}]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock(task: Value) -> Shared {
        Arc::new(Mutex::new(Mock {
            projects: vec![json!({"id": 0, "nextProjectScanID": 1, "projectName": "p", "projectUUID": "u"})],
            scans: vec![],
            task: Some(task),
            cancelled: None,
            next_task_id: 1,
            cancel_linger: 13.0,
            battery: 87,
            on_ac: false,
            speed: 100.0,
            replay: None,
            fallback_mode: None,
            fixtures: None,
        }))
    }

    #[tokio::test]
    async fn replay_serves_recorded_stations_and_refuses_the_rest() {
        let dir = std::env::temp_dir().join(format!("x9-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("station_01.tzf"), b"station one").unwrap();
        let m = mock(json!(null));
        {
            let mut g = m.lock().unwrap();
            g.task = None;
            g.replay = Some((dir.to_string_lossy().into_owned(), vec![20_060_402]));
            g.scans.push(json!({"id": 0, "tzfId": 0, "tzf": {"file": "p/1.tzf", "nbPoints": 20_060_402u64}}));
        }
        assert_eq!(fixture(&m, 0, "tzf").await.unwrap(), b"station one");
        let area = json!({"scanType": "AreaScan", "projectName": "p", "scanMode": "STD_L1"});
        let (code, Json(body)) = launch_scan(State(m.clone()), Json(area)).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("no area scans"));
        let dome = json!({"scanType": "FullDome", "projectName": "p", "scanMode": "STD_L1"});
        let (code, Json(body)) = launch_scan(State(m.clone()), Json(dome)).await;
        assert_eq!(code, StatusCode::CONFLICT, "{body}");
        assert!(body["error"].as_str().unwrap().contains("all 1 recorded stations"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn task(state: &str, pct: f64) -> Value {
        json!({"id": 0, "type": "scan", "state": state, "percentage": pct, "exitResult": "none", "file": "",
               "scanRequest": {"scanMode": "HS_L1", "scanType": "FullDome", "colorMode": "OFF", "projectName": "p"}})
    }

    #[tokio::test]
    async fn cancelled_sweep_lingers_then_files_a_partial_scan() {
        let m = mock(task("running", 0.5));
        assert_eq!(cancel_task(State(m.clone()), Path(0)).await, StatusCode::OK);
        // Still listed right after the cancel, and a new scan is refused.
        assert!(m.lock().unwrap().task.is_some());
        let (code, _) = launch_scan(State(m.clone()), Json(json!({"scanMode": "STD_L0"}))).await;
        assert_eq!(code, StatusCode::CONFLICT);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let g = m.lock().unwrap();
        assert!(g.task.is_none());
        assert_eq!(g.scans.len(), 1);
        assert_eq!(g.scans[0]["tzf"]["file"], "p/1.tzf");
        assert_eq!(g.scans[0]["tzf"]["nbPoints"], 6_281_859 / 2);
    }

    #[tokio::test]
    async fn scan_and_tzf_items_follow_the_positional_ids() {
        let m = mock(task("running", 0.5));
        {
            let mut g = m.lock().unwrap();
            g.task = None;
            let req = json!({"scanMode": "STD_L0", "scanType": "FullDome", "projectName": "p"});
            file_scan(&mut g, &req, 10);
            file_scan(&mut g, &req, 20);
        }
        let Json(s) = scan(State(m.clone()), Path(1)).await.unwrap();
        assert_eq!(s["tzf"]["file"], "p/2.tzf");
        let Json(t) = tzf(State(m.clone()), Path(1)).await.unwrap();
        assert_eq!((t["id"].as_i64(), t["nbPoints"].as_u64()), (Some(1), Some(20)));
        assert_eq!(scan(State(m.clone()), Path(2)).await.unwrap_err(), StatusCode::NOT_FOUND);
        assert_eq!(tzf(State(m.clone()), Path(2)).await.unwrap_err(), StatusCode::NOT_FOUND);
        // After a delete the survivor is renumbered to 0.
        assert_eq!(delete_scan(State(m.clone()), Path(0)).await, StatusCode::OK);
        let Json(s) = scan(State(m.clone()), Path(0)).await.unwrap();
        assert_eq!(s["tzf"]["file"], "p/2.tzf");
        assert_eq!(tzf(State(m.clone()), Path(1)).await.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_while_leveling_files_nothing() {
        let m = mock(task("initializing", 0.0));
        assert_eq!(cancel_task(State(m.clone()), Path(0)).await, StatusCode::OK);
        assert_eq!(cancel_task(State(m.clone()), Path(9)).await, StatusCode::NOT_FOUND);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let g = m.lock().unwrap();
        assert!(g.task.is_none() && g.scans.is_empty());
    }

    // ---- Through the router: `spawn` plus a tiny HTTP/1.1 client ----

    /// One request to a spawned mock; returns (status, content type, body).
    async fn call(base: &str, method: &str, path: &str, body: Option<Value>) -> (u16, String, Vec<u8>) {
        use std::io::{Read, Write};
        let addr = base.trim_start_matches("http://").trim_end_matches("/api/v1").to_string();
        let path = format!("/api/v1{path}");
        let method = method.to_string();
        tokio::task::spawn_blocking(move || {
            let mut s = std::net::TcpStream::connect(&addr).unwrap();
            let body = body.map(|b| b.to_string()).unwrap_or_default();
            write!(s, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n").unwrap();
            if !body.is_empty() {
                write!(s, "Content-Type: application/json\r\n").unwrap();
            }
            write!(s, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut raw = Vec::new();
            s.read_to_end(&mut raw).unwrap();
            let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("header end");
            let head = String::from_utf8_lossy(&raw[..split]).to_string();
            let code = head[9..12].parse().unwrap();
            let ctype = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type: ").map(str::to_string))
                .unwrap_or_default();
            (code, ctype, raw[split + 4..].to_vec())
        })
        .await
        .unwrap()
    }

    async fn get_json(base: &str, path: &str) -> Value {
        let (code, _, body) = call(base, "GET", path, None).await;
        assert_eq!(code, 200, "GET {path}");
        serde_json::from_slice(&body).unwrap()
    }

    fn fast() -> Config {
        Config { speed: 1000.0, cancel_linger: 0.0, ..Config::default() }
    }

    /// Poll /tasks until it is empty (at most ~2 s).
    async fn idle(base: &str) {
        for _ in 0..400 {
            if get_json(base, "/tasks").await == json!([]) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("mock never went idle");
    }

    /// Poll /tasks until `pred` holds for the listed task.
    async fn until(base: &str, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..400 {
            let t = get_json(base, "/tasks").await;
            if let Some(t) = t.get(0) {
                if pred(t) {
                    return t.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("task never reached the expected state");
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("x9-mock-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn config_defaults_and_from_env() {
        let d = Config::default();
        assert_eq!((d.speed, d.battery, d.on_ac, d.cancel_linger), (10.0, 87, false, 13.0));
        assert!(d.fallback_mode.is_none() && d.fixtures.is_none() && d.replay.is_none());
        // The test environment sets none of the X9_MOCK_* variables.
        let vars = ["X9_MOCK_SPEED", "X9_MOCK_BATTERY", "X9_MOCK_ON_AC", "X9_MOCK_CANCEL_LINGER",
                    "X9_MOCK_FALLBACK_MODE", "X9_MOCK_FIXTURES", "X9_MOCK_REPLAY"];
        if vars.iter().all(|v| std::env::var_os(v).is_none()) {
            let e = Config::from_env();
            assert_eq!((e.speed, e.battery, e.on_ac, e.cancel_linger), (10.0, 87, false, 13.0));
            assert!(e.fallback_mode.is_none() && e.fixtures.is_none() && e.replay.is_none());
        }
    }

    #[tokio::test]
    async fn status_reports_battery_and_ac() {
        let base = spawn(Config { battery: 33, on_ac: true, ..fast() }).await;
        let s = get_json(&base, "/status").await;
        assert_eq!(s["batteryLevel"], 33);
        assert_eq!(s["pluggedToAC"], true);
        assert_eq!(s["currentTaskID"], -1);
        assert_eq!(s["instrumentMovedSinceLastScan"], false);
        assert_eq!(s["leveling"]["levelingStatus"], "ok");
        let base = spawn(fast()).await;
        let s = get_json(&base, "/status").await;
        assert_eq!((s["batteryLevel"].as_u64(), s["pluggedToAC"].as_bool()), (Some(87), Some(false)));
    }

    #[tokio::test]
    async fn read_routes() {
        let base = spawn(fast()).await;
        let scans = get_json(&base, "/scans").await;
        assert_eq!(scans.as_array().unwrap().len(), 1);
        assert_eq!(get_json(&base, "/scans/0").await["scanUUID"], "5b5314cf-76b8-4f59-a445-ceee1e6df4c3");
        assert_eq!(call(&base, "GET", "/scans/3", None).await.0, 404);
        let modes = get_json(&base, "/scans/modes").await;
        assert_eq!(modes.as_array().unwrap().len(), 4);
        assert_eq!(modes[3]["scanMode"], "HS_L1");
        assert_eq!(get_json(&base, "/tasks").await, json!([]));
        assert_eq!(get_json(&base, "/tzfs/0").await["nbPoints"], 24_844_238u64);
        assert_eq!(call(&base, "GET", "/tzfs/5", None).await.0, 404);
        assert_eq!(get_json(&base, "/errors").await[0]["type"], "NoError");
        assert_eq!(get_json(&base, "/diagnostics").await, json!([]));
        assert_eq!(call(&base, "GET", "/nope", None).await.0, 404);
    }

    #[tokio::test]
    async fn projects_create_and_list() {
        let base = spawn(fast()).await;
        assert_eq!(get_json(&base, "/projects").await.as_array().unwrap().len(), 1);
        let (code, _, body) = call(&base, "POST", "/projects",
            Some(json!({"projectName": "site", "projectUUID": "u-1"}))).await;
        assert_eq!(code, 200);
        let p: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(p, json!({"id": 1, "nextProjectScanID": 1, "projectName": "site", "projectUUID": "u-1"}));
        let all = get_json(&base, "/projects").await;
        assert_eq!(all[1]["projectName"], "site");
    }

    #[tokio::test]
    async fn files_and_previews_without_fixtures_are_404() {
        let base = spawn(fast()).await;
        assert_eq!(call(&base, "GET", "/tzfs/0/file", None).await.0, 404);
        assert_eq!(call(&base, "GET", "/tzfs/0/preview", None).await.0, 404);
        // An unknown scan is 404 even with fixtures.
        let dir = temp_dir("fx-empty");
        let base = spawn(Config { fixtures: Some(dir.to_string_lossy().into_owned()), ..fast() }).await;
        assert_eq!(call(&base, "GET", "/tzfs/9/file", None).await.0, 404);
        // A fixtures dir missing the file is 404 too.
        assert_eq!(call(&base, "GET", "/tzfs/0/file", None).await.0, 404);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn full_dome_and_area_scans_with_fixtures() {
        let dir = temp_dir("fx");
        for (name, body) in [("dome.tzf", "D-TZF"), ("dome.jpg", "D-JPG"), ("area.tzf", "A-TZF"), ("area.jpg", "A-JPG")] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let base = spawn(Config { fixtures: Some(dir.to_string_lossy().into_owned()), ..fast() }).await;

        let (code, _, body) = call(&base, "GET", "/tzfs/0/file", None).await;
        assert_eq!((code, body.as_slice()), (200, &b"D-TZF"[..]));
        let (code, ctype, body) = call(&base, "GET", "/tzfs/0/preview", None).await;
        assert_eq!((code, ctype.as_str(), body.as_slice()), (200, "image/jpeg", &b"D-JPG"[..]));

        // A color full dome runs through every phase and is filed as station 2.
        let req = json!({"scanMode": "STD_L0", "scanType": "FullDome", "colorMode": "ON",
                         "projectName": "mock_project", "projectUUID": "39187455-8278-428c-89ad-8a4835f95be6"});
        let (code, _, body) = call(&base, "POST", "/scans", Some(req)).await;
        assert_eq!(code, 202);
        let task: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!((task["id"].as_i64(), task["state"].as_str()), (Some(0), Some("initializing")));
        assert_eq!(get_json(&base, "/status").await["currentTaskID"], 0);
        idle(&base).await;
        let s = get_json(&base, "/scans/1").await;
        assert_eq!(s["tzf"]["file"], "mock_project/2.tzf");
        assert_eq!(s["tzf"]["nbPoints"], 6_281_859);
        assert_eq!(s["image"]["nbImages"], 30);
        assert_eq!(s["image"]["colorMode"], "ON");
        assert_eq!(get_json(&base, "/projects").await[0]["nextProjectScanID"], 3);
        let (_, _, body) = call(&base, "GET", "/tzfs/1/file", None).await;
        assert_eq!(body, b"D-TZF");

        // An area scan hangs off the previous station: 2_1, served the area fixtures.
        let req = json!({"scanMode": "STD_L0", "scanType": "AreaScan", "projectName": "mock_project"});
        assert_eq!(call(&base, "POST", "/scans", Some(req.clone())).await.0, 202);
        idle(&base).await;
        let s = get_json(&base, "/scans/2").await;
        assert_eq!(s["tzf"]["file"], "mock_project/2_1.tzf");
        assert_eq!(s["tzf"]["nbPoints"], 777_580);
        assert_eq!(s["fastMode"], false);
        let (_, _, body) = call(&base, "GET", "/tzfs/2/file", None).await;
        assert_eq!(body, b"A-TZF");
        let (_, _, body) = call(&base, "GET", "/tzfs/2/preview", None).await;
        assert_eq!(body, b"A-JPG");
        // A second area scan numbers on.
        assert_eq!(call(&base, "POST", "/scans", Some(req)).await.0, 202);
        idle(&base).await;
        assert_eq!(get_json(&base, "/scans/3").await["tzf"]["file"], "mock_project/2_2.tzf");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn delete_scan_and_cancel_task_routes() {
        // A cancelled task lingers 100 / 1000 s: long enough to cancel it twice.
        let base = spawn(Config { cancel_linger: 100.0, ..fast() }).await;
        assert_eq!(call(&base, "DELETE", "/scans/0", None).await.0, 200);
        assert_eq!(get_json(&base, "/scans").await, json!([]));
        assert_eq!(call(&base, "DELETE", "/scans/0", None).await.0, 404);

        assert_eq!(call(&base, "DELETE", "/tasks/0", None).await.0, 404);
        let (code, _, _) = call(&base, "POST", "/scans", Some(json!({"scanMode": "STD_L0"}))).await;
        assert_eq!(code, 202);
        // Busy while it runs.
        let (code, _, body) = call(&base, "POST", "/scans", Some(json!({"scanMode": "STD_L0"}))).await;
        assert_eq!(code, 409);
        assert!(String::from_utf8_lossy(&body).contains("busy"));
        assert_eq!(call(&base, "DELETE", "/tasks/0", None).await.0, 200);
        assert_eq!(call(&base, "DELETE", "/tasks/0", None).await.0, 200, "repeat cancel while lingering");
        idle(&base).await;
        // Task ids keep counting.
        let (_, _, body) = call(&base, "POST", "/scans", Some(json!({"scanMode": "STD_L0"}))).await;
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["id"], 1);
    }

    #[tokio::test]
    async fn cancel_mid_sweep_through_the_router_files_a_partial_scan() {
        let base = spawn(Config { speed: 400.0, cancel_linger: 0.0, ..Config::default() }).await;
        let req = json!({"scanMode": "STD_L0", "scanType": "FullDome", "projectName": "mock_project"});
        assert_eq!(call(&base, "POST", "/scans", Some(req)).await.0, 202);
        let t = until(&base, |t| t["state"] == "running").await;
        assert_eq!(call(&base, "DELETE", &format!("/tasks/{}", t["id"]), None).await.0, 200);
        idle(&base).await;
        let scans = get_json(&base, "/scans").await;
        assert_eq!(scans.as_array().unwrap().len(), 2);
        let pts = scans[1]["tzf"]["nbPoints"].as_u64().unwrap();
        assert!(pts > 0 && pts < 6_281_859, "{pts}");
    }

    #[tokio::test]
    async fn cancel_during_color_files_the_full_scan_once() {
        let base = spawn(Config { speed: 400.0, cancel_linger: 0.0, ..Config::default() }).await;
        let req = json!({"scanMode": "STD_L0", "colorMode": "ON", "projectName": "mock_project"});
        assert_eq!(call(&base, "POST", "/scans", Some(req)).await.0, 202);
        let t = until(&base, |t| t["file"] != "").await;
        assert_eq!(t["file"], "2.tzf");
        assert_eq!(t["scanID"], 1);
        assert_eq!(call(&base, "DELETE", "/tasks/0", None).await.0, 200);
        idle(&base).await;
        let scans = get_json(&base, "/scans").await;
        assert_eq!(scans.as_array().unwrap().len(), 2);
        assert_eq!(scans[1]["tzf"]["nbPoints"], 6_281_859);
    }

    #[tokio::test]
    async fn fallback_mode_runs_as_a_default_color_full_dome() {
        let base = spawn(Config { fallback_mode: Some("HSP_L3".into()), ..fast() }).await;
        let req = json!({"scanMode": "HSP_L3", "scanType": "AreaScan", "colorMode": "OFF",
                         "projectName": "mock_project", "projectUUID": "pu"});
        let (code, _, body) = call(&base, "POST", "/scans", Some(req)).await;
        assert_eq!(code, 202);
        let r = &serde_json::from_slice::<Value>(&body).unwrap()["scanRequest"];
        assert_eq!((r["scanMode"].as_str(), r["scanType"].as_str(), r["colorMode"].as_str()),
                   (Some("STD_L0"), Some("FullDome"), Some("ON")));
        assert_eq!((r["projectName"].as_str(), r["projectUUID"].as_str()), (Some("mock_project"), Some("pu")));
        idle(&base).await;
        // Other modes are echoed unchanged.
        let (_, _, body) = call(&base, "POST", "/scans", Some(json!({"scanMode": "HS_L1"}))).await;
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["scanRequest"], json!({"scanMode": "HS_L1"}));
    }

    #[tokio::test]
    async fn replay_through_the_router() {
        let dir = temp_dir("replay");
        std::fs::write(dir.join("manifest.json"),
            br#"[{"station": 2, "points": 222}, {"station": 1, "points": 111}, {"bad": true}]"#).unwrap();
        std::fs::write(dir.join("station_01.tzf"), b"one").unwrap();
        std::fs::write(dir.join("station_01.jpg"), b"one-jpg").unwrap();
        let base = spawn(Config { replay: Some(dir.to_string_lossy().into_owned()), ..fast() }).await;
        assert_eq!(get_json(&base, "/status").await["instrumentMovedSinceLastScan"], true);

        let dome = json!({"scanMode": "STD_L0", "scanType": "FullDome", "projectName": "survey"});
        assert_eq!(call(&base, "POST", "/scans", Some(dome.clone())).await.0, 202);
        idle(&base).await;
        let s = get_json(&base, "/scans/1").await;
        assert_eq!((s["tzf"]["file"].as_str(), s["tzf"]["nbPoints"].as_u64()), (Some("survey/1.tzf"), Some(111)));
        let (code, _, body) = call(&base, "GET", "/tzfs/1/file", None).await;
        assert_eq!((code, body.as_slice()), (200, &b"one"[..]));
        let (_, _, body) = call(&base, "GET", "/tzfs/1/preview", None).await;
        assert_eq!(body, b"one-jpg");

        // Station 2's point count comes from the manifest; its file is missing.
        // The project "survey" is unknown, so the mock keeps numbering it 1:
        // the second dome is filed as survey/1.tzf again.
        assert_eq!(call(&base, "POST", "/scans", Some(dome.clone())).await.0, 202);
        idle(&base).await;
        assert_eq!(get_json(&base, "/scans/2").await["tzf"]["nbPoints"], 222);
        assert_eq!(call(&base, "POST", "/scans", Some(dome)).await.0, 409);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[should_panic(expected = "manifest.json is missing or not JSON")]
    fn replay_without_a_manifest_panics() {
        let _ = app(Config { replay: Some("/nonexistent-x9-mock-replay".into()), ..fast() });
    }
}
