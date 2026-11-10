//! x9-client against the in-process mock (`x9_mock::spawn`), plus a tiny
//! canned-response server for the error paths the mock never produces.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use x9_client::{models, Client, X9Error, DEFAULT_BASE};
use x9_mock::Config;

const POLL: Duration = Duration::from_millis(5);

fn fast() -> Config {
    Config { speed: 1000.0, cancel_linger: 0.0, ..Config::default() }
}

async fn client(cfg: Config) -> Client {
    Client::connect(&x9_mock::spawn(cfg).await).unwrap()
}

/// A fresh, empty directory under the system temp dir.
fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "x9-client-test-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A fixtures dir with tiny fake `dome.tzf` and `dome.jpg`.
fn fixtures() -> PathBuf {
    let d = temp_dir("fixtures");
    std::fs::write(d.join("dome.tzf"), b"fake tzf points").unwrap();
    std::fs::write(d.join("dome.jpg"), b"\xFF\xD8fake jpeg\xFF\xD9").unwrap();
    d
}

/// Serve one canned HTTP response on a local port; returns an API base.
fn canned(status: &str, content_type: &str, body: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
        }
    });
    format!("http://{addr}/api/v1")
}

#[test]
fn connect_accepts_with_or_without_trailing_slash_and_rejects_bad_urls() {
    assert!(Client::connect(DEFAULT_BASE).is_ok());
    assert!(Client::connect(&format!("{DEFAULT_BASE}/")).is_ok());
    let err = Client::connect("not a url").err().expect("bad URL must fail");
    assert!(matches!(err, X9Error::Url(_)), "{err:?}");
    assert!(err.to_string().starts_with("invalid base URL"), "{err}");
}

#[tokio::test]
async fn status_and_leveling() {
    let c = client(Config { battery: 42, on_ac: true, ..fast() }).await;
    let s = c.status().await.unwrap();
    assert_eq!(s.battery_level, Some(42));
    assert_eq!(s.plugged_to_ac, Some(true));
    assert_eq!(s.current_task_id, Some(-1));
    assert_eq!(s.instrument_moved_since_last_scan, Some(false));
    assert_eq!(s.error_list[0].kind.as_deref(), Some("NoError"));
    assert_eq!(s.storage.as_ref().unwrap().file_system.as_deref(), Some("FAT32"));
    assert_eq!(s.wifi_configuration.as_ref().unwrap().ssid.as_deref(), Some("TRIMBLE-X9-MOCK"));
    assert_eq!(s.laser_pointer_state.as_ref().unwrap().laser_enabled, Some(false));
    assert_eq!(s.imu_data.as_ref().unwrap().confidence, Some(1.0));
    // Round-trips through Serialize with the device's field names.
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["pluggedToAC"], true);
    assert_eq!(v["currentTaskID"], -1);

    let l = c.leveling().await.unwrap().expect("mock reports leveling");
    assert_eq!(l.compensator_status.as_deref(), Some("on"));
    assert_eq!(l.leveling_status.as_deref(), Some("ok"));
    assert!(l.sighting.is_some() && l.trunnion.is_some());
}

#[tokio::test]
async fn scans_scan_modes_and_tzf() {
    let c = client(fast()).await;
    let scans = c.scans().await.unwrap();
    assert_eq!(scans.len(), 1);
    let s = c.scan(0).await.unwrap();
    assert_eq!(s.scan_mode.as_deref(), Some("STD_L1"));
    assert_eq!(s.tzf_id, Some(0));
    let tzf = s.tzf.as_ref().unwrap();
    assert_eq!(tzf.file.as_deref(), Some("mock_project/1.tzf"));
    assert_eq!(s.image.as_ref().unwrap().color_mode.as_deref(), Some("OFF"));

    let modes = c.scan_modes().await.unwrap();
    let names: Vec<_> = modes.iter().filter_map(|m| m.scan_mode.as_deref()).collect();
    assert_eq!(names, ["STD_L0", "STD_L1", "STD_L2", "HS_L1"]);
    assert_eq!(modes[0].area_scan_time.as_ref().unwrap().acquisition_time, Some(17.0));
    assert_eq!(modes[1].range, Some(150.0));

    let t = c.tzf(0).await.unwrap();
    assert_eq!((t.id, t.nb_points, t.file_size), (Some(0), Some(24_844_238), Some(102_108_747)));
}

#[tokio::test]
async fn projects_and_create_project() {
    let c = client(fast()).await;
    let p = c.projects().await.unwrap();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].project_name.as_deref(), Some("mock_project"));
    assert_eq!(p[0].next_project_scan_id, Some(2));

    let uuid = "11111111-2222-3333-4444-555555555555";
    let made = c.create_project("site_a", uuid).await.unwrap();
    assert_eq!(made.project_name.as_deref(), Some("site_a"));
    assert_eq!(made.project_uuid.as_deref(), Some(uuid));
    assert_eq!(made.id, Some(1));
    assert_eq!(c.projects().await.unwrap().len(), 2);
}

#[tokio::test]
async fn tasks_errors_and_diagnostics() {
    let c = client(fast()).await;
    assert!(c.tasks().await.unwrap().is_empty());
    let e = c.errors().await.unwrap();
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].error_level.as_deref(), Some("Information"));
    assert_eq!(e[0].description.as_deref(), Some(""));
    assert!(c.diagnostics().await.unwrap().is_empty());
}

#[tokio::test]
async fn downloads_stream_fixtures_to_disk() {
    let fx = fixtures();
    let c = client(Config { fixtures: Some(fx.to_string_lossy().into_owned()), ..fast() }).await;
    let out = temp_dir("downloads");

    let dest = out.join("dome.tzf");
    let n = c.download_tzf(0, &dest).await.unwrap();
    assert_eq!(n, 15);
    assert_eq!(std::fs::read(&dest).unwrap(), b"fake tzf points");

    let dest = out.join("dome.jpg");
    let n = c.download_tzf_preview(0, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(fx.join("dome.jpg")).unwrap());
    assert_eq!(n, std::fs::metadata(&dest).unwrap().len());

    // The generic helper with a leading slash.
    let n = c.download("/tzfs/0/file", &out.join("again.tzf")).await.unwrap();
    assert_eq!(n, 15);

    // The mock has no thumbnail route: an API error, and no file is created.
    let dest = out.join("thumb.jpg");
    match c.download_tzf_thumbnail(0, &dest).await {
        Err(X9Error::Api { status: 404, path, .. }) => assert_eq!(path, "/api/v1/tzfs/0/thumbnail"),
        other => panic!("expected a 404, got {other:?}"),
    }
    assert!(!dest.exists());

    // A destination that cannot be created maps to an I/O error.
    let err = c.download_tzf(0, &out.join("no/such/dir/x.tzf")).await.unwrap_err();
    assert!(matches!(err, X9Error::Io(_)), "{err:?}");
    assert!(err.to_string().starts_with("I/O error"), "{err}");

    std::fs::remove_dir_all(out).unwrap();
    std::fs::remove_dir_all(fx).unwrap();
}

#[tokio::test]
async fn downloads_without_fixtures_are_not_found() {
    let c = client(fast()).await;
    let out = temp_dir("nofix");
    let err = c.download_tzf(0, &out.join("x.tzf")).await.unwrap_err();
    assert!(matches!(err, X9Error::Api { status: 404, .. }), "{err:?}");
    let err = c.download_tzf_preview(7, &out.join("x.jpg")).await.unwrap_err();
    assert!(matches!(err, X9Error::Api { status: 404, .. }), "{err:?}");
    std::fs::remove_dir_all(out).unwrap();
}

#[tokio::test]
async fn launch_scan_in_runs_to_completion() {
    // Slower than `fast()` so the task is reliably still listed right after launch.
    let c = client(Config { speed: 400.0, ..fast() }).await;
    let project = c.create_project("site_b", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").await.unwrap();
    let task = c.launch_scan_in(&project, "STD_L0").await.unwrap();
    assert_eq!(task.kind.as_deref(), Some("scan"));
    assert_eq!(task.state.as_deref(), Some("initializing"));
    assert_eq!(task.exit_result.as_deref(), Some("none"));
    assert!(task.task_uuid.is_some());
    let req = task.scan_request.as_ref().unwrap();
    assert_eq!(req["projectName"], "site_b");
    assert_eq!(req["projectUUID"], "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    assert_eq!(req["scanMode"], "STD_L0");

    assert_eq!(c.status().await.unwrap().current_task_id, task.id);
    // A single poll while it runs returns the running task.
    let running = c.wait_until_idle(POLL, 1).await.unwrap().expect("task still listed");
    assert_eq!(running.id, task.id);

    assert!(c.wait_until_idle(POLL, 400).await.unwrap().is_none());
    let scans = c.scans().await.unwrap();
    assert_eq!(scans.len(), 2);
    assert_eq!(scans[1].tzf.as_ref().unwrap().file.as_deref(), Some("site_b/1.tzf"));
    assert_eq!(scans[1].scan_mode.as_deref(), Some("STD_L0"));
}

#[tokio::test]
async fn launch_scan_with_a_full_request_and_busy_refusal() {
    let c = client(fast()).await;
    let req = models::ScanRequest {
        scan_mode: Some("STD_L1".into()),
        fast_mode: Some(true),
        color_mode: Some("OFF".into()),
        indoor_only: Some(false),
        scan_type: Some("AreaScan".into()),
        start_ha: Some(0.5),
        delta_ha: Some(1.0),
        start_va: Some(1.2),
        delta_va: Some(0.4),
        ..Default::default()
    };
    let task = c.launch_scan(&req).await.unwrap();
    let echoed = task.scan_request.unwrap();
    assert_eq!(echoed["scanType"], "AreaScan");
    assert_eq!(echoed["startHA"], 0.5);
    assert_eq!(echoed["deltaVA"], 0.4);
    assert!(echoed.get("projectName").is_none(), "None fields are not sent");

    // Busy: the device refuses a second scan with 409.
    match c.launch_scan(&req).await {
        Err(X9Error::Api { status: 409, path, body }) => {
            assert_eq!(path, "/api/v1/scans");
            assert!(body.contains("busy"), "{body}");
        }
        other => panic!("expected 409, got {other:?}"),
    }
    assert!(c.wait_until_idle(POLL, 400).await.unwrap().is_none());
    // An area scan with no project: filed under mock_project as 1_1.
    let scans = c.scans().await.unwrap();
    assert_eq!(scans[1].tzf.as_ref().unwrap().file.as_deref(), Some("mock_project/1_1.tzf"));
}

#[tokio::test]
async fn cancel_task_stops_a_scan() {
    // A cancelled task lingers 100 / 1000 s: long enough to cancel it twice.
    let c = client(Config { cancel_linger: 100.0, ..fast() }).await;
    let task = c.launch_scan(&models::ScanRequest::default()).await.unwrap();
    let id = task.id.unwrap();
    c.cancel_task(id).await.unwrap();
    // Cancelling again while it lingers is fine.
    c.cancel_task(id).await.unwrap();
    assert!(c.wait_until_idle(POLL, 400).await.unwrap().is_none());
    // Cancelled while leveling: nothing filed.
    assert_eq!(c.scans().await.unwrap().len(), 1);

    match c.cancel_task(99).await {
        Err(X9Error::Api { status: 404, path, .. }) => assert_eq!(path, "/api/v1/tasks/99"),
        other => panic!("expected 404, got {other:?}"),
    }
}

#[tokio::test]
async fn delete_scan_removes_it() {
    let c = client(fast()).await;
    c.delete_scan(0).await.unwrap();
    assert!(c.scans().await.unwrap().is_empty());
    let err = c.delete_scan(0).await.unwrap_err();
    assert!(matches!(err, X9Error::Api { status: 404, .. }), "{err:?}");
}

#[tokio::test]
async fn http_error_status_maps_to_api_error() {
    let c = client(fast()).await;
    let err = c.scan(42).await.unwrap_err();
    match &err {
        X9Error::Api { status, path, body } => {
            assert_eq!(*status, 404);
            assert_eq!(path, "/api/v1/scans/42");
            assert!(body.is_empty());
        }
        other => panic!("expected Api, got {other:?}"),
    }
    assert_eq!(err.to_string(), "instrument returned status 404 for /api/v1/scans/42: ");
    assert!(matches!(c.tzf(42).await, Err(X9Error::Api { status: 404, .. })));

    // A server error with a body keeps the body.
    let c = Client::connect(&canned("500 Internal Server Error", "text/plain", "boom")).unwrap();
    match c.errors().await {
        Err(X9Error::Api { status: 500, path, body }) => {
            assert_eq!(path, "/api/v1/errors");
            assert_eq!(body, "boom");
        }
        other => panic!("expected 500, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_json_maps_to_transport_error() {
    let c = Client::connect(&canned("200 OK", "application/json", "not json")).unwrap();
    let err = c.status().await.unwrap_err();
    match &err {
        X9Error::Http(e) => assert!(e.is_decode(), "{e:?}"),
        other => panic!("expected Http decode error, got {other:?}"),
    }
    assert!(err.to_string().starts_with("HTTP transport error"), "{err}");

    // A well-formed body of the wrong shape is a decode error too.
    let c = Client::connect(&canned("200 OK", "application/json", "{\"not\": \"a list\"}")).unwrap();
    assert!(matches!(c.scans().await, Err(X9Error::Http(e)) if e.is_decode()));
}

#[tokio::test]
async fn unreachable_host_maps_to_transport_error() {
    let c = Client::connect("http://127.0.0.1:9/api/v1").unwrap();
    let err = c.status().await.unwrap_err();
    assert!(matches!(&err, X9Error::Http(e) if e.is_connect()), "{err:?}");
    let out = temp_dir("unreach");
    assert!(matches!(c.download_tzf(0, &out.join("x")).await, Err(X9Error::Http(_))));
    assert!(matches!(c.delete_scan(0).await, Err(X9Error::Http(_))));
    assert!(matches!(c.create_project("a", "b").await, Err(X9Error::Http(_))));
    std::fs::remove_dir_all(out).unwrap();
}

#[test]
fn json_error_variant_formats() {
    let err: X9Error = serde_json::from_str::<u8>("x").unwrap_err().into();
    assert!(err.to_string().starts_with("JSON error"), "{err}");
}
