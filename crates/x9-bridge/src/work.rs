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

use crate::register::{Audit, Pair, Pose, Wall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
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

type Delivery = Result<(WorkResult, Vec<u8>), String>;

struct Offer {
    project_id: String,
    stations: Vec<(u16, String)>,
    pairs: Vec<Pair>,
    /// (worker name, the caller who claimed it).
    claimed_by: Option<(String, String)>,
    merged: Option<Vec<u8>>,
    done: Option<oneshot::Sender<Delivery>>,
}

#[derive(Default)]
pub struct WorkState {
    /// Worker name → last claim, and the caller behind it.
    workers: HashMap<String, (Instant, String)>,
    /// Offered registration jobs, by job id.
    offers: HashMap<String, Offer>,
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
                let job = self.offers.iter().find(|(_, o)| o.claimed_by.as_ref().is_some_and(|c| &c.0 == name)).map(|(j, _)| j.clone());
                json!({"worker": name, "token": by, "seen_s_ago": (t.elapsed().as_secs_f64() * 10.0).round() / 10.0, "job_id": job})
            })
            .collect();
        v.sort_by(|a, b| a["worker"].as_str().cmp(&b["worker"].as_str()));
        v
    }

    /// A worker asks for work: note it, and hand it the oldest unclaimed job.
    pub fn claim(&mut self, worker: &str, caller: &str) -> Option<Value> {
        self.workers.insert(worker.to_string(), (Instant::now(), caller.to_string()));
        let (job_id, o) = self.offers.iter_mut().find(|(_, o)| o.claimed_by.is_none())?;
        o.claimed_by = Some((worker.to_string(), caller.to_string()));
        Some(json!({
            "job_id": job_id,
            "kind": "register",
            "project_id": o.project_id,
            "stations": o.stations.iter().map(|(n, id)| json!({"station": n, "scan_id": id})).collect::<Vec<_>>(),
            "pairs": o.pairs,
        }))
    }

    /// The offer `job_id`, if `caller` claimed it.
    fn mine(&mut self, job_id: &str, caller: &str) -> Result<&mut Offer, String> {
        match self.offers.get_mut(job_id) {
            None => Err("no such job is offered (it finished, was cancelled, or the bridge took it back)".into()),
            Some(o) if o.claimed_by.as_ref().is_none_or(|c| c.1 != caller) => Err("this job was not claimed with your token".into()),
            Some(o) => Ok(o),
        }
    }

    /// Whether `caller` holds the offered job (for progress reports).
    pub fn holds(&mut self, job_id: &str, caller: &str) -> Result<(), String> {
        self.mine(job_id, caller).map(|_| ())
    }

    /// Keep the worker's merged points (already checked) for its result.
    pub fn put_merged(&mut self, job_id: &str, caller: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.mine(job_id, caller)?.merged = Some(bytes);
        Ok(())
    }

    pub fn deliver(&mut self, job_id: &str, caller: &str, r: Result<WorkResult, String>) -> Result<(), String> {
        let o = self.mine(job_id, caller)?;
        let d = match r {
            Ok(res) => {
                let kept = o.stations.len() - res.skipped.len().min(o.stations.len());
                if res.poses.len() != kept || res.audits.len() != kept {
                    return Err(format!("poses and audits must list the {kept} stations offered, less those skipped"));
                }
                Ok((res, o.merged.take().ok_or("upload the merged points (PUT …/merged) before the result")?))
            }
            Err(e) => Err(e),
        };
        if let Some(tx) = o.done.take() {
            let _ = tx.send(d);
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
            stations: stations.to_vec(),
            pairs: pairs.to_vec(),
            claimed_by: None,
            merged: None,
            done: Some(tx),
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
        Ok(Ok((res, merged))) => Offered::Done(res, merged, worker),
        Ok(Err(e)) => Offered::Failed(format!("{worker}: {e}")),
        Err(_) => Offered::Failed(format!("{worker} went away")),
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
            stations: stations.iter().map(|(n, s)| (*n, s.to_string())).collect(),
            pairs: vec![],
            claimed_by: None,
            merged: None,
            done: None,
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
        let (res, bytes) = rx.try_recv().unwrap().unwrap();
        assert_eq!((res.poses.len(), bytes), (2, merged_bytes()));
        // Delivered once: a second result has no one to go to, but is accepted.
        w.put_merged("job_1", "agent", merged_bytes()).unwrap();
        assert!(w.deliver("job_1", "agent", Ok(result(2, vec![(2, "bad".into())]))).is_ok());
    }

    #[test]
    fn a_failure_is_delivered_as_the_workers_error() {
        let mut w = WorkState::default();
        let (tx, mut rx) = oneshot::channel();
        w.offers.insert("job_1".into(), Offer { done: Some(tx), ..offer_of(&[(1, "s1")]) });
        w.claim("laptop", "agent").unwrap();
        w.deliver("job_1", "agent", Err("out of memory".into())).unwrap();
        assert_eq!(rx.try_recv().unwrap().unwrap_err(), "out of memory");
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
}
