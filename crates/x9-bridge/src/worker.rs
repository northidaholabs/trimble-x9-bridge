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
pub(crate) mod tests {
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

    // ------------------------------------------------------------------
    // The job worker on a mock scanner (see `testkit`).

    use super::{crop_cells, queue_register, run_export, run_measure, Cells};
    use crate::model::{Job, JobState, ScanKind, ScanRequest};
    use crate::register::{Cloud, Pose, Registration, StationPose};
    use crate::state::{AppState, JobRecord};
    use crate::testkit::{fast_mock, Bridge, AGENT, OPERATOR};
    use crate::util::now_rfc3339;
    use axum::{http::StatusCode, routing::get, Json, Router};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// The mock's project.
    pub(crate) const PID: &str = "39187455-8278-428c-89ad-8a4835f95be6";
    /// The mock's one scan, `mock_project/1.tzf`.
    pub(crate) const SCAN0: &str = "5b5314cf-76b8-4f59-a445-ceee1e6df4c3";

    fn dome(mode: &str, color: bool) -> Value {
        json!({"project_id": PID, "kind": "full_dome", "mode": mode, "color": color})
    }

    /// Queue a scan as the operator; returns its job id.
    async fn submit(b: &Bridge, body: Value) -> String {
        let (code, job) = b.call("POST", "/v1/scans", Some(OPERATOR), Some(body)).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{job}");
        job["job_id"].as_str().unwrap().to_string()
    }

    /// Put a queued scan job straight into the state, past the admission
    /// checks (which would refuse it on a dead or flat scanner).
    fn queue_direct(st: &AppState, req: ScanRequest) -> String {
        let id = crate::util::new_id("job");
        let job = Job {
            job_id: id.clone(),
            kind: "scan".into(),
            state: JobState::Queued,
            phase: None,
            progress: None,
            requested_by: "crew".into(),
            run_id: None,
            created_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            result: None,
            problem: None,
            partial_scan_id: None,
            request: Some(req.clone()),
            project_name: Some("mock_project".into()),
        };
        st.inner.lock().unwrap().jobs.push(JobRecord {
            job,
            request: Some(req.clone()),
            project_id: req.project_id.clone(),
            project_name: "mock_project".into(),
            scanner_task_id: None,
            cancel_requested: false,
            grant_id: None,
        });
        st.wake.notify_one();
        id
    }

    fn dome_req() -> ScanRequest {
        ScanRequest { project_id: PID.into(), kind: ScanKind::FullDome, mode: "STD_L0".into(), color: false, window: None, note: None }
    }

    /// A running job record that no worker drives (for `run_export` and friends).
    fn running_job(st: &AppState, kind: &str, cancel: bool) -> String {
        let id = crate::util::new_id("job");
        let job = Job {
            job_id: id.clone(),
            kind: kind.into(),
            state: JobState::Running,
            phase: None,
            progress: None,
            requested_by: "crew".into(),
            run_id: None,
            created_at: now_rfc3339(),
            started_at: Some(now_rfc3339()),
            finished_at: None,
            result: None,
            problem: None,
            partial_scan_id: None,
            request: None,
            project_name: None,
        };
        st.inner.lock().unwrap().jobs.push(JobRecord {
            job,
            request: None,
            project_id: PID.into(),
            project_name: String::new(),
            scanner_task_id: None,
            cancel_requested: cancel,
            grant_id: None,
        });
        id
    }

    fn is_final(j: &Value) -> bool {
        matches!(j["state"].as_str(), Some("succeeded" | "failed" | "cancelled"))
    }

    pub(crate) async fn wait_job(b: &Bridge, id: &str, secs: f64) -> Value {
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, secs, is_final).await
    }

    fn job_state(b: &Bridge, id: &str) -> crate::state::JobRecord {
        b.st.inner.lock().unwrap().jobs.iter().find(|j| j.job.job_id == id).cloned().unwrap()
    }

    fn logged(st: &AppState, needle: &str) -> bool {
        st.inner.lock().unwrap().log.iter().any(|e| e.text.contains(needle))
    }

    /// The registration jobs, newest last.
    fn register_jobs(st: &AppState) -> Vec<Job> {
        st.inner.lock().unwrap().jobs.iter().filter(|j| j.job.kind == "register").map(|j| j.job.clone()).collect()
    }

    /// Phases a job's events went through, in order, without repeats.
    fn phases(rx: &mut tokio::sync::broadcast::Receiver<crate::state::Event>, id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(ev) if ev.kind == "job" && ev.data["job_id"] == id => {
                    if let Some(p) = ev.data["phase"].as_str() {
                        if out.last().map(String::as_str) != Some(p) {
                            out.push(p.to_string());
                        }
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return out,
            }
        }
    }

    /// A local port nothing listens on.
    pub(crate) async fn dead_base() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}/api/v1")
    }

    /// Fixture files the mock serves for every scan: not a real scan file or
    /// JPEG (those are private), so decoding them fails.
    pub(crate) fn junk_fixtures() -> (std::path::PathBuf, x9_mock::Config) {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-fixtures"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["dome.tzf", "dome.jpg", "area.tzf", "area.jpg"] {
            std::fs::write(dir.join(f), b"not a real scan file").unwrap();
        }
        let cfg = x9_mock::Config { fixtures: Some(dir.to_string_lossy().into_owned()), ..fast_mock() };
        (dir, cfg)
    }

    // ---------------------------------------------------------------- scans ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_dome_goes_from_leveling_through_sweeping_to_a_new_scan_and_registers() {
        let b = Bridge::new().await;
        let mut rx = b.st.events.subscribe();
        b.start_worker();
        let id = submit(&b, dome("STD_L0", false)).await;
        let j = wait_job(&b, &id, 10.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!(j["progress"], 1.0);
        let scan_id = j["result"]["scan_id"].as_str().unwrap().to_string();
        assert!(!scan_id.is_empty() && scan_id != SCAN0);
        let seen = phases(&mut rx, &id);
        assert_eq!(seen.first().map(String::as_str), Some("leveling"), "{seen:?}");
        assert!(seen.iter().any(|p| p == "sweeping"), "{seen:?}");
        assert!(!seen.iter().any(|p| p == "imaging"), "no color, no imaging: {seen:?}");
        // The bridge keeps when it ran and how long each part took.
        let (_, scan) = b.call("GET", &format!("/v1/scans/{scan_id}"), Some(OPERATOR), None).await;
        assert_eq!(scan["kind"], "full_dome");
        assert!(scan["timings"]["geometry_ready_s"].as_f64().is_some(), "{scan}");
        assert!(scan["timings"]["done_s"].as_f64().is_some(), "{scan}");
        assert!(logged(&b.st, "Geometry ready: mock_project/2.tzf (station 2)"));
        assert!(logged(&b.st, "Scanner started STD_L0 (color off)"));
        assert!(logged(&b.st, &format!("Scan job {id} finished")));
        // A new station is registered at once. No scan file can be fetched
        // from the mock, so every station is skipped, and that is logged.
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |_| !register_jobs(&b.st).is_empty()).await;
        let reg = register_jobs(&b.st);
        assert_eq!(reg.len(), 1);
        let r = wait_job(&b, &reg[0].job_id, 10.0).await;
        assert_eq!(r["state"], "succeeded", "{r}");
        assert_eq!(r["result"]["placed"], 0, "{r}");
        assert!(logged(&b.st, "Station 1 could not be registered"));
        assert!(logged(&b.st, "Station 2 could not be registered"));
        assert!(b.st.inner.lock().unwrap().registrations.contains_key(PID));
        assert!(b.dir.join("data/registrations.json").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_color_scan_is_imaging_after_its_geometry_lands() {
        let b = Bridge::new().await;
        let mut rx = b.st.events.subscribe();
        b.start_worker();
        let id = submit(&b, dome("STD_L0", true)).await;
        let j = wait_job(&b, &id, 10.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        let seen = phases(&mut rx, &id);
        let (s, i) = (seen.iter().position(|p| p == "sweeping"), seen.iter().position(|p| p == "imaging"));
        assert!(s.is_some() && i.is_some() && s < i, "{seen:?}");
        assert!(logged(&b.st, "(color on)"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_area_scan_keeps_its_window_and_queues_no_registration() {
        let b = Bridge::new().await;
        b.start_worker();
        let w = json!({"start_ha": 1.0, "delta_ha": 0.5, "start_va": 1.2, "delta_va": 0.4});
        let id = submit(&b, json!({"project_id": PID, "kind": "area", "mode": "STD_L0", "window": w})).await;
        let j = wait_job(&b, &id, 10.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        let scan_id = j["result"]["scan_id"].as_str().unwrap();
        let (_, scan) = b.call("GET", &format!("/v1/scans/{scan_id}"), Some(OPERATOR), None).await;
        assert_eq!(scan["kind"], "area", "{scan}");
        assert_eq!(scan["window"]["start_ha"], 1.0, "{scan}");
        assert!(register_jobs(&b.st).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_clients_task_fails_the_scan_as_scanner_busy() {
        let b = Bridge::with_mock(x9_mock::Config { speed: 20.0, ..fast_mock() }).await;
        // Someone else (the vendor app) starts a scan on the scanner directly.
        let req = x9_client::models::ScanRequest { scan_mode: Some("STD_L0".into()), project_name: Some("mock_project".into()), ..Default::default() };
        let theirs = b.st.scanner.launch_scan(&req).await.unwrap();
        b.start_worker();
        let id = submit(&b, dome("STD_L0", false)).await;
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "failed", "{j}");
        assert_eq!(j["problem"]["code"], "scanner_busy", "{j}");
        assert_eq!(j["problem"]["detail"], "1 task(s) running", "{j}");
        // The bridge left their task alone.
        let tasks = b.st.scanner.tasks().await.unwrap();
        assert_eq!(tasks.first().and_then(|t| t.id), theirs.id);
        assert!(logged(&b.st, &format!("Scan job {id} failed: scanner_busy")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_queued_scan_gives_its_grant_use_back_and_never_reaches_the_scanner() {
        let b = Bridge::new().await;
        // An agent's scan under a grant: queued without approval, using the grant.
        let g = json!({"project_id": PID, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 1});
        let (code, grant) = b.call("POST", "/v1/grants", Some(OPERATOR), Some(g)).await;
        assert_eq!(code, StatusCode::CREATED, "{grant}");
        let (code, job) = b.call("POST", "/v1/scans", Some(AGENT), Some(dome("STD_L0", false))).await;
        assert_eq!((code, job["state"].as_str()), (StatusCode::ACCEPTED, Some("queued")), "{job}");
        assert_eq!(b.st.inner.lock().unwrap().grants[0].scans_used, 1);
        let id = job["job_id"].as_str().unwrap();
        let (code, j) = b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(AGENT), None).await;
        assert_eq!((code, j["state"].as_str()), (StatusCode::OK, Some("cancelled")), "{j}");
        assert_eq!(b.st.inner.lock().unwrap().grants[0].scans_used, 0);
        // The worker passes over it.
        b.start_worker();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(b.st.scanner.tasks().await.unwrap().is_empty());
        assert_eq!(b.st.scanner.scans().await.unwrap().len(), 1);
        assert_eq!(job_state(&b, id).job.state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_mid_sweep_stops_the_task_and_points_at_the_partial_scan() {
        // Slow enough to cancel mid-sweep; the cancelled task lingers 0.1 s.
        let b = Bridge::with_mock(x9_mock::Config { speed: 40.0, cancel_linger: 4.0, ..fast_mock() }).await;
        b.start_worker();
        let id = submit(&b, dome("STD_L0", false)).await;
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |j| j["phase"] == "sweeping" && j["progress"].as_f64().unwrap_or(0.0) > 0.0).await;
        let (code, j) = b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(OPERATOR), None).await;
        assert_eq!((code, j["state"].as_str()), (StatusCode::OK, Some("running")), "{j}");
        let j = b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |j| !j["partial_scan_id"].is_null()).await;
        assert_eq!(j["state"], "cancelled", "{j}");
        let partial = j["partial_scan_id"].as_str().unwrap();
        assert_ne!(partial, SCAN0);
        assert!(logged(&b.st, &format!("kept a partial scan from cancelled job {id}")));
        assert!(b.st.inner.lock().unwrap().scan_meta.contains_key(partial));
        // The scanner listed it: fewer points than a full sweep.
        let scans = b.st.scanner.scans().await.unwrap();
        let s = scans.iter().find(|s| s.scan_uuid.as_deref() == Some(partial)).unwrap();
        assert!(s.tzf.as_ref().and_then(|t| t.nb_points).unwrap() < 6_281_859);
        // A cancelled scan queues no registration.
        assert!(register_jobs(&b.st).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_while_leveling_leaves_no_partial_scan() {
        let b = Bridge::with_mock(x9_mock::Config { speed: 20.0, cancel_linger: 2.0, ..fast_mock() }).await;
        b.start_worker();
        let id = submit(&b, dome("STD_L0", false)).await;
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |_| job_state(&b, &id).scanner_task_id.is_some()).await;
        b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(OPERATOR), None).await;
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "cancelled", "{j}");
        // Wait for the worker to be done following the cancelled task.
        let t0 = std::time::Instant::now();
        while !b.st.inner.lock().unwrap().cancelled_tasks.is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(5), "the cancelled task was never seen to clear");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(job_state(&b, &id).job.partial_scan_id.is_none());
        assert_eq!(b.st.scanner.scans().await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fallback_echo_fails_the_job_and_the_next_waits_out_the_cancelled_task() {
        // The mock runs HS_L1 requests as a default color full dome. Its
        // cancelled task stays listed 0.25 s.
        let cfg = x9_mock::Config { fallback_mode: Some("HS_L1".into()), cancel_linger: 100.0, ..fast_mock() };
        let b = Bridge::with_mock(cfg).await;
        let mut rx = b.st.events.subscribe();
        let bad = submit(&b, dome("HS_L1", false)).await;
        let good = submit(&b, dome("STD_L0", false)).await;
        b.start_worker();
        let j = wait_job(&b, &bad, 5.0).await;
        assert_eq!(j["state"], "failed", "{j}");
        let detail = j["problem"]["detail"].as_str().unwrap();
        assert!(detail.contains("did not accept the request as sent"), "{detail}");
        assert!(detail.contains(r#"scanMode: sent "HS_L1", echoed "STD_L0""#), "{detail}");
        assert!(detail.contains(r#"colorMode: sent "OFF", echoed "ON""#), "{detail}");
        // The next scan waited for the stopped task to clear, then ran.
        let j = wait_job(&b, &good, 5.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        let seen = phases(&mut rx, &good);
        assert_eq!(&seen[..3], ["leveling", "waiting", "leveling"], "{seen:?}");
        assert!(logged(&b.st, "Waiting for the scanner to clear the cancelled task"));
        assert!(b.st.inner.lock().unwrap().cancelled_tasks.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_scan_waiting_on_a_cancelled_task_gives_its_grant_use_back() {
        // The fallback task lingers a full second.
        let cfg = x9_mock::Config { fallback_mode: Some("HS_L1".into()), cancel_linger: 400.0, ..fast_mock() };
        let b = Bridge::with_mock(cfg).await;
        let g = json!({"project_id": PID, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 5});
        b.call("POST", "/v1/grants", Some(OPERATOR), Some(g)).await;
        let bad = submit(&b, dome("HS_L1", false)).await;
        let (_, job) = b.call("POST", "/v1/scans", Some(AGENT), Some(dome("STD_L0", false))).await;
        let id = job["job_id"].as_str().unwrap().to_string();
        assert_eq!(b.st.inner.lock().unwrap().grants[0].scans_used, 1);
        b.start_worker();
        assert_eq!(wait_job(&b, &bad, 5.0).await["state"], "failed");
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |j| j["phase"] == "waiting").await;
        b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(AGENT), None).await;
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "cancelled", "{j}");
        assert!(j["partial_scan_id"].is_null());
        assert_eq!(b.st.inner.lock().unwrap().grants[0].scans_used, 0);
        assert!(std::fs::read_to_string(b.dir.join("data/grants.json")).unwrap().contains("\"scans_used\": 0"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_scanner_fails_the_job_as_disconnected() {
        let b = Bridge::on_scanner(&dead_base().await);
        let id = queue_direct(&b.st, dome_req());
        b.start_worker();
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "failed", "{j}");
        assert_eq!(j["problem"]["code"], "scanner_disconnected", "{j}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flat_battery_fails_the_job_before_the_scanner_starts() {
        let b = Bridge::with_mock(x9_mock::Config { battery: 5, ..fast_mock() }).await;
        let id = queue_direct(&b.st, dome_req());
        b.start_worker();
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["problem"]["code"], "battery_low", "{j}");
        assert!(b.st.scanner.tasks().await.unwrap().is_empty());
    }

    // A scripted scanner for failures the mock can't produce.

    #[derive(Clone, Default)]
    struct Fake {
        /// `/tasks` answers this many times, then fails.
        tasks_ok: usize,
        scans_fail: bool,
        launch_fail: bool,
        calls: Arc<AtomicUsize>,
    }

    async fn fake_scanner(f: Fake) -> String {
        let status = || async { Json(json!({"batteryLevel": 80, "pluggedToAC": false, "storage": {"availableSize": 20_000_000_000u64}})) };
        let (f1, f2, f3) = (f.clone(), f.clone(), f.clone());
        let app = Router::new()
            .route("/api/v1/status", get(status))
            .route("/api/v1/projects", get(|| async { Json(json!([{"id": 0, "projectName": "mock_project", "projectUUID": PID}])) }))
            .route(
                "/api/v1/tasks",
                get(move || {
                    let f = f1.clone();
                    async move {
                        if f.calls.fetch_add(1, Ordering::SeqCst) < f.tasks_ok {
                            Ok(Json(json!([])))
                        } else {
                            Err(StatusCode::INTERNAL_SERVER_ERROR)
                        }
                    }
                }),
            )
            .route(
                "/api/v1/scans",
                get(move || {
                    let f = f2.clone();
                    async move { if f.scans_fail { Err(StatusCode::INTERNAL_SERVER_ERROR) } else { Ok(Json(json!([]))) } }
                })
                .post(move |Json(req): Json<Value>| {
                    let f = f3.clone();
                    async move {
                        if f.launch_fail {
                            Err(StatusCode::INTERNAL_SERVER_ERROR)
                        } else {
                            Ok((StatusCode::ACCEPTED, Json(json!({"id": 7, "state": "initializing", "scanRequest": req}))))
                        }
                    }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}/api/v1")
    }

    async fn run_on_fake(f: Fake) -> Value {
        let b = Bridge::on_scanner(&fake_scanner(f).await);
        let id = queue_direct(&b.st, dome_req());
        b.start_worker();
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "failed", "{j}");
        j["problem"].clone()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scanner_whose_task_list_fails_fails_the_job_before_launching() {
        let p = run_on_fake(Fake::default()).await;
        assert_eq!(p["code"], "scanner_error", "{p}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scanner_whose_scan_list_fails_fails_the_job_before_launching() {
        let p = run_on_fake(Fake { tasks_ok: usize::MAX, scans_fail: true, ..Default::default() }).await;
        assert_eq!(p["code"], "scanner_error", "{p}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_launch_fails_the_job() {
        let p = run_on_fake(Fake { tasks_ok: usize::MAX, launch_fail: true, ..Default::default() }).await;
        assert_eq!(p["code"], "scanner_error", "{p}");
        assert!(p["title"].as_str().unwrap().contains("(500)"), "{p}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn losing_the_scanner_mid_scan_fails_after_ten_errors_in_a_row() {
        // One good `/tasks` (the idle check), then errors.
        let f = Fake { tasks_ok: 1, ..Default::default() };
        let calls = f.calls.clone();
        let p = run_on_fake(f).await;
        assert_eq!(p["code"], "scanner_error", "{p}");
        assert_eq!(calls.load(Ordering::SeqCst), 11);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scan_that_ends_without_a_file_fails() {
        // The scanner takes the scan, lists no task and files nothing.
        let p = run_on_fake(Fake { tasks_ok: usize::MAX, ..Default::default() }).await;
        assert_eq!(p["detail"], "the scanner finished without producing a scan file", "{p}");
    }

    // -------------------------------------------------------- registration ---

    type Rect = ([f32; 3], [f32; 3]);

    fn boxed(lo: [f32; 3], hi: [f32; 3]) -> Vec<Rect> {
        (0..3)
            .flat_map(|k| {
                [lo[k], hi[k]].map(|v| {
                    let (mut a, mut b) = (lo, hi);
                    a[k] = v;
                    b[k] = v;
                    (a, b)
                })
            })
            .collect()
    }

    /// A station's 5 cm cloud of a 12 × 8 m room with a partition (so it has
    /// one right fit), as a scanner at `at` turned by `turn` (rad) sees it.
    pub(crate) fn room_cloud(at: [f32; 3], turn: f32) -> Cloud {
        let mut rects = boxed([-6.0, -4.0, -1.5], [6.0, 4.0, 1.2]);
        rects.push(([3.0, -4.0, -1.5], [3.0, -1.0, 0.5]));
        let (s, c) = turn.sin_cos();
        let (mut pts, mut range) = (Vec::new(), Vec::new());
        for (lo, hi) in rects {
            let steps = |k: usize| ((hi[k] - lo[k]) / 0.05).round() as usize;
            for i in 0..=steps(0) {
                for j in 0..=steps(1) {
                    for k in 0..=steps(2) {
                        let w = [lo[0] + i as f32 * 0.05, lo[1] + j as f32 * 0.05, lo[2] + k as f32 * 0.05];
                        let d = [w[0] - at[0], w[1] - at[1], w[2] - at[2]];
                        pts.push([c * d[0] + s * d[1], -s * d[0] + c * d[1], d[2]]);
                        range.push((d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt());
                    }
                }
            }
        }
        Cloud { step_rad: 0.0005, pts, range }
    }

    /// Run a scan on the mock outside the bridge; returns its scan id.
    async fn scan_on_mock(b: &Bridge) -> String {
        let before: HashSet<String> = b.st.scanner.scans().await.unwrap().into_iter().filter_map(|s| s.scan_uuid).collect();
        let req = x9_client::models::ScanRequest {
            scan_mode: Some("STD_L0".into()),
            scan_type: Some("FullDome".into()),
            project_name: Some("mock_project".into()),
            project_uuid: Some(PID.into()),
            ..Default::default()
        };
        b.st.scanner.launch_scan(&req).await.unwrap();
        let t0 = std::time::Instant::now();
        loop {
            assert!(t0.elapsed() < Duration::from_secs(5), "the mock's scan never finished");
            tokio::time::sleep(Duration::from_millis(20)).await;
            if !b.st.scanner.tasks().await.unwrap().is_empty() {
                continue;
            }
            let now = b.st.scanner.scans().await.unwrap();
            if let Some(id) = now.into_iter().filter_map(|s| s.scan_uuid).find(|u| !before.contains(u)) {
                return id;
            }
        }
    }

    async fn register(b: &Bridge) -> Value {
        let (code, job) = b.call("POST", &format!("/v1/projects/{PID}/registration"), Some(OPERATOR), None).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{job}");
        wait_job(b, job["job_id"].as_str().unwrap(), 10.0).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_bridge_registers_a_station_whose_cloud_it_already_has() {
        let b = Bridge::new().await;
        // As if decoded before: no scan file is needed (the real ones are private).
        let dir = b.dir.join("data/registration");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(super::cloud_name(SCAN0)), room_cloud([1.0, 0.5, 0.0], 0.0).to_bytes()).unwrap();
        // Clouds cut at an older range are dropped.
        std::fs::write(dir.join(format!("{SCAN0}.r60.cloud")), b"old").unwrap();
        let j = register(&b).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!((j["result"]["stations"].as_u64(), j["result"]["placed"].as_u64()), (Some(1), Some(1)), "{j}");
        assert!(!dir.join(format!("{SCAN0}.r60.cloud")).exists());
        assert!(logged(&b.st, "Station 1 is the project frame"));
        assert!(b.st.merged_path(PID).unwrap().exists());
        assert_eq!(b.st.prepared.lock().unwrap().len(), 1);
        // Again, from the prepared station in memory; an unchanged verdict logs nothing.
        let n = b.st.inner.lock().unwrap().log.len();
        assert_eq!(register(&b).await["result"]["placed"], 1);
        let after: Vec<String> = b.st.inner.lock().unwrap().log[n..].iter().map(|e| e.text.clone()).collect();
        assert!(!after.iter().any(|t| t.starts_with("Station ")), "{after:?}");
    }

    fn audit(verdict: &str, ties: &[&str]) -> crate::register::Audit {
        crate::register::Audit { verdict: verdict.into(), ties: ties.iter().map(|s| s.to_string()).collect(), reasons: vec!["a reason".into()], ..Default::default() }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_frame_carries_the_mission_faces_and_each_verdict_is_logged() {
        let b = Bridge::new().await;
        let mut ids = vec![SCAN0.to_string()];
        for _ in 0..5 {
            ids.push(scan_on_mock(&b).await);
        }
        // A mission with one wall face, in station 2's frame (the frame before).
        let m = json!({"goal": "Survey the room", "kind": "room", "done_when": "every wall is seen",
                       "targets": [{"target_id": "east", "name": "East wall", "kind": "wall", "face": {"from": [1.0, 0.0], "to": [1.0, 2.0]}}]});
        let (code, v) = b.call("PUT", &format!("/v1/projects/{PID}/mission"), Some(OPERATOR), Some(m)).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let prev = Registration {
            project_id: PID.into(),
            frame_scan_id: Some(ids[1].clone()),
            stations: vec![StationPose { scan_id: ids[1].clone(), station: 2, pose: Some(Pose { turn_deg: 0.0, xyz_m: [0.0; 3] }), audit: None }],
            pairs: vec![],
            walls: vec![],
            summary: String::new(),
            updated_at: now_rfc3339(),
        };
        b.st.inner.lock().unwrap().registrations.insert(PID.into(), prev);

        // A worker registers the six stations (station 6 it could not decode).
        b.st.work.lock().unwrap().claim("laptop", "agent");
        let (_, job) = b.call("POST", &format!("/v1/projects/{PID}/registration"), Some(OPERATOR), None).await;
        let id = job["job_id"].as_str().unwrap().to_string();
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |_| b.st.work.lock().unwrap().claim("laptop", "agent").is_some()).await;
        let p2 = Pose { turn_deg: 30.0, xyz_m: [-3.0, -1.5, 0.0] };
        let res = crate::work::WorkResult {
            pairs: vec![],
            poses: vec![Some(Pose { turn_deg: 0.0, xyz_m: [0.0; 3] }), Some(p2), Some(p2), Some(p2), None],
            audits: vec![audit("frame", &[]), audit("confirmed", &[&ids[0]]), audit("single", &[&ids[0]]), audit("doubtful", &[]), audit("ambiguous", &[])],
            walls: vec![],
            skipped: vec![(6, "open: not a scan file".into())],
            frame: Some(ids[0].clone()),
        };
        {
            let mut w = b.st.work.lock().unwrap();
            w.put_merged(&id, "agent", crate::register::Merged::default().to_bytes()).unwrap();
            w.deliver(&id, "agent", Ok(res)).unwrap();
        }
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!((j["result"]["stations"].as_u64(), j["result"]["placed"].as_u64()), (Some(5), Some(4)), "{j}");
        let reg = b.st.inner.lock().unwrap().registrations.get(PID).cloned().unwrap();
        assert_eq!(reg.frame_scan_id.as_deref(), Some(ids[0].as_str()));
        // The face moved with station 2 into station 1's frame.
        let face = b.st.inner.lock().unwrap().missions[PID].targets[0].face.unwrap();
        let (s, c) = 30f64.to_radians().sin_cos();
        assert!((face.from[0] - (c - 3.0)).abs() < 1e-9 && (face.from[1] - (s - 1.5)).abs() < 1e-9, "{face:?}");
        assert!(std::fs::read_to_string(b.dir.join("data/missions.json")).unwrap().contains("East wall"));
        for text in [
            "The project frame is now station 1's",
            "Station 1 is the project frame",
            "Station 2 registered and confirmed",
            "Station 3 registered on one tie",
            "Station 4 is doubtful: a reason",
            "Station 5 is not placed: a reason",
            "Station 6 could not be registered: open: not a scan file",
            "Registration (registered on laptop)",
        ] {
            assert!(logged(&b.st, text), "{text}");
        }
        // Untrusted verdicts are checks, trusted ones notes.
        let kind = |t: &str| b.st.inner.lock().unwrap().log.iter().find(|e| e.text.starts_with(t)).map(|e| e.kind.clone()).unwrap();
        assert_eq!((kind("Station 2 registered").as_str(), kind("Station 4 is doubtful").as_str()), ("note", "check"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_station_that_lands_during_a_registration_runs_it_once_more() {
        let b = Bridge::new().await;
        let first = queue_register(&b.st, PID, "crew");
        // While it runs, another full dome lands: the same job, run again.
        let again = queue_register(&b.st, PID, "crew");
        assert_eq!(first.job_id, again.job_id);
        let j = wait_job(&b, &first.job_id, 10.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!(register_jobs(&b.st).len(), 1);
        assert!(b.st.inner.lock().unwrap().registering.is_empty());
        assert!(b.st.inner.lock().unwrap().register_again.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn junk_scan_files_are_skipped_with_the_decoder_error() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let j = register(&b).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!(j["result"]["stations"], 0, "{j}");
        let log = b.st.inner.lock().unwrap().log.clone();
        let skip = log.iter().find(|e| e.text.starts_with("Station 1 could not be registered")).unwrap();
        assert!(skip.text.contains("open: "), "{}", skip.text);
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unwritable_registration_directory_fails_the_job_as_a_cache_error() {
        let b = Bridge::new().await;
        // A file where the directory should be.
        std::fs::write(b.dir.join("data/registration"), b"in the way").unwrap();
        let j = register(&b).await;
        assert_eq!(j["state"], "failed", "{j}");
        assert_eq!(j["problem"]["code"], "cache_error", "{j}");
        assert!(j["problem"]["detail"].as_str().unwrap().starts_with("registration directory"), "{j}");
        assert!(logged(&b.st, "Registration job"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreadable_station_cloud_fails_the_registration() {
        let b = Bridge::new().await;
        let dir = b.dir.join("data/registration");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(super::cloud_name(SCAN0)), b"not a cloud").unwrap();
        let j = register(&b).await;
        assert_eq!(j["state"], "failed", "{j}");
        assert_eq!(j["problem"]["code"], "cache_error", "{j}");
        assert_eq!(j["problem"]["detail"], "not a station cloud", "{j}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_registration_stops_it() {
        let b = Bridge::new().await;
        // Hold the worker list so the bridge offers the job and waits.
        b.st.work.lock().unwrap().claim("laptop", "agent");
        let (_, job) = b.call("POST", &format!("/v1/projects/{PID}/registration"), Some(OPERATOR), None).await;
        let id = job["job_id"].as_str().unwrap();
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |j| j["phase"] == "waiting for a worker").await;
        b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(OPERATOR), None).await;
        let j = wait_job(&b, id, 5.0).await;
        assert_eq!(j["state"], "cancelled", "{j}");
        assert!(logged(&b.st, &format!("Registration job {id} cancelled")));
    }

    #[test]
    fn set_phase_reports_a_workers_progress_on_the_job() {
        let st = AppState::new(
            x9_client::Client::connect("http://127.0.0.1:1/api/v1").unwrap(),
            Default::default(),
            Default::default(),
            Duration::from_millis(20),
            std::env::temp_dir(),
            std::env::temp_dir(),
        );
        let id = running_job(&st, "register", false);
        super::set_phase(&st, &id, "matching on laptop", Some(0.5));
        super::set_phase(&st, &id, "checking", None);
        let j = st.inner.lock().unwrap().jobs[0].job.clone();
        assert_eq!((j.phase.as_deref(), j.progress), (Some("checking"), Some(0.5)));
        // An unknown job is a no-op.
        super::set_phase(&st, "job_nope", "x", None);
    }

    // ------------------------------------------------- exports, measuring ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_export_of_a_scan_with_no_file_fails() {
        let b = Bridge::new().await;
        let (code, job) = b.call("POST", &format!("/v1/scans/{SCAN0}/exports"), Some(OPERATOR), Some(json!({"format": "ply"}))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{job}");
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 5.0).await;
        assert_eq!(j["state"], "failed", "{j}");
        assert_eq!(j["problem"]["code"], "scanner_error", "{j}");
        assert!(logged(&b.st, "Export job"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_export_of_a_junk_scan_file_fails_to_decode_and_leaves_no_file() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let (_, job) = b.call("POST", &format!("/v1/scans/{SCAN0}/exports"), Some(OPERATOR), Some(json!({"format": "las"}))).await;
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 5.0).await;
        assert_eq!(j["problem"]["code"], "decode_failed", "{j}");
        let left = std::fs::read_dir(b.dir.join("data/exports")).map(|d| d.count()).unwrap_or(0);
        assert_eq!(left, 0);
        // Without intensity it skips the preview, and fails the same way.
        let (_, job) = b.call("POST", &format!("/v1/scans/{SCAN0}/exports"), Some(OPERATOR), Some(json!({"format": "ply", "intensity": "none"}))).await;
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 5.0).await;
        assert_eq!(j["problem"]["code"], "decode_failed", "{j}");
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unwritable_exports_directory_fails_the_export_as_a_cache_error() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        std::fs::write(b.dir.join("data/exports"), b"in the way").unwrap();
        let (_, job) = b.call("POST", &format!("/v1/scans/{SCAN0}/exports"), Some(OPERATOR), Some(json!({"format": "ply"}))).await;
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 5.0).await;
        assert_eq!(j["problem"]["code"], "cache_error", "{j}");
        assert!(j["problem"]["detail"].as_str().unwrap().starts_with("exports directory"), "{j}");
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_export_or_measurement_cancelled_before_it_starts_is_cancelled() {
        let b = Bridge::new().await;
        let req: crate::export::ExportRequest = serde_json::from_value(json!({"format": "ply"})).unwrap();
        let id = running_job(&b.st, "export", true);
        run_export(b.st.clone(), id.clone(), SCAN0.into(), req).await;
        assert_eq!(job_state(&b, &id).job.state, JobState::Cancelled);
        assert!(logged(&b.st, &format!("Export job {id} cancelled")));
        let id = running_job(&b.st, "measure", true);
        run_measure(b.st.clone(), id.clone(), SCAN0.into(), None).await;
        assert_eq!(job_state(&b, &id).job.state, JobState::Cancelled);
        assert!(logged(&b.st, &format!("Measurement job {id} cancelled")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn measuring_a_junk_scan_file_fails() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let (code, job) = b.call("POST", &format!("/v1/scans/{SCAN0}/measurements"), Some(OPERATOR), Some(json!({"kind": "opening"}))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{job}");
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 5.0).await;
        assert_eq!(j["problem"]["code"], "measure_failed", "{j}");
        assert!(j["problem"]["detail"].as_str().unwrap().starts_with("open: "), "{j}");
        // A crop needs the preview's size: a junk JPEG fails first.
        let body = json!({"kind": "opening", "box": {"x0": 10, "y0": 10, "x1": 20, "y1": 20}});
        let (_, job) = b.call("POST", &format!("/v1/scans/{SCAN0}/measurements"), Some(OPERATOR), Some(body)).await;
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 5.0).await;
        assert_eq!(j["problem"]["code"], "invalid_request", "{j}");
        assert!(j["problem"]["detail"].as_str().unwrap().starts_with("preview: "), "{j}");
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn measuring_a_scan_with_no_file_fails() {
        let b = Bridge::new().await;
        let id = running_job(&b.st, "measure", false);
        run_measure(b.st.clone(), id.clone(), SCAN0.into(), Some(([0, 0, 10, 10], 0.01))).await;
        let j = job_state(&b, &id).job;
        assert_eq!(j.state, JobState::Failed);
        assert_eq!(j.problem.unwrap()["code"], "scanner_error");
    }

    fn meta(rows: u32, cols: u32) -> trimble_tzf::ScanMeta {
        trimble_tzf::ScanMeta {
            grid_rows: rows,
            grid_cols: cols,
            tile_size: 0,
            n_points: 0,
            angle_step_rad: [std::f64::consts::TAU / cols as f64, std::f64::consts::PI / rows as f64],
            start_ha_rad: 0.0,
            start_va_rad: 0.0,
            tilt: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    #[test]
    fn a_preview_box_maps_onto_grid_cells_with_its_margin() {
        let m = meta(1000, 2000);
        // Preview 200 × 100: 10 cells a pixel each way; no margin.
        let c = crop_cells(&m, 200, 100, [20, 10, 40, 30], 0.0).unwrap();
        assert_eq!((c.r0, c.r1, c.c0, c.c1), (100, 300, 200, 400));
        assert!(c.contains(150, 300) && !c.contains(50, 300) && !c.contains(150, 500));
        // A margin of 5 cells' angle each way, clamped at the top row.
        let step = m.angle_step_rad[0];
        let c = crop_cells(&m, 200, 100, [20, 0, 40, 30], 5.0 * step).unwrap();
        assert_eq!((c.r0, c.c0, c.c1), (0, 195, 405));
        // Across the seam: x1 < x0 wraps.
        let c = crop_cells(&m, 200, 100, [190, 10, 10, 30], 0.0).unwrap();
        assert_eq!((c.c0, c.c1), (1900, 100));
        assert!(c.contains(150, 1950) && c.contains(150, 50) && !c.contains(150, 1000));
        let w = Cells { r0: 0, r1: 1, c0: 5, c1: 2 };
        assert!(w.contains(0, 6) && w.contains(1, 1) && !w.contains(2, 1));
    }

    #[test]
    fn a_preview_box_outside_the_preview_or_empty_is_refused() {
        let m = meta(100, 200);
        for b in [[-1, 0, 10, 10], [0, 0, 201, 10], [0, 10, 10, 10], [0, 0, 10, 101], [5, 0, 5, 10]] {
            assert!(crop_cells(&m, 200, 100, b, 0.0).err().unwrap().contains("200x100"), "{b:?}");
        }
    }

    #[test]
    fn cloud_names_carry_the_range_they_were_cut_at() {
        assert_eq!(super::cloud_name("abc"), "abc.r60t30.cloud");
        assert!(super::prepared_budget() > 0);
    }
}
