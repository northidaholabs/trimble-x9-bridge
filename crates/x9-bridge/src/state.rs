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
    /// What each question target was last judged on (`ask::Basis`): a re-check
    /// skips a target whose basis hasn't changed. Not saved: after a restart
    /// every target is judged again once.
    pub basis: HashMap<(String, String), crate::ask::Basis>,
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
    /// The scan cache is over its budget with nothing it may drop, and the
    /// field log has said so (once, until it gets back under).
    pub cache_noted: bool,
    pub register_again: HashSet<String>,
    /// Scan files being read on the Pi now (scan id → readers): the cache
    /// never drops these (`files::reading`).
    pub reading: HashMap<String, usize>,
    /// Each project's latest save to a laptop (`saves.json`).
    pub saves: HashMap<String, Value>,
    /// Each project's latest count of openings, and its face pictures by
    /// (project, face). Not saved: after a restart, count again.
    pub counts: HashMap<String, crate::openings::Counted>,
    pub count_pictures: HashMap<(String, String), Vec<u8>>,
    /// When each project's latest finished count started (`count::count`
    /// skips a count asked before it).
    pub count_started: HashMap<String, std::time::Instant>,
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

    /// Write the latest saves to `saves.json` (after every save).
    pub fn save_saves(&self) {
        let saves = self.inner.lock().unwrap().saves.clone();
        let path = self.data_dir.join("saves.json");
        let tmp = path.with_extension(format!("json.{}", new_id("tmp")));
        let res = serde_json::to_vec(&saves)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            self.log("error", format!("Could not keep the record of saves: {e}"), "bridge", None, &[]);
        }
    }

    /// Load the saves recorded before a restart. Returns how many projects.
    pub fn load_saves(&self) -> usize {
        let Ok(bytes) = std::fs::read(self.data_dir.join("saves.json")) else { return 0 };
        match serde_json::from_slice::<HashMap<String, Value>>(&bytes) {
            Ok(m) => {
                let n = m.len();
                self.inner.lock().unwrap().saves = m;
                n
            }
            Err(e) => {
                self.log("error", format!("Ignoring saves.json: {e}"), "bridge", None, &[]);
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

    // ------------------------------------------------- saved state, callers ---

    use crate::register::{Merged, Registration};
    use crate::testkit::{Bridge, ADMIN, AGENT, OPERATOR, VIEWER};

    /// A bridge whose scanner is never reached.
    fn bridge() -> Bridge {
        Bridge::on_scanner("http://127.0.0.1:1/api/v1")
    }

    fn errors(st: &AppState) -> Vec<String> {
        st.inner.lock().unwrap().log.iter().filter(|e| e.kind == "error").map(|e| e.text.clone()).collect()
    }

    fn mission(project_id: &str, goal: &str) -> crate::mission::Mission {
        let req: crate::mission::MissionRequest = serde_json::from_value(serde_json::json!({"goal": goal, "kind": "exterior", "done_when": "all four walls are seen"})).unwrap();
        crate::mission::Mission::new(&req, project_id, "crew")
    }

    fn registration(project_id: &str) -> Registration {
        Registration {
            project_id: project_id.into(),
            frame_scan_id: Some("s1".into()),
            stations: vec![],
            pairs: vec![],
            walls: vec![],
            summary: "1 station".into(),
            updated_at: now_rfc3339(),
        }
    }

    #[tokio::test]
    async fn grants_survive_a_restart_but_expired_ones_do_not() {
        let b = bridge();
        let mut live = grant(1);
        live.grant_id = "live".into();
        let mut old = grant(0);
        old.grant_id = "old".into();
        old.req.expires_at = "2001-01-01T00:00:00Z".into();
        let mut bad = grant(0);
        bad.grant_id = "bad".into();
        bad.req.expires_at = "tomorrow".into();
        b.st.inner.lock().unwrap().grants = vec![live, old, bad];
        b.st.save_grants();
        assert!(b.dir.join("data/grants.json").exists());
        // No stray temporary files.
        assert_eq!(std::fs::read_dir(b.dir.join("data")).unwrap().count(), 1);

        let again = Bridge::on_scanner("http://127.0.0.1:1/api/v1");
        std::fs::copy(b.dir.join("data/grants.json"), again.dir.join("data/grants.json")).unwrap();
        assert_eq!(again.st.load_grants(), 1);
        let gs = again.st.inner.lock().unwrap().grants.clone();
        assert_eq!((gs[0].grant_id.as_str(), gs[0].scans_used), ("live", 1));
        assert_eq!(gs[0].expires_unix, crate::util::parse_rfc3339("2099-01-01T00:00:00Z").unwrap());
    }

    #[tokio::test]
    async fn missing_or_broken_saved_files_load_nothing() {
        let b = bridge();
        assert_eq!((b.st.load_grants(), b.st.load_missions(), b.st.load_registrations()), (0, 0, 0));
        assert!(errors(&b.st).is_empty());
        for f in ["grants.json", "missions.json", "registrations.json"] {
            std::fs::write(b.dir.join("data").join(f), b"{not json").unwrap();
        }
        assert_eq!((b.st.load_grants(), b.st.load_missions(), b.st.load_registrations()), (0, 0, 0));
        let e = errors(&b.st);
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(e[0].starts_with("Ignoring grants.json") && e[1].starts_with("Ignoring missions.json") && e[2].starts_with("Ignoring registrations.json"), "{e:?}");
    }

    #[tokio::test]
    async fn missions_survive_a_restart() {
        let b = bridge();
        b.st.inner.lock().unwrap().missions.insert("p1".into(), mission("p1", "Survey the house"));
        b.st.inner.lock().unwrap().missions.insert("p2".into(), mission("p2", "Survey the barn"));
        b.st.save_missions();
        b.st.inner.lock().unwrap().missions.clear();
        assert_eq!(b.st.load_missions(), 2);
        let ms = b.st.inner.lock().unwrap().missions.clone();
        assert_eq!(ms["p1"].goal, "Survey the house");
        assert_eq!(ms["p2"].project_id, "p2");
    }

    #[tokio::test]
    async fn registrations_survive_a_restart() {
        let b = bridge();
        b.st.inner.lock().unwrap().registrations.insert("p1".into(), registration("p1"));
        b.st.save_registrations();
        b.st.inner.lock().unwrap().registrations.clear();
        assert_eq!(b.st.load_registrations(), 1);
        let r = b.st.inner.lock().unwrap().registrations["p1"].clone();
        assert_eq!((r.frame_scan_id.as_deref(), r.summary.as_str()), (Some("s1"), "1 station"));
    }

    #[tokio::test]
    async fn a_save_that_cannot_write_is_logged_and_leaves_nothing_behind() {
        let b = bridge();
        b.st.inner.lock().unwrap().grants = vec![grant(0)];
        std::fs::remove_dir_all(b.dir.join("data")).unwrap();
        b.st.save_grants();
        b.st.save_missions();
        b.st.save_registrations();
        let e = errors(&b.st);
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(e[0].starts_with("Could not save grants") && e[1].starts_with("Could not save missions") && e[2].starts_with("Could not save registrations"), "{e:?}");
        assert!(!b.dir.join("data").exists());
    }

    #[tokio::test]
    async fn merged_points_load_from_disk_once_and_only_for_safe_project_ids() {
        let b = bridge();
        for bad in ["", "../up", "a/b", "sp ace", &"x".repeat(129)] {
            assert!(b.st.merged_path(bad).is_none(), "{bad:?}");
            assert!(b.st.merged(bad).is_none());
        }
        let path = b.st.merged_path("p-1_A").unwrap();
        assert!(path.ends_with("registration/p-1_A.merged"));
        assert!(b.st.merged("p-1_A").is_none());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"junk").unwrap();
        assert!(b.st.merged("p-1_A").is_none());
        let mut m = Merged::default();
        m.pts.push([1.0, 2.0, 3.0]);
        m.spacing_mm.push(4.0);
        m.station.push(1);
        m.plan.push([1.0, 2.0]);
        std::fs::write(&path, m.to_bytes()).unwrap();
        assert_eq!(b.st.merged("p-1_A").unwrap().pts, m.pts);
        // Kept in memory: the file is no longer read.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(b.st.merged("p-1_A").unwrap().station, vec![1]);
    }

    #[tokio::test]
    async fn geo_is_empty_until_the_project_is_registered() {
        let b = bridge();
        let g = b.st.geo("p1");
        assert!(g.scans.is_empty() && g.walls.is_empty() && g.merged.is_none());
        b.st.inner.lock().unwrap().registrations.insert("p1".into(), registration("p1"));
        b.st.inner.lock().unwrap().merged.insert("p1".into(), std::sync::Arc::new(Merged::default()));
        let g = b.st.geo("p1");
        assert!(g.merged.is_some());
    }

    #[tokio::test]
    async fn callers_are_known_by_their_bearer_token_and_role() {
        let b = bridge();
        let headers = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(axum::http::header::AUTHORIZATION, v.parse().unwrap());
            h
        };
        assert_eq!(b.st.caller(&HeaderMap::new()).unwrap_err().code, "unauthorized");
        assert_eq!(b.st.caller(&headers(&format!("Basic {AGENT}"))).unwrap_err().code, "unauthorized");
        assert_eq!(b.st.caller(&headers("Bearer nope")).unwrap_err().code, "unauthorized");
        assert_eq!(b.st.caller(&headers(&format!("Bearer {AGENT}  "))).unwrap().role, Role::Agent);
        assert_eq!(b.st.require(&headers(&format!("Bearer {OPERATOR}")), Role::is_operator, "approve").unwrap().name, "crew");
        assert_eq!(b.st.require(&headers(&format!("Bearer {ADMIN}")), Role::is_operator, "approve").unwrap().role, Role::Admin);
        assert_eq!(b.st.require(&headers(&format!("Bearer {VIEWER}")), Role::is_operator, "approve").unwrap_err().code, "forbidden");
    }

    #[tokio::test]
    async fn the_log_is_kept_and_published() {
        let b = bridge();
        let mut rx = b.st.events.subscribe();
        let e = b.st.log("note", "Tripod moved", "crew", Some("p1"), &[("scan_id", "s1")]);
        assert_eq!((e.kind.as_str(), e.project_id.as_deref(), e.refs["scan_id"].as_str()), ("note", Some("p1"), "s1"));
        let ev = rx.try_recv().unwrap();
        assert_eq!((ev.kind, ev.data["text"].as_str()), ("log", Some("Tripod moved")));
        b.st.emit_job(&job("j1", "scan", JobState::Queued, None).job);
        let ev = rx.try_recv().unwrap();
        assert_eq!((ev.kind, ev.data["job_id"].as_str()), ("job", Some("j1")));
        assert_eq!(b.st.inner.lock().unwrap().log.len(), 1);
    }

    #[tokio::test]
    async fn a_snapshot_reads_the_scanner_or_says_it_is_not_connected() {
        let b = bridge();
        let (snap, raw) = b.st.snapshot().await;
        assert!(!snap.connected && raw.is_none());
        let b = Bridge::with_mock(x9_mock::Config { battery: 42, on_ac: true, ..crate::testkit::fast_mock() }).await;
        let (snap, raw) = b.st.snapshot().await;
        assert!(snap.connected && snap.on_ac);
        assert_eq!(snap.battery_pct, Some(42));
        assert!(snap.storage_free.unwrap() > 0);
        assert!(raw.is_some());
    }

    #[test]
    fn finished_jobs_are_left_alone_by_a_cancel() {
        let mut inner = Inner { grants: vec![grant(1)], ..Default::default() };
        inner.jobs.push(job("j1", "scan", JobState::Succeeded, Some("g1")));
        inner.jobs.push(job("j2", "scan", JobState::AwaitingApproval, None));
        assert_eq!(inner.cancel_job("j1").unwrap().state, JobState::Succeeded);
        assert_eq!(inner.grants[0].scans_used, 1);
        let j = inner.cancel_job("j2").unwrap();
        assert_eq!(j.state, JobState::Cancelled);
        assert!(j.finished_at.is_some());
        // A grant that is gone: nothing to give back.
        inner.jobs.push(job("j3", "scan", JobState::Queued, Some("g9")));
        inner.cancel_job("j3");
        assert_eq!(inner.grants[0].scans_used, 1);
    }
}
