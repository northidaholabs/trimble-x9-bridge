//! Runs queued scan jobs one at a time. Re-checks the rules right before the
//! scanner starts, then follows the scanner's task through its phases:
//! (waiting, while a task the bridge cancelled clears) → leveling → sweeping →
//! (scan file appears) → imaging if color → done.

use crate::map::parse_file;
use crate::model::{Job, JobState, ScanKind, Timings};
use crate::problem::ApiError;
use crate::rules::preflight;
use crate::state::{AppState, ScanMeta};
use crate::util::now_rfc3339;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x9_client::models;

const MAX_SCAN: Duration = Duration::from_secs(45 * 60);
const MAX_SCANNER_ERRORS: u32 = 10;
/// How long a task the bridge cancelled may stay in `/tasks` (13 s on a real X9).
const CANCEL_LINGER: Duration = Duration::from_secs(60);

pub async fn run(st: Arc<AppState>) {
    loop {
        if let Some(job_id) = claim_next(&st) {
            run_scan(&st, &job_id).await;
            continue;
        }
        tokio::select! {
            _ = st.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

/// Mark the oldest queued job running and return its id.
fn claim_next(st: &AppState) -> Option<String> {
    let job = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.jobs.iter_mut().find(|j| j.job.state == JobState::Queued && j.request.is_some())?;
        rec.job.state = JobState::Running;
        rec.job.started_at = Some(now_rfc3339());
        rec.job.phase = Some("leveling".into());
        rec.job.progress = Some(0.0);
        rec.job.clone()
    };
    st.emit_job(&job);
    Some(job.job_id)
}

/// Apply a change to a job, publish it, and return whether a cancel was asked for.
fn update(st: &AppState, job_id: &str, f: impl FnOnce(&mut crate::state::JobRecord)) -> bool {
    let (job, cancel) = {
        let mut inner = st.inner.lock().unwrap();
        let Some(rec) = inner.jobs.iter_mut().find(|j| j.job.job_id == job_id) else { return true };
        f(rec);
        (rec.job.clone(), rec.cancel_requested)
    };
    st.emit_job(&job);
    cancel
}

fn finish(st: &AppState, job_id: &str, state: JobState, result: Option<serde_json::Value>, problem: Option<&ApiError>) {
    let project_id = {
        let inner = st.inner.lock().unwrap();
        inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| (j.project_id.clone(), j.job.kind.clone()))
    };
    update(st, job_id, |r| {
        r.job.state = state;
        r.job.finished_at = Some(now_rfc3339());
        r.job.phase = None;
        if state == JobState::Succeeded {
            r.job.progress = Some(1.0);
        }
        r.job.result = result;
        r.job.problem = problem.map(|p| p.to_json());
    });
    let verb = match state {
        JobState::Succeeded => "finished".to_string(),
        JobState::Cancelled => "cancelled".to_string(),
        _ => format!("failed: {}", problem.map(|p| p.code).unwrap_or("error")),
    };
    let (project_id, job_kind) = project_id.unzip();
    let job_kind = job_kind.unwrap_or_else(|| "scan".into());
    let kind = if state == JobState::Failed { "error" } else { job_kind.as_str() };
    let label = match job_kind.as_str() {
        "export" => "Export",
        "register" => "Registration",
        "measure" | "measurement" => "Measurement",
        _ => "Scan",
    };
    st.log(kind, format!("{label} job {job_id} {verb}"), "bridge", project_id.as_deref(), &[("job_id", job_id)]);
}

async fn run_scan(st: &Arc<AppState>, job_id: &str) {
    let (req, project_name) = {
        let inner = st.inner.lock().unwrap();
        let Some(rec) = inner.jobs.iter().find(|j| j.job.job_id == job_id) else { return };
        let Some(req) = rec.request.clone() else { return };
        (req, rec.project_name.clone())
    };

    let (snap, _) = st.snapshot().await;
    if let Err(e) = preflight(&st.rules, &snap) {
        return finish(st, job_id, JobState::Failed, None, Some(&e));
    }
    match wait_idle(st, job_id, &req.project_id).await {
        Idle::Ready => {}
        Idle::Cancelled => {
            // The scanner never got this scan: give its grant use back.
            st.inner.lock().unwrap().refund_grant(job_id);
            st.save_grants();
            return finish(st, job_id, JobState::Cancelled, None, None);
        }
        Idle::Failed(e) => return finish(st, job_id, JobState::Failed, None, Some(&e)),
    }
    // Taken after the wait: a cancelled scan's partial sweep may be filed during it.
    let before: HashSet<String> = match st.scanner.scans().await {
        Ok(s) => s.into_iter().filter_map(|s| s.scan_uuid).collect(),
        Err(e) => return finish(st, job_id, JobState::Failed, None, Some(&e.into())),
    };

    let w = req.window;
    let body = models::ScanRequest {
        project_name: Some(project_name.clone()),
        project_uuid: Some(req.project_id.clone()),
        scan_mode: Some(req.mode.clone()),
        color_mode: Some(if req.color { "ON" } else { "OFF" }.into()),
        scan_type: Some(match req.kind {
            ScanKind::FullDome => "FullDome",
            ScanKind::Area => "AreaScan",
        }.into()),
        start_ha: w.map(|w| w.start_ha),
        delta_ha: w.map(|w| w.delta_ha),
        start_va: w.map(|w| w.start_va),
        delta_va: w.map(|w| w.delta_va),
        ..Default::default()
    };
    let t0 = Instant::now();
    let task = match st.scanner.launch_scan(&body).await {
        Ok(t) => t,
        Err(e) => return finish(st, job_id, JobState::Failed, None, Some(&e.into())),
    };
    let task_id = task.id;
    // The scanner answers 202 even when it can't parse a request, then runs a
    // default full-dome scan. Compare its echo with what was sent; on any
    // difference, stop the task at once.
    let sent = serde_json::to_value(&body).unwrap_or_default();
    let echo = task.scan_request.clone().unwrap_or_default();
    let diffs = echo_mismatch(&sent, &echo);
    if !diffs.is_empty() {
        // Stopped before any sweep, so there's no partial scan to wait for;
        // the next job waits out the lingering task in `wait_idle`.
        cancel_task(st, task_id).await;
        let e = ApiError::conflict(format!("the scanner did not accept the request as sent ({}); its task was cancelled", diffs.join(", ")));
        return finish(st, job_id, JobState::Failed, None, Some(&e));
    }
    update(st, job_id, |r| r.scanner_task_id = task_id);
    st.log("scan", format!("Scanner started {} ({})", req.mode, if req.color { "color on" } else { "color off" }), "bridge", Some(&req.project_id), &[("job_id", job_id)]);

    let mut timings = Timings::default();
    let mut new_scan: Option<String> = None;
    let mut idle_since: Option<Instant> = None;
    let mut errors = 0u32;
    let secs = || (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;

    loop {
        tokio::time::sleep(st.poll).await;
        if t0.elapsed() > MAX_SCAN {
            let e = ApiError::conflict("the scan did not finish within 45 minutes");
            return finish(st, job_id, JobState::Failed, None, Some(&e));
        }
        let (tasks, scans) = match (st.scanner.tasks().await, st.scanner.scans().await) {
            (Ok(t), Ok(s)) => {
                errors = 0;
                (t, s)
            }
            (Err(e), _) | (_, Err(e)) => {
                errors += 1;
                if errors >= MAX_SCANNER_ERRORS {
                    return finish(st, job_id, JobState::Failed, None, Some(&e.into()));
                }
                continue;
            }
        };
        let running = tasks.iter().find(|t| task_id.is_none() || t.id == task_id);

        // The scan file appears whole once the sweep is done (before color).
        if new_scan.is_none() {
            let fresh: Vec<&models::Scan> = scans.iter().filter(|s| s.scan_uuid.as_ref().is_some_and(|u| !before.contains(u))).collect();
            let want = running.and_then(|t| t.file.clone()).filter(|f| !f.is_empty());
            let pick = fresh
                .iter()
                .find(|s| match (&want, s.tzf.as_ref().and_then(|t| t.file.as_deref())) {
                    (Some(w), Some(f)) => f.ends_with(&format!("/{w}")),
                    _ => false,
                })
                .or(fresh.first());
            if let Some(s) = pick {
                new_scan = s.scan_uuid.clone();
                timings.geometry_ready_s = Some(secs());
                if let (Some(g), Some(l)) = (timings.geometry_ready_s, timings.leveling_s) {
                    timings.sweep_s = Some(((g - l) * 10.0).round() / 10.0);
                }
                let file = s.tzf.as_ref().and_then(|t| t.file.clone()).unwrap_or_default();
                let station = parse_file(&file).map(|(_, st, _)| st).unwrap_or(0);
                st.log("scan", format!("Geometry ready: {file} (station {station}), {} points", s.tzf.as_ref().and_then(|t| t.nb_points).unwrap_or(0)), "bridge", Some(&req.project_id), &[("job_id", job_id), ("scan_id", new_scan.as_deref().unwrap_or(""))]);
            }
        }

        let phase = match running {
            None => None,
            Some(t) if matches!(t.state.as_deref(), Some("running")) => {
                if timings.leveling_s.is_none() {
                    timings.leveling_s = Some(secs());
                }
                // With color off, the file appears a few seconds before the
                // task clears; that tail still counts as the sweep.
                Some(if new_scan.is_some() && req.color { "imaging" } else { "sweeping" })
            }
            Some(_) => Some("leveling"),
        };
        let progress = running.and_then(|t| t.percentage);
        let cancel = update(st, job_id, |r| {
            if let Some(p) = phase {
                r.job.phase = Some(p.into());
            }
            if progress.is_some() {
                r.job.progress = progress;
            }
        });

        if cancel {
            cancel_task(st, task_id).await;
            finish(st, job_id, JobState::Cancelled, None, None);
            return follow_cancelled(st, job_id, task_id, &before, new_scan, &req).await;
        }

        // Done once the scanner has been idle for a few polls with the file in
        // hand (it can start a short follow-up task right after a color scan).
        if running.is_none() && tasks.is_empty() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if new_scan.is_some() && since.elapsed() >= st.poll * 3 {
                break;
            }
            if new_scan.is_none() && since.elapsed() >= st.poll * 10 {
                let e = ApiError::conflict("the scanner finished without producing a scan file");
                return finish(st, job_id, JobState::Failed, None, Some(&e));
            }
        } else {
            idle_since = None;
        }
    }

    timings.done_s = Some(secs());
    let scan_id = new_scan.unwrap_or_default();
    let created_at = {
        let inner = st.inner.lock().unwrap();
        inner.jobs.iter().find(|j| j.job.job_id == job_id).and_then(|j| j.job.started_at.clone()).unwrap_or_else(now_rfc3339)
    };
    st.inner.lock().unwrap().scan_meta.insert(scan_id.clone(), ScanMeta { created_at, timings, window: req.window });
    finish(st, job_id, JobState::Succeeded, Some(serde_json::json!({"scan_id": scan_id})), None);
    // A new station: bring it into the project frame.
    if req.kind == ScanKind::FullDome {
        queue_register(st, &req.project_id, "bridge");
    }
}

/// Register the project's stations (see `register`), or, while a
/// registration runs, have it run once more when it ends. Returns its job.
pub fn queue_register(st: &Arc<AppState>, project_id: &str, by: &str) -> Job {
    let job = {
        let mut inner = st.inner.lock().unwrap();
        if let Some(running) = inner.registering.get(project_id).cloned() {
            inner.register_again.insert(project_id.to_string());
            if let Some(j) = inner.jobs.iter().find(|j| j.job.job_id == running) {
                return j.job.clone();
            }
        }
        let now = now_rfc3339();
        let job = Job {
            job_id: crate::util::new_id("job"),
            kind: "register".into(),
            state: JobState::Running,
            phase: Some("listing stations".into()),
            progress: None,
            requested_by: by.to_string(),
            run_id: None,
            created_at: now.clone(),
            started_at: Some(now),
            finished_at: None,
            result: None,
            problem: None,
            partial_scan_id: None,
            request: None,
            project_name: None,
        };
        inner.jobs.push(crate::state::JobRecord {
            job: job.clone(),
            request: None,
            project_id: project_id.to_string(),
            project_name: String::new(),
            scanner_task_id: None,
            cancel_requested: false,
            grant_id: None,
        });
        inner.registering.insert(project_id.to_string(), job.job_id.clone());
        job
    };
    st.emit_job(&job);
    tokio::spawn(run_register(st.clone(), job.job_id.clone(), project_id.to_string()));
    job
}

async fn run_register(st: Arc<AppState>, job_id: String, project_id: String) {
    let res = loop {
        let r = register_once(&st, &job_id, &project_id).await;
        let again = {
            let mut inner = st.inner.lock().unwrap();
            let again = inner.register_again.remove(&project_id) && !cancel_requested_locked(&inner, &job_id);
            if !again {
                inner.registering.remove(&project_id);
            }
            again
        };
        if !again {
            break r;
        }
    };
    match res {
        Ok(v) => finish(&st, &job_id, JobState::Succeeded, Some(v), None),
        Err(None) => finish(&st, &job_id, JobState::Cancelled, None, None),
        Err(Some(e)) => finish(&st, &job_id, JobState::Failed, None, Some(&e)),
    }
}

fn cancel_requested_locked(inner: &crate::state::Inner, job_id: &str) -> bool {
    inner.jobs.iter().find(|j| j.job.job_id == job_id).is_none_or(|j| j.cancel_requested)
}

/// One pass: list the project's full domes; a connected worker registers
/// them (see `work`), or the bridge does; then store the result, log what
/// changed and re-judge the mission's wall faces. `Err(None)`: cancelled.
async fn register_once(st: &Arc<AppState>, job_id: &str, project_id: &str) -> Result<serde_json::Value, Option<ApiError>> {
    use crate::register::{Merged, Registration, StationPose};
    use crate::work::Offered;
    let mut stations: Vec<(u16, String)> = crate::routes::all_scans(st)
        .await
        .map_err(Some)?
        .into_iter()
        .filter(|s| s["project_id"] == project_id && s["kind"] == "full_dome")
        .filter_map(|s| Some((s["station"].as_u64()? as u16, s["scan_id"].as_str()?.to_string())))
        .collect();
    stations.sort();
    let prev = st.inner.lock().unwrap().registrations.get(project_id).cloned();
    let pairs0 = prev.as_ref().map(|r| r.pairs.clone()).unwrap_or_default();
    let cancelled = || cancel_requested(st, job_id);
    let phase = |p: String| {
        update(st, job_id, |r| r.job.phase = Some(p));
    };
    let offered = crate::work::offer(st, job_id, project_id, &stations, &pairs0, &cancelled, &phase).await;
    let (skipped, pairs, poses, audits, merged, ws, frame, by) = match offered {
        Offered::Cancelled => return Err(None),
        Offered::Done(res, bytes, worker) => {
            let merged = Merged::from_bytes(&bytes).map_err(|e| Some(ApiError::invalid(e)))?;
            (res.skipped, res.pairs, res.poses, res.audits, merged, res.walls, res.frame, Some(worker))
        }
        other => {
            if let Offered::Failed(why) = other {
                st.log("check", format!("Registration came back to the bridge: {why}"), "bridge", Some(project_id), &[("job_id", job_id)]);
            }
            let (skipped, pairs, poses, audits, merged, ws, frame) = register_here(st, job_id, &stations, pairs0).await?;
            (skipped, pairs, poses, audits, merged, ws, frame, None)
        }
    };
    stations.retain(|(n, _)| !skipped.iter().any(|s| s.0 == *n));
    let ids: Vec<String> = stations.iter().map(|s| s.1.clone()).collect();
    if cancel_requested(st, job_id) {
        return Err(None);
    }
    let mut reg = Registration {
        project_id: project_id.to_string(),
        frame_scan_id: frame.filter(|f| ids.contains(f)).or_else(|| ids.first().cloned()),
        stations: stations.iter().zip(&poses).zip(audits).map(|(((n, id), p), a)| StationPose { scan_id: id.clone(), station: *n, pose: *p, audit: Some(a) }).collect(),
        pairs,
        walls: ws,
        summary: String::new(),
        updated_at: now_rfc3339(),
    };
    reg.summarise();
    // The frame moved to a bigger group: carry the mission's wall faces along,
    // when the old frame's station is placed in the new one.
    if let Some(old) = prev.as_ref().and_then(|p| p.frame_scan_id.clone()).filter(|old| Some(old) != reg.frame_scan_id.as_ref()) {
        if let Some(pose) = reg.stations.iter().find(|s| s.scan_id == old).and_then(|s| s.pose) {
            let (s, c) = pose.turn_deg.to_radians().sin_cos();
            let mv = |p: [f64; 2]| [c * p[0] - s * p[1] + pose.xyz_m[0], s * p[0] + c * p[1] + pose.xyz_m[1]];
            let moved = {
                let mut inner = st.inner.lock().unwrap();
                inner.missions.get_mut(project_id).map(|m| {
                    for t in m.targets.iter_mut() {
                        if let Some(f) = t.face.as_mut() {
                            f.from = mv(f.from);
                            f.to = mv(f.to);
                        }
                    }
                })
            };
            if moved.is_some() {
                st.save_missions();
            }
        }
        st.log("note", format!("The project frame is now station {}'s (the largest group of stations)", reg.frame_station()), "bridge", Some(project_id), &[]);
    }
    if let Some(path) = st.merged_path(project_id) {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| Some(ApiError::cache(format!("registration directory: {e}"))))?;
        }
        let tmp = path.with_extension(format!("merged.{}", crate::util::new_id("tmp")));
        let w = tokio::fs::write(&tmp, merged.to_bytes()).await.and(tokio::fs::rename(&tmp, &path).await);
        if let Err(e) = w {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Some(ApiError::cache(format!("merged points: {e}"))));
        }
    }
    {
        let mut inner = st.inner.lock().unwrap();
        inner.registrations.insert(project_id.to_string(), reg.clone());
        inner.merged.insert(project_id.to_string(), Arc::new(merged));
    }
    st.save_registrations();
    st.emit("registration", reg.to_json());

    // What changed, for the log: each station whose verdict is new.
    let before = |id: &str| {
        prev.as_ref().and_then(|p| p.stations.iter().find(|s| s.scan_id == id)).map(|s| s.audit.as_ref().map(|a| a.verdict.clone()).unwrap_or_default())
    };
    for (s, id) in reg.stations.iter().zip(&ids) {
        let a = s.audit.clone().unwrap_or_default();
        if before(id).as_deref() == Some(a.verdict.as_str()) {
            continue;
        }
        let tied: Vec<String> = a
            .ties
            .iter()
            .filter_map(|t| {
                let n = reg.stations.iter().find(|x| &x.scan_id == t)?.station;
                let p = reg.pairs.iter().find(|p| p.good && ((p.a == *id && p.b == *t) || (p.b == *id && p.a == *t)))?;
                Some(format!("{n} ({} mm)", p.rms_mm.unwrap_or(0.0)))
            })
            .collect();
        let why = a.reasons.join("; ");
        let text = match a.verdict.as_str() {
            "frame" => format!("Station {} is the project frame", s.station),
            "confirmed" => format!("Station {} registered and confirmed: tied to stations {}, and the independent checks agree", s.station, tied.join(", ")),
            "single" => format!(
                "Station {} registered on one tie (station {}); the independent checks agree. A station that also sees what it saw would confirm it",
                s.station,
                tied.join(", ")
            ),
            "doubtful" => format!("Station {} is doubtful: {why}. It does not count towards coverage until a later station settles it", s.station),
            "ambiguous" => format!(
                "Station {} is not placed: {why}. Set up the next station where it sees something that breaks the symmetry (a corner, a door, a tree) along with walls an earlier station saw",
                s.station
            ),
            _ => format!("Station {} did not register: {why}. Set up the next station where it sees walls an earlier station saw", s.station),
        };
        let kind = if s.pose.is_some() && a.trusted() { "note" } else { "check" };
        st.log(kind, text, "bridge", Some(project_id), &[("scan_id", id)]);
    }
    for (n, why) in &skipped {
        st.log("check", format!("Station {n} could not be registered: {why}"), "bridge", Some(project_id), &[]);
    }
    let on = by.as_deref().map(|w| format!(" (registered on {w})")).unwrap_or_default();
    st.log("note", format!("Registration{on}: {}", reg.summary), "bridge", Some(project_id), &[("job_id", job_id)]);

    // The mission's wall faces, judged again.
    let geo = st.geo(project_id);
    let mission = {
        let mut inner = st.inner.lock().unwrap();
        inner.missions.get_mut(project_id).map(|m| (m.refresh(&geo), m.clone()))
    };
    if let Some((changed, m)) = mission {
        st.save_missions();
        st.emit("mission", serde_json::json!(m));
        if !changed.is_empty() {
            st.log("note", format!("Ledger from the registration: {}", changed.join("; ")), "bridge", Some(project_id), &[("mission_id", &m.mission_id)]);
        }
    }
    crate::ask::recheck(st, project_id).await;
    let placed = reg.stations.iter().filter(|s| s.pose.is_some()).count();
    Ok(serde_json::json!({"project_id": project_id, "stations": reg.stations.len(), "placed": placed, "summary": reg.summary}))
}

type Registered = (
    Vec<(u16, String)>,
    Vec<crate::register::Pair>,
    Vec<Option<crate::register::Pose>>,
    Vec<crate::register::Audit>,
    crate::register::Merged,
    Vec<crate::register::Wall>,
    Option<String>,
);

/// A station's cloud on disk, named by the range it was cut at.
pub fn cloud_name(scan_id: &str) -> String {
    format!("{scan_id}.r{}t{}.cloud", crate::register::RANGE as u32, crate::register::NEAR_M as u32)
}

/// The prepared-station cache's budget: `BRIDGE_PREPARED_MB`, default 2048.
fn prepared_budget() -> usize {
    std::env::var("BRIDGE_PREPARED_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2048) << 20
}

/// Register on the bridge: decode stations not seen before into 5 cm clouds
/// (kept on disk), then place, audit and merge. Prepared clouds and pair
/// checks stay in memory, so a new station costs about one station's work.
/// Returns the stations that couldn't be decoded first.
async fn register_here(st: &Arc<AppState>, job_id: &str, stations: &[(u16, String)], pairs0: Vec<crate::register::Pair>) -> Result<Registered, Option<ApiError>> {
    use crate::files::{scan_file, FileKind};
    use crate::register::{audit, prepare, register_all, walls, Cloud, Merged, Prepared};
    let dir = st.data_dir.join("registration");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| Some(ApiError::cache(format!("registration directory: {e}"))))?;
    let mut skipped: Vec<(u16, String)> = Vec::new();
    let total = stations.len().max(1);
    for (k, (n, id)) in stations.iter().enumerate() {
        // Clouds cut at an older range are dropped.
        for old in [format!("{id}.cloud"), format!("{id}.r60.cloud")] {
            let _ = tokio::fs::remove_file(dir.join(old)).await;
        }
        let path = dir.join(cloud_name(id));
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            continue;
        }
        if update(st, job_id, |r| {
            r.job.phase = Some(format!("decoding station {n}"));
            r.job.progress = Some(k as f64 / total as f64 * 0.4);
        }) {
            return Err(None);
        }
        let tzf = match unless_cancelled(st, job_id, scan_file(st, id, FileKind::Tzf)).await {
            None => return Err(None),
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                skipped.push((*n, e.title.to_string()));
                continue;
            }
        };
        let res = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
            let t = trimble_tzf::Tzf::open(&tzf).map_err(|e| format!("open: {e}"))?;
            let meta = t.scan_meta().map_err(|e| format!("metadata: {e}"))?;
            let pts = t.grid_points().map_err(|e| format!("decode: {e}"))?;
            Ok(Cloud::from_grid(&pts, crate::view::step_rad(&meta)).to_bytes())
        })
        .await;
        match res {
            Ok(Ok(bytes)) => {
                let tmp = path.with_extension(format!("cloud.{}", crate::util::new_id("tmp")));
                let w = tokio::fs::write(&tmp, &bytes).await.and(tokio::fs::rename(&tmp, &path).await);
                if let Err(e) = w {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(Some(ApiError::cache(format!("station cloud: {e}"))));
                }
            }
            Ok(Err(e)) => skipped.push((*n, e)),
            Err(e) => skipped.push((*n, e.to_string())),
        }
    }
    let kept: Vec<(u16, String)> = stations.iter().filter(|(n, _)| !skipped.iter().any(|s| s.0 == *n)).cloned().collect();
    if update(st, job_id, |r| {
        r.job.phase = Some("matching".into());
        r.job.progress = Some(0.4);
    }) {
        return Err(None);
    }
    let ids: Vec<String> = kept.iter().map(|s| s.1.clone()).collect();
    let numbers: Vec<u16> = kept.iter().map(|s| s.0).collect();
    let (st2, job2) = (st.clone(), job_id.to_string());
    let out = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let mut prep: Vec<Arc<Prepared>> = Vec::with_capacity(ids.len());
        let mut over: Option<usize> = None;
        for id in &ids {
            let cached = st2.prepared.lock().unwrap().get(id).cloned();
            let p = match cached {
                Some(p) => p,
                None => {
                    let bytes = std::fs::read(dir.join(cloud_name(id))).map_err(|e| format!("station cloud {id}: {e}"))?;
                    let p = Arc::new(prepare(&Cloud::from_bytes(&bytes)?));
                    st2.prepared.lock().unwrap().insert(id.clone(), p.clone());
                    p
                }
            };
            prep.push(p);
        }
        // Keep memory to the project at hand once it grows.
        {
            let mut c = st2.prepared.lock().unwrap();
            // Over budget: other projects' stations go first.
            let total = |c: &std::collections::HashMap<String, Arc<Prepared>>| c.values().map(|p| p.approx_bytes()).sum::<usize>();
            if total(&c) > prepared_budget() {
                c.retain(|k, _| ids.contains(k));
            }
            let mine = total(&c);
            if mine > prepared_budget() {
                over = Some(mine >> 20);
            }
        }
        let refs: Vec<&Prepared> = prep.iter().map(|p| p.as_ref()).collect();
        let say = |b: usize, a: usize| {
            update(&st2, &job2, |r| r.job.phase = Some(format!("matching station {} against {}", numbers[b], numbers[a])));
        };
        let (pairs, groups) = register_all(&ids, &refs, pairs0, &say);
        let poses = groups.poses();
        let frame = ids.get(groups.frame()).cloned();
        update(&st2, &job2, |r| r.job.phase = Some("checking".into()));
        let names: Vec<String> = numbers.iter().map(u16::to_string).collect();
        let mut cache = std::mem::take(&mut *st2.audit_cache.lock().unwrap());
        let audits = audit(&ids, &names, &refs, &poses, &pairs, &mut cache);
        if cache.len() > 4000 {
            cache.retain(|(a, b), _| ids.contains(a) && ids.contains(b));
        }
        *st2.audit_cache.lock().unwrap() = cache;
        let mut merged = Merged::default();
        for (k, p) in poses.iter().enumerate() {
            if let Some(p) = p {
                merged.add(refs[k], p, numbers[k]);
            }
        }
        let ws = walls(&merged);
        Ok((pairs, poses, audits, merged, ws, frame, over))
    })
    .await;
    match out {
        Ok(Ok((pairs, poses, audits, merged, ws, frame, over))) => {
            if let Some(mb) = over {
                st.log(
                    "check",
                    format!("This project's stations take {mb} MB prepared, over the {} MB budget (BRIDGE_PREPARED_MB): a laptop worker would take the load off the Pi", prepared_budget() >> 20),
                    "bridge",
                    None,
                    &[("job_id", job_id)],
                );
            }
            Ok((skipped, pairs, poses, audits, merged, ws, frame))
        }
        Ok(Err(e)) => Err(Some(ApiError::cache(e))),
        Err(e) => Err(Some(ApiError::conflict(e.to_string()))),
    }
}

/// Set a job's phase (and progress) from elsewhere, e.g. a worker's report.
pub fn set_phase(st: &AppState, job_id: &str, phase: &str, progress: Option<f64>) {
    update(st, job_id, |r| {
        r.job.phase = Some(phase.to_string());
        if progress.is_some() {
            r.job.progress = progress;
        }
    });
}

enum Idle {
    Ready,
    Cancelled,
    Failed(ApiError),
}

#[derive(Debug, PartialEq)]
enum Busy {
    Idle,
    /// Only tasks the bridge cancelled, which the scanner still lists.
    Ours,
    /// Tasks someone else started.
    Other(usize),
}

fn busy(tasks: &[models::Task], ours: &HashSet<i64>) -> Busy {
    let other = tasks.iter().filter(|t| !t.id.is_some_and(|id| ours.contains(&id))).count();
    match (tasks.is_empty(), other) {
        (true, _) => Busy::Idle,
        (false, 0) => Busy::Ours,
        (false, n) => Busy::Other(n),
    }
}

fn scanner_busy(detail: String) -> ApiError {
    ApiError::new(axum::http::StatusCode::CONFLICT, "scanner_busy", "The scanner is busy with another task")
        .detail(detail)
        .hint("Wait for the other task to finish, then queue the scan again.")
}

fn cancel_requested(st: &AppState, job_id: &str) -> bool {
    st.inner.lock().unwrap().jobs.iter().find(|j| j.job.job_id == job_id).is_none_or(|j| j.cancel_requested)
}

/// Stop a scanner task and remember it was the bridge that did.
async fn cancel_task(st: &AppState, task_id: Option<i64>) {
    if let Some(id) = task_id {
        st.inner.lock().unwrap().cancelled_tasks.insert(id);
        let _ = st.scanner.cancel_task(id).await;
    }
}

/// Wait until the scanner has no task. A task the bridge cancelled stays listed
/// for a while after the cancel; wait that out (up to `CANCEL_LINGER`). Any
/// other task means another client (the vendor app, a script) is driving the
/// scanner: fail at once.
async fn wait_idle(st: &AppState, job_id: &str, project_id: &str) -> Idle {
    let t0 = Instant::now();
    let mut logged = false;
    loop {
        let tasks = match st.scanner.tasks().await {
            Ok(t) => t,
            Err(e) => return Idle::Failed(e.into()),
        };
        let ours = st.inner.lock().unwrap().cancelled_tasks.clone();
        match busy(&tasks, &ours) {
            Busy::Idle => {
                st.inner.lock().unwrap().cancelled_tasks.clear();
                if logged {
                    update(st, job_id, |r| r.job.phase = Some("leveling".into()));
                }
                return Idle::Ready;
            }
            Busy::Other(n) => return Idle::Failed(scanner_busy(format!("{n} task(s) running"))),
            Busy::Ours if t0.elapsed() >= CANCEL_LINGER => {
                return Idle::Failed(scanner_busy(format!("a task the bridge cancelled is still listed after {} s", CANCEL_LINGER.as_secs())));
            }
            Busy::Ours => {
                if !logged {
                    update(st, job_id, |r| r.job.phase = Some("waiting".into()));
                    st.log("scan", "Waiting for the scanner to clear the cancelled task", "bridge", Some(project_id), &[("job_id", job_id)]);
                    logged = true;
                }
            }
        }
        if cancel_requested(st, job_id) {
            return Idle::Cancelled;
        }
        tokio::time::sleep(st.poll).await;
    }
}

/// After a cancel the scanner keeps the task listed for a while, and may still
/// file what it had swept as a scan. Wait for that (up to `CANCEL_LINGER`) and
/// point the job at it, so a person can keep or delete it. `known` is a scan
/// already seen during the run.
async fn follow_cancelled(st: &AppState, job_id: &str, task_id: Option<i64>, before: &HashSet<String>, known: Option<String>, req: &crate::model::ScanRequest) {
    let t0 = Instant::now();
    let mut gone_since: Option<Instant> = None;
    let mut found: Option<models::Scan> = None;
    while found.is_none() && t0.elapsed() < CANCEL_LINGER {
        let (Ok(tasks), Ok(scans)) = (st.scanner.tasks().await, st.scanner.scans().await) else {
            tokio::time::sleep(st.poll).await;
            continue;
        };
        found = scans.into_iter().find(|s| match (&known, &s.scan_uuid) {
            (Some(k), Some(u)) => k == u,
            (None, Some(u)) => !before.contains(u),
            _ => false,
        });
        if task_id.is_none_or(|id| !tasks.iter().any(|t| t.id == Some(id))) {
            if let Some(id) = task_id {
                st.inner.lock().unwrap().cancelled_tasks.remove(&id);
            }
            // The scan may be filed just after the task clears.
            if gone_since.get_or_insert_with(Instant::now).elapsed() >= st.poll * 5 {
                break;
            }
        }
        if found.is_none() {
            tokio::time::sleep(st.poll).await;
        }
    }
    let Some(scan) = found else { return };
    let Some(scan_id) = scan.scan_uuid.clone() else { return };
    {
        let mut inner = st.inner.lock().unwrap();
        let created_at = inner.jobs.iter().find(|j| j.job.job_id == job_id).and_then(|j| j.job.started_at.clone()).unwrap_or_else(now_rfc3339);
        inner.scan_meta.entry(scan_id.clone()).or_insert_with(|| ScanMeta { created_at, timings: Timings::default(), window: req.window });
    }
    update(st, job_id, |r| r.job.partial_scan_id = Some(scan_id.clone()));
    let file = scan.tzf.as_ref().and_then(|t| t.file.clone()).unwrap_or_default();
    let points = scan.tzf.as_ref().and_then(|t| t.nb_points).unwrap_or(0);
    st.log(
        "scan",
        format!("The scanner kept a partial scan from cancelled job {job_id}: {file}, {points} points. Keep it or delete it."),
        "bridge",
        Some(&req.project_id),
        &[("job_id", job_id), ("scan_id", &scan_id)],
    );
}

/// Resolves once someone asks the job to stop.
async fn until_cancelled(st: &AppState, job_id: &str) {
    while !cancel_requested(st, job_id) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run `fut` unless the job is cancelled first (then `None`; `fut` is dropped).
async fn unless_cancelled<T>(st: &AppState, job_id: &str, fut: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::select! {
        v = fut => Some(v),
        _ = until_cancelled(st, job_id) => None,
    }
}

/// Decode a scan into a point file. Runs alongside scans (it never touches the
/// scanner beyond downloading the file once). A cancel stops it at the next
/// check: during the download, between decoding and writing, or while writing.
pub async fn run_export(st: Arc<AppState>, job_id: String, scan_id: String, req: crate::export::ExportRequest) {
    use crate::export::Intensity;
    use crate::files::{scan_file, FileKind};
    let cancelled = |st: &AppState| finish(st, &job_id, JobState::Cancelled, None, None);
    let tzf = match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Tzf)).await {
        None => return cancelled(&st),
        Some(Ok(p)) => p,
        Some(Err(e)) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
    };
    let preview = if req.intensity == Intensity::None {
        None
    } else {
        match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Preview)).await {
            None => return cancelled(&st),
            Some(Ok(p)) => tokio::fs::read(p).await.ok(),
            Some(Err(_)) => None,
        }
    };
    if update(&st, &job_id, |r| r.job.phase = Some("decoding".into())) {
        return cancelled(&st);
    }
    let export_id = crate::util::new_id("exp");
    let out = st.data_dir.join("exports").join(format!("{export_id}.{}", req.format.ext()));
    if let Some(dir) = out.parent() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("exports directory: {e}"))));
        }
    }
    let (out2, req2, st2, job2) = (out.clone(), req.clone(), st.clone(), job_id.clone());
    let res = tokio::task::spawn_blocking(move || {
        let stop = || cancel_requested(&st2, &job2);
        crate::export::run(&tzf, preview.as_deref(), &req2, &out2, &stop)
    })
    .await;
    if cancel_requested(&st, &job_id) {
        let _ = tokio::fs::remove_file(&out).await;
        return cancelled(&st);
    }
    match res {
        Ok(Ok(mut v)) => {
            v["export_id"] = serde_json::json!(export_id);
            v["scan_id"] = serde_json::json!(scan_id);
            let failed: Vec<String> = v["checks"]
                .as_array()
                .map(|cs| cs.iter().filter(|c| c["ok"] == false).filter_map(|c| c["name"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let project_id = {
                let mut inner = st.inner.lock().unwrap();
                inner.exports.insert(export_id.clone(), v.clone());
                inner.export_files.insert(export_id.clone(), out);
                inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| j.project_id.clone())
            };
            if !failed.is_empty() {
                st.log("check", format!("Export {export_id}: checks failed: {}", failed.join(", ")), "bridge", project_id.as_deref(), &[("export_id", &export_id)]);
            }
            finish(&st, &job_id, JobState::Succeeded, Some(serde_json::json!({"export_id": export_id})), None);
        }
        Ok(Err(crate::export::Failure::Write(e))) => {
            let _ = tokio::fs::remove_file(&out).await;
            finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("writing the export: {e}"))));
        }
        Ok(Err(crate::export::Failure::Decode(e))) => {
            let _ = tokio::fs::remove_file(&out).await;
            let p = ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not decode the scan").detail(e);
            finish(&st, &job_id, JobState::Failed, None, Some(&p));
        }
        // Only returned when a cancel was asked for, which is handled above.
        Ok(Err(crate::export::Failure::Cancelled)) => cancelled(&st),
        Err(e) => finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::conflict(e.to_string()))),
    }
}

/// Fields of `sent` the scanner's echo doesn't repeat (floats within 1e-9).
pub fn echo_mismatch(sent: &serde_json::Value, echo: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(sent) = sent.as_object() else { return out };
    for (k, v) in sent {
        let got = echo.get(k);
        let same = match (v, got) {
            (serde_json::Value::Number(a), Some(serde_json::Value::Number(b))) => {
                (a.as_f64().unwrap_or(f64::NAN) - b.as_f64().unwrap_or(f64::NAN)).abs() < 1e-9
            }
            (a, Some(b)) => a == b,
            (_, None) => false,
        };
        if !same {
            out.push(format!("{k}: sent {v}, echoed {}", got.map(|g| g.to_string()).unwrap_or_else(|| "nothing".into())));
        }
    }
    out
}

/// Measure a scan (blocking work off the runtime): download its file, decode
/// levelled points, find the opening, store the measurement and evidence.
/// Grid cells inside a preview box (pixels, y down; `x1 < x0` wraps across the
/// seam), padded by `margin` radians: rows `r0..=r1`, columns `c0..=c1`
/// (wrapping when `c1 < c0`).
pub struct Cells {
    pub r0: u32,
    pub r1: u32,
    pub c0: u32,
    pub c1: u32,
}

impl Cells {
    pub fn contains(&self, r: u32, c: u32) -> bool {
        r >= self.r0 && r <= self.r1 && if self.c0 <= self.c1 { c >= self.c0 && c <= self.c1 } else { c >= self.c0 || c <= self.c1 }
    }
}

pub fn crop_cells(m: &trimble_tzf::ScanMeta, pw: usize, ph: usize, b: [i64; 4], margin: f64) -> Result<Cells, String> {
    let [x0, y0, x1, y1] = b;
    if !(0..=pw as i64).contains(&x0) || !(0..=pw as i64).contains(&x1) || !(0 <= y0 && y0 < y1 && y1 <= ph as i64) || x0 == x1 {
        return Err(format!("box must lie within the {pw}x{ph} preview, with y0 < y1 and some width"));
    }
    let (cols, rows) = (m.grid_cols as i64, m.grid_rows as i64);
    let (sx, sy) = (cols as f64 / pw as f64, rows as f64 / ph as f64);
    let pad_c = (margin / m.angle_step_rad[0].abs().max(1e-9)).round() as i64;
    let pad_r = (margin / m.angle_step_rad[1].abs().max(1e-9)).round() as i64;
    let r0 = ((y0 as f64 * sy) as i64 - pad_r).clamp(0, rows - 1) as u32;
    let r1 = ((y1 as f64 * sy) as i64 + pad_r).clamp(0, rows - 1) as u32;
    let c0 = ((x0 as f64 * sx) as i64 - pad_c).rem_euclid(cols) as u32;
    let c1 = ((x1 as f64 * sx) as i64 + pad_c).rem_euclid(cols) as u32;
    Ok(Cells { r0, r1, c0, c1 })
}

pub async fn run_measure(st: Arc<AppState>, job_id: String, scan_id: String, crop: Option<([i64; 4], f64)>) {
    use crate::files::{scan_file, FileKind};
    let cancelled = |st: &AppState| finish(st, &job_id, JobState::Cancelled, None, None);
    let tzf = match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Tzf)).await {
        None => return cancelled(&st),
        Some(Ok(p)) => p,
        Some(Err(e)) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
    };
    // A crop is a box on the preview: its size maps the box onto the scan grid.
    let preview_size = match crop {
        None => None,
        Some(_) => match unless_cancelled(&st, &job_id, scan_file(&st, &scan_id, FileKind::Preview)).await {
            None => return cancelled(&st),
            Some(Ok(p)) => match tokio::fs::read(&p).await.map_err(|e| e.to_string()).and_then(|b| crate::aim::jpeg_size(&b)) {
                Ok(wh) => Some(wh),
                Err(e) => return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::invalid(format!("preview: {e}")))),
            },
            Some(Err(e)) => return finish(&st, &job_id, JobState::Failed, None, Some(&e)),
        },
    };
    if update(&st, &job_id, |r| r.job.phase = Some("measuring".into())) {
        return cancelled(&st);
    }
    let (st2, job2) = (st.clone(), job_id.clone());
    let res = tokio::task::spawn_blocking(move || -> Result<(serde_json::Value, Vec<u8>), String> {
        let t = trimble_tzf::Tzf::open(&tzf).map_err(|e| format!("open: {e}"))?;
        let meta = t.scan_meta().map_err(|e| format!("metadata: {e}"))?;
        let pts = t.grid_points().map_err(|e| format!("decode: {e}"))?;
        if cancel_requested(&st2, &job2) {
            return Err("cancelled".into());
        }
        let cells = match (crop, preview_size) {
            (Some((b, margin)), Some((pw, ph))) => Some(crop_cells(&meta, pw, ph, b, margin)?),
            _ => None,
        };
        let keep = |p: &trimble_tzf::GridPoint| cells.as_ref().is_none_or(|c| c.contains(p.row, p.col));
        let xyz: Vec<[f64; 3]> = pts.iter().filter(|p| keep(p)).map(|p| [p.point.x as f64, p.point.y as f64, p.point.z as f64]).collect();
        drop(pts);
        let o = crate::measure::opening(&xyz)?;
        let (mut v, png) = crate::measure::report(&o);
        if let (Some(c), Some((b, margin))) = (&cells, crop) {
            v["crop"] = serde_json::json!({"box": {"x0": b[0], "y0": b[1], "x1": b[2], "y1": b[3]}, "margin_rad": margin,
                                           "rows": [c.r0, c.r1], "cols": [c.c0, c.c1], "points": xyz.len(),
                                           "angle_step_mrad": (meta.angle_step_rad[0].abs().max(meta.angle_step_rad[1].abs()) * 1e4).round() / 10.0});
        }
        Ok((v, png))
    })
    .await;
    if cancel_requested(&st, &job_id) {
        return cancelled(&st);
    }
    match res {
        Ok(Ok((mut v, png))) => {
            let id = crate::util::new_id("msr");
            let path = st.data_dir.join("measurements").join(format!("{id}.png"));
            if let Some(dir) = path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(dir).await {
                    return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("measurements directory: {e}"))));
                }
            }
            if let Err(e) = tokio::fs::write(&path, &png).await {
                return finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::cache(format!("evidence image: {e}"))));
            }
            v["measurement_id"] = serde_json::json!(id);
            v["scan_id"] = serde_json::json!(scan_id);
            v["evidence_url"] = serde_json::json!(format!("/v1/measurements/{id}/evidence"));
            let failed: Vec<String> = v["checks"]
                .as_array()
                .map(|cs| cs.iter().filter(|c| c["ok"] == false).filter_map(|c| c["name"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let project_id = {
                let mut inner = st.inner.lock().unwrap();
                inner.measurements.insert(id.clone(), v.clone());
                inner.measurement_files.insert(id.clone(), path);
                inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| j.project_id.clone())
            };
            let text = format!("Opening {} × {} mm (± {} mm) in scan {scan_id}", v["opening_width_mm"], v["opening_height_mm"], v["uncertainty_mm"]);
            st.log("measure", text, "bridge", project_id.as_deref(), &[("measurement_id", &id), ("scan_id", &scan_id)]);
            if !failed.is_empty() {
                st.log("check", format!("Measurement {id}: checks failed: {}", failed.join(", ")), "bridge", project_id.as_deref(), &[("measurement_id", &id)]);
            }
            finish(&st, &job_id, JobState::Succeeded, Some(serde_json::json!({"measurement_id": id})), None);
        }
        Ok(Err(e)) => {
            let p = ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "measure_failed", "Could not measure the opening").detail(e);
            finish(&st, &job_id, JobState::Failed, None, Some(&p));
        }
        Err(e) => finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::conflict(e.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::{busy, echo_mismatch, Busy};
    use serde_json::json;
    use std::collections::HashSet;
    use x9_client::models::Task;

    #[test]
    fn busy_tells_our_cancelled_task_from_another_client() {
        let task = |id: Option<i64>| Task { id, ..Default::default() };
        let ours: HashSet<i64> = [4].into();
        assert_eq!(busy(&[], &ours), Busy::Idle);
        // The task we cancelled is still listed: wait for it.
        assert_eq!(busy(&[task(Some(4))], &ours), Busy::Ours);
        // Anything else is another client.
        assert_eq!(busy(&[task(Some(5))], &ours), Busy::Other(1));
        assert_eq!(busy(&[task(Some(4)), task(Some(5))], &ours), Busy::Other(1));
        assert_eq!(busy(&[task(None)], &ours), Busy::Other(1));
        assert_eq!(busy(&[task(Some(4))], &HashSet::new()), Busy::Other(1));
    }

    #[test]
    fn echo_check() {
        let sent = json!({"scanMode": "STD_L2", "scanType": "AreaScan", "startHA": 5.163, "colorMode": "OFF"});
        let good = json!({"scanMode": "STD_L2", "scanType": "AreaScan", "startHA": 5.163000000000001, "colorMode": "OFF", "autoLeveling": true});
        assert!(echo_mismatch(&sent, &good).is_empty());
        // The failure mode: the scanner fell back to a default full dome.
        let fallback = json!({"scanMode": "STD_L0", "scanType": "FullDome", "startHA": -1, "colorMode": "ON"});
        assert_eq!(echo_mismatch(&sent, &fallback).len(), 4);
        assert_eq!(echo_mismatch(&sent, &json!({})).len(), 4);
    }
}
