//! `x9-bridge worker`: do the bridge's heavy work on this computer (see
//! `work`). Claims registration jobs, fetches the scans over the bridge,
//! decodes, registers and audits them here, and sends the result back. Also
//! claims question checks: fits the offered faces on this computer's cores
//! (`ask::look_many`, as the bridge would) and sends back what it found.
//!
//! The scan files it fetches are kept in the cache directory, with each
//! station's decoded 5 cm cloud, so a check needs no transfer for a station
//! already registered here; the cache drops the least recently used project's
//! files first past `--cache-gb`. Prepared stations and pair checks are kept
//! in memory, so each new station costs about one station's work.
//!
//!   x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR] [--cache-gb N]
//!
//! Defaults: the bridge over the laptop cable (http://10.55.0.1:8795), the
//! token from `X9_WORKER_TOKEN` or ~/.config/x9-worker/token (an agent or
//! operator token), this computer's name, ~/.cache/x9-worker, and 20 GB.

use crate::register::{audit, prepare, register_all, walls, AuditCache, Cloud, Merged, Pair, Prepared};
use crate::work::WorkResult;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Worker {
    http: reqwest::Client,
    base: String,
    token: String,
    name: String,
    cache: PathBuf,
    /// The scan files' budget in the cache (bytes).
    cache_bytes: u64,
    prepared: HashMap<String, Arc<Prepared>>,
    checks: AuditCache,
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let mut base = "http://10.55.0.1:8795".to_string();
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    let mut token_file = PathBuf::from(&home).join(".config/x9-worker/token");
    let mut name = hostname();
    let mut cache = PathBuf::from(&home).join(".cache/x9-worker");
    let mut cache_gb: f64 = 20.0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--bridge" => base = val()?.trim_end_matches('/').to_string(),
            "--token-file" => token_file = PathBuf::from(val()?),
            "--name" => name = val()?.clone(),
            "--cache" => cache = PathBuf::from(val()?),
            "--cache-gb" => cache_gb = val()?.parse().ok().filter(|g: &f64| *g > 0.0).context("--cache-gb: a number of gigabytes")?,
            _ => bail!("usage: x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR] [--cache-gb N]"),
        }
    }
    let token = match std::env::var("X9_WORKER_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => std::fs::read_to_string(&token_file).with_context(|| format!("reading the token from {}", token_file.display()))?.trim().to_string(),
    };
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).build()?;
    let mut w = Worker { http, base, token, name, cache, cache_bytes: (cache_gb * 1e9) as u64, prepared: HashMap::new(), checks: AuditCache::new() };
    eprintln!("x9-bridge worker {}: {} as {}, cache {}", crate::routes::VERSION, w.base, w.name, w.cache.display());
    let mut said_down = false;
    loop {
        match w.claim().await {
            Ok(Some(job)) => {
                said_down = false;
                let id = job["job_id"].as_str().unwrap_or("").to_string();
                let t0 = std::time::Instant::now();
                let done = if job["kind"] == "check" { w.check(&job).await } else { w.register(&job).await };
                match done {
                    Ok(summary) => eprintln!("{id}: done in {:.1} s: {summary}", t0.elapsed().as_secs_f64()),
                    Err(e) => {
                        eprintln!("{id}: failed: {e:#}");
                        let _ = w.post(&format!("/v1/work/{id}/fail"), &json!({"error": format!("{e:#}")})).await;
                    }
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(2)).await,
            Err(e) => {
                if !said_down {
                    eprintln!("bridge not reachable ({e:#}); retrying");
                    said_down = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "worker".into())
}

impl Worker {
    async fn post(&self, path: &str, body: &Value) -> anyhow::Result<reqwest::Response> {
        let r = self.http.post(format!("{}{path}", self.base)).bearer_auth(&self.token).json(body).send().await?;
        if !r.status().is_success() {
            bail!("{path}: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(r)
    }

    async fn claim(&self) -> anyhow::Result<Option<Value>> {
        let r = self.post("/v1/work/claim", &json!({"worker": self.name})).await?;
        if r.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(r.json().await?))
    }

    async fn progress(&self, job: &str, phase: &str, progress: Option<f64>) {
        let _ = self.post(&format!("/v1/work/{job}/progress"), &json!({"phase": phase, "progress": progress})).await;
    }

    /// The station's scan file: from the cache, or fetched over the bridge and
    /// kept (the laptop is where the data lands: `specs/laptop-offload.md`).
    async fn tzf(&self, scan_id: &str, project_id: &str) -> anyhow::Result<PathBuf> {
        crate::files::safe_id(scan_id).map_err(|_| anyhow::anyhow!("not a scan id: {scan_id}"))?;
        let path = self.cache.join(format!("{scan_id}.tzf"));
        if path.exists() {
            // Used again: it is the newest of its project now.
            let _ = std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(std::time::SystemTime::now()));
            return Ok(path);
        }
        let r = self.http.get(format!("{}/v1/scans/{scan_id}/tzf", self.base)).bearer_auth(&self.token).timeout(Duration::from_secs(600)).send().await?;
        if !r.status().is_success() {
            bail!("fetching the scan: {}", r.status());
        }
        let part = self.cache.join(format!("{scan_id}.tzf.part"));
        tokio::fs::write(&part, r.bytes().await?).await?;
        tokio::fs::rename(&part, &path).await?;
        self.note_project(scan_id, project_id);
        self.trim(project_id);
        Ok(path)
    }

    /// The station's 5 cm cloud: from the cache, or decoded from its scan file.
    async fn cloud(&self, scan_id: &str, project_id: &str) -> anyhow::Result<Cloud> {
        let path = self.cache.join(crate::worker::cloud_name(scan_id));
        if let Ok(b) = tokio::fs::read(&path).await {
            if let Ok(c) = Cloud::from_bytes(&b) {
                return Ok(c);
            }
        }
        let tzf = self.tzf(scan_id, project_id).await?;
        let t = tzf.clone();
        let decoded = tokio::task::spawn_blocking(move || -> anyhow::Result<Cloud> {
            let f = trimble_tzf::Tzf::open(&t)?;
            let meta = f.scan_meta()?;
            let pts = f.grid_points()?;
            Ok(Cloud::from_grid(&pts, crate::view::step_rad(&meta)))
        })
        .await;
        let c = match decoded {
            Ok(Ok(c)) => c,
            other => {
                // A file that doesn't decode is no use to keep.
                let _ = tokio::fs::remove_file(&tzf).await;
                other??
            }
        };
        tokio::fs::write(&path, c.to_bytes()).await?;
        Ok(c)
    }

    /// Which project each kept scan file belongs to, for dropping by project.
    fn note_project(&self, scan_id: &str, project_id: &str) {
        let at = self.cache.join("projects.json");
        let mut m: HashMap<String, String> = std::fs::read(&at).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        m.insert(scan_id.to_string(), project_id.to_string());
        let _ = std::fs::write(&at, serde_json::to_vec(&m).unwrap_or_default());
    }

    /// Keep the scan files within the budget: drop the least recently used
    /// project's files first, never the project at work.
    fn trim(&self, working: &str) {
        let Ok(dir) = std::fs::read_dir(&self.cache) else { return };
        // (path, scan id, bytes, last used)
        type Kept = (PathBuf, String, u64, std::time::SystemTime);
        let files: Vec<Kept> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".tzf")?.to_string();
                let m = e.metadata().ok()?;
                Some((e.path(), id, m.len(), m.modified().ok()?))
            })
            .collect();
        let mut used: u64 = files.iter().map(|f| f.2).sum();
        if used <= self.cache_bytes {
            return;
        }
        let projects: HashMap<String, String> = std::fs::read(self.cache.join("projects.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut groups: HashMap<String, Vec<&Kept>> = HashMap::new();
        for f in &files {
            let p = projects.get(&f.1).cloned().unwrap_or_default();
            if p != working {
                groups.entry(p).or_default().push(f);
            }
        }
        let mut order: Vec<(std::time::SystemTime, Vec<&Kept>)> =
            groups.into_values().map(|g| (g.iter().map(|f| f.3).max().unwrap_or(std::time::UNIX_EPOCH), g)).collect();
        order.sort_by_key(|g| g.0);
        'outer: for (_, mut group) in order {
            group.sort_by_key(|f| f.3);
            for f in group {
                if used <= self.cache_bytes {
                    break 'outer;
                }
                if std::fs::remove_file(&f.0).is_ok() {
                    used -= f.2;
                }
            }
        }
    }

    /// A question check: fit the offered faces here, as the bridge would, and
    /// send back what each look found, with the fits uploaded first.
    async fn check(&mut self, job: &Value) -> anyhow::Result<String> {
        let id = job["job_id"].as_str().context("job_id")?.to_string();
        let project = job["project_id"].as_str().unwrap_or("").to_string();
        let faces: Vec<(String, crate::register::Face, Value)> = job["faces"]
            .as_array()
            .context("faces")?
            .iter()
            .map(|f| Ok((f["target_id"].as_str().context("target_id")?.to_string(), serde_json::from_value(f["face"].clone())?, f["coverage"].clone())))
            .collect::<anyhow::Result<_>>()?;
        let stations: Vec<(u16, String, crate::register::Pose)> = job["stations"]
            .as_array()
            .context("stations")?
            .iter()
            .map(|s| Ok((s["station"].as_u64().context("station")? as u16, s["scan_id"].as_str().context("scan_id")?.to_string(), serde_json::from_value(s["pose"].clone())?)))
            .collect::<anyhow::Result<_>>()?;
        let mut files = Vec::new();
        for (k, (n, scan, pose)) in stations.iter().enumerate() {
            self.progress(&id, &format!("fetching station {n} on {}", self.name), Some(k as f64 / stations.len().max(1) as f64 * 0.3)).await;
            files.push((*n, *pose, self.tzf(scan, &project).await?));
        }
        self.progress(&id, &format!("fitting {} faces on {}", faces.len(), self.name), Some(0.3)).await;
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
        let looks = tokio::task::spawn_blocking(move || crate::ask::look_many(&faces, &files, &crate::analyze::Opts::default(), threads)).await??;
        let mut fits = Vec::new();
        let checks: Vec<crate::work::CheckOut> = job["faces"]
            .as_array()
            .context("faces")?
            .iter()
            .zip(looks)
            .map(|(f, (c, fit))| {
                let fitted = fit.is_some();
                if let Some(ff) = fit {
                    fits.push(ff.to_bytes());
                }
                crate::work::CheckOut { target_id: f["target_id"].as_str().unwrap_or("").to_string(), seen_share: c.seen_share, length_m: c.length_m, stations: c.stations, fit: c.fit, fitted }
            })
            .collect();
        if !fits.is_empty() {
            let r = self.http.put(format!("{}/v1/work/{id}/merged", self.base)).bearer_auth(&self.token).body(crate::work::pack_fits(&fits)).send().await?;
            if !r.status().is_success() {
                bail!("uploading the fits: {} {}", r.status(), r.text().await.unwrap_or_default());
            }
        }
        let n = checks.len();
        let answered = checks.iter().filter(|c| c.fit.is_some()).count();
        self.post(&format!("/v1/work/{id}/result"), &serde_json::to_value(crate::work::CheckResult { checks })?).await?;
        Ok(format!("{n} faces checked, {answered} with a reading"))
    }

    async fn register(&mut self, job: &Value) -> anyhow::Result<String> {
        let id = job["job_id"].as_str().context("job_id")?.to_string();
        let stations: Vec<(u16, String)> = job["stations"]
            .as_array()
            .context("stations")?
            .iter()
            .filter_map(|s| Some((s["station"].as_u64()? as u16, s["scan_id"].as_str()?.to_string())))
            .collect();
        let pairs0: Vec<Pair> = serde_json::from_value(job["pairs"].clone()).unwrap_or_default();
        let (mut kept, mut skipped) = (Vec::new(), Vec::new());
        for (k, (n, scan)) in stations.iter().enumerate() {
            if self.prepared.contains_key(scan) {
                kept.push((*n, scan.clone()));
                continue;
            }
            self.progress(&id, &format!("decoding station {n} on {}", self.name), Some(k as f64 / stations.len().max(1) as f64 * 0.4)).await;
            match self.cloud(scan, job["project_id"].as_str().unwrap_or("")).await {
                Ok(c) => {
                    let p = tokio::task::spawn_blocking(move || prepare(&c)).await?;
                    self.prepared.insert(scan.clone(), Arc::new(p));
                    kept.push((*n, scan.clone()));
                }
                Err(e) => skipped.push((*n, format!("{e:#}"))),
            }
        }
        self.progress(&id, &format!("matching on {}", self.name), Some(0.5)).await;
        let ids: Vec<String> = kept.iter().map(|s| s.1.clone()).collect();
        let numbers: Vec<u16> = kept.iter().map(|s| s.0).collect();
        let prep: Vec<Arc<Prepared>> = ids.iter().map(|i| self.prepared[i].clone()).collect();
        let mut checks = std::mem::take(&mut self.checks);
        let (ids2, numbers2) = (ids.clone(), numbers.clone());
        let (result, merged, checks) = tokio::task::spawn_blocking(move || {
            let refs: Vec<&Prepared> = prep.iter().map(|p| p.as_ref()).collect();
            let (pairs, groups) = register_all(&ids2, &refs, pairs0, &|_, _| {});
            let poses = groups.poses();
            let frame = ids2.get(groups.frame()).cloned();
            let names: Vec<String> = numbers2.iter().map(u16::to_string).collect();
            let audits = audit(&ids2, &names, &refs, &poses, &pairs, &mut checks);
            let mut merged = Merged::default();
            for (k, p) in poses.iter().enumerate() {
                if let Some(p) = p {
                    merged.add(refs[k], p, numbers2[k]);
                }
            }
            let ws = walls(&merged);
            (WorkResult { pairs, poses, audits, walls: ws, skipped: vec![], frame }, merged.to_bytes(), checks)
        })
        .await?;
        self.checks = checks;
        let result = WorkResult { skipped, ..result };
        let r = self.http.put(format!("{}/v1/work/{id}/merged", self.base)).bearer_auth(&self.token).body(merged).send().await?;
        if !r.status().is_success() {
            bail!("uploading merged points: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        let placed = result.poses.iter().flatten().count();
        let trusted = result.audits.iter().filter(|a| a.trusted()).count();
        self.post(&format!("/v1/work/{id}/result"), &serde_json::to_value(&result)?).await?;
        Ok(format!("{placed} of {} placed, {trusted} trusted", numbers.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Bridge, AGENT, OPERATOR, VIEWER};
    use crate::worker::tests::{dead_base, junk_fixtures, room_cloud, wait_job, PID, SCAN0};

    /// Serve the bridge's routes on a local port; returns its base URL.
    async fn serve(b: &Bridge) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let app = crate::routes::router(b.st.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn cache_dir() -> PathBuf {
        let d = std::env::temp_dir().join(crate::util::new_id("x9-worker-cache"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn worker(base: &str, token: &str) -> Worker {
        Worker { http: reqwest::Client::new(), base: base.into(), token: token.into(), name: "laptop".into(), cache: cache_dir(), cache_bytes: 20_000_000_000, prepared: HashMap::new(), checks: AuditCache::new() }
    }

    /// Have the bridge offer the project's registration and claim it.
    async fn claim_registration(b: &Bridge, w: &Worker) -> (String, Value) {
        // Seen first, so the bridge offers rather than registers itself.
        assert!(w.claim().await.unwrap().is_none());
        let (_, job) = b.call("POST", &format!("/v1/projects/{PID}/registration"), Some(OPERATOR), None).await;
        let id = job["job_id"].as_str().unwrap().to_string();
        let t0 = std::time::Instant::now();
        loop {
            if let Some(j) = w.claim().await.unwrap() {
                assert_eq!(j["job_id"], id.as_str());
                return (id, j);
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "never offered");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn log_has(b: &Bridge, needle: &str) -> bool {
        b.st.inner.lock().unwrap().log.iter().any(|e| e.text.contains(needle))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_registers_a_cached_station_and_the_bridge_records_it() {
        let b = Bridge::new().await;
        let w = &mut worker(&serve(&b).await, AGENT);
        std::fs::write(w.cache.join(crate::worker::cloud_name(SCAN0)), room_cloud([1.0, 0.5, 0.0], 0.0).to_bytes()).unwrap();
        let (id, job) = claim_registration(&b, w).await;
        let summary = w.register(&job).await.unwrap();
        assert_eq!(summary, "1 of 1 placed, 1 trusted");
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!(j["result"]["placed"], 1, "{j}");
        assert!(log_has(&b, "Registration (registered on laptop)"));
        assert!(b.st.merged(PID).is_some());
        assert!(w.prepared.contains_key(SCAN0));

        // The next job reuses the prepared station: no cloud is read.
        std::fs::remove_file(w.cache.join(crate::worker::cloud_name(SCAN0))).unwrap();
        let (id, job) = claim_registration(&b, w).await;
        assert_eq!(w.register(&job).await.unwrap(), "1 of 1 placed, 1 trusted");
        assert_eq!(wait_job(&b, &id, 5.0).await["state"], "succeeded");
        std::fs::remove_dir_all(&w.cache).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_station_whose_scan_cannot_be_fetched_is_skipped() {
        // The mock has no scan file to give the bridge.
        let b = Bridge::new().await;
        let w = &mut worker(&serve(&b).await, AGENT);
        // A broken cached cloud is fetched again.
        std::fs::write(w.cache.join(crate::worker::cloud_name(SCAN0)), b"junk").unwrap();
        let (id, job) = claim_registration(&b, w).await;
        assert_eq!(w.register(&job).await.unwrap(), "0 of 0 placed, 0 trusted");
        let j = wait_job(&b, &id, 5.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert!(log_has(&b, "Station 1 could not be registered: fetching the scan: 502"));
        std::fs::remove_dir_all(&w.cache).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scan_file_that_does_not_decode_is_skipped_with_the_decoder_error() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let w = &mut worker(&serve(&b).await, AGENT);
        let (id, job) = claim_registration(&b, w).await;
        assert_eq!(w.register(&job).await.unwrap(), "0 of 0 placed, 0 trusted");
        assert_eq!(wait_job(&b, &id, 5.0).await["state"], "succeeded");
        assert!(!w.cache.join(crate::worker::cloud_name(SCAN0)).exists());
        assert!(log_has(&b, "Station 1 could not be registered"));
        std::fs::remove_dir_all(&w.cache).unwrap();
        std::fs::remove_dir_all(fx).unwrap();
    }

    /// A worker that claims and runs checks until `stop` is set.
    fn run_checks(mut w: Worker, stop: Arc<std::sync::atomic::AtomicBool>) -> tokio::task::JoinHandle<Worker> {
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match w.claim().await {
                    Ok(Some(job)) if job["kind"] == "check" => {
                        w.check(&job).await.unwrap();
                    }
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            w
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_question_checked_on_the_laptop_gets_the_answer_the_pi_gives() {
        use crate::ask::tests::{yard_bridge, PROJECT};
        let key = (PROJECT.to_string(), "east".to_string());
        // The Pi alone.
        let (pi, fx_pi) = yard_bridge().await;
        let alone = crate::ask::on_bridge(&pi.st, PROJECT, "east", "agent").await.unwrap();
        assert_eq!(alone["checked_on"], "bridge");
        let pi_kept = pi.st.inner.lock().unwrap().settling.get(&key).cloned().expect("settling");
        // The same question, with a laptop on the cable.
        let (b, fx) = yard_bridge().await;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let laptop = run_checks(worker(&serve(&b).await, AGENT), stop.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while b.st.work.lock().unwrap().workers().is_empty() {
            assert!(std::time::Instant::now() < deadline, "the worker never showed up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let on = crate::ask::on_bridge(&b.st, PROJECT, "east", "agent").await.unwrap();
        assert_eq!(on["checked_on"], "laptop", "{on}");
        for k in ["verdict", "reason", "stations", "seen_pct", "face"] {
            assert_eq!(on[k], alone[k], "{k}");
        }
        let kept = b.st.inner.lock().unwrap().settling.get(&key).cloned().expect("settling");
        assert_eq!(serde_json::to_value(&kept).unwrap(), serde_json::to_value(&pi_kept).unwrap(), "the same reading, to the last digit");
        let log: Vec<String> = b.st.inner.lock().unwrap().log.iter().map(|e| e.text.clone()).collect();
        assert!(log.iter().any(|t| t.starts_with("Asked Target east (plumb)") && t.ends_with("checked on laptop")), "{log:#?}");
        assert!(log.iter().any(|t| t == "Worker laptop took the question check"), "{log:#?}");
        // A re-check goes to the laptop too, and the evidence is drawn on the bridge.
        b.st.inner.lock().unwrap().basis.clear();
        crate::ask::recheck(&b.st, PROJECT).await;
        let log: Vec<String> = b.st.inner.lock().unwrap().log.iter().map(|e| e.text.clone()).collect();
        assert!(log.iter().any(|t| t.starts_with("Re-checked 3 questions") && t.contains("fitted on laptop")), "{log:#?}");
        // The laptop kept the scan file for next time.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let w = laptop.await.unwrap();
        assert!(std::fs::read_dir(&w.cache).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tzf")));
        std::fs::remove_dir_all(&w.cache).unwrap();
        std::fs::remove_dir_all(fx).unwrap();
        std::fs::remove_dir_all(fx_pi).unwrap();
    }

    #[test]
    fn the_cache_drops_the_least_recently_used_project_first_never_the_one_at_work() {
        let mut w = worker("http://127.0.0.1:9", AGENT);
        w.cache_bytes = 250;
        let put = |id: &str, project: &str, age: u64| {
            let path = w.cache.join(format!("{id}.tzf"));
            std::fs::write(&path, [0u8; 100]).unwrap();
            let when = std::time::SystemTime::now() - Duration::from_secs(age);
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
            w.note_project(id, project);
        };
        put("old-a", "A", 900);
        put("old-b", "B", 800);
        put("new-b", "B", 10);
        put("now", "C", 0);
        // 400 bytes for a 250 budget: A last used longest ago goes first, then B; C is at work.
        w.trim("C");
        let mut left: Vec<String> = std::fs::read_dir(&w.cache).unwrap().filter_map(|e| e.unwrap().file_name().to_string_lossy().strip_suffix(".tzf").map(str::to_string)).collect();
        left.sort();
        assert_eq!(left, ["new-b", "now"], "A, then B's oldest, until under budget");
        std::fs::remove_dir_all(&w.cache).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scan_file_that_does_not_decode_leaves_no_download_behind() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let w = &mut worker(&serve(&b).await, AGENT);
        let (_, job) = claim_registration(&b, w).await;
        w.register(&job).await.unwrap();
        // No scan file and no cloud: only the index of which project each kept file is for.
        let left: Vec<_> = std::fs::read_dir(&w.cache).unwrap().map(|e| e.unwrap().file_name()).filter(|n| n != "projects.json").collect();
        std::fs::remove_dir_all(&w.cache).unwrap();
        std::fs::remove_dir_all(fx).unwrap();
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_the_bridge_took_back_cannot_be_delivered() {
        let b = Bridge::new().await;
        let w = &mut worker(&serve(&b).await, AGENT);
        std::fs::write(w.cache.join(crate::worker::cloud_name(SCAN0)), room_cloud([1.0, 0.5, 0.0], 0.0).to_bytes()).unwrap();
        let (id, job) = claim_registration(&b, w).await;
        b.call("POST", &format!("/v1/jobs/{id}/cancel"), Some(OPERATOR), None).await;
        assert_eq!(wait_job(&b, &id, 5.0).await["state"], "cancelled");
        let e = w.register(&job).await.unwrap_err().to_string();
        assert!(e.starts_with("uploading merged points: 409"), "{e}");
        std::fs::remove_dir_all(&w.cache).unwrap();
    }

    #[tokio::test]
    async fn a_job_without_an_id_or_stations_is_refused() {
        let mut w = worker("http://127.0.0.1:1", AGENT);
        assert_eq!(w.register(&json!({})).await.unwrap_err().to_string(), "job_id");
        assert_eq!(w.register(&json!({"job_id": "job_1"})).await.unwrap_err().to_string(), "stations");
        std::fs::remove_dir_all(&w.cache).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_request_is_an_error_with_the_bridges_status() {
        let b = Bridge::new().await;
        let base = serve(&b).await;
        let w = worker(&base, VIEWER);
        let e = w.claim().await.unwrap_err().to_string();
        assert!(e.starts_with("/v1/work/claim: 403"), "{e}");
        let w = worker(&base, AGENT);
        let e = w.post("/v1/work/claim", &json!({"worker": "no spaces"})).await.unwrap_err().to_string();
        assert!(e.starts_with("/v1/work/claim: 400"), "{e}");
        // Progress on a job it doesn't hold is dropped quietly.
        w.progress("job_nope", "matching", Some(0.5)).await;
        let w = worker(&dead_base().await, AGENT);
        assert!(w.claim().await.is_err());
        std::fs::remove_dir_all(&w.cache).unwrap();
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// `run` reads `X9_WORKER_TOKEN` before the token file; with it set these
    /// tests would talk to the default bridge, so they stand down.
    fn token_in_env() -> bool {
        std::env::var("X9_WORKER_TOKEN").is_ok_and(|t| !t.trim().is_empty())
    }

    #[tokio::test]
    async fn the_worker_command_refuses_bad_arguments() {
        let e = run(&args(&["--frobnicate"])).await.unwrap_err().to_string();
        assert!(e.starts_with("usage: x9-bridge worker"), "{e}");
        let e = run(&args(&["--name", "laptop", "--bridge"])).await.unwrap_err().to_string();
        assert_eq!(e, "--bridge needs a value");
        if token_in_env() {
            return;
        }
        let missing = std::env::temp_dir().join(crate::util::new_id("no-token"));
        let e = run(&args(&["--token-file", missing.to_str().unwrap()])).await.unwrap_err().to_string();
        assert!(e.starts_with("reading the token from"), "{e}");
    }

    #[tokio::test]
    async fn the_worker_command_cannot_create_its_cache() {
        if token_in_env() {
            return;
        }
        let dir = cache_dir();
        std::fs::write(dir.join("token"), format!("{AGENT}\n")).unwrap();
        std::fs::write(dir.join("file"), b"in the way").unwrap();
        let cache = dir.join("file/cache");
        let e = run(&args(&["--bridge", "http://127.0.0.1:1", "--token-file", dir.join("token").to_str().unwrap(), "--cache", cache.to_str().unwrap()])).await;
        assert!(e.unwrap_err().to_string().starts_with("creating "));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_worker_command_takes_registrations_until_stopped() {
        if token_in_env() {
            return;
        }
        let b = Bridge::new().await;
        let base = serve(&b).await;
        let dir = cache_dir();
        std::fs::write(dir.join("token"), format!("{AGENT}\n")).unwrap();
        let a = args(&["--bridge", &format!("{base}/"), "--token-file", dir.join("token").to_str().unwrap(), "--name", "desk-1", "--cache", dir.join("cache").to_str().unwrap()]);
        let task = tokio::spawn(async move { run(&a).await });
        b.wait_for("/v1/workers", OPERATOR, 5.0, |v| v[0]["worker"] == "desk-1").await;
        let (_, job) = b.call("POST", &format!("/v1/projects/{PID}/registration"), Some(OPERATOR), None).await;
        // It claims within its 2 s poll and delivers (the station is skipped:
        // the mock has no scan file).
        let j = wait_job(&b, job["job_id"].as_str().unwrap(), 8.0).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert!(log_has(&b, "Worker desk-1 took the registration job"));
        assert!(log_has(&b, "Registration (registered on desk-1)"));
        assert!(!task.is_finished());
        task.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_worker_command_keeps_trying_an_unreachable_bridge() {
        if token_in_env() {
            return;
        }
        let dir = cache_dir();
        std::fs::write(dir.join("token"), AGENT).unwrap();
        let a = args(&["--bridge", &dead_base().await.replace("/api/v1", ""), "--token-file", dir.join("token").to_str().unwrap(), "--cache", dir.join("cache").to_str().unwrap()]);
        let task = tokio::spawn(async move { run(&a).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!task.is_finished());
        task.abort();
        assert!(dir.join("cache").is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_worker_is_named_after_the_computer() {
        assert!(!hostname().is_empty());
    }
}
