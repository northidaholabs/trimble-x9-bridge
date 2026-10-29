//! Shared state: tokens, rules, jobs, grants, the field log and the event bus.
//! Everything is in memory for now; it is rebuilt from the scanner on restart.

use crate::model::{Caller, Grant, Job, JobState, LogEntry, Role, Rules, ScanRequest, Timings, Window};
use crate::problem::ApiError;
use crate::rules::Snapshot;
use crate::util::{new_id, now_rfc3339};
use axum::http::HeaderMap;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{broadcast, Notify};

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: &'static str,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub job: Job,
    /// Set for scan jobs.
    pub request: Option<ScanRequest>,
    pub project_id: String,
    pub project_name: String,
    pub scanner_task_id: Option<i64>,
    pub cancel_requested: bool,
    /// The grant this scan used, until the use is given back.
    pub grant_id: Option<String>,
}

/// What the bridge remembers about scans it ran (the scanner keeps none of this).
#[derive(Debug, Clone, Default)]
pub struct ScanMeta {
    pub created_at: String,
    pub timings: Timings,
    pub window: Option<Window>,
}

#[derive(Default)]
pub struct Inner {
    pub jobs: Vec<JobRecord>,
    pub grants: Vec<Grant>,
    pub log: Vec<LogEntry>,
    pub scan_meta: HashMap<String, ScanMeta>,
    pub exports: HashMap<String, Value>,
    pub export_files: HashMap<String, std::path::PathBuf>,
    pub measurements: HashMap<String, Value>,
    /// Evidence images, by measurement id.
    pub measurement_files: HashMap<String, std::path::PathBuf>,
    /// The agent's latest next-station proposal per scan: its JSON and picture.
    pub next_stations: HashMap<String, (Value, Vec<u8>)>,
    /// Each project's mission (what the job is for, and its ledger).
    pub missions: HashMap<String, crate::mission::Mission>,
    /// A question's reading waiting for its second pass, by (project, target):
    /// kept here, out of the mission, so no client can quote it.
    pub settling: HashMap<(String, String), (crate::register::Face, crate::ask::Reading)>,
    /// One look at a question target at a time, by (project, target).
    pub asking: HashMap<(String, String), std::sync::Arc<tokio::sync::Mutex<()>>>,
    /// A question's latest evidence image, by (project, target).
    pub evidence: HashMap<(String, String), Vec<u8>>,
    /// Each project's registration (`registrations.json`).
    pub registrations: HashMap<String, crate::register::Registration>,
    /// Each registered project's merged wall points, loaded on first use.
    pub merged: HashMap<String, std::sync::Arc<crate::register::Merged>>,
    /// Projects with a registration job running (its job id), and those whose
    /// job should run once more when it ends (a station landed meanwhile).
    pub registering: HashMap<String, String>,
    pub register_again: HashSet<String>,
    pub runs: HashMap<String, crate::runs::RunRecord>,
    /// Scanner tasks the bridge cancelled that may still be listed in `/tasks`.
    pub cancelled_tasks: HashSet<i64>,
}

impl Inner {
    /// Cancel a job that has not started (given back its grant use), or ask
    /// whoever runs it (the scan worker, an export) to stop. Finished jobs are
    /// left alone. Returns the job as it now stands.
    pub fn cancel_job(&mut self, job_id: &str) -> Option<Job> {
        let rec = self.jobs.iter_mut().find(|j| j.job.job_id == job_id)?;
        match rec.job.state {
            JobState::Queued | JobState::AwaitingApproval => {
                rec.job.state = JobState::Cancelled;
                rec.job.finished_at = Some(now_rfc3339());
            }
            JobState::Running => rec.cancel_requested = true,
            _ => {}
        }
        let job = rec.job.clone();
        if job.state == JobState::Cancelled {
            self.refund_grant(job_id);
        }
        Some(job)
    }

    /// Give back the grant use of a scan that never reached the scanner.
    pub fn refund_grant(&mut self, job_id: &str) {
        let Some(gid) = self.jobs.iter_mut().find(|j| j.job.job_id == job_id).and_then(|j| j.grant_id.take()) else { return };
        if let Some(g) = self.grants.iter_mut().find(|g| g.grant_id == gid) {
            g.scans_used = g.scans_used.saturating_sub(1);
        }
    }

    /// The scan job the worker is running (exports run beside it and don't count).
    pub fn current_scan_job(&self) -> Option<&Job> {
        self.jobs.iter().map(|j| &j.job).find(|j| j.state == JobState::Running && j.kind == "scan")
    }
}

pub struct AppState {
    pub scanner: x9_client::Client,
    pub tokens: HashMap<String, Caller>,
    pub rules: Rules,
    pub inner: Mutex<Inner>,
    pub events: broadcast::Sender<Event>,
    pub wake: Notify,
    pub started_at: String,
    pub poll: Duration,
    /// Cached scan files, previews and exports.
    pub data_dir: std::path::PathBuf,
    /// Recipe files (`*.toml`), read on each request so new ones need no restart.
    pub recipes_dir: std::path::PathBuf,
    /// Viewer and coverage builds run one at a time (each decodes a whole scan).
    pub view_lock: tokio::sync::Mutex<()>,
    /// Workers on other computers and the registration jobs offered to them.
    pub work: Mutex<crate::work::WorkState>,
    /// Prepared stations, by scan id, so a registration redoes only what's new.
    pub prepared: Mutex<HashMap<String, std::sync::Arc<crate::register::Prepared>>>,
    /// Pair checks already made (see `register::audit`).
    pub audit_cache: Mutex<crate::register::AuditCache>,
    /// Each recently projected-onto scan's depth and ground (`view3d`), a few at most.
    pub depth: Mutex<Vec<(String, std::sync::Arc<crate::view3d::DepthView>)>>,
}

impl AppState {
    pub fn new(scanner: x9_client::Client, tokens: HashMap<String, Caller>, rules: Rules, poll: Duration, data_dir: std::path::PathBuf, recipes_dir: std::path::PathBuf) -> Self {
        let (events, _) = broadcast::channel(256);
        AppState {
            scanner,
            tokens,
            rules,
            inner: Mutex::new(Inner::default()),
            events,
            wake: Notify::new(),
            started_at: now_rfc3339(),
            poll,
            data_dir,
            recipes_dir,
            view_lock: tokio::sync::Mutex::new(()),
            work: Mutex::new(Default::default()),
            prepared: Mutex::new(HashMap::new()),
            audit_cache: Mutex::new(HashMap::new()),
            depth: Mutex::new(Vec::new()),
        }
    }

    fn grants_path(&self) -> std::path::PathBuf {
        self.data_dir.join("grants.json")
    }

    /// Write the grants to `grants.json`, so a restart (a link coming up, a
    /// reboot) keeps the permission a person gave for the visit. Call after
    /// every change: a grant given, used, given back or revoked.
    pub fn save_grants(&self) {
        let gs = self.inner.lock().unwrap().grants.clone();
        let path = self.grants_path();
        let tmp = path.with_extension(format!("json.{}", new_id("tmp")));
        let res = serde_json::to_vec_pretty(&gs)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            self.log("error", format!("Could not save grants: {e}"), "bridge", None, &[]);
        }
    }

    /// Load the grants saved before a restart, dropping expired ones.
    /// Returns how many are active.
    pub fn load_grants(&self) -> usize {
        let Ok(bytes) = std::fs::read(self.grants_path()) else { return 0 };
        let gs: Vec<Grant> = match serde_json::from_slice(&bytes) {
            Ok(g) => g,
            Err(e) => {
                self.log("error", format!("Ignoring grants.json: {e}"), "bridge", None, &[]);
                return 0;
            }
        };
        let now = crate::util::now_unix();
        let live: Vec<Grant> = gs
            .into_iter()
            .filter_map(|mut g| {
                g.expires_unix = crate::util::parse_rfc3339(&g.req.expires_at)?;
                (g.expires_unix > now).then_some(g)
            })
            .collect();
        let n = live.len();
        self.inner.lock().unwrap().grants = live;
        n
    }

    /// Write the missions to `missions.json` (after every change), so a
    /// restart keeps the job's ledger.
    pub fn save_missions(&self) {
        let ms: Vec<crate::mission::Mission> = self.inner.lock().unwrap().missions.values().cloned().collect();
        let path = self.data_dir.join("missions.json");
        let tmp = path.with_extension(format!("json.{}", new_id("tmp")));
        let res = serde_json::to_vec_pretty(&ms)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            self.log("error", format!("Could not save missions: {e}"), "bridge", None, &[]);
        }
    }

    /// Write the registrations to `registrations.json` (after every change).
    pub fn save_registrations(&self) {
        let rs: Vec<crate::register::Registration> = self.inner.lock().unwrap().registrations.values().cloned().collect();
        let path = self.data_dir.join("registrations.json");
        let tmp = path.with_extension(format!("json.{}", new_id("tmp")));
        let res = serde_json::to_vec(&rs)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            self.log("error", format!("Could not save registrations: {e}"), "bridge", None, &[]);
        }
    }

    /// Load the registrations saved before a restart. Returns how many.
    pub fn load_registrations(&self) -> usize {
        let Ok(bytes) = std::fs::read(self.data_dir.join("registrations.json")) else { return 0 };
        match serde_json::from_slice::<Vec<crate::register::Registration>>(&bytes) {
            Ok(rs) => {
                let n = rs.len();
                self.inner.lock().unwrap().registrations = rs.into_iter().map(|r| (r.project_id.clone(), r)).collect();
                n
            }
            Err(e) => {
                self.log("error", format!("Ignoring registrations.json: {e}"), "bridge", None, &[]);
                0
            }
        }
    }

    /// Where a project's merged points live.
    pub fn merged_path(&self, project_id: &str) -> Option<std::path::PathBuf> {
        let ok = !project_id.is_empty() && project_id.len() <= 128 && project_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok.then(|| self.data_dir.join("registration").join(format!("{project_id}.merged")))
    }

    /// The project's merged wall points (from disk on first use).
    pub fn merged(&self, project_id: &str) -> Option<std::sync::Arc<crate::register::Merged>> {
        if let Some(m) = self.inner.lock().unwrap().merged.get(project_id) {
            return Some(m.clone());
        }
        let bytes = std::fs::read(self.merged_path(project_id)?).ok()?;
        let m = std::sync::Arc::new(crate::register::Merged::from_bytes(&bytes).ok()?);
        self.inner.lock().unwrap().merged.insert(project_id.to_string(), m.clone());
        Some(m)
    }

    /// The registration as missions use it (empty when not registered).
    pub fn geo(&self, project_id: &str) -> crate::register::Geo {
        let reg = self.inner.lock().unwrap().registrations.get(project_id).cloned();
        match reg {
            None => crate::register::Geo::none(),
            Some(r) => crate::register::Geo { scans: r.station_scans(), walls: r.walls, merged: self.merged(project_id) },
        }
    }

    /// Load the missions saved before a restart. Returns how many.
    pub fn load_missions(&self) -> usize {
        let Ok(bytes) = std::fs::read(self.data_dir.join("missions.json")) else { return 0 };
        match serde_json::from_slice::<Vec<crate::mission::Mission>>(&bytes) {
            Ok(ms) => {
                let n = ms.len();
                self.inner.lock().unwrap().missions = ms.into_iter().map(|mut m| { m.refresh_said(); (m.project_id.clone(), m) }).collect();
                n
            }
            Err(e) => {
                self.log("error", format!("Ignoring missions.json: {e}"), "bridge", None, &[]);
                0
            }
        }
    }

    /// Who is calling, from `Authorization: Bearer <token>`.
    pub fn caller(&self, headers: &HeaderMap) -> Result<Caller, ApiError> {
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        self.tokens.get(token.trim()).cloned().ok_or_else(ApiError::unauthorized)
    }

    pub fn require(&self, headers: &HeaderMap, ok: impl Fn(Role) -> bool, what: &str) -> Result<Caller, ApiError> {
        let c = self.caller(headers)?;
        if ok(c.role) {
            Ok(c)
        } else {
            Err(ApiError::forbidden(what))
        }
    }

    pub fn emit(&self, kind: &'static str, data: Value) {
        let _ = self.events.send(Event { kind, data });
    }

    pub fn emit_job(&self, job: &Job) {
        self.emit("job", serde_json::to_value(job).unwrap_or_default());
    }

    /// Append to the field log and publish it. Must not be called while
    /// holding `inner`.
    pub fn log(&self, kind: &str, text: impl Into<String>, author: &str, project_id: Option<&str>, refs: &[(&str, &str)]) -> LogEntry {
        let entry = LogEntry {
            entry_id: new_id("log"),
            at: now_rfc3339(),
            kind: kind.into(),
            text: text.into(),
            author: author.into(),
            project_id: project_id.map(str::to_string),
            refs: refs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        };
        self.inner.lock().unwrap().log.push(entry.clone());
        self.emit("log", serde_json::to_value(&entry).unwrap_or_default());
        entry
    }

    /// A fresh read of the scanner for the rules. Never fails: an unreachable
    /// scanner is `connected: false`.
    pub async fn snapshot(&self) -> (Snapshot, Option<x9_client::models::Status>) {
        match self.scanner.status().await {
            Ok(st) => (
                Snapshot {
                    connected: true,
                    battery_pct: st.battery_level,
                    on_ac: st.plugged_to_ac.unwrap_or(false),
                    storage_free: st.storage.as_ref().and_then(|s| s.available_size),
                },
                Some(st),
            ),
            Err(_) => (Snapshot::default(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GrantRequest;

    fn job(id: &str, kind: &str, state: JobState, grant: Option<&str>) -> JobRecord {
        JobRecord {
            job: Job {
                job_id: id.into(),
                kind: kind.into(),
                state,
                phase: None,
                progress: None,
                requested_by: "agent-1".into(),
                run_id: None,
                created_at: now_rfc3339(),
                started_at: None,
                finished_at: None,
                result: None,
                problem: None,
                partial_scan_id: None,
                request: None,
                project_name: None,
            },
            request: None,
            project_id: "p1".into(),
            project_name: "p".into(),
            scanner_task_id: None,
            cancel_requested: false,
            grant_id: grant.map(str::to_string),
        }
    }

    fn grant(used: u32) -> Grant {
        let req = GrantRequest {
            project_id: "p1".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            max_scans: Some(2),
            allowed_modes: vec![],
            allow_color: false,
            run_id: None,
            note: None,
        };
        Grant { req, grant_id: "g1".into(), granted_by: "operator-1".into(), scans_used: used, expires_unix: u64::MAX }
    }

    #[test]
    fn cancelling_before_the_start_gives_the_grant_use_back() {
        let mut inner = Inner { grants: vec![grant(2)], ..Default::default() };
        inner.jobs.push(job("j1", "scan", JobState::Queued, Some("g1")));
        inner.jobs.push(job("j2", "scan", JobState::Running, Some("g1")));
        assert_eq!(inner.cancel_job("j1").unwrap().state, JobState::Cancelled);
        assert_eq!(inner.grants[0].scans_used, 1);
        // Only once.
        inner.cancel_job("j1");
        assert_eq!(inner.grants[0].scans_used, 1);
        // A running job is only asked to stop; the worker decides.
        assert_eq!(inner.cancel_job("j2").unwrap().state, JobState::Running);
        assert!(inner.jobs[1].cancel_requested);
        assert_eq!(inner.grants[0].scans_used, 1);
        inner.refund_grant("j2");
        assert_eq!(inner.grants[0].scans_used, 0);
        assert!(inner.cancel_job("nope").is_none());
    }

    #[test]
    fn exports_are_not_the_current_job() {
        let mut inner = Inner::default();
        inner.jobs.push(job("e1", "export", JobState::Running, None));
        assert!(inner.current_scan_job().is_none());
        inner.jobs.push(job("s1", "scan", JobState::Running, None));
        assert_eq!(inner.current_scan_job().unwrap().job_id, "s1");
    }
}
