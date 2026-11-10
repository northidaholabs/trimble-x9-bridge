//! Translate the scanner's HTTP API shapes into the bridge API's.

use crate::state::ScanMeta;
use serde_json::{json, Value};
use std::collections::HashMap;
use x9_client::models;

pub fn status(st: Option<&models::Status>, busy_job: Option<&str>, queue_len: usize, version: &str, started_at: &str) -> Value {
    let scanner = match st {
        None => json!({"connected": false}),
        Some(s) => {
            let lev = s.leveling.as_ref();
            let state = match lev.and_then(|l| l.leveling_status.as_deref()) {
                Some("ok") => "ok",
                Some("none") | None => "none",
                Some("error") => "error",
                Some(_) => "in_progress",
            };
            json!({
                "connected": true,
                "busy": s.current_task_id.unwrap_or(-1) >= 0,
                "current_job_id": busy_job,
                "battery_pct": s.battery_level,
                "on_ac_power": s.plugged_to_ac.unwrap_or(false),
                "storage_free_bytes": s.storage.as_ref().and_then(|x| x.available_size),
                "moved_since_last_scan": s.instrument_moved_since_last_scan.unwrap_or(false),
                "leveling": {
                    "state": state,
                    "compensator_on": lev.and_then(|l| l.compensator_status.as_deref()) == Some("on"),
                    "tilt_rad": [lev.and_then(|l| l.sighting).unwrap_or(0.0), lev.and_then(|l| l.trunnion).unwrap_or(0.0)],
                }
            })
        }
    };
    json!({
        "bridge": {"version": version, "started_at": started_at, "queue_length": queue_len, "worker_ready": false},
        "scanner": scanner,
    })
}

pub fn scan_mode(m: &models::ScanMode) -> Value {
    json!({
        "mode": m.scan_mode,
        "range_m": m.range,
        "angular_step_rad": m.spacing,
        "points_full_dome": m.nb_points,
        "fixed_seconds": m.area_scan_time.as_ref().and_then(|a| a.constant_time),
        "full_dome_seconds": m.scan_time,
    })
}

pub fn project(p: &models::Project) -> Value {
    json!({
        "project_id": p.project_uuid,
        "name": p.project_name,
        "scan_count": p.next_project_scan_id.map(|n| (n - 1).max(0)),
    })
}

/// `project/3_2.tzf` → (`project`, station 3, Some(2) for an area scan).
pub fn parse_file(file: &str) -> Option<(String, i64, Option<i64>)> {
    let (proj, base) = file.rsplit_once('/')?;
    let stem = base.strip_suffix(".tzf")?;
    match stem.split_once('_') {
        Some((st, k)) => Some((proj.to_string(), st.parse().ok()?, Some(k.parse().ok()?))),
        None => Some((proj.to_string(), stem.parse().ok()?, None)),
    }
}

/// One scanner scan as an API `Scan`. `all` resolves area scans' parents and
/// `projects` turns project names into ids.
pub fn scan(s: &models::Scan, all: &[models::Scan], projects: &[models::Project], meta: &HashMap<String, ScanMeta>) -> Option<Value> {
    let uuid = s.scan_uuid.clone()?;
    let file = s.tzf.as_ref().and_then(|t| t.file.clone()).unwrap_or_default();
    let (proj_name, station, area_k) = parse_file(&file).unwrap_or_default();
    let project_id = projects
        .iter()
        .find(|p| p.project_name.as_deref() == Some(proj_name.as_str()))
        .and_then(|p| p.project_uuid.clone())
        .unwrap_or(proj_name.clone());
    let parent = area_k.and_then(|_| {
        let want = format!("{proj_name}/{station}.tzf");
        all.iter()
            .find(|o| o.tzf.as_ref().and_then(|t| t.file.as_deref()) == Some(want.as_str()))
            .and_then(|o| o.scan_uuid.clone())
    });
    let m = meta.get(&uuid);
    let mut v = json!({
        "scan_id": uuid,
        "project_id": project_id,
        "parent_scan_id": parent,
        "station": station,
        "kind": if area_k.is_some() { "area" } else { "full_dome" },
        "mode": s.scan_mode,
        "color": s.image.as_ref().and_then(|i| i.color_mode.as_deref()) == Some("ON"),
        "created_at": m.map(|m| m.created_at.clone()),
        "point_count": s.tzf.as_ref().and_then(|t| t.nb_points),
        "tzf_bytes": s.tzf.as_ref().and_then(|t| t.file_size),
    });
    if let Some(m) = m {
        v["timings"] = serde_json::to_value(&m.timings).unwrap_or_default();
        if let Some(w) = m.window {
            v["window"] = serde_json::to_value(w).unwrap_or_default();
        }
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files() {
        assert_eq!(parse_file("2026-10-19_live-probe/3_2.tzf"), Some(("2026-10-19_live-probe".into(), 3, Some(2))));
        assert_eq!(parse_file("p/1.tzf"), Some(("p".into(), 1, None)));
        assert_eq!(parse_file("nonsense"), None);
    }

    fn status_of(v: Value) -> models::Status {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_disconnected_scanner_says_only_that() {
        let v = status(None, None, 3, "0.16.0", "2026-11-10T10:00:00Z");
        assert_eq!(v["scanner"], json!({"connected": false}));
        assert_eq!(v["bridge"], json!({"version": "0.16.0", "started_at": "2026-11-10T10:00:00Z", "queue_length": 3, "worker_ready": false}));
    }

    #[test]
    fn a_connected_scanner_reports_power_storage_and_leveling() {
        let s = status_of(json!({"batteryLevel": 87, "currentTaskID": 4, "pluggedToAC": true, "instrumentMovedSinceLastScan": true,
                                 "storage": {"availableSize": 1234u64},
                                 "leveling": {"compensatorStatus": "on", "levelingStatus": "ok", "sighting": 0.01, "trunnion": -0.02}}));
        let v = status(Some(&s), Some("job_1"), 0, "v", "t")["scanner"].clone();
        assert_eq!(v["busy"], true);
        assert_eq!(v["current_job_id"], "job_1");
        assert_eq!((v["battery_pct"].as_u64(), v["storage_free_bytes"].as_u64()), (Some(87), Some(1234)));
        assert_eq!((v["on_ac_power"].as_bool(), v["moved_since_last_scan"].as_bool()), (Some(true), Some(true)));
        assert_eq!(v["leveling"], json!({"state": "ok", "compensator_on": true, "tilt_rad": [0.01, -0.02]}));
    }

    #[test]
    fn leveling_states_map_to_four_words() {
        let state = |lev: Value| status(Some(&status_of(json!({"leveling": lev}))), None, 0, "v", "t")["scanner"]["leveling"]["state"].clone();
        assert_eq!(state(json!({"levelingStatus": "none"})), "none");
        assert_eq!(state(json!({})), "none");
        assert_eq!(state(json!({"levelingStatus": "error"})), "error");
        assert_eq!(state(json!({"levelingStatus": "leveling"})), "in_progress");
        // No task, no power or storage figures, compensator off: the quiet defaults.
        let v = status(Some(&status_of(json!({"currentTaskID": -1}))), None, 0, "v", "t")["scanner"].clone();
        assert_eq!((v["busy"].as_bool(), v["on_ac_power"].as_bool(), v["moved_since_last_scan"].as_bool()), (Some(false), Some(false), Some(false)));
        assert_eq!(v["leveling"], json!({"state": "none", "compensator_on": false, "tilt_rad": [0.0, 0.0]}));
        assert!(v["battery_pct"].is_null() && v["storage_free_bytes"].is_null());
    }

    #[test]
    fn scan_modes_and_projects_use_the_bridge_names() {
        let m: models::ScanMode = serde_json::from_value(json!({"scanMode": "STD_L2", "range": 80.0, "spacing": 0.0005, "nbPoints": 123u64,
                                                                "scanTime": 54.0, "areaScanTime": {"constantTime": 7.5}})).unwrap();
        assert_eq!(scan_mode(&m), json!({"mode": "STD_L2", "range_m": 80.0, "angular_step_rad": 0.0005, "points_full_dome": 123,
                                         "fixed_seconds": 7.5, "full_dome_seconds": 54.0}));
        let p: models::Project = serde_json::from_value(json!({"projectUUID": "u1", "projectName": "site", "nextProjectScanID": 4})).unwrap();
        assert_eq!(project(&p), json!({"project_id": "u1", "name": "site", "scan_count": 3}));
        let fresh: models::Project = serde_json::from_value(json!({"projectUUID": "u2", "nextProjectScanID": 0})).unwrap();
        assert_eq!(project(&fresh)["scan_count"], 0);
    }

    #[test]
    fn file_names_that_are_not_scans_do_not_parse() {
        assert_eq!(parse_file("p/1.las"), None);
        assert_eq!(parse_file("p/x.tzf"), None);
        assert_eq!(parse_file("p/2_y.tzf"), None);
        assert_eq!(parse_file("a/b/12_3.tzf"), Some(("a/b".into(), 12, Some(3))));
    }

    fn scan_of(uuid: Option<&str>, file: &str) -> models::Scan {
        serde_json::from_value(json!({"scanUUID": uuid, "scanMode": "STD_L1", "image": {"colorMode": "ON"},
                                      "tzf": {"file": file, "nbPoints": 99u64, "fileSize": 891u64}})).unwrap()
    }

    #[test]
    fn a_scan_finds_its_project_id_and_area_parent() {
        let dome = scan_of(Some("s-dome"), "site/3.tzf");
        let area = scan_of(Some("s-area"), "site/3_2.tzf");
        let other = scan_of(Some("s-other"), "elsewhere/1.tzf");
        let all = vec![dome.clone(), area.clone(), other.clone()];
        let projects: Vec<models::Project> = vec![serde_json::from_value(json!({"projectUUID": "p-site", "projectName": "site"})).unwrap()];
        let window = crate::model::Window { start_ha: 1.0, delta_ha: 0.2, start_va: 1.2, delta_va: 0.3 };
        let meta: HashMap<String, ScanMeta> = [("s-area".to_string(), ScanMeta {
            created_at: "2026-11-10T10:00:00Z".into(),
            timings: crate::model::Timings { sweep_s: Some(12.0), ..Default::default() },
            window: Some(window),
        })].into();

        let d = scan(&dome, &all, &projects, &meta).unwrap();
        assert_eq!((d["project_id"].as_str(), d["kind"].as_str(), d["station"].as_i64()), (Some("p-site"), Some("full_dome"), Some(3)));
        assert!(d["parent_scan_id"].is_null() && d["created_at"].is_null() && d.get("timings").is_none());
        assert_eq!((d["color"].as_bool(), d["point_count"].as_u64(), d["tzf_bytes"].as_u64()), (Some(true), Some(99), Some(891)));

        let a = scan(&area, &all, &projects, &meta).unwrap();
        assert_eq!((a["kind"].as_str(), a["parent_scan_id"].as_str()), (Some("area"), Some("s-dome")));
        assert_eq!(a["created_at"], "2026-11-10T10:00:00Z");
        assert_eq!(a["timings"], json!({"sweep_s": 12.0}));
        assert_eq!(a["window"]["delta_va"], 0.3);

        // An unknown project keeps its name as the id; no uuid, no scan.
        assert_eq!(scan(&other, &all, &projects, &meta).unwrap()["project_id"], "elsewhere");
        assert!(scan(&scan_of(None, "site/4.tzf"), &all, &projects, &meta).is_none());
        // A file name the bridge can't read leaves the station at 0.
        assert_eq!(scan(&scan_of(Some("odd"), "odd-name"), &all, &projects, &meta).unwrap()["station"], 0);
    }
}
