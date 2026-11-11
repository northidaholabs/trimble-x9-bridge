//! Workers: a faster computer (the field laptop, a desk machine) can do the
//! heavy part of registration for the bridge. A worker polls
//! `POST /v1/work/claim`; when a registration job is waiting, the bridge hands
//! it the stations and the pairs already matched. The worker fetches the scans
//! (`GET /v1/scans/{id}/tzf`), registers and audits them, uploads the merged
//! points (`PUT /v1/work/{job}/merged`) and posts the result
//! (`POST /v1/work/{job}/result`). The bridge stays the record: it stores,
//! logs and re-judges the mission exactly as when it registers itself.
//!
//! The bridge registers on its own when no worker has been seen for 15 s,
//! when none claims the job within 10 s, or when the worker fails or takes
//! longer than 10 minutes. A job never waits on a laptop that went away.
//!
//! A worker can also take question checks (`specs/laptop-offload.md`, part
//! B): a `check` job lists faces with their coverage and the stations that
//! saw them. The worker fits them (`ask::look_many`, as the bridge would) and
//! sends back what each look found, with the fits themselves uploaded as one
//! blob (`PUT …/merged`). The bridge decides, records and draws the evidence.
//! It fits them itself on the same rules, with a shorter limit: 120 s for a
//! re-check, 60 s for one ask.
//!
//! The laptop is also where the data lands (part D). Each claim lists the
//! scan files the worker holds, so the Pi can drop its own copies when its
//! cache is full. A `save` job writes the project's registered stations and
//! scan files to a folder on the worker's computer (`save`). A save has no
//! fallback on the Pi: it waits for a worker, and goes back on offer if the
//! one that took it goes quiet for 5 minutes.

use crate::register::{Audit, Pair, Pose, Wall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// A worker counts as connected this long after its last claim.
pub const WORKER_SEEN: Duration = Duration::from_secs(15);
/// How long a job is offered before the bridge does it itself.
const CLAIM_WAIT: Duration = Duration::from_secs(10);
/// How long a claimed job may take.
const RUN_LIMIT: Duration = Duration::from_secs(600);

/// What a worker sends back. `pairs` cover every pair it matched; `poses`
/// and `audits` follow the offered stations in order, less `skipped`.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkResult {
    pub pairs: Vec<Pair>,
    pub poses: Vec<Option<Pose>>,
    pub audits: Vec<Audit>,
    pub walls: Vec<Wall>,
    /// Stations it couldn't decode: (station number, why).
    #[serde(default)]
    pub skipped: Vec<(u16, String)>,
    /// The scan whose frame is the project's (the largest group's first
    /// station); the first station when not given.
    #[serde(default)]
    pub frame: Option<String>,
}

/// What a worker found for one face of a check job: the look's coverage, as
/// offered, and its reading when it fitted. `fitted` faces have their fit in
/// the uploaded blob, in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckOut {
    pub target_id: String,
    pub seen_share: f64,
    pub length_m: f64,
    pub stations: Vec<u16>,
    pub fit: Option<crate::ask::Reading>,
    #[serde(default)]
    pub fitted: bool,
}

/// A file a save wrote, relative to its project folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// What a save did with one station: its registered cloud (when placed) and
/// whether it was kept from the last save unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedStation {
    pub station: u16,
    pub scan_id: String,
    #[serde(default)]
    pub copc: Option<String>,
    #[serde(default)]
    pub point_count: Option<u64>,
    #[serde(default)]
    pub kept: bool,
    /// Why a station has no cloud or no scan file, when it hasn't.
    #[serde(default)]
    pub note: Option<String>,
}

/// A worker's answer to a save job: the folder, as that computer names it,
/// and what is in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveResult {
    pub folder: String,
    pub files: Vec<SavedFile>,
    pub stations: Vec<SavedStation>,
}

/// A worker's answer to a check job: one entry per offered face, in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub checks: Vec<CheckOut>,
}

/// The fits of a check job as one blob: a count, then each fit's length and
/// bytes (`FaceFit::to_bytes`).
pub fn pack_fits(fits: &[Vec<u8>]) -> Vec<u8> {
    let mut b = (fits.len() as u32).to_le_bytes().to_vec();
    for f in fits {
        b.extend_from_slice(&(f.len() as u32).to_le_bytes());
        b.extend_from_slice(f);
    }
    b
}

pub fn unpack_fits(b: &[u8]) -> Result<Vec<crate::analyze::FaceFit>, String> {
    let take = |at: usize, n: usize| b.get(at..at + n).ok_or_else(|| "fits cut short".to_string());
    let count = u32::from_le_bytes(take(0, 4)?.try_into().unwrap()) as usize;
    let (mut at, mut out) = (4, Vec::new());
    for _ in 0..count.min(1000) {
        let n = u32::from_le_bytes(take(at, 4)?.try_into().unwrap()) as usize;
        out.push(crate::analyze::FaceFit::from_bytes(take(at + 4, n)?)?);
        at += 4 + n;
    }
    if out.len() != count || at != b.len() {
        return Err("fits don't add up".into());
    }
    Ok(out)
}

enum Delivered {
    Register(WorkResult),
    Check(CheckResult),
    Save(SaveResult),
}

type Delivery = Result<(Delivered, Option<Vec<u8>>), String>;

enum Job {
    Register { stations: Vec<(u16, String)>, pairs: Vec<Pair> },
    /// The offer's body as the worker gets it, and how many faces it lists.
    Check { body: Value, faces: usize },
    /// Save the project to a folder on the worker; the body is built when it
    /// is claimed, from the registration as it then stands.
    Save,
}

struct Offer {
    project_id: String,
    job: Job,
    /// (worker name, the caller who claimed it).
    claimed_by: Option<(String, String)>,
    merged: Option<Vec<u8>>,
    done: Option<oneshot::Sender<Delivery>>,
    /// When the worker holding it last claimed or reported progress.
    heard: Option<Instant>,
}

#[derive(Default)]
pub struct WorkState {
    /// Worker name → last claim, and the caller behind it.
    workers: HashMap<String, (Instant, String)>,
    /// Offered jobs, by job id.
    offers: HashMap<String, Offer>,
    /// The scan files each worker holds, as it last said.
    holds: HashMap<String, HashSet<String>>,
}

pub enum Offered {
    /// The worker's result, the merged points (bytes) and the worker's name.
    Done(WorkResult, Vec<u8>, String),
    NoWorker,
    Failed(String),
    Cancelled,
}

impl WorkState {
    /// Workers seen recently: name, seconds since, the job each holds.
    pub fn workers(&self) -> Vec<Value> {
        let mut v: Vec<Value> = self
            .workers
            .iter()
            .filter(|(_, (t, _))| t.elapsed() < WORKER_SEEN)
            .map(|(name, (t, by))| {
                let job = self.offers.iter().find(|(_, o)| o.claimed_by.as_ref().is_some_and(|c| &c.0 == name));
                let kind = job.map(|(_, o)| o.job.kind());
                json!({"worker": name, "token": by, "seen_s_ago": (t.elapsed().as_secs_f64() * 10.0).round() / 10.0, "job_id": job.map(|(j, _)| j.clone()), "job_kind": kind})
            })
            .collect();
        v.sort_by(|a, b| a["worker"].as_str().cmp(&b["worker"].as_str()));
        v
    }

    /// A worker asks for work: note it, and hand it an unclaimed job,
    /// registrations before checks (a check follows its registration), and
    /// saves last.
    #[cfg(test)]
    pub fn claim(&mut self, worker: &str, caller: &str) -> Option<Value> {
        self.claim_with(worker, caller, &HashSet::new())
    }

    /// As `claim`, holding back the saves of `registering` projects (a save
    /// should carry the registration that is coming) and of projects a save
    /// is already running for (one writer per folder).
    pub fn claim_with(&mut self, worker: &str, caller: &str, registering: &HashSet<String>) -> Option<Value> {
        self.workers.insert(worker.to_string(), (Instant::now(), caller.to_string()));
        let saving: HashSet<String> = self.offers.values().filter(|o| matches!(o.job, Job::Save) && o.claimed_by.is_some()).map(|o| o.project_id.clone()).collect();
        let pick = self
            .offers
            .iter()
            .filter(|(_, o)| o.claimed_by.is_none())
            .filter(|(_, o)| !matches!(o.job, Job::Save) || !(registering.contains(&o.project_id) || saving.contains(&o.project_id)))
            .min_by_key(|(id, o)| (o.job.rank(), id.to_string()))
            .map(|(id, _)| id.clone())?;
        let o = self.offers.get_mut(&pick)?;
        o.claimed_by = Some((worker.to_string(), caller.to_string()));
        o.heard = Some(Instant::now());
        Some(match &o.job {
            Job::Register { stations, pairs } => json!({
                "job_id": pick,
                "kind": "register",
                "project_id": o.project_id,
                "stations": stations.iter().map(|(n, id)| json!({"station": n, "scan_id": id})).collect::<Vec<_>>(),
                "pairs": pairs,
            }),
            Job::Check { body, .. } => {
                let mut v = body.clone();
                v["job_id"] = json!(pick);
                v["kind"] = json!("check");
                v["project_id"] = json!(o.project_id);
                v
            }
            Job::Save => json!({"job_id": pick, "kind": "save", "project_id": o.project_id}),
        })
    }

    /// The kind of an offered job: `register`, `check` or `save`.
    pub fn kind(&self, job_id: &str) -> Option<&'static str> {
        self.offers.get(job_id).map(|o| o.job.kind())
    }

    /// Note the scan files a worker says it holds.
    pub fn note_holds(&mut self, worker: &str, ids: HashSet<String>) {
        self.holds.insert(worker.to_string(), ids);
    }

    /// The scan files held by the workers seen recently.
    pub fn held(&self) -> HashSet<String> {
        self.holds
            .iter()
            .filter(|(w, _)| self.workers.get(*w).is_some_and(|(t, _)| t.elapsed() < WORKER_SEEN))
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect()
    }

    /// The offer `job_id`, if `caller` claimed it.
    fn mine(&mut self, job_id: &str, caller: &str) -> Result<&mut Offer, String> {
        match self.offers.get_mut(job_id) {
            None => Err("no such job is offered (it finished, was cancelled, or the bridge took it back)".into()),
            Some(o) if o.claimed_by.as_ref().is_none_or(|c| c.1 != caller) => Err("this job was not claimed with your token".into()),
            Some(o) => Ok(o),
        }
    }

    /// Whether `caller` holds the offered job (for progress reports, which
    /// count as hearing from it).
    pub fn holds(&mut self, job_id: &str, caller: &str) -> Result<(), String> {
        self.mine(job_id, caller)?.heard = Some(Instant::now());
        Ok(())
    }

    /// Keep the worker's merged points (already checked) for its result.
    pub fn put_merged(&mut self, job_id: &str, caller: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.mine(job_id, caller)?.merged = Some(bytes);
        Ok(())
    }

    pub fn deliver(&mut self, job_id: &str, caller: &str, r: Result<WorkResult, String>) -> Result<(), String> {
        let o = self.mine(job_id, caller)?;
        let d = match (r, &o.job) {
            (Ok(res), Job::Register { stations, .. }) => {
                let kept = stations.len() - res.skipped.len().min(stations.len());
                if res.poses.len() != kept || res.audits.len() != kept {
                    return Err(format!("poses and audits must list the {kept} stations offered, less those skipped"));
                }
                Ok((Delivered::Register(res), Some(o.merged.take().ok_or("upload the merged points (PUT …/merged) before the result")?)))
            }
            (Ok(_), Job::Check { .. }) => return Err("this is a check job: send its checks".into()),
            (Ok(_), Job::Save) => return Err("this is a save job: send what it saved".into()),
            (Err(e), _) => Err(e),
        };
        if let Some(tx) = o.done.take() {
            let _ = tx.send(d);
        }
        Ok(())
    }

    /// A worker's answer to a check job: one entry per offered face, and the
    /// fits uploaded first when any face fitted.
    pub fn deliver_check(&mut self, job_id: &str, caller: &str, r: CheckResult) -> Result<(), String> {
        let o = self.mine(job_id, caller)?;
        let Job::Check { faces, .. } = &o.job else { return Err("this is a registration job: send its result".into()) };
        if r.checks.len() != *faces {
            return Err(format!("checks must list the {faces} faces offered, in order"));
        }
        let fitted = r.checks.iter().filter(|c| c.fitted).count();
        let blob = match (fitted, o.merged.take()) {
            (0, b) => b,
            (_, Some(b)) => Some(b),
            (_, None) => return Err("upload the fits (PUT …/merged) before the checks".into()),
        };
        if let Some(tx) = o.done.take() {
            let _ = tx.send(Ok((Delivered::Check(r), blob)));
        }
        Ok(())
    }
}

impl Job {
    fn kind(&self) -> &'static str {
        match self {
            Job::Register { .. } => "register",
            Job::Check { .. } => "check",
            Job::Save => "save",
        }
    }

    /// Claim order: registrations, then checks, then saves.
    fn rank(&self) -> u8 {
        match self {
            Job::Register { .. } => 0,
            Job::Check { .. } => 1,
            Job::Save => 2,
        }
    }
}

impl WorkState {
    /// A worker's answer to a save job.
    pub fn deliver_save(&mut self, job_id: &str, caller: &str, r: SaveResult) -> Result<(), String> {
        let o = self.mine(job_id, caller)?;
        if !matches!(o.job, Job::Save) {
            return Err("this is not a save job".into());
        }
        if r.folder.trim().is_empty() || r.files.is_empty() {
            return Err("a save names its folder and lists the files it wrote".into());
        }
        if let Some(tx) = o.done.take() {
            let _ = tx.send(Ok((Delivered::Save(r), None)));
        }
        Ok(())
    }
}

/// Offer a registration job to the workers and wait for the result.
/// `cancelled` is polled while waiting; `phase` reports where it stands.
pub async fn offer(
    st: &crate::state::AppState,
    job_id: &str,
    project_id: &str,
    stations: &[(u16, String)],
    pairs: &[Pair],
    cancelled: &(dyn Fn() -> bool + Sync),
    phase: &(dyn Fn(String) + Sync),
) -> Offered {
    let mut rx = {
        let mut w = st.work.lock().unwrap();
        w.workers.retain(|_, (t, _)| t.elapsed() < WORKER_SEEN);
        if w.workers.is_empty() {
            return Offered::NoWorker;
        }
        let (tx, rx) = oneshot::channel();
        w.offers.insert(job_id.to_string(), Offer {
            project_id: project_id.to_string(),
            job: Job::Register { stations: stations.to_vec(), pairs: pairs.to_vec() },
            claimed_by: None,
            merged: None,
            done: Some(tx),
            heard: None,
        });
        rx
    };
    phase("waiting for a worker".into());
    let take_back = || st.work.lock().unwrap().offers.remove(job_id);
    let t0 = Instant::now();
    let worker = loop {
        if cancelled() {
            take_back();
            return Offered::Cancelled;
        }
        if let Some((name, _)) = st.work.lock().unwrap().offers.get(job_id).and_then(|o| o.claimed_by.clone()) {
            break name;
        }
        if t0.elapsed() > CLAIM_WAIT {
            take_back();
            return Offered::NoWorker;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    phase(format!("registering on {worker}"));
    let t1 = Instant::now();
    let got = loop {
        if cancelled() {
            take_back();
            return Offered::Cancelled;
        }
        if t1.elapsed() > RUN_LIMIT {
            take_back();
            return Offered::Failed(format!("{worker} took longer than {} minutes", RUN_LIMIT.as_secs() / 60));
        }
        match tokio::time::timeout(Duration::from_millis(250), &mut rx).await {
            Ok(r) => break r,
            Err(_) => continue,
        }
    };
    take_back();
    match got {
        Ok(Ok((Delivered::Register(res), Some(merged)))) => Offered::Done(res, merged, worker),
        Ok(Ok(_)) => Offered::Failed(format!("{worker} sent the wrong kind of result")),
        Ok(Err(e)) => Offered::Failed(format!("{worker}: {e}")),
        Err(_) => Offered::Failed(format!("{worker} went away")),
    }
}

/// How a check job offered to the workers ended.
pub enum CheckOffered {
    /// What each face's look found, its fit when it fitted, and the worker.
    Done(Vec<(crate::ask::Check, Option<crate::analyze::FaceFit>)>, String),
    NoWorker,
    Failed(String),
}

/// How long a claimed check may take before the bridge fits it itself.
pub const CHECK_LIMIT_RECHECK: Duration = Duration::from_secs(120);
pub const CHECK_LIMIT_ASK: Duration = Duration::from_secs(60);

/// Offer faces to the workers to fit, and wait for what they found. `body`
/// lists them (`faces`: target id, face, coverage) and the stations (number,
/// scan id, pose); `limit` is how long a claimed check may run.
pub async fn offer_check(st: &crate::state::AppState, project_id: &str, body: Value, faces: usize, limit: Duration) -> CheckOffered {
    let job_id = crate::util::new_id("check");
    let mut rx = {
        let mut w = st.work.lock().unwrap();
        w.workers.retain(|_, (t, _)| t.elapsed() < WORKER_SEEN);
        if w.workers.is_empty() {
            return CheckOffered::NoWorker;
        }
        let (tx, rx) = oneshot::channel();
        w.offers.insert(job_id.clone(), Offer { project_id: project_id.to_string(), job: Job::Check { body, faces }, claimed_by: None, merged: None, done: Some(tx), heard: None });
        rx
    };
    let take_back = || st.work.lock().unwrap().offers.remove(&job_id);
    let t0 = Instant::now();
    let worker = loop {
        if let Some((name, _)) = st.work.lock().unwrap().offers.get(&job_id).and_then(|o| o.claimed_by.clone()) {
            break name;
        }
        if t0.elapsed() > CLAIM_WAIT {
            take_back();
            return CheckOffered::NoWorker;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let got = tokio::time::timeout(limit, &mut rx).await;
    take_back();
    let (r, blob) = match got {
        Err(_) => return CheckOffered::Failed(format!("{worker} took longer than {} s", limit.as_secs())),
        Ok(Err(_)) => return CheckOffered::Failed(format!("{worker} went away")),
        Ok(Ok(Err(e))) => return CheckOffered::Failed(format!("{worker}: {e}")),
        Ok(Ok(Ok((Delivered::Check(r), blob)))) => (r, blob),
        Ok(Ok(Ok(_))) => return CheckOffered::Failed(format!("{worker} sent the wrong kind of result")),
    };
    let mut fits = match blob.map(|b| unpack_fits(&b)).transpose() {
        Ok(f) => f.unwrap_or_default().into_iter(),
        Err(e) => return CheckOffered::Failed(format!("{worker}: {e}")),
    };
    let mut out = Vec::new();
    for c in r.checks {
        let fit = if c.fitted {
            match fits.next() {
                Some(f) => Some(f),
                None => return CheckOffered::Failed(format!("{worker}: fewer fits than faces that fitted")),
            }
        } else {
            None
        };
        out.push((crate::ask::Check { seen_share: c.seen_share, length_m: c.length_m, stations: c.stations, fit: c.fit }, fit));
    }
    if fits.next().is_some() {
        return CheckOffered::Failed(format!("{worker}: more fits than faces that fitted"));
    }
    CheckOffered::Done(out, worker)
}

/// How a save offered to the workers ended.
pub enum SaveOffered {
    /// What the worker saved, and its name.
    Done(SaveResult, String),
    Failed(String),
    Cancelled,
}

/// A worker that took a save and has said nothing for this long loses it,
/// and the save goes back on offer.
pub const SAVE_QUIET: Duration = Duration::from_secs(300);

/// A save on offer, to wait on with `offer_save`.
pub struct SaveOffer(oneshot::Receiver<Delivery>);

/// Put a save on offer now, so the next claim can take it.
pub fn put_save(st: &crate::state::AppState, job_id: &str, project_id: &str) -> SaveOffer {
    let (tx, rx) = oneshot::channel();
    st.work.lock().unwrap().offers.insert(job_id.to_string(), Offer { project_id: project_id.to_string(), job: Job::Save, claimed_by: None, merged: None, done: Some(tx), heard: None });
    SaveOffer(rx)
}

/// Wait, however long, for a worker to do the save. `on_worker` is told
/// when a worker takes it (`Some(name)`) and when it goes back on offer
/// (`None`).
pub async fn offer_save(
    st: &crate::state::AppState,
    offer: SaveOffer,
    job_id: &str,
    project_id: &str,
    quiet: Duration,
    cancelled: &(dyn Fn() -> bool + Sync),
    on_worker: &(dyn Fn(Option<&str>) + Sync),
) -> SaveOffered {
    let mut rx = offer.0;
    let take_back = || st.work.lock().unwrap().offers.remove(job_id);
    let mut holder: Option<String> = None;
    loop {
        if cancelled() {
            take_back();
            return SaveOffered::Cancelled;
        }
        let now = st.work.lock().unwrap().offers.get(job_id).and_then(|o| o.claimed_by.clone().map(|c| (c.0, o.heard)));
        match (&holder, now) {
            (None, Some((w, _))) => {
                on_worker(Some(&w));
                holder = Some(w);
            }
            (Some(w), Some((_, Some(heard)))) if heard.elapsed() > quiet => {
                if let Some(o) = st.work.lock().unwrap().offers.get_mut(job_id) {
                    (o.claimed_by, o.heard) = (None, None);
                }
                st.log("check", format!("The save on {w} went quiet for {} s; it is offered again", quiet.as_secs()), "bridge", Some(project_id), &[("job_id", job_id)]);
                on_worker(None);
                holder = None;
            }
            _ => {}
        }
        match tokio::time::timeout(Duration::from_millis(250), &mut rx).await {
            Err(_) => continue,
            Ok(got) => {
                // Named from the offer: a worker may answer before this loop saw it claim.
                let w = take_back().and_then(|o| o.claimed_by).map(|c| c.0).or(holder).unwrap_or_else(|| "the worker".into());
                return match got {
                    Ok(Ok((Delivered::Save(r), _))) => SaveOffered::Done(r, w),
                    Ok(Ok(_)) => SaveOffered::Failed(format!("{w} sent the wrong kind of result")),
                    Ok(Err(e)) => SaveOffered::Failed(format!("{w}: {e}")),
                    Err(_) => SaveOffered::Failed(format!("{w} went away")),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::register::Merged;
    use crate::testkit::{Bridge, ADMIN, AGENT, OPERATOR, VIEWER};
    use crate::worker::tests::{wait_job, PID, SCAN0};
    use axum::http::StatusCode;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn offer_of(stations: &[(u16, &str)]) -> Offer {
        Offer {
            project_id: "p1".into(),
            job: Job::Register { stations: stations.iter().map(|(n, s)| (*n, s.to_string())).collect(), pairs: vec![] },
            claimed_by: None,
            merged: None,
            done: None,
            heard: None,
        }
    }

    fn result(n: usize, skipped: Vec<(u16, String)>) -> WorkResult {
        WorkResult {
            pairs: vec![],
            poses: vec![Some(Pose { turn_deg: 0.0, xyz_m: [0.0; 3] }); n],
            audits: vec![Audit::default(); n],
            walls: vec![],
            skipped,
            frame: None,
        }
    }

    fn merged_bytes() -> Vec<u8> {
        Merged::default().to_bytes()
    }

    #[test]
    fn a_claim_notes_the_worker_and_hands_out_the_oldest_unclaimed_job_once() {
        let mut w = WorkState::default();
        assert!(w.claim("laptop", "agent").is_none());
        assert_eq!(w.workers().len(), 1);
        w.offers.insert("job_1".into(), offer_of(&[(1, "s1"), (2, "s2")]));
        let job = w.claim("laptop", "agent").unwrap();
        assert_eq!(job["job_id"], "job_1");
        assert_eq!(job["kind"], "register");
        assert_eq!(job["stations"], json!([{"station": 1, "scan_id": "s1"}, {"station": 2, "scan_id": "s2"}]));
        // Claimed: nobody else gets it.
        assert!(w.claim("desk", "operator").is_none());
        let ws = w.workers();
        assert_eq!(ws.len(), 2);
        assert_eq!((ws[0]["worker"].as_str(), ws[0]["job_id"].as_str(), ws[0]["token"].as_str()), (Some("desk"), None, Some("operator")));
        assert_eq!((ws[1]["worker"].as_str(), ws[1]["job_id"].as_str()), (Some("laptop"), Some("job_1")));
        // A worker not seen for a while drops off the list.
        w.workers.get_mut("desk").unwrap().0 = Instant::now() - WORKER_SEEN;
        assert_eq!(w.workers().len(), 1);
    }

    #[test]
    fn only_the_claimer_may_report_on_a_job() {
        let mut w = WorkState::default();
        assert!(w.holds("job_1", "agent").unwrap_err().contains("no such job"));
        w.offers.insert("job_1".into(), offer_of(&[(1, "s1")]));
        assert!(w.holds("job_1", "agent").unwrap_err().contains("not claimed with your token"));
        w.claim("laptop", "agent").unwrap();
        assert!(w.holds("job_1", "agent").is_ok());
        assert!(w.holds("job_1", "operator").unwrap_err().contains("not claimed with your token"));
        assert!(w.put_merged("job_1", "operator", vec![]).is_err());
        assert!(w.deliver("job_1", "operator", Err("x".into())).is_err());
    }

    #[test]
    fn a_result_must_cover_the_stations_and_follow_the_merged_points() {
        let mut w = WorkState::default();
        let (tx, mut rx) = oneshot::channel();
        w.offers.insert("job_1".into(), Offer { done: Some(tx), ..offer_of(&[(1, "s1"), (2, "s2"), (3, "s3")]) });
        w.claim("laptop", "agent").unwrap();
        // Three stations, one skipped: two poses and audits.
        assert!(w.deliver("job_1", "agent", Ok(result(3, vec![(2, "bad".into())]))).unwrap_err().contains("the 2 stations"));
        assert!(w.deliver("job_1", "agent", Ok(result(2, vec![(2, "bad".into())]))).unwrap_err().contains("PUT …/merged"));
        assert!(rx.try_recv().is_err(), "nothing delivered yet");
        w.put_merged("job_1", "agent", merged_bytes()).unwrap();
        w.deliver("job_1", "agent", Ok(result(2, vec![(2, "bad".into())]))).unwrap();
        let (Delivered::Register(res), Some(bytes)) = rx.try_recv().unwrap().unwrap() else { panic!("a registration result with its points") };
        assert_eq!((res.poses.len(), bytes), (2, merged_bytes()));
        // Delivered once: a second result has no one to go to, but is accepted.
        w.put_merged("job_1", "agent", merged_bytes()).unwrap();
        assert!(w.deliver("job_1", "agent", Ok(result(2, vec![(2, "bad".into())]))).is_ok());
    }

    fn check_offer(faces: usize) -> Offer {
        Offer { project_id: "p1".into(), job: Job::Check { body: json!({"faces": [], "stations": []}), faces }, claimed_by: None, merged: None, done: None, heard: None }
    }

    fn out(id: &str, fitted: bool) -> CheckOut {
        CheckOut { target_id: id.into(), seen_share: 0.9, length_m: 8.0, stations: vec![1], fit: None, fitted }
    }

    #[test]
    fn registrations_go_out_before_checks() {
        let mut w = WorkState::default();
        w.offers.insert("check_a".into(), check_offer(1));
        w.offers.insert("job_z".into(), offer_of(&[(1, "s1")]));
        let first = w.claim("laptop", "agent").unwrap();
        assert_eq!((first["kind"].as_str(), first["job_id"].as_str()), (Some("register"), Some("job_z")));
        let second = w.claim("laptop", "agent").unwrap();
        assert_eq!((second["kind"].as_str(), second["project_id"].as_str()), (Some("check"), Some("p1")));
        assert_eq!((w.kind("check_a"), w.kind("job_z"), w.kind("nope")), (Some("check"), Some("register"), None));
    }

    #[test]
    fn a_check_lists_every_face_and_uploads_its_fits_first() {
        let mut w = WorkState::default();
        let (tx, mut rx) = oneshot::channel();
        w.offers.insert("check_1".into(), Offer { done: Some(tx), ..check_offer(2) });
        w.claim("laptop", "agent").unwrap();
        let res = |checks| CheckResult { checks };
        assert!(w.deliver_check("check_1", "agent", res(vec![out("a", false)])).unwrap_err().contains("the 2 faces"));
        assert!(w.deliver_check("check_1", "agent", res(vec![out("a", true), out("b", false)])).unwrap_err().contains("PUT …/merged"));
        assert!(w.deliver("check_1", "agent", Ok(result(0, vec![]))).unwrap_err().contains("send its checks"));
        assert!(w.deliver_check("check_1", "someone-else", res(vec![out("a", false), out("b", false)])).unwrap_err().contains("not claimed with your token"));
        assert!(rx.try_recv().is_err());
        w.deliver_check("check_1", "agent", res(vec![out("a", false), out("b", false)])).unwrap();
        let (Delivered::Check(r), None) = rx.try_recv().unwrap().unwrap() else { panic!("a check result, no fits") };
        assert_eq!(r.checks.len(), 2);
        // The other way round: a registration job refuses checks.
        w.offers.insert("job_2".into(), offer_of(&[(1, "s1")]));
        w.claim("laptop", "agent").unwrap();
        assert!(w.deliver_check("job_2", "agent", res(vec![])).unwrap_err().contains("registration job"));
    }

    #[test]
    fn fits_pack_and_unpack_and_refuse_a_short_blob() {
        assert!(unpack_fits(&pack_fits(&[])).unwrap().is_empty());
        let packed = pack_fits(&[b"not a fit".to_vec()]);
        assert!(unpack_fits(&packed).err().unwrap().contains("not a deflated fit"));
        assert!(unpack_fits(&packed[..6]).err().unwrap().contains("cut short"));
        let mut extra = pack_fits(&[]);
        extra.push(0);
        assert!(unpack_fits(&extra).err().unwrap().contains("don't add up"));
    }

    #[tokio::test]
    async fn a_check_with_no_worker_seen_is_not_offered() {
        let b = Bridge::new().await;
        assert!(matches!(offer_check(&b.st, "p1", json!({}), 1, Duration::from_secs(1)).await, CheckOffered::NoWorker));
    }

    #[tokio::test]
    async fn a_check_nobody_claims_comes_back_within_ten_seconds() {
        let b = Bridge::new().await;
        b.st.work.lock().unwrap().claim("laptop", "agent"); // seen, but it never claims the check
        let t0 = Instant::now();
        assert!(matches!(offer_check(&b.st, "p1", json!({}), 1, Duration::from_secs(60)).await, CheckOffered::NoWorker));
        let s = t0.elapsed().as_secs_f64();
        assert!((9.5..12.0).contains(&s), "{s} s");
        assert!(b.st.work.lock().unwrap().offers.is_empty(), "taken back");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_that_claims_and_goes_silent_loses_the_check_and_its_late_result_is_refused() {
        let b = Bridge::new().await;
        b.st.work.lock().unwrap().claim("laptop", "agent");
        let st = b.st.clone();
        let offered = tokio::spawn(async move { offer_check(&st, "p1", json!({"faces": [], "stations": []}), 1, Duration::from_millis(400)).await });
        let job = loop {
            if let Some(j) = b.st.work.lock().unwrap().claim("laptop", "agent") {
                break j;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let CheckOffered::Failed(why) = offered.await.unwrap() else { panic!("taken back") };
        assert!(why.contains("took longer than"), "{why}");
        let id = job["job_id"].as_str().unwrap();
        let (code, body) = b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(json!({"checks": [serde_json::to_value(out("a", false)).unwrap()]}))).await;
        assert_eq!(code, StatusCode::CONFLICT, "{body}");
    }

    #[test]
    fn a_failure_is_delivered_as_the_workers_error() {
        let mut w = WorkState::default();
        let (tx, mut rx) = oneshot::channel();
        w.offers.insert("job_1".into(), Offer { done: Some(tx), ..offer_of(&[(1, "s1")]) });
        w.claim("laptop", "agent").unwrap();
        w.deliver("job_1", "agent", Err("out of memory".into())).unwrap();
        assert_eq!(rx.try_recv().unwrap().err().as_deref(), Some("out of memory"));
    }

    fn state() -> crate::state::AppState {
        crate::state::AppState::new(
            x9_client::Client::connect("http://127.0.0.1:1/api/v1").unwrap(),
            Default::default(),
            Default::default(),
            Duration::from_millis(20),
            std::env::temp_dir(),
            std::env::temp_dir(),
        )
    }

    /// Run `offer` for one station, with `act` playing the worker once the
    /// job is offered; returns the outcome and the phases reported.
    async fn offered(st: &crate::state::AppState, cancel: &AtomicBool, act: impl FnOnce(&mut WorkState)) -> (Offered, Vec<String>) {
        let phases = std::sync::Mutex::new(Vec::new());
        let stations = [(1u16, "s1".to_string())];
        let cancelled = || cancel.load(Ordering::SeqCst);
        let phase = |p: String| phases.lock().unwrap().push(p);
        let fut = offer(st, "job_1", "p1", &stations, &[], &cancelled, &phase);
        let worker = async {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut w = st.work.lock().unwrap();
                if w.offers.contains_key("job_1") {
                    act(&mut w);
                    return;
                }
            }
        };
        let (o, _) = tokio::join!(fut, worker);
        (o, phases.into_inner().unwrap())
    }

    #[tokio::test]
    async fn with_no_worker_seen_the_job_is_not_offered() {
        let st = state();
        let stations = [(1u16, "s1".to_string())];
        let o = offer(&st, "job_1", "p1", &stations, &[], &|| false, &|_| panic!("no phase without a worker")).await;
        assert!(matches!(o, Offered::NoWorker));
        // A worker last seen too long ago does not count, and is forgotten.
        st.work.lock().unwrap().workers.insert("old".into(), (Instant::now() - WORKER_SEEN, "agent".into()));
        let o = offer(&st, "job_1", "p1", &stations, &[], &|| false, &|_| {}).await;
        assert!(matches!(o, Offered::NoWorker));
        assert!(st.work.lock().unwrap().workers.is_empty());
    }

    #[tokio::test]
    async fn a_claimed_job_comes_back_done_with_the_workers_name() {
        let st = state();
        st.work.lock().unwrap().claim("laptop", "agent");
        let cancel = AtomicBool::new(false);
        let (o, phases) = offered(&st, &cancel, |w| {
            w.claim("laptop", "agent").unwrap();
            w.put_merged("job_1", "agent", merged_bytes()).unwrap();
            w.deliver("job_1", "agent", Ok(result(1, vec![]))).unwrap();
        })
        .await;
        let Offered::Done(res, bytes, name) = o else { panic!("not done") };
        assert_eq!((res.poses.len(), bytes, name.as_str()), (1, merged_bytes(), "laptop"));
        assert_eq!(phases, ["waiting for a worker", "registering on laptop"]);
        // Taken off the offers once done.
        assert!(st.work.lock().unwrap().offers.is_empty());
    }

    #[tokio::test]
    async fn a_workers_failure_comes_back_for_the_bridge_to_take_over() {
        let st = state();
        st.work.lock().unwrap().claim("laptop", "agent");
        let cancel = AtomicBool::new(false);
        let (o, _) = offered(&st, &cancel, |w| {
            w.claim("laptop", "agent").unwrap();
            w.deliver("job_1", "agent", Err("decoder crashed".into())).unwrap();
        })
        .await;
        let Offered::Failed(why) = o else { panic!("not failed") };
        assert_eq!(why, "laptop: decoder crashed");
        assert!(st.work.lock().unwrap().offers.is_empty());
    }

    #[tokio::test]
    async fn a_cancel_takes_the_job_back_before_or_after_the_claim() {
        let st = state();
        st.work.lock().unwrap().claim("laptop", "agent");
        // Before anyone claims it.
        let cancel = AtomicBool::new(false);
        let (o, _) = offered(&st, &cancel, |_| cancel.store(true, Ordering::SeqCst)).await;
        assert!(matches!(o, Offered::Cancelled));
        assert!(st.work.lock().unwrap().offers.is_empty());
        // While the worker has it (cancelled once it is registering): its
        // later result finds no job.
        let phases = std::sync::Mutex::new(Vec::<String>::new());
        let cancelled = || phases.lock().unwrap().iter().any(|p| p.starts_with("registering"));
        let phase = |p: String| phases.lock().unwrap().push(p);
        let stations = [(1u16, "s1".to_string())];
        let worker = async {
            while st.work.lock().unwrap().claim("laptop", "agent").is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let (o, _) = tokio::join!(offer(&st, "job_1", "p1", &stations, &[], &cancelled, &phase), worker);
        assert!(matches!(o, Offered::Cancelled));
        assert_eq!(phases.lock().unwrap().last().map(String::as_str), Some("registering on laptop"));
        let err = st.work.lock().unwrap().deliver("job_1", "agent", Err("late".into())).unwrap_err();
        assert!(err.contains("the bridge took it back"), "{err}");
    }

    #[tokio::test]
    async fn a_worker_that_goes_quiet_after_the_claim_is_a_failure() {
        let st = state();
        st.work.lock().unwrap().claim("laptop", "agent");
        let cancel = AtomicBool::new(false);
        // The sender is dropped without a delivery (the offer is replaced).
        let (o, _) = offered(&st, &cancel, |w| {
            w.claim("laptop", "agent").unwrap();
            w.offers.get_mut("job_1").unwrap().done = None;
        })
        .await;
        let Offered::Failed(why) = o else { panic!("not failed") };
        assert_eq!(why, "laptop went away");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_seen_but_not_claiming_leaves_the_job_to_the_bridge() {
        // CLAIM_WAIT is 10 s: the slowest test here, and the only way to see it.
        let st = state();
        st.work.lock().unwrap().claim("laptop", "agent");
        let stations = [(1u16, "s1".to_string())];
        let t0 = Instant::now();
        let o = offer(&st, "job_1", "p1", &stations, &[], &|| false, &|_| {}).await;
        assert!(matches!(o, Offered::NoWorker));
        assert!(t0.elapsed() >= CLAIM_WAIT);
        assert!(st.work.lock().unwrap().offers.is_empty());
    }

    // Through the bridge's routes, as `x9-bridge worker` sees it.

    async fn claim(b: &Bridge, who: &str) -> (StatusCode, Value) {
        b.call("POST", "/v1/work/claim", Some(who), Some(json!({"worker": "e2e-laptop"}))).await
    }

    async fn put_merged(b: &Bridge, job: &str, who: &str, bytes: Vec<u8>) -> StatusCode {
        let req = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/v1/work/{job}/merged"))
            .header("authorization", format!("Bearer {who}"))
            .body(axum::body::Body::from(bytes))
            .unwrap();
        use tower::ServiceExt;
        b.app.clone().oneshot(req).await.unwrap().status()
    }

    /// Offer the project's registration to a worker that has said hello, and
    /// claim it; returns the claimed job.
    async fn claimed(b: &Bridge) -> Value {
        assert_eq!(claim(b, AGENT).await.0, StatusCode::NO_CONTENT);
        let (code, job) = b.call("POST", &format!("/v1/projects/{PID}/registration"), Some(OPERATOR), None).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{job}");
        let t0 = Instant::now();
        loop {
            let (code, got) = claim(b, AGENT).await;
            if code == StatusCode::OK {
                assert_eq!(got["job_id"], job["job_id"]);
                return got;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "never offered");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_claims_reports_and_delivers_a_registration() {
        let b = Bridge::new().await;
        // Viewers can't work, and names are checked.
        assert_eq!(claim(&b, VIEWER).await.0, StatusCode::FORBIDDEN);
        let (code, _) = b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "no spaces"}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let job = claimed(&b).await;
        let id = job["job_id"].as_str().unwrap();
        assert_eq!(job["stations"], json!([{"station": 1, "scan_id": SCAN0}]));
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text == "Worker e2e-laptop took the registration job"));
        let (_, workers) = b.call("GET", "/v1/workers", Some(OPERATOR), None).await;
        assert_eq!(workers[0]["job_id"], id, "{workers}");

        // Progress shows on the job; only from the claimer.
        let p = json!({"phase": "matching on e2e-laptop", "progress": 1.7});
        let (code, _) = b.call("POST", &format!("/v1/work/{id}/progress"), Some(AGENT), Some(p.clone())).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let (_, j) = b.call("GET", &format!("/v1/jobs/{id}"), Some(OPERATOR), None).await;
        assert_eq!((j["phase"].as_str(), j["progress"].as_f64()), (Some("matching on e2e-laptop"), Some(1.0)), "{j}");
        let (code, _) = b.call("POST", &format!("/v1/work/{id}/progress"), Some(ADMIN), Some(p)).await;
        assert_eq!(code, StatusCode::CONFLICT);

        // The merged points are checked; the result needs them first.
        let r = serde_json::to_value(result(1, vec![])).unwrap();
        let (code, b2) = b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(r.clone())).await;
        assert_eq!(code, StatusCode::CONFLICT, "{b2}");
        assert_eq!(put_merged(&b, id, AGENT, b"junk".to_vec()).await, StatusCode::BAD_REQUEST);
        assert_eq!(put_merged(&b, id, ADMIN, merged_bytes()).await, StatusCode::CONFLICT);
        assert_eq!(put_merged(&b, id, AGENT, merged_bytes()).await, StatusCode::NO_CONTENT);
        let (code, b2) = b.call("POST", &format!("/v1/work/{id}/result"), Some(ADMIN), Some(r.clone())).await;
        assert_eq!(code, StatusCode::CONFLICT, "{b2}");
        assert!(b2["detail"].as_str().unwrap().contains("not claimed"), "{b2}");
        let (code, _) = b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(r)).await;
        assert_eq!(code, StatusCode::NO_CONTENT);

        let j = wait_job(&b, id, 5.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!((j["result"]["stations"].as_u64(), j["result"]["placed"].as_u64()), (Some(1), Some(1)), "{j}");
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text.starts_with("Registration (registered on e2e-laptop)")));
        assert!(b.st.merged_path(PID).unwrap().exists());
        // Finished: the offer is gone.
        let (code, b2) = b.call("POST", &format!("/v1/work/{id}/fail"), Some(AGENT), Some(json!({"error": "late"}))).await;
        assert_eq!(code, StatusCode::CONFLICT, "{b2}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_that_fails_hands_the_registration_back_to_the_bridge() {
        let b = Bridge::new().await;
        let job = claimed(&b).await;
        let id = job["job_id"].as_str().unwrap();
        let (code, _) = b.call("POST", &format!("/v1/work/{id}/fail"), Some(AGENT), Some(json!({"error": "out of memory"}))).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let j = wait_job(&b, id, 5.0).await;
        // The bridge registered it itself (the mock has no scan file: skipped).
        assert_eq!(j["state"], "succeeded", "{j}");
        let log = b.st.inner.lock().unwrap().log.clone();
        assert!(log.iter().any(|e| e.text == "Registration came back to the bridge: e2e-laptop: out of memory"));
        assert!(log.iter().any(|e| e.text.starts_with("Registration: ")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_workers_merged_points_that_cannot_be_stored_fail_the_job() {
        let b = Bridge::new().await;
        std::fs::write(b.dir.join("data/registration"), b"in the way").unwrap();
        let job = claimed(&b).await;
        let id = job["job_id"].as_str().unwrap();
        assert_eq!(put_merged(&b, id, AGENT, merged_bytes()).await, StatusCode::NO_CONTENT);
        let r = serde_json::to_value(result(1, vec![])).unwrap();
        b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(r)).await;
        let j = wait_job(&b, id, 5.0).await;
        assert_eq!(j["problem"]["code"], "cache_error", "{j}");
        assert!(j["problem"]["detail"].as_str().unwrap().starts_with("registration directory"), "{j}");
    }

    // ------------------------------------------------- part D: held files, saves ---

    fn save_offer(project: &str) -> Offer {
        Offer { project_id: project.into(), job: Job::Save, claimed_by: None, merged: None, done: None, heard: None }
    }

    #[test]
    fn held_files_count_while_their_worker_is_seen() {
        let mut w = WorkState::default();
        w.claim("laptop", "agent");
        w.note_holds("laptop", HashSet::from(["s1".to_string()]));
        w.note_holds("never-claimed", HashSet::from(["s2".to_string()]));
        assert_eq!(w.held(), HashSet::from(["s1".to_string()]));
        w.workers.get_mut("laptop").unwrap().0 = Instant::now().checked_sub(WORKER_SEEN + Duration::from_secs(1)).unwrap();
        assert!(w.held().is_empty(), "a laptop gone quiet may have left with its files");
    }

    #[test]
    fn saves_come_after_other_work_one_per_project_and_not_while_it_registers() {
        let mut w = WorkState::default();
        w.offers.insert("a-save".into(), save_offer("p1"));
        w.offers.insert("b-save".into(), save_offer("p1"));
        w.offers.insert("c-save".into(), save_offer("p2"));
        w.offers.insert("z-reg".into(), offer_of(&[(1, "s1")]));
        assert_eq!(w.claim("k", "agent").unwrap()["kind"], "register");
        let registering = HashSet::from(["p2".to_string()]);
        let j = w.claim_with("l", "agent", &registering).unwrap();
        assert_eq!((j["job_id"].as_str(), j["kind"].as_str(), j["project_id"].as_str()), (Some("a-save"), Some("save"), Some("p1")));
        assert_eq!(w.workers().iter().find(|v| v["worker"] == "l").unwrap()["job_kind"], "save");
        // p1 has a save running (one writer per folder); p2 is registering.
        assert!(w.claim_with("m", "agent", &registering).is_none());
        assert_eq!(w.claim("m", "agent").unwrap()["job_id"], "c-save");
        // A save takes a save's answer only, naming its folder and files.
        let empty = SaveResult { folder: " ".into(), files: vec![], stations: vec![] };
        assert_eq!(w.deliver_save("a-save", "agent", empty).unwrap_err(), "a save names its folder and lists the files it wrote");
        assert_eq!(w.deliver("a-save", "agent", Ok(result(0, vec![]))).unwrap_err(), "this is a save job: send what it saved");
        assert_eq!(w.deliver_save("z-reg", "agent", saved("/x")).unwrap_err(), "this is not a save job");
        assert!(w.deliver_save("a-save", "agent", saved("/x")).is_ok());
    }

    fn saved(folder: &str) -> SaveResult {
        SaveResult {
            folder: folder.into(),
            files: vec![SavedFile { path: "manifest.json".into(), bytes: 2, sha256: "00".into() }],
            stations: vec![SavedStation { station: 1, scan_id: SCAN0.into(), copc: Some("stations/station-01.copc.laz".into()), point_count: Some(9), kept: false, note: None }],
        }
    }

    fn registered(b: &Bridge) {
        use crate::register::{Registration, StationPose};
        let reg = Registration {
            project_id: PID.into(), frame_scan_id: Some(SCAN0.into()), pairs: vec![], walls: vec![], summary: String::new(), updated_at: String::new(),
            stations: vec![StationPose { scan_id: SCAN0.into(), station: 1, pose: Some(Pose { turn_deg: 0.0, xyz_m: [0.0; 3] }), audit: None }],
        };
        b.st.inner.lock().unwrap().registrations.insert(PID.into(), reg);
    }

    fn logged(b: &Bridge, needle: &str) -> bool {
        b.st.inner.lock().unwrap().log.iter().any(|e| e.text.contains(needle))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_waits_for_a_laptop_and_the_bridge_keeps_what_it_saved() {
        let b = Bridge::new().await;
        let path = format!("/v1/projects/{PID}/save");
        let (code, e) = b.call("POST", &path, Some(AGENT), None).await;
        assert_eq!(code, StatusCode::CONFLICT, "{e}");
        assert_eq!(b.call("POST", &path, Some(VIEWER), None).await.0, StatusCode::FORBIDDEN);
        registered(&b);
        let (code, job) = b.call("POST", &path, Some(AGENT), None).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{job}");
        assert_eq!((job["kind"].as_str(), job["state"].as_str(), job["phase"].as_str()), (Some("save"), Some("running"), Some("waiting for the laptop")));
        assert!(logged(&b, "Saving the project to the laptop (asked). No laptop is connected: it is saved when one connects."));
        let id = job["job_id"].as_str().unwrap();
        // Asking again while it waits is the same save.
        assert_eq!(b.call("POST", &path, Some(AGENT), None).await.1["job_id"], id);
        let (_, status) = b.call("GET", &path, Some(VIEWER), None).await;
        assert_eq!((status["job"]["job_id"].as_str(), status["last"].is_null()), (Some(id), true));
        // A laptop connects, says what it holds, and takes the save with what it needs.
        let (code, body) = b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "laptop", "holds": [SCAN0, "../escape"]}))).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!((body["job_id"].as_str(), body["kind"].as_str()), (Some(id), Some("save")));
        assert_eq!(body["stations"][0]["scan_id"], SCAN0);
        assert_eq!(body["stations"][0]["trusted"], true);
        assert_eq!(body["frame_station"], 1);
        assert_eq!(body["registration"]["project_id"], PID);
        assert!(!body["folder"].as_str().unwrap().contains('/'), "{body}");
        assert_eq!(b.st.work.lock().unwrap().held(), HashSet::from([SCAN0.to_string()]));
        assert!(logged(&b, "Worker laptop took the save"));
        b.wait_for(&format!("/v1/jobs/{id}"), OPERATOR, 5.0, |j| j["phase"] == "saving on laptop").await;
        // It uploads nothing, and its answer must name the folder and files.
        assert_eq!(put_merged(&b, id, AGENT, merged_bytes()).await, StatusCode::CONFLICT);
        let bad = json!({"folder": "", "files": [], "stations": []});
        assert_eq!(b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(bad)).await.0, StatusCode::CONFLICT);
        let r = serde_json::to_value(saved("/Users/crew/X9 Projects/yard-p1")).unwrap();
        assert_eq!(b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(r.clone())).await.0, StatusCode::NO_CONTENT);
        let j = wait_job(&b, id, 5.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!((j["result"]["worker"].as_str(), j["result"]["files"].as_u64()), (Some("laptop"), Some(1)));
        assert!(logged(&b, "Saved the project on laptop: /Users/crew/X9 Projects/yard-p1 (1 of 1 stations in the project frame, 0.0 GB)"));
        let (_, status) = b.call("GET", &path, Some(VIEWER), None).await;
        assert_eq!((status["last"]["folder"].as_str(), status["job"].is_null()), (Some("/Users/crew/X9 Projects/yard-p1"), true));
        // A late answer is refused; the record outlives a restart.
        assert_eq!(b.call("POST", &format!("/v1/work/{id}/result"), Some(AGENT), Some(r)).await.0, StatusCode::CONFLICT);
        b.st.inner.lock().unwrap().saves.clear();
        assert_eq!(b.st.load_saves(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_that_fails_or_is_cancelled_says_so_and_leaves_nothing_on_offer() {
        let b = Bridge::new().await;
        registered(&b);
        let path = format!("/v1/projects/{PID}/save");
        let id = b.call("POST", &path, Some(AGENT), None).await.1["job_id"].as_str().unwrap().to_string();
        assert_eq!(b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "laptop"}))).await.1["job_id"], id.as_str());
        b.call("POST", &format!("/v1/work/{id}/fail"), Some(AGENT), Some(json!({"error": "the disk is full"}))).await;
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!((j["state"].as_str(), j["problem"]["code"].as_str()), (Some("failed"), Some("save_failed")), "{j}");
        assert!(logged(&b, "The save failed (laptop: the disk is full). Nothing on the Pi was dropped for it; ask for the save again once fixed."));
        // Cancelled while it waits for a laptop.
        let id = b.call("POST", &path, Some(AGENT), None).await.1["job_id"].as_str().unwrap().to_string();
        b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(OPERATOR), None).await;
        assert_eq!(wait_job(&b, &id, 5.0).await["state"], "cancelled");
        assert!(b.st.work.lock().unwrap().offers.is_empty());
        assert_eq!(b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "laptop"}))).await.0, StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_whose_laptop_goes_quiet_is_offered_again() {
        let b = Bridge::new().await;
        let st = b.st.clone();
        let task = tokio::spawn(async move {
            let took = std::sync::Mutex::new(Vec::new());
            let r = offer_save(&st, put_save(&st, "job_s", PID), "job_s", PID, Duration::from_secs(1), &|| false, &|w| took.lock().unwrap().push(w.map(str::to_string))).await;
            (r, took.into_inner().unwrap())
        });
        let first = loop {
            if let Some(j) = b.st.work.lock().unwrap().claim("laptop", "agent") {
                break j;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(first["job_id"], "job_s");
        // Progress keeps it; silence loses it.
        tokio::time::sleep(Duration::from_millis(700)).await;
        b.st.work.lock().unwrap().holds("job_s", "agent").unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!logged(&b, "went quiet"));
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(logged(&b, "The save on laptop went quiet for 1 s; it is offered again"));
        assert_eq!(b.st.work.lock().unwrap().holds("job_s", "agent").unwrap_err(), "this job was not claimed with your token");
        assert_eq!(b.st.work.lock().unwrap().claim("desk", "operator").unwrap()["job_id"], "job_s");
        b.st.work.lock().unwrap().deliver_save("job_s", "operator", saved("/d")).unwrap();
        let (r, took) = task.await.unwrap();
        assert!(matches!(r, SaveOffered::Done(_, ref w) if w == "desk"));
        assert_eq!(took[..2], [Some("laptop".to_string()), None]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signing_off_saves_the_project_to_the_laptop() {
        let b = Bridge::new().await;
        registered(&b);
        let m: crate::mission::Mission = serde_json::from_value(json!({
            "mission_id": "m1", "project_id": PID, "goal": "Measure the yard", "kind": "custom", "done_when": "measured",
            "state": "ready", "created_by": "crew", "created_at": "2026-11-11T10:00:00Z", "targets": []})).unwrap();
        b.st.inner.lock().unwrap().missions.insert(PID.into(), m);
        let (code, body) = b.call("POST", &format!("/v1/projects/{PID}/mission/sign-off"), Some(OPERATOR), None).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(logged(&b, "Saving the project to the laptop (after the sign-off). No laptop is connected"));
        let (_, status) = b.call("GET", &format!("/v1/projects/{PID}/save"), Some(VIEWER), None).await;
        assert_eq!(status["job"]["phase"], "waiting for the laptop");
    }
}
