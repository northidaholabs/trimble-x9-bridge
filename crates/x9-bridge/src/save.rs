//! The laptop is where the data lands (`specs/laptop-offload.md`, part D).
//! A save job writes a project to a folder on a worker's computer: every
//! station's scan file, each placed station's points in the project frame
//! as COPC, the registration and mission, and a manifest with checksums.
//! The bridge offers it after the mission's sign-off and whenever asked
//! (`POST /v1/projects/{id}/save`), waits for a worker however long it
//! takes, and keeps the latest save of each project (`saves.json`).

use crate::model::{Job, JobState};
use crate::problem::ApiError;
use crate::register::Audit;
use crate::state::AppState;
use crate::util::now_rfc3339;
use serde_json::{json, Value};
use std::sync::Arc;

/// The project's save job that is still running, if any.
fn running(st: &AppState, project_id: &str) -> Option<Job> {
    st.inner.lock().unwrap().jobs.iter().rev().find(|j| j.project_id == project_id && j.job.kind == "save" && !j.job.state.is_final()).map(|j| j.job.clone())
}

/// Queue a save of the project (`why` goes in the log). One waiting save per
/// project: asking again while one waits for a worker returns that one, which
/// will carry the registration as it stands when a worker takes it.
pub fn queue(st: &Arc<AppState>, project_id: &str, by: &str, why: &str) -> Result<Job, ApiError> {
    if !st.inner.lock().unwrap().registrations.contains_key(project_id) {
        return Err(ApiError::conflict("no station of this project is registered yet, so there is nothing to save"));
    }
    if let Some(j) = running(st, project_id).filter(|j| j.phase.as_deref() == Some(WAITING)) {
        return Ok(j);
    }
    let now = now_rfc3339();
    let job = Job {
        job_id: crate::util::new_id("job"),
        kind: "save".into(),
        state: JobState::Running,
        phase: Some(WAITING.into()),
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
    st.inner.lock().unwrap().jobs.push(crate::state::JobRecord {
        job: job.clone(),
        request: None,
        project_id: project_id.to_string(),
        project_name: String::new(),
        scanner_task_id: None,
        cancel_requested: false,
        grant_id: None,
    });
    st.emit_job(&job);
    let none_seen = st.work.lock().unwrap().workers().is_empty();
    let when = if none_seen { "No laptop is connected: it is saved when one connects" } else { "Offered to the laptop" };
    st.log("note", format!("Saving the project to the laptop ({why}). {when}."), by, Some(project_id), &[("job_id", &job.job_id)]);
    let offer = crate::work::put_save(st, &job.job_id, project_id);
    tokio::spawn(run(st.clone(), offer, job.job_id.clone(), project_id.to_string(), crate::work::SAVE_QUIET));
    Ok(job)
}

const WAITING: &str = "waiting for the laptop";

async fn run(st: Arc<AppState>, offer: crate::work::SaveOffer, job_id: String, project_id: String, quiet: std::time::Duration) {
    let cancelled = || st.inner.lock().unwrap().jobs.iter().find(|j| j.job.job_id == job_id).is_none_or(|j| j.cancel_requested);
    let on_worker = |w: Option<&str>| {
        let phase = w.map_or(WAITING.to_string(), |w| format!("saving on {w}"));
        crate::worker::update(&st, &job_id, |r| {
            r.job.phase = Some(phase);
            r.job.progress = None;
        });
    };
    match crate::work::offer_save(&st, offer, &job_id, &project_id, quiet, &cancelled, &on_worker).await {
        crate::work::SaveOffered::Done(r, w) => {
            let record = record(&r, &w, &job_id);
            st.log("note", summary(&r, &w), "bridge", Some(&project_id), &[("job_id", &job_id)]);
            st.inner.lock().unwrap().saves.insert(project_id.clone(), record.clone());
            st.save_saves();
            crate::worker::finish(&st, &job_id, JobState::Succeeded, Some(record), None);
        }
        crate::work::SaveOffered::Failed(why) => {
            st.log("error", format!("The save failed ({why}). Nothing on the Pi was dropped for it; ask for the save again once fixed."), "bridge", Some(&project_id), &[("job_id", &job_id)]);
            crate::worker::finish(&st, &job_id, JobState::Failed, None, Some(&ApiError::new(axum::http::StatusCode::BAD_GATEWAY, "save_failed", why)));
        }
        crate::work::SaveOffered::Cancelled => crate::worker::finish(&st, &job_id, JobState::Cancelled, None, None),
    }
}

/// What the bridge keeps of a save, and the job's result.
fn record(r: &crate::work::SaveResult, worker: &str, job_id: &str) -> Value {
    json!({
        "job_id": job_id,
        "worker": worker,
        "folder": r.folder,
        "saved_at": now_rfc3339(),
        "bytes": r.files.iter().map(|f| f.bytes).sum::<u64>(),
        "files": r.files.len(),
        "stations": r.stations,
    })
}

/// The field log's line for a finished save.
fn summary(r: &crate::work::SaveResult, worker: &str) -> String {
    let placed = r.stations.iter().filter(|s| s.copc.is_some()).count();
    let kept = r.stations.iter().filter(|s| s.kept).count();
    let gb = r.files.iter().map(|f| f.bytes).sum::<u64>() as f64 / 1e9;
    let mut line = format!("Saved the project on {worker}: {} ({placed} of {} stations in the project frame", r.folder, r.stations.len());
    if kept > 0 {
        line += &format!(", {kept} unchanged since the last save");
    }
    line += &format!(", {gb:.1} GB)");
    let notes: Vec<String> = r.stations.iter().filter_map(|s| s.note.as_ref().map(|n| format!("station {}: {n}", s.station))).collect();
    if !notes.is_empty() {
        line += &format!("; {}", notes.join("; "));
    }
    line
}

/// A folder name for the project: its name made safe for any file system,
/// and the start of its id so two projects of one name don't share a folder.
pub fn folder_name(name: &str, project_id: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '-' }).collect();
    let clean = clean.trim().trim_start_matches('.').chars().take(60).collect::<String>().trim().to_string();
    let short: String = project_id.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    match (clean.is_empty(), short.is_empty()) {
        (true, _) => format!("project-{short}"),
        (false, true) => clean,
        (false, false) => format!("{clean}-{short}"),
    }
}

/// The save job's body as the worker gets it: the stations with their poses,
/// the registration and mission to keep beside them, and the folder's name.
pub async fn body(st: &AppState, job_id: &str, project_id: &str) -> Value {
    let name = match st.scanner.projects().await {
        Ok(ps) => ps.into_iter().find(|p| p.project_uuid.as_deref() == Some(project_id)).and_then(|p| p.project_name),
        Err(_) => None,
    }
    .unwrap_or_else(|| project_id.to_string());
    let (reg, mission) = {
        let inner = st.inner.lock().unwrap();
        (inner.registrations.get(project_id).cloned(), inner.missions.get(project_id).cloned())
    };
    let stations: Vec<Value> = reg
        .iter()
        .flat_map(|r| r.stations.iter())
        .map(|s| json!({"station": s.station, "scan_id": s.scan_id, "pose": s.pose, "trusted": s.pose.is_some() && s.audit.as_ref().is_none_or(Audit::trusted)}))
        .collect();
    json!({
        "job_id": job_id,
        "kind": "save",
        "project_id": project_id,
        "project_name": name,
        "folder": folder_name(&name, project_id),
        "frame_station": reg.as_ref().map_or(1, |r| r.frame_station()),
        "stations": stations,
        "registration": reg,
        "mission": mission,
        "bridge_version": crate::routes::VERSION,
    })
}

/// The project's latest save, and the save job now running, if any.
pub fn status(st: &AppState, project_id: &str) -> Value {
    let last = st.inner.lock().unwrap().saves.get(project_id).cloned();
    json!({"project_id": project_id, "last": last, "job": running(st, project_id)})
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn a_folder_is_named_after_the_project_safely() {
        assert_eq!(folder_name("Howell Rd", "a1b2c3d4-e5f6"), "Howell Rd-a1b2c3d4");
        // One path component, never a way out of the save directory.
        assert_eq!(folder_name("../../etc/passwd", "p1"), "-..-etc-passwd-p1");
        assert_eq!(folder_name("..", "p1"), "project-p1");
        assert_eq!(folder_name("..hidden", "p1"), "hidden-p1");
        assert_eq!(folder_name("///", "p-1"), "----p1");
        assert_eq!(folder_name("", "x"), "project-x");
        assert_eq!(folder_name("Same", "--"), "Same");
        assert!(!folder_name("a/b\\c:d", "p").contains(['/', '\\', ':']));
    }
}
