//! `x9-bridge worker`: do the bridge's heavy work on this computer (see
//! `work`). Claims registration jobs, fetches the scans over the bridge,
//! decodes, registers and audits them here, and sends the result back.
//! Decoded stations are kept in a cache directory, and prepared stations and
//! pair checks in memory, so each new station costs about one station's work.
//!
//!   x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR]
//!
//! Defaults: the bridge over the laptop cable (http://10.55.0.1:8795), the
//! token from `X9_WORKER_TOKEN` or ~/.config/x9-worker/token (an agent or
//! operator token), this computer's name, and ~/.cache/x9-worker.

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
    prepared: HashMap<String, Arc<Prepared>>,
    checks: AuditCache,
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let mut base = "http://10.55.0.1:8795".to_string();
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    let mut token_file = PathBuf::from(&home).join(".config/x9-worker/token");
    let mut name = hostname();
    let mut cache = PathBuf::from(&home).join(".cache/x9-worker");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--bridge" => base = val()?.trim_end_matches('/').to_string(),
            "--token-file" => token_file = PathBuf::from(val()?),
            "--name" => name = val()?.clone(),
            "--cache" => cache = PathBuf::from(val()?),
            _ => bail!("usage: x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR]"),
        }
    }
    let token = match std::env::var("X9_WORKER_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => std::fs::read_to_string(&token_file).with_context(|| format!("reading the token from {}", token_file.display()))?.trim().to_string(),
    };
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).build()?;
    let mut w = Worker { http, base, token, name, cache, prepared: HashMap::new(), checks: AuditCache::new() };
    eprintln!("x9-bridge worker {}: {} as {}, cache {}", crate::routes::VERSION, w.base, w.name, w.cache.display());
    let mut said_down = false;
    loop {
        match w.claim().await {
            Ok(Some(job)) => {
                said_down = false;
                let id = job["job_id"].as_str().unwrap_or("").to_string();
                let t0 = std::time::Instant::now();
                match w.register(&job).await {
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

    /// The station's 5 cm cloud: from the cache, or fetched and decoded.
    async fn cloud(&self, scan_id: &str) -> anyhow::Result<Cloud> {
        let path = self.cache.join(crate::worker::cloud_name(scan_id));
        if let Ok(b) = tokio::fs::read(&path).await {
            if let Ok(c) = Cloud::from_bytes(&b) {
                return Ok(c);
            }
        }
        let r = self.http.get(format!("{}/v1/scans/{scan_id}/tzf", self.base)).bearer_auth(&self.token).timeout(Duration::from_secs(600)).send().await?;
        if !r.status().is_success() {
            bail!("fetching the scan: {}", r.status());
        }
        let tzf = self.cache.join(format!("{scan_id}.tzf.part"));
        tokio::fs::write(&tzf, r.bytes().await?).await?;
        let t = tzf.clone();
        let decoded = tokio::task::spawn_blocking(move || -> anyhow::Result<Cloud> {
            let f = trimble_tzf::Tzf::open(&t)?;
            let meta = f.scan_meta()?;
            let pts = f.grid_points()?;
            Ok(Cloud::from_grid(&pts, crate::view::step_rad(&meta)))
        })
        .await;
        // The download is only needed to decode: drop it whether or not that worked.
        let _ = tokio::fs::remove_file(&tzf).await;
        let c = decoded??;
        tokio::fs::write(&path, c.to_bytes()).await?;
        Ok(c)
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
            match self.cloud(scan).await {
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
        Worker { http: reqwest::Client::new(), base: base.into(), token: token.into(), name: "laptop".into(), cache: cache_dir(), prepared: HashMap::new(), checks: AuditCache::new() }
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scan_file_that_does_not_decode_leaves_no_download_behind() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let w = &mut worker(&serve(&b).await, AGENT);
        let (_, job) = claim_registration(&b, w).await;
        w.register(&job).await.unwrap();
        let left: Vec<_> = std::fs::read_dir(&w.cache).unwrap().map(|e| e.unwrap().file_name()).collect();
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
