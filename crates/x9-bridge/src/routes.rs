//! HTTP handlers for the implemented part of API v1.

use crate::files::{scan_file, FileKind};
use crate::{aim, export, map};
use crate::model::{Grant, GrantRequest, Job, JobState, Role, ScanRequest};
use crate::problem::ApiError;
use crate::rules::{admit_scan, grant_covers, Admission};
use crate::state::{AppState, JobRecord};
use crate::util::{new_id, now_rfc3339, now_unix, parse_rfc3339};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;

type St = State<Arc<AppState>>;
type ApiResult<T> = Result<T, ApiError>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(crate::ui::to_ui))
        .route("/ui", get(crate::ui::to_ui))
        .route("/ui/", get(crate::ui::index))
        .route("/ui/:file", get(crate::ui::asset))
        .route("/v1/health", get(health))
        .route("/v1/whoami", get(whoami))
        .route("/v1/status", get(status))
        .route("/v1/rules", get(rules))
        .route("/v1/scan-modes", get(scan_modes))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/scans", get(list_scans).post(start_scan))
        .route("/v1/scans/:scan_id", get(get_scan).delete(delete_scan))
        .route("/v1/scans/:scan_id/preview", get(scan_preview))
        .route("/v1/scans/:scan_id/tzf", get(scan_tzf))
        .route("/v1/scans/:scan_id/view", get(scan_view))
        .route("/v1/scans/:scan_id/coverage", get(scan_coverage))
        .route("/v1/scans/:scan_id/coverage/image", get(scan_coverage_image))
        .route("/v1/scans/:scan_id/next-station", get(get_next_station).put(put_next_station))
        .route("/v1/scans/:scan_id/next-station/image", get(get_next_station_image))
        .route("/v1/scans/:scan_id/next-station/plan", get(plan_next_station))
        .route("/v1/next-stations", get(list_next_stations))
        .route("/v1/missions", get(list_missions))
        .route("/v1/projects/:project_id/registration", get(get_registration).post(post_registration))
        .route("/v1/projects/:project_id/registration/plan", get(get_registration_plan))
        .route("/v1/projects/:project_id/registration/face", post(post_face))
        .route("/v1/projects/:project_id/minimap", get(get_minimap))
        .route("/v1/workers", get(list_workers))
        .route("/v1/work/claim", post(work_claim))
        .route("/v1/work/:job_id/progress", post(work_progress))
        .route("/v1/work/:job_id/merged", axum::routing::put(work_merged).layer(axum::extract::DefaultBodyLimit::max(1 << 29)))
        .route("/v1/work/:job_id/result", post(work_result).layer(axum::extract::DefaultBodyLimit::max(1 << 26)))
        .route("/v1/work/:job_id/fail", post(work_fail))
        .route("/v1/projects/:project_id/mission", get(get_mission).put(put_mission))
        .route("/v1/projects/:project_id/mission/targets", post(update_targets))
        .route("/v1/projects/:project_id/mission/ready", post(mission_ready))
        .route("/v1/projects/:project_id/mission/sign-off", post(mission_sign_off))
        .route("/v1/scans/:scan_id/exports", post(start_export))
        .route("/v1/exports/:export_id", get(get_export))
        .route("/v1/exports/:export_id/file", get(get_export_file))
        .route("/v1/scans/:scan_id/measurements", post(start_measurement))
        .route("/v1/measurements/:measurement_id", get(get_measurement))
        .route("/v1/measurements/:measurement_id/evidence", get(get_measurement_evidence))
        .route("/v1/aim", post(aim_from_preview))
        .route("/v1/recipes", get(list_recipes))
        .route("/v1/recipes/:recipe_id", get(get_recipe))
        .route("/v1/runs", get(list_runs).post(start_run))
        .route("/v1/runs/:run_id", get(get_run))
        .route("/v1/runs/:run_id/advance", post(advance_run))
        .route("/v1/runs/:run_id/propose", post(propose_run))
        .route("/v1/runs/:run_id/cancel", post(cancel_run))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/:job_id", get(get_job))
        .route("/v1/jobs/:job_id/cancel", post(cancel_job))
        .route("/v1/jobs/:job_id/approve", post(approve_job))
        .route("/v1/grants", get(list_grants).post(create_grant))
        .route("/v1/grants/:grant_id", delete(revoke_grant))
        .route("/v1/log", get(get_log).post(add_log))
        .route("/v1/events", get(events))
        .with_state(st)
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiError::invalid(e.to_string()))
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": VERSION}))
}

/// The caller's token name and role, so a client can show what it may do.
async fn whoami(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    let c = st.caller(&h)?;
    Ok(Json(json!({"name": c.name, "role": c.role})))
}

async fn status(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (_, raw) = st.snapshot().await;
    let (busy_job, queue_len) = {
        let inner = st.inner.lock().unwrap();
        let running = inner.current_scan_job().map(|j| j.job_id.clone());
        let q = inner.jobs.iter().filter(|j| matches!(j.job.state, JobState::Queued | JobState::AwaitingApproval)).count();
        (running, q)
    };
    Ok(Json(map::status(raw.as_ref(), busy_job.as_deref(), queue_len, VERSION, &st.started_at)))
}

async fn rules(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    Ok(Json(serde_json::to_value(&st.rules).unwrap_or_default()))
}

async fn scan_modes(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let modes = st.scanner.scan_modes().await?;
    Ok(Json(Value::Array(modes.iter().map(map::scan_mode).collect())))
}

async fn list_projects(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let ps = st.scanner.projects().await?;
    Ok(Json(Value::Array(ps.iter().map(map::project).collect())))
}

async fn create_project(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "create projects")?;
    #[derive(Deserialize)]
    struct Req {
        name: String,
    }
    let req: Req = parse(&body)?;
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 || name.contains('/') {
        return Err(ApiError::invalid("name must be 1–64 characters with no '/'"));
    }
    if st.scanner.projects().await?.iter().any(|p| p.project_name.as_deref() == Some(name)) {
        return Err(ApiError::conflict(format!("a project named {name:?} already exists")));
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    let p = st.scanner.create_project(name, &uuid).await?;
    st.log("note", format!("Project {name:?} created"), &c.name, Some(&uuid), &[]);
    Ok((StatusCode::CREATED, Json(map::project(&p))).into_response())
}

#[derive(Deserialize)]
struct ScanFilter {
    project_id: Option<String>,
}

pub(crate) async fn all_scans(st: &AppState) -> ApiResult<Vec<Value>> {
    let (scans, projects) = (st.scanner.scans().await?, st.scanner.projects().await?);
    let meta = st.inner.lock().unwrap().scan_meta.clone();
    Ok(scans.iter().rev().filter_map(|s| map::scan(s, &scans, &projects, &meta)).collect())
}

async fn list_scans(State(st): St, h: HeaderMap, Query(f): Query<ScanFilter>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let mut v = all_scans(&st).await?;
    if let Some(p) = f.project_id {
        v.retain(|s| s["project_id"] == p.as_str());
    }
    Ok(Json(Value::Array(v)))
}

async fn get_scan(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    all_scans(&st).await?.into_iter().find(|s| s["scan_id"] == id.as_str()).map(Json).ok_or_else(|| ApiError::not_found("scan"))
}

async fn delete_scan(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let c = st.require(&h, Role::is_operator, "delete scans")?;
    let confirm = h.get("x-confirm").and_then(|v| v.to_str().ok()).unwrap_or("");
    if confirm != id {
        return Err(ApiError::new(StatusCode::PRECONDITION_FAILED, "confirm_mismatch", "Delete not confirmed")
            .hint("Repeat the scan id in the X-Confirm header."));
    }
    // Scanner ids are positional: resolve right before deleting.
    let scans = st.scanner.scans().await?;
    let numeric = scans
        .iter()
        .find(|s| s.scan_uuid.as_deref() == Some(id.as_str()))
        .and_then(|s| s.id)
        .ok_or_else(|| ApiError::not_found("scan"))?;
    st.scanner.delete_scan(numeric).await?;
    crate::files::evict(&st, &id).await;
    st.inner.lock().unwrap().scan_meta.remove(&id);
    st.log("note", format!("Scan {id} deleted"), &c.name, None, &[("scan_id", &id)]);
    Ok(StatusCode::NO_CONTENT)
}

async fn start_scan(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "start scans")?;
    let req: ScanRequest = parse(&body)?;
    let job = submit_scan(&st, &c, req, None).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// Admit a scan under the caller's role (shared by direct requests and runs).
pub async fn submit_scan(st: &Arc<AppState>, c: &crate::model::Caller, req: ScanRequest, run_id: Option<&str>) -> ApiResult<Job> {
    req.validate().map_err(ApiError::invalid)?;
    let modes = st.scanner.scan_modes().await?;
    if !modes.iter().any(|m| m.scan_mode.as_deref() == Some(req.mode.as_str())) {
        let valid: Vec<&str> = modes.iter().filter_map(|m| m.scan_mode.as_deref()).collect();
        return Err(ApiError::invalid(format!("unknown mode {:?}; valid: {}", req.mode, valid.join(", "))));
    }
    let project = st
        .scanner
        .projects()
        .await?
        .into_iter()
        .find(|p| p.project_uuid.as_deref() == Some(req.project_id.as_str()))
        .ok_or_else(|| ApiError::not_found("project"))?;
    let (snap, _) = st.snapshot().await;

    let job = {
        let mut inner = st.inner.lock().unwrap();
        let queue_len = inner.jobs.iter().filter(|j| !j.job.state.is_final() && j.job.state != JobState::Running).count();
        let now = now_unix();
        let grant_idx = inner.grants.iter().position(|g| grant_covers(g, &req, now, run_id));
        let admission = admit_scan(&st.rules, &snap, c.role, queue_len, grant_idx.is_some())?;
        let mut grant_id = None;
        if admission == Admission::Queue && c.role == Role::Agent {
            if let Some(i) = grant_idx {
                inner.grants[i].scans_used += 1;
                grant_id = Some(inner.grants[i].grant_id.clone());
            }
        }
        let job = Job {
            job_id: new_id("job"),
            kind: "scan".into(),
            state: if admission == Admission::Queue { JobState::Queued } else { JobState::AwaitingApproval },
            phase: None,
            progress: None,
            requested_by: c.name.clone(),
            run_id: run_id.map(str::to_string),
            created_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            result: None,
            problem: None,
            partial_scan_id: None,
            request: Some(req.clone()),
            project_name: project.project_name.clone(),
        };
        inner.jobs.push(JobRecord {
            job: job.clone(),
            request: Some(req.clone()),
            project_id: req.project_id.clone(),
            project_name: project.project_name.clone().unwrap_or_default(),
            scanner_task_id: None,
            cancel_requested: false,
            grant_id,
        });
        job
    };
    let what = match req.kind {
        crate::model::ScanKind::FullDome => "full-dome",
        crate::model::ScanKind::Area => "area",
    };
    let waiting = if job.state == JobState::AwaitingApproval { ", awaiting approval" } else { "" };
    st.save_grants();
    let note = req.note.as_deref().map(|n| format!(": {n}")).unwrap_or_default();
    st.log("scan", format!("{what} {} scan requested{waiting}{note}", req.mode), &c.name, Some(&req.project_id), &[("job_id", &job.job_id)]);
    st.emit_job(&job);
    st.wake.notify_one();
    Ok(job)
}

async fn list_jobs(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    Ok(Json(json!(inner.jobs.iter().rev().map(|j| &j.job).collect::<Vec<_>>())))
}

async fn get_job(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    inner.jobs.iter().find(|j| j.job.job_id == id).map(|j| Json(j.job.clone())).ok_or_else(|| ApiError::not_found("job"))
}

async fn cancel_job(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    let c = st.require(&h, |r| r != Role::Viewer, "cancel jobs")?;
    let (job, project_id) = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.jobs.iter().find(|j| j.job.job_id == id).ok_or_else(|| ApiError::not_found("job"))?;
        if c.role == Role::Agent && rec.job.requested_by != c.name {
            return Err(ApiError::forbidden("cancel other people's jobs"));
        }
        let project_id = rec.project_id.clone();
        // A running job is stopped by whoever runs it: the scan worker or the export.
        (inner.cancel_job(&id).ok_or_else(|| ApiError::not_found("job"))?, project_id)
    };
    st.save_grants();
    st.log("scan", format!("Cancel requested for {id}"), &c.name, Some(&project_id), &[("job_id", &id)]);
    st.emit_job(&job);
    Ok(Json(job))
}

#[derive(Deserialize, Default)]
struct ApproveReq {
    /// For a run's scan: also grant the run the scans it may still start.
    #[serde(default)]
    run: bool,
}

/// Hours a run approval lasts.
const RUN_APPROVAL_HOURS: u64 = 2;

async fn approve_job(State(st): St, h: HeaderMap, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<Job>> {
    let c = st.require(&h, Role::is_operator, "approve jobs")?;
    let opts: ApproveReq = if body.iter().all(u8::is_ascii_whitespace) { ApproveReq::default() } else { parse(&body)? };
    let (job, project_id) = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.jobs.iter_mut().find(|j| j.job.job_id == id).ok_or_else(|| ApiError::not_found("job"))?;
        if rec.job.state != JobState::AwaitingApproval {
            return Err(ApiError::conflict(format!("job is {}, not awaiting approval", rec.job.state.as_str())));
        }
        rec.job.state = JobState::Queued;
        (rec.job.clone(), rec.project_id.clone())
    };
    if let Some(run_id) = &job.run_id {
        crate::runs::note_approval(&st, run_id);
        // One approval for the whole run: grant it the scans after this one.
        let more = crate::runs::scans_left(&st, run_id).saturating_sub(1);
        if opts.run && more > 0 {
            let expires_unix = now_unix() + RUN_APPROVAL_HOURS * 3600;
            let req = GrantRequest {
                project_id: project_id.clone(),
                expires_at: crate::util::rfc3339(expires_unix),
                max_scans: Some(more),
                allowed_modes: Vec::new(),
                allow_color: false,
                run_id: Some(run_id.clone()),
                note: None,
            };
            let g = Grant { req, grant_id: new_id("grant"), granted_by: c.name.clone(), scans_used: 0, expires_unix };
            st.inner.lock().unwrap().grants.push(g.clone());
            st.save_grants();
            st.log("approval", format!("Run {run_id} approved: its next {more} scan{} need no approval", if more == 1 { "" } else { "s" }),
                   &c.name, Some(&project_id), &[("run_id", run_id), ("grant_id", &g.grant_id)]);
        }
    }
    st.log("approval", format!("Job {id} approved"), &c.name, Some(&project_id), &[("job_id", &id)]);
    st.emit_job(&job);
    st.wake.notify_one();
    Ok(Json(job))
}

async fn list_grants(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let now = now_unix();
    let inner = st.inner.lock().unwrap();
    Ok(Json(json!(inner.grants.iter().filter(|g| g.expires_unix > now).collect::<Vec<_>>())))
}

async fn create_grant(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, Role::is_operator, "create grants")?;
    let req: GrantRequest = parse(&body)?;
    let expires_unix = parse_rfc3339(&req.expires_at).ok_or_else(|| ApiError::invalid("expires_at must be RFC 3339"))?;
    if expires_unix <= now_unix() {
        return Err(ApiError::invalid("expires_at is in the past"));
    }
    let g = Grant { req, grant_id: new_id("grant"), granted_by: c.name.clone(), scans_used: 0, expires_unix };
    st.inner.lock().unwrap().grants.push(g.clone());
    st.save_grants();
    let limit = g.req.max_scans.map(|m| format!("up to {m} scans")).unwrap_or_else(|| "scans".into());
    let how = g.req.note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
    st.log("approval", format!("Grant: {limit} until {}{how}", g.req.expires_at), &c.name, Some(&g.req.project_id), &[("grant_id", &g.grant_id)]);
    st.wake.notify_one();
    Ok((StatusCode::CREATED, Json(g)).into_response())
}

async fn revoke_grant(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let c = st.require(&h, Role::is_operator, "revoke grants")?;
    let removed = {
        let mut inner = st.inner.lock().unwrap();
        let before = inner.grants.len();
        inner.grants.retain(|g| g.grant_id != id);
        before != inner.grants.len()
    };
    if !removed {
        return Err(ApiError::not_found("grant"));
    }
    st.save_grants();
    st.log("approval", format!("Grant {id} revoked"), &c.name, None, &[("grant_id", &id)]);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LogFilter {
    since: Option<String>,
    project_id: Option<String>,
}

async fn get_log(State(st): St, h: HeaderMap, Query(f): Query<LogFilter>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let since = match f.since.as_deref() {
        Some(s) => Some(parse_rfc3339(s).ok_or_else(|| ApiError::invalid("since must be RFC 3339"))?),
        None => None,
    };
    let inner = st.inner.lock().unwrap();
    let v: Vec<_> = inner
        .log
        .iter()
        .filter(|e| since.is_none_or(|s| parse_rfc3339(&e.at).unwrap_or(0) >= s))
        .filter(|e| f.project_id.as_deref().is_none_or(|p| e.project_id.as_deref() == Some(p)))
        .collect();
    Ok(Json(json!(v)))
}

async fn add_log(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "write to the log")?;
    #[derive(Deserialize)]
    struct Req {
        text: String,
        project_id: Option<String>,
    }
    let req: Req = parse(&body)?;
    check_note(&req.text)?;
    let e = st.log("note", req.text, &c.name, req.project_id.as_deref(), &[]);
    Ok((StatusCode::CREATED, Json(e)).into_response())
}

/// A log note's text (shared by `POST /log` and `addLogNote` steps).
pub fn check_note(text: &str) -> ApiResult<()> {
    if text.trim().is_empty() || text.len() > 4000 {
        return Err(ApiError::invalid("text must be 1–4000 characters"));
    }
    Ok(())
}

async fn events(State(st): St, h: HeaderMap) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    st.caller(&h)?;
    let rx = st.events.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((Ok(SseEvent::default().event(ev.kind).data(ev.data.to_string())), rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
}

fn io_err(e: std::io::Error) -> ApiError {
    ApiError::cache(format!("cache: {e}"))
}

async fn scan_preview(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let p = scan_file(&st, &id, FileKind::Preview).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response())
}

async fn scan_tzf(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let p = scan_file(&st, &id, FileKind::Tzf).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    let disp = format!("attachment; filename=\"{id}.tzf\"");
    Ok(([(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_DISPOSITION, disp)], bytes).into_response())
}

#[derive(Deserialize)]
struct ViewQuery {
    max_points: Option<usize>,
}

/// Thinned points for the viewer, in the `X9V1` binary format (see `view.rs`).
async fn scan_view(State(st): St, h: HeaderMap, Path(id): Path<String>, Query(q): Query<ViewQuery>) -> ApiResult<Response> {
    use crate::files::{view_file, ViewPart, VIEW_POINTS};
    st.caller(&h)?;
    let max = q.max_points.unwrap_or(VIEW_POINTS).clamp(10_000, 8_000_000);
    let p = view_file(&st, &id, ViewPart::Points(max)).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn scan_coverage(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    use crate::files::{view_file, ViewPart};
    st.caller(&h)?;
    let p = view_file(&st, &id, ViewPart::Coverage).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    let mut v: Value = serde_json::from_slice(&bytes).map_err(|e| ApiError::cache(format!("coverage: {e}")))?;
    v["image_url"] = json!(format!("/v1/scans/{id}/coverage/image"));
    Ok(Json(v))
}

async fn scan_coverage_image(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    use crate::files::{view_file, ViewPart};
    st.caller(&h)?;
    let p = view_file(&st, &id, ViewPart::Image).await?;
    let bytes = tokio::fs::read(&p).await.map_err(io_err)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], bytes).into_response())
}

#[derive(Deserialize)]
struct PlanQuery {
    /// Preview columns `x0,x1` framing the job's subject (`x1 < x0` wraps).
    subject: Option<String>,
}

/// Where to set up next, from this scan's points (`planner.rs`): up to three
/// spots on reachable ground, best first, each with its preview pixel and a
/// landmark beside it. The plan without a subject is computed once per scan
/// and cached.
async fn plan_next_station(State(st): St, h: HeaderMap, Path(id): Path<String>, Query(q): Query<PlanQuery>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let id = crate::files::safe_id(&id)?.to_string();
    let subject = match q.subject.as_deref() {
        None => None,
        Some(s) => {
            let cols: Vec<usize> = s.split(',').map(|v| v.trim().parse()).collect::<Result<_, _>>().map_err(|_| ApiError::invalid("subject: two preview columns, x0,x1"))?;
            let [x0, x1] = cols[..] else { return Err(ApiError::invalid("subject: two preview columns, x0,x1")) };
            if x0 == x1 {
                return Err(ApiError::invalid("subject: x0 and x1 must differ"));
            }
            Some((x0, x1))
        }
    };
    let tzf = scan_file(&st, &id, FileKind::Tzf).await?;
    let jpg = match scan_file(&st, &id, FileKind::Preview).await {
        Ok(p) => tokio::fs::read(p).await.ok(),
        Err(_) => None,
    };
    let preview = jpg.as_deref().and_then(|j| aim::jpeg_size(j).ok()).unwrap_or((0, 0));
    if let Some((x0, x1)) = subject {
        if preview.0 == 0 || x0 >= preview.0 || x1 >= preview.0 {
            return Err(ApiError::invalid(format!("subject: columns must lie within the {}-pixel-wide preview", preview.0)));
        }
    }
    // Only the plan without a subject is kept: a subject is one question.
    let cached = st.data_dir.join("views").join(format!("{id}.plan.json"));
    let read_cached = || async {
        match subject {
            None => tokio::fs::read(&cached).await.ok(),
            Some(_) => None,
        }
    };
    let bytes = match read_cached().await {
        Some(b) => b,
        None => {
            let _one = st.view_lock.lock().await;
            match read_cached().await {
                Some(b) => b,
                None => {
                    let v = tokio::task::spawn_blocking(move || -> Result<Value, String> {
                        let t = trimble_tzf::Tzf::open(&tzf).map_err(|e| format!("open: {e}"))?;
                        let meta = t.scan_meta().map_err(|e| format!("metadata: {e}"))?;
                        let pts = t.grid_points().map_err(|e| format!("decode: {e}"))?;
                        let mut v = crate::planner::plan(&pts, (meta.grid_cols, meta.grid_rows), preview, subject);
                        v["preview_size"] = json!([preview.0, preview.1]);
                        Ok(v)
                    })
                    .await
                    .map_err(|e| ApiError::cache(format!("planning: {e}")))?
                    .map_err(|e| ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not decode the scan").detail(e))?;
                    let b = serde_json::to_vec_pretty(&v).unwrap_or_default();
                    if subject.is_none() {
                        if let Some(dir) = cached.parent() {
                            let _ = tokio::fs::create_dir_all(dir).await;
                            let tmp = cached.with_extension(format!("part-{}", crate::util::new_id("p")));
                            if tokio::fs::write(&tmp, &b).await.is_ok() {
                                let _ = tokio::fs::rename(&tmp, &cached).await;
                            }
                        }
                    }
                    b
                }
            }
        }
    };
    let mut v: Value = serde_json::from_slice(&bytes).map_err(|e| ApiError::cache(format!("plan: {e}")))?;
    v["scan_id"] = json!(id);
    Ok(Json(v))
}

/// The agent's next-station proposal on a scan's preview: checked, drawn and
/// kept (the latest per scan), and announced to every client.
async fn put_next_station(State(st): St, h: HeaderMap, Path(id): Path<String>, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "propose stations")?;
    let p: crate::station::Proposal = parse(&body)?;
    let scan = all_scans(&st).await?.into_iter().find(|s| s["scan_id"] == id.as_str()).ok_or_else(|| ApiError::not_found("scan"))?;
    let path = scan_file(&st, &id, FileKind::Preview).await?;
    let jpg = tokio::fs::read(&path).await.map_err(io_err)?;
    let (pw, ph, grey) = aim::jpeg_luma(&jpg).map_err(|e| ApiError::invalid(format!("preview: {e}")))?;
    p.check(pw, ph).map_err(ApiError::invalid)?;
    let p2 = p.clone();
    // The scan, when it decodes, says how far off the marker's ground is.
    let tzf = scan_file(&st, &id, FileKind::Tzf).await.ok();
    let (png, ground) = tokio::task::spawn_blocking(move || {
        let ground = tzf.and_then(|f| {
            let t = trimble_tzf::Tzf::open(&f).ok()?;
            let meta = t.scan_meta().ok()?;
            let pts = t.grid_points().ok()?;
            crate::station::ground_at(&pts, (meta.grid_cols, meta.grid_rows), (pw, ph), (p2.marker.x, p2.marker.y))
        });
        (crate::station::render(&grey, pw, ph, &p2, ground), ground)
    })
    .await
    .map_err(|e| ApiError::cache(format!("drawing: {e}")))?;
    let project_id = scan["project_id"].as_str().unwrap_or("").to_string();
    let mut v = p.to_json();
    if let Some(g) = ground {
        v["marker"]["distance_m"] = json!((g.out_m * 10.0).round() / 10.0);
    }
    v["scan_id"] = json!(id);
    v["project_id"] = json!(project_id);
    v["station"] = scan["station"].clone();
    v["proposed_by"] = json!(c.name);
    v["at"] = json!(now_rfc3339());
    v["preview_size"] = json!([pw, ph]);
    v["image_url"] = json!(format!("/v1/scans/{id}/next-station/image"));
    st.inner.lock().unwrap().next_stations.insert(id.clone(), (v.clone(), png));
    st.log("note", format!("Next station proposed from station {}: {}", scan["station"], p.note), &c.name, Some(&project_id), &[("scan_id", &id)]);
    st.emit("next_station", v.clone());
    Ok(Json(v).into_response())
}

async fn get_next_station(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    inner.next_stations.get(&id).map(|(v, _)| Json(v.clone())).ok_or_else(|| ApiError::not_found("next-station proposal"))
}

async fn get_next_station_image(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let png = st.inner.lock().unwrap().next_stations.get(&id).map(|(_, p)| p.clone()).ok_or_else(|| ApiError::not_found("next-station proposal"))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// Every proposal the bridge holds, newest first.
async fn list_next_stations(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let mut all: Vec<Value> = st.inner.lock().unwrap().next_stations.values().map(|(v, _)| v.clone()).collect();
    all.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));
    Ok(Json(json!(all)))
}

/// A project's scan ids (a mission's targets may only name these).
async fn project_scans(st: &AppState, project_id: &str) -> ApiResult<Vec<String>> {
    Ok(all_scans(st)
        .await?
        .into_iter()
        .filter(|s| s["project_id"] == project_id)
        .filter_map(|s| s["scan_id"].as_str().map(str::to_string))
        .collect())
}

/// Store a changed mission: save, announce, return it.
fn mission_changed(st: &AppState, m: &crate::mission::Mission) -> Json<Value> {
    st.inner.lock().unwrap().missions.insert(m.project_id.clone(), m.clone());
    st.save_missions();
    let v = json!(m);
    st.emit("mission", v.clone());
    Json(v)
}

fn mission_of(st: &AppState, project_id: &str) -> ApiResult<crate::mission::Mission> {
    st.inner.lock().unwrap().missions.get(project_id).cloned().ok_or_else(|| ApiError::not_found("mission"))
}

async fn list_missions(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let mut all: Vec<crate::mission::Mission> = st.inner.lock().unwrap().missions.values().cloned().collect();
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(Json(json!(all)))
}

async fn get_mission(State(st): St, h: HeaderMap, Path(project_id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    Ok(Json(json!(mission_of(&st, &project_id)?)))
}

/// Set the project's mission (replacing any earlier one), optionally with its first targets.
async fn put_mission(State(st): St, h: HeaderMap, Path(project_id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    let c = st.require(&h, |r| r != Role::Viewer, "set missions")?;
    let req: crate::mission::MissionRequest = parse(&body)?;
    req.check().map_err(ApiError::invalid)?;
    if !st.scanner.projects().await?.iter().any(|p| p.project_uuid.as_deref() == Some(project_id.as_str())) {
        return Err(ApiError::not_found("project"));
    }
    let mut m = crate::mission::Mission::new(&req, &project_id, &c.name);
    if !req.targets.is_empty() {
        m.update(&req.targets, &project_scans(&st, &project_id).await?, &st.geo(&project_id)).map_err(ApiError::invalid)?;
    }
    st.log("note", format!("Mission: {} (done when: {})", m.goal, m.done_when), &c.name, Some(&project_id), &[("mission_id", &m.mission_id)]);
    Ok(mission_changed(&st, &m))
}

#[derive(Deserialize)]
struct TargetsReq {
    targets: Vec<crate::mission::TargetUpdate>,
}

async fn update_targets(State(st): St, h: HeaderMap, Path(project_id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    st.require(&h, |r| r != Role::Viewer, "update missions")?;
    let req: TargetsReq = parse(&body)?;
    let mut m = mission_of(&st, &project_id)?;
    let known = project_scans(&st, &project_id).await?;
    m.update(&req.targets, &known, &st.geo(&project_id)).map_err(|e| if e.contains("signed off") { ApiError::conflict(e) } else { ApiError::invalid(e) })?;
    Ok(mission_changed(&st, &m))
}

#[derive(Deserialize)]
struct ReadyReq {
    summary: String,
}

/// The agent says the job is done: refused while any target is open.
async fn mission_ready(State(st): St, h: HeaderMap, Path(project_id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    let c = st.require(&h, |r| r != Role::Viewer, "update missions")?;
    let req: ReadyReq = parse(&body)?;
    let mut m = mission_of(&st, &project_id)?;
    m.ready(&req.summary).map_err(ApiError::conflict)?;
    st.log("note", format!("Ready to leave: {} ({})", req.summary.trim(), m.tally()), &c.name, Some(&project_id), &[("mission_id", &m.mission_id)]);
    Ok(mission_changed(&st, &m))
}

#[derive(Deserialize, Default)]
struct SignOffReq {
    #[serde(default)]
    confirmed_via: Option<String>,
}

/// The person signs the job off (operator): the mission is done.
async fn mission_sign_off(State(st): St, h: HeaderMap, Path(project_id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    let c = st.require(&h, Role::is_operator, "sign off missions")?;
    let req: SignOffReq = if body.iter().all(u8::is_ascii_whitespace) { SignOffReq::default() } else { parse(&body)? };
    let mut m = mission_of(&st, &project_id)?;
    m.sign_off(&c.name, req.confirmed_via).map_err(ApiError::conflict)?;
    let via = m.confirmed_via.as_deref().map(|v| format!(" ({v})")).unwrap_or_default();
    st.log("approval", format!("Mission signed off: {} ({}){via}", m.goal, m.tally()), &c.name, Some(&project_id), &[("mission_id", &m.mission_id)]);
    Ok(mission_changed(&st, &m))
}

/// The project's registration: stations in one frame, the pairs that tie
/// them, and the walls found in the merged plan.
async fn get_registration(State(st): St, h: HeaderMap, Path(project_id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (reg, running) = {
        let inner = st.inner.lock().unwrap();
        (inner.registrations.get(&project_id).cloned(), inner.registering.get(&project_id).cloned())
    };
    let mut v = match reg {
        Some(r) => r.to_json(),
        None => json!({"project_id": project_id, "stations": [], "walls": [], "pairs": [],
                       "summary": "Not registered yet: the bridge registers each full dome as it lands."}),
    };
    v["registering_job_id"] = json!(running);
    Ok(Json(v))
}

/// Register again (the bridge does this after every full dome by itself).
async fn post_registration(State(st): St, h: HeaderMap, Path(project_id): Path<String>) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "register stations")?;
    if !st.scanner.projects().await?.iter().any(|p| p.project_uuid.as_deref() == Some(project_id.as_str())) {
        return Err(ApiError::not_found("project"));
    }
    let job = crate::worker::queue_register(&st, &project_id, &c.name);
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// The registered plan: stations, walls and the mission's wall faces.
async fn get_registration_plan(State(st): St, h: HeaderMap, Path(project_id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let reg = st.inner.lock().unwrap().registrations.get(&project_id).cloned().ok_or_else(|| ApiError::not_found("registration"))?;
    let merged = st.merged(&project_id).ok_or_else(|| ApiError::not_found("registration"))?;
    let faces = mission_faces(&st, &project_id);
    let png = tokio::task::spawn_blocking(move || {
        let stations: Vec<(u16, crate::register::Pose, bool)> =
            reg.stations.iter().filter_map(|s| Some((s.station, s.pose?, s.audit.as_ref().is_none_or(crate::register::Audit::trusted)))).collect();
        crate::register::plan_png(&merged, &crate::register::PlanMarks { stations: &stations, walls: &reg.walls, faces })
    })
    .await
    .map_err(|e| ApiError::cache(format!("drawing: {e}")))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

#[derive(Deserialize)]
struct FaceReq {
    #[serde(default)]
    wall_id: Option<String>,
    #[serde(default)]
    from: Option<[f64; 2]>,
    #[serde(default)]
    to: Option<[f64; 2]>,
}

/// The mission's wall targets: id, face, colour by status.
fn mission_faces(st: &AppState, project_id: &str) -> Vec<(String, crate::register::Face, [u8; 3])> {
    st.inner
        .lock()
        .unwrap()
        .missions
        .get(project_id)
        .map(|m| {
            m.targets
                .iter()
                .filter_map(|t| {
                    let col = match t.status {
                        crate::mission::Status::Open => [248, 113, 113],
                        crate::mission::Status::Declined => [148, 163, 184],
                        _ => [74, 222, 128],
                    };
                    Some((t.target_id.clone(), t.face?, col))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct MinimapQuery {
    #[serde(default)]
    format: Option<String>,
}

/// The project as a plain line map (see `minimap`): SVG with
/// `?format=svg`, PNG otherwise. Always drawn, even before any station.
async fn get_minimap(State(st): St, h: HeaderMap, Path(project_id): Path<String>, Query(q): Query<MinimapQuery>) -> ApiResult<Response> {
    st.caller(&h)?;
    let (reg, registering) = {
        let inner = st.inner.lock().unwrap();
        (inner.registrations.get(&project_id).cloned(), inner.registering.contains_key(&project_id))
    };
    // Full domes the registration doesn't hold yet: just scanned.
    let mut waiting: Vec<u16> = all_scans(&st)
        .await?
        .into_iter()
        .filter(|s| s["project_id"] == project_id.as_str() && s["kind"] == "full_dome")
        .filter(|s| reg.as_ref().is_none_or(|r| !r.stations.iter().any(|x| s["scan_id"] == x.scan_id.as_str())))
        .filter_map(|s| s["station"].as_u64().map(|n| n as u16))
        .collect();
    waiting.sort();
    let map = crate::minimap::Minimap::new(reg.as_ref(), mission_faces(&st, &project_id), &waiting, registering);
    if q.format.as_deref() == Some("svg") {
        return Ok(([(header::CONTENT_TYPE, "image/svg+xml")], map.svg()).into_response());
    }
    let png = tokio::task::spawn_blocking(move || map.png()).await.map_err(|e| ApiError::cache(format!("drawing: {e}")))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// How much of a wall face the registered stations saw (changes nothing).
async fn post_face(State(st): St, h: HeaderMap, Path(project_id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let req: FaceReq = parse(&body)?;
    let geo = st.geo(&project_id);
    let face = match (&req.wall_id, req.from, req.to) {
        (Some(w), _, _) => geo.wall(w).ok_or_else(|| ApiError::invalid(format!("there is no wall {w:?} in the registration")))?,
        (None, Some(from), Some(to)) => crate::register::Face { from, to },
        _ => return Err(ApiError::invalid("name a wall_id, or the face's from and to (project frame, m)")),
    };
    let mut v = geo.judge(&face).ok_or_else(|| ApiError::conflict("the project is not registered yet"))?;
    v["face"] = json!(face);
    Ok(Json(v))
}

/// Workers seen in the last 15 s (see `work`).
async fn list_workers(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    Ok(Json(json!(st.work.lock().unwrap().workers())))
}

#[derive(Deserialize)]
struct ClaimReq {
    worker: String,
}

/// A worker asks for work: 200 with a job, or 204.
async fn work_claim(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "do work for the bridge")?;
    let req: ClaimReq = parse(&body)?;
    let w = req.worker.trim();
    if w.is_empty() || w.len() > 64 || !w.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch)) {
        return Err(ApiError::invalid("worker: 1–64 letters, digits, '-', '_' or '.'"));
    }
    let job = st.work.lock().unwrap().claim(w, &c.name);
    match job {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(v) => {
            let project = v["project_id"].as_str().map(str::to_string);
            st.log("note", format!("Worker {w} took the registration job"), &c.name, project.as_deref(), &[("job_id", v["job_id"].as_str().unwrap_or(""))]);
            Ok(Json(v).into_response())
        }
    }
}

#[derive(Deserialize)]
struct ProgressReq {
    phase: String,
    #[serde(default)]
    progress: Option<f64>,
}

async fn work_progress(State(st): St, h: HeaderMap, Path(job_id): Path<String>, body: Bytes) -> ApiResult<StatusCode> {
    let c = st.require(&h, |r| r != Role::Viewer, "do work for the bridge")?;
    let req: ProgressReq = parse(&body)?;
    st.work.lock().unwrap().holds(&job_id, &c.name).map_err(ApiError::conflict)?;
    let phase: String = req.phase.chars().take(120).collect();
    crate::worker::set_phase(&st, &job_id, &phase, req.progress.map(|p| p.clamp(0.0, 1.0)));
    Ok(StatusCode::NO_CONTENT)
}

async fn work_merged(State(st): St, h: HeaderMap, Path(job_id): Path<String>, body: Bytes) -> ApiResult<StatusCode> {
    let c = st.require(&h, |r| r != Role::Viewer, "do work for the bridge")?;
    let bytes = body.to_vec();
    let checked = tokio::task::spawn_blocking(move || crate::register::Merged::from_bytes(&bytes).map(|_| bytes))
        .await
        .map_err(|e| ApiError::conflict(e.to_string()))?
        .map_err(ApiError::invalid)?;
    st.work.lock().unwrap().put_merged(&job_id, &c.name, checked).map_err(ApiError::conflict)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn work_result(State(st): St, h: HeaderMap, Path(job_id): Path<String>, body: Bytes) -> ApiResult<StatusCode> {
    let c = st.require(&h, |r| r != Role::Viewer, "do work for the bridge")?;
    let res: crate::work::WorkResult = parse(&body)?;
    st.work.lock().unwrap().deliver(&job_id, &c.name, Ok(res)).map_err(ApiError::conflict)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct FailReq {
    error: String,
}

async fn work_fail(State(st): St, h: HeaderMap, Path(job_id): Path<String>, body: Bytes) -> ApiResult<StatusCode> {
    let c = st.require(&h, |r| r != Role::Viewer, "do work for the bridge")?;
    let req: FailReq = parse(&body)?;
    let why: String = req.error.chars().take(300).collect();
    st.work.lock().unwrap().deliver(&job_id, &c.name, Err(why)).map_err(ApiError::conflict)?;
    Ok(StatusCode::NO_CONTENT)
}

fn default_margin() -> f64 {
    0.009
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct AimBox {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
}

#[derive(Deserialize)]
pub struct AimReq {
    reference_scan_id: String,
    #[serde(rename = "box")]
    bx: AimBox,
    #[serde(default = "default_margin")]
    margin_rad: f64,
}

async fn aim_from_preview(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let req: AimReq = parse(&body)?;
    Ok(Json(compute_aim(&st, req).await?))
}

/// Box on a preview → area-scan window (shared by direct requests and runs).
pub async fn compute_aim(st: &AppState, req: AimReq) -> ApiResult<Value> {
    if st.rules.refuse_aim_after_move {
        if let (_, Some(s)) = st.snapshot().await {
            if s.instrument_moved_since_last_scan == Some(true) {
                return Err(ApiError::new(StatusCode::CONFLICT, "instrument_moved", "The scanner moved since its last scan")
                    .hint("Take a new overview scan at this station and aim from its preview."));
            }
        }
    }
    let tzf = scan_file(st, &req.reference_scan_id, FileKind::Tzf).await?;
    let jpg = tokio::fs::read(scan_file(st, &req.reference_scan_id, FileKind::Preview).await?).await.map_err(io_err)?;
    let (g, (pw, ph)) = tokio::task::spawn_blocking(move || -> Result<_, String> { Ok((aim::geo(&tzf)?, aim::jpeg_size(&jpg)?)) })
        .await
        .map_err(|e| ApiError::conflict(e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not read the reference scan").detail(e))?;
    let b = &req.bx;
    let w = aim::window_from_box(&g, pw, ph, [b.x0, b.y0, b.x1, b.y1], req.margin_rad).map_err(ApiError::invalid)?;
    Ok(json!({
        "window": w,
        "preview_size": [pw, ph],
        "cells_per_pixel": [g.cols as f64 / pw as f64, g.rows as f64 / ph as f64],
    }))
}

async fn start_export(State(st): St, h: HeaderMap, Path(scan_id): Path<String>, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "export scans")?;
    let req: export::ExportRequest = parse(&body)?;
    let job = submit_export(&st, &c, scan_id, req, None).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// Start an export under the caller's role (shared by direct requests and runs).
pub async fn submit_export(st: &Arc<AppState>, c: &crate::model::Caller, scan_id: String, req: export::ExportRequest, run_id: Option<&str>) -> ApiResult<Job> {
    let scan = all_scans(st).await?.into_iter().find(|s| s["scan_id"] == scan_id.as_str()).ok_or_else(|| ApiError::not_found("scan"))?;
    let project_id = scan["project_id"].as_str().unwrap_or("").to_string();
    let now = now_rfc3339();
    let job = Job {
        job_id: new_id("job"),
        kind: "export".into(),
        state: JobState::Running,
        phase: Some("downloading".into()),
        progress: None,
        requested_by: c.name.clone(),
        run_id: run_id.map(str::to_string),
        created_at: now.clone(),
        started_at: Some(now),
        finished_at: None,
        result: None,
        problem: None,
        partial_scan_id: None,
        request: None,
        project_name: None,
    };
    st.inner.lock().unwrap().jobs.push(JobRecord {
        job: job.clone(),
        request: None,
        project_id: project_id.clone(),
        project_name: String::new(),
        scanner_task_id: None,
        cancel_requested: false,
        grant_id: None,
    });
    st.log("export", format!("{} export of scan {scan_id} requested", req.format.ext()), &c.name, Some(&project_id), &[("job_id", &job.job_id), ("scan_id", &scan_id)]);
    st.emit_job(&job);
    tokio::spawn(crate::worker::run_export(st.clone(), job.job_id.clone(), scan_id, req));
    Ok(job)
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeasureRequest {
    pub kind: String,
    /// Measure only this box on the scan's preview (pixels, y down; `x1 < x0`
    /// wraps across the seam), padded by `margin_rad`: a window in a full
    /// dome, without a close-up.
    #[serde(default, rename = "box")]
    pub crop: Option<AimBox>,
    #[serde(default = "default_crop_margin")]
    pub margin_rad: f64,
}

fn default_crop_margin() -> f64 {
    0.017
}

impl MeasureRequest {
    pub fn opening() -> Self {
        MeasureRequest { kind: "opening".into(), crop: None, margin_rad: default_crop_margin() }
    }
}

async fn start_measurement(State(st): St, h: HeaderMap, Path(scan_id): Path<String>, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "measure scans")?;
    let req: MeasureRequest = parse(&body)?;
    let job = submit_measure(&st, &c, scan_id, req, None).await?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// Start a measurement under the caller's role (shared by direct requests and runs).
pub async fn submit_measure(st: &Arc<AppState>, c: &crate::model::Caller, scan_id: String, req: MeasureRequest, run_id: Option<&str>) -> ApiResult<Job> {
    if req.kind != "opening" {
        return Err(ApiError::invalid(format!("unknown measurement kind {:?}; this bridge measures: opening", req.kind)));
    }
    let scan = all_scans(st).await?.into_iter().find(|s| s["scan_id"] == scan_id.as_str()).ok_or_else(|| ApiError::not_found("scan"))?;
    let project_id = scan["project_id"].as_str().unwrap_or("").to_string();
    let now = now_rfc3339();
    let job = Job {
        job_id: new_id("job"),
        kind: "measure".into(),
        state: JobState::Running,
        phase: Some("downloading".into()),
        progress: None,
        requested_by: c.name.clone(),
        run_id: run_id.map(str::to_string),
        created_at: now.clone(),
        started_at: Some(now),
        finished_at: None,
        result: None,
        problem: None,
        partial_scan_id: None,
        request: None,
        project_name: None,
    };
    st.inner.lock().unwrap().jobs.push(JobRecord {
        job: job.clone(),
        request: None,
        project_id: project_id.clone(),
        project_name: String::new(),
        scanner_task_id: None,
        cancel_requested: false,
        grant_id: None,
    });
    st.log("measure", format!("Opening measurement of scan {scan_id} requested"), &c.name, Some(&project_id), &[("job_id", &job.job_id), ("scan_id", &scan_id)]);
    st.emit_job(&job);
    let crop = req.crop.map(|b| ([b.x0, b.y0, b.x1, b.y1], req.margin_rad.max(0.0)));
    tokio::spawn(crate::worker::run_measure(st.clone(), job.job_id.clone(), scan_id, crop));
    Ok(job)
}

async fn get_measurement(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    inner.measurements.get(&id).cloned().map(Json).ok_or_else(|| ApiError::not_found("measurement"))
}

async fn get_measurement_evidence(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let path = st.inner.lock().unwrap().measurement_files.get(&id).cloned().ok_or_else(|| ApiError::not_found("measurement"))?;
    let bytes = tokio::fs::read(&path).await.map_err(io_err)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], bytes).into_response())
}

async fn get_export(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let inner = st.inner.lock().unwrap();
    inner.exports.get(&id).cloned().map(Json).ok_or_else(|| ApiError::not_found("export"))
}

async fn get_export_file(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Response> {
    st.caller(&h)?;
    let path = st.inner.lock().unwrap().export_files.get(&id).cloned().ok_or_else(|| ApiError::not_found("export"))?;
    let bytes = tokio::fs::read(&path).await.map_err(io_err)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("export").to_string();
    let disp = format!("attachment; filename=\"{name}\"");
    Ok(([(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_DISPOSITION, disp)], bytes).into_response())
}

async fn list_recipes(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (rs, _) = crate::recipe::load_dir(&st.recipes_dir);
    Ok(Json(json!(rs
        .iter()
        .map(|r| json!({"recipe_id": r.recipe_id, "title": r.title, "version": r.version, "summary": r.summary}))
        .collect::<Vec<_>>())))
}

async fn get_recipe(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let (rs, _) = crate::recipe::load_dir(&st.recipes_dir);
    rs.into_iter().find(|r| r.recipe_id == id).map(|r| Json(serde_json::to_value(r).unwrap_or_default())).ok_or_else(|| ApiError::not_found("recipe"))
}

#[derive(Deserialize)]
struct StartRun {
    recipe_id: String,
    project_id: String,
    #[serde(default)]
    inputs: Value,
}

async fn start_run(State(st): St, h: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let c = st.require(&h, |r| r != Role::Viewer, "start runs")?;
    let req: StartRun = parse(&body)?;
    let (rs, _) = crate::recipe::load_dir(&st.recipes_dir);
    let recipe = rs.into_iter().find(|r| r.recipe_id == req.recipe_id).ok_or_else(|| ApiError::not_found("recipe"))?;
    if !st.scanner.projects().await?.iter().any(|p| p.project_uuid.as_deref() == Some(req.project_id.as_str())) {
        return Err(ApiError::not_found("project"));
    }
    let run = crate::runs::start(&st, c, recipe, req.project_id, req.inputs)?;
    Ok((StatusCode::CREATED, Json(run)).into_response())
}

async fn list_runs(State(st): St, h: HeaderMap) -> ApiResult<Json<Value>> {
    st.caller(&h)?;
    let mut runs: Vec<crate::runs::Run> = st.inner.lock().unwrap().runs.values().map(|r| r.run.clone()).collect();
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.run_id.cmp(&a.run_id)));
    Ok(Json(json!(runs)))
}

async fn get_run(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<crate::runs::Run>> {
    st.caller(&h)?;
    crate::runs::snapshot(&st, &id).map(Json).ok_or_else(|| ApiError::not_found("run"))
}

#[derive(Deserialize)]
struct Advance {
    step_id: String,
    #[serde(default)]
    input: Value,
}

async fn advance_run(State(st): St, h: HeaderMap, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<crate::runs::Run>> {
    let c = st.require(&h, |r| r != Role::Viewer, "advance runs")?;
    crate::runs::may_steer(&st, &id, &c, "advance other people's runs")?;
    let a: Advance = parse(&body)?;
    // A person's step: anyone but an agent acting on its own run.
    let by_person = c.role != Role::Agent;
    Ok(Json(crate::runs::advance(&st, &id, &a.step_id, a.input, by_person)?))
}

#[derive(Deserialize)]
struct Propose {
    step_id: String,
    input: Value,
    #[serde(default)]
    note: Option<String>,
}

async fn propose_run(State(st): St, h: HeaderMap, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<crate::runs::Run>> {
    let c = st.require(&h, |r| r != Role::Viewer, "propose answers")?;
    crate::runs::may_steer(&st, &id, &c, "propose answers in other people's runs")?;
    let p: Propose = parse(&body)?;
    let note = p.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if note.as_ref().is_some_and(|n| n.chars().count() > 2000) {
        return Err(ApiError::invalid("note must be at most 2000 characters"));
    }
    Ok(Json(crate::runs::propose(&st, &id, &p.step_id, p.input, note, &c.name)?))
}

async fn cancel_run(State(st): St, h: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<crate::runs::Run>> {
    let c = st.require(&h, |r| r != Role::Viewer, "cancel runs")?;
    crate::runs::may_steer(&st, &id, &c, "cancel other people's runs")?;
    Ok(Json(crate::runs::cancel(&st, &id)?))
}
