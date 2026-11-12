//! `x9-bridge worker`: do the bridge's heavy work on this computer (see
//! `work`). Claims registration jobs, fetches the scans over the bridge,
//! decodes, registers and audits them here, and sends the result back. Also
//! claims question checks: fits the offered faces on this computer's cores
//! (`ask::look_many`, as the bridge would) and sends back what it found; and
//! counts: finds the openings on the bound faces (`count::measure`, as the
//! bridge would) and sends back what it found, with each face's picture.
//!
//! The scan files it fetches are kept in the cache directory, with each
//! station's decoded 5 cm cloud, so a check needs no transfer for a station
//! already registered here; the cache drops the least recently used project's
//! files first past `--cache-gb`. Prepared stations and pair checks are kept
//! in memory, so each new station costs about one station's work. Each claim
//! tells the bridge which scan files are kept here, so the Pi can drop its
//! copies when its own cache is full.
//!
//! The laptop is where the data lands: a save job writes the project to a
//! folder under `--save-dir`, one per project (`save`).
//!
//!   x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR] [--cache-gb N] [--mem-gb N] [--save-dir DIR]
//!
//! Defaults: the bridge over the laptop cable (http://10.55.0.1:8795), the
//! token from `X9_WORKER_TOKEN` or ~/.config/x9-worker/token (an agent or
//! operator token), this computer's name, ~/.cache/x9-worker, 20 GB on disk,
//! 3 GB in memory (or a quarter of the computer's memory, if less), and
//! "~/X9 Projects".
//!
//! Decoded stations are kept in memory up to `--mem-gb` (0 keeps none), the
//! least recently used dropped first: each station as it registers, and
//! each one a check or a count decodes. A question asked after a re-check
//! then decodes no station again. A kept station is known by its file's
//! path, length and inode, so marking a scan file used again (as the disk
//! cache does) keeps it. A file counts as new only when it is renamed into
//! place, which is the only way the worker writes scan files; a file
//! rewritten in place, or deleted and made again, at the same length could
//! keep its old points.
//! The stations a check or count needs are decoded up to four at a time
//! (`points.rs`).

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
    /// Where saved projects go, one folder each.
    save_dir: PathBuf,
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let mut base = "http://10.55.0.1:8795".to_string();
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    let mut token_file = PathBuf::from(&home).join(".config/x9-worker/token");
    let mut name = hostname();
    let mut cache = PathBuf::from(&home).join(".cache/x9-worker");
    let mut cache_gb: f64 = 20.0;
    let mut mem_gb: Option<f64> = None;
    let mut save_dir = PathBuf::from(&home).join("X9 Projects");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--bridge" => base = val()?.trim_end_matches('/').to_string(),
            "--token-file" => token_file = PathBuf::from(val()?),
            "--name" => name = val()?.clone(),
            "--cache" => cache = PathBuf::from(val()?),
            "--cache-gb" => cache_gb = val()?.parse().ok().filter(|g: &f64| *g > 0.0).context("--cache-gb: a number of gigabytes")?,
            "--mem-gb" => mem_gb = Some(val()?.parse().ok().filter(|g: &f64| g.is_finite() && *g >= 0.0).context("--mem-gb: a number of gigabytes, 0 for none")?),
            "--save-dir" => save_dir = PathBuf::from(val()?),
            _ => bail!("usage: x9-bridge worker [--bridge URL] [--token-file FILE] [--name NAME] [--cache DIR] [--cache-gb N] [--mem-gb N] [--save-dir DIR]"),
        }
    }
    let token = match std::env::var("X9_WORKER_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => std::fs::read_to_string(&token_file).with_context(|| format!("reading the token from {}", token_file.display()))?.trim().to_string(),
    };
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).build()?;
    let mem_gb = mem_gb.unwrap_or_else(|| default_mem_gb(physical_memory()));
    let threads = decode_threads();
    // Tests set what is kept themselves, so a worker run inside them leaves
    // it alone.
    #[cfg(not(test))]
    crate::points::configure((mem_gb * 1e9) as u64, threads);
    let mut w = Worker { http, base, token, name, cache, cache_bytes: (cache_gb * 1e9) as u64, prepared: HashMap::new(), checks: AuditCache::new(), save_dir };
    eprintln!("x9-bridge worker {}: {} as {}, cache {}, up to {mem_gb:.1} GB of stations in memory, decoding {threads} at once, saves to {}", crate::routes::VERSION, w.base, w.name, w.cache.display(), w.save_dir.display());
    let mut said_down = false;
    loop {
        match w.claim().await {
            Ok(Some(job)) => {
                said_down = false;
                let id = job["job_id"].as_str().unwrap_or("").to_string();
                let t0 = std::time::Instant::now();
                let done = match job["kind"].as_str() {
                    Some("check") => w.check(&job).await,
                    Some("count") => w.count(&job).await,
                    Some("save") => w.save(&job).await,
                    _ => w.register(&job).await,
                };
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

/// The memory budget when none is given: 3 GB (about a 12-station survey),
/// or a quarter of the computer's memory if that is less.
fn default_mem_gb(physical: Option<u64>) -> f64 {
    physical.map_or(3.0, |b| (b as f64 / 4.0 / 1e9).min(3.0))
}

/// The computer's memory, in bytes, where it can be read.
fn physical_memory() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        let kb: u64 = s.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse().ok()?;
        return Some(kb * 1024);
    }
    let out = std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Stations decoded at once: a full-resolution station needs about 0.5 GB
/// while it decodes, so four at most.
fn decode_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4)
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
        let r = self.post("/v1/work/claim", &json!({"worker": self.name, "holds": self.holds()})).await?;
        if r.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(r.json().await?))
    }

    async fn progress(&self, job: &str, phase: &str, progress: Option<f64>) {
        let _ = self.post(&format!("/v1/work/{job}/progress"), &json!({"phase": phase, "progress": progress})).await;
    }

    /// Report progress, and stop if the bridge took the job back (a save
    /// should not go on writing for nobody). A bridge out of reach is not a
    /// reason to stop.
    async fn progress_or_stop(&self, job: &str, phase: &str, progress: Option<f64>) -> anyhow::Result<()> {
        match self.post(&format!("/v1/work/{job}/progress"), &json!({"phase": phase, "progress": progress})).await {
            Err(e) if e.to_string().contains(": 409") => bail!("the bridge took the job back"),
            _ => Ok(()),
        }
    }

    /// The scan files kept here, by scan id.
    fn holds(&self) -> Vec<String> {
        let Ok(dir) = std::fs::read_dir(&self.cache) else { return vec![] };
        let mut ids: Vec<String> = dir.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".tzf").map(str::to_string)).collect();
        ids.sort();
        ids
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
            // Keep its points too, so the check after this registration
            // doesn't decode it again.
            crate::points::remember(&t, &pts);
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

    /// The stations a check or count job lists, fetched into the cache (or
    /// found there), in order: number, pose, file.
    async fn job_files(&mut self, job: &Value, id: &str) -> anyhow::Result<Vec<(u16, crate::register::Pose, PathBuf)>> {
        let project = job["project_id"].as_str().unwrap_or("").to_string();
        let stations: Vec<(u16, String, crate::register::Pose)> = job["stations"]
            .as_array()
            .context("stations")?
            .iter()
            .map(|s| Ok((s["station"].as_u64().context("station")? as u16, s["scan_id"].as_str().context("scan_id")?.to_string(), serde_json::from_value(s["pose"].clone())?)))
            .collect::<anyhow::Result<_>>()?;
        let mut files = Vec::new();
        for (k, (n, scan, pose)) in stations.iter().enumerate() {
            self.progress(id, &format!("fetching station {n} on {}", self.name), Some(k as f64 / stations.len().max(1) as f64 * 0.3)).await;
            files.push((*n, *pose, self.tzf(scan, &project).await?));
        }
        Ok(files)
    }

    /// Report `phase` every 5 s until aborted, so the bridge sees this
    /// worker while it measures.
    fn heartbeat(&self, id: &str, phase: String) -> tokio::task::JoinHandle<()> {
        let (http, url, token) = (self.http.clone(), format!("{}/v1/work/{id}/progress", self.base), self.token.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = http.post(&url).bearer_auth(&token).json(&json!({"phase": phase})).send().await;
            }
        })
    }

    /// Upload a job's blob (`PUT …/merged`).
    async fn put_blob(&self, id: &str, what: &str, parts: &[Vec<u8>]) -> anyhow::Result<()> {
        let r = self.http.put(format!("{}/v1/work/{id}/merged", self.base)).bearer_auth(&self.token).body(crate::work::pack_blobs(parts)).send().await?;
        if !r.status().is_success() {
            bail!("uploading the {what}: {} {}", r.status(), r.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// A count: find the openings on the offered faces here, as the bridge
    /// would, and send back what each face holds, with the pictures uploaded
    /// first. The bridge matches, judges and records.
    async fn count(&mut self, job: &Value) -> anyhow::Result<String> {
        let id = job["job_id"].as_str().context("job_id")?.to_string();
        let specs: Vec<crate::count::FaceSpec> = serde_json::from_value(job["faces"].clone()).context("faces")?;
        let files = self.job_files(job, &id).await?;
        self.progress(&id, &format!("counting openings on {} faces on {}", specs.len(), self.name), Some(0.3)).await;
        let these = specs.clone();
        let beat = self.heartbeat(&id, format!("counting openings on {}", self.name));
        let reads = tokio::task::spawn_blocking(move || crate::count::measure(&these, &files)).await;
        beat.abort();
        let reads = reads??;
        let mut pictures = Vec::new();
        let faces: Vec<crate::work::CountOut> = specs
            .iter()
            .zip(reads)
            .map(|(f, (found, png))| {
                let pictured = png.is_some();
                pictures.extend(png);
                crate::work::CountOut { face_id: f.face_id.clone(), found, pictured }
            })
            .collect();
        if !pictures.is_empty() {
            self.put_blob(&id, "pictures", &pictures).await?;
        }
        let read: Vec<String> = faces.iter().map(|f| format!("{} {}", f.face_id, f.found.openings.len())).collect();
        self.post(&format!("/v1/work/{id}/result"), &serde_json::to_value(crate::work::CountResult { faces })?).await?;
        // Openings read per face, before the bridge counts each one once.
        Ok(format!("openings read per face: {}", read.join(", ")))
    }

    /// A question check: fit the offered faces here, as the bridge would, and
    /// send back what each look found, with the fits uploaded first.
    async fn check(&mut self, job: &Value) -> anyhow::Result<String> {
        let id = job["job_id"].as_str().context("job_id")?.to_string();
        let faces: Vec<(String, crate::register::Face, Value)> = job["faces"]
            .as_array()
            .context("faces")?
            .iter()
            .map(|f| Ok((f["target_id"].as_str().context("target_id")?.to_string(), serde_json::from_value(f["face"].clone())?, f["coverage"].clone())))
            .collect::<anyhow::Result<_>>()?;
        let files = self.job_files(job, &id).await?;
        self.progress(&id, &format!("fitting {} faces on {}", faces.len(), self.name), Some(0.3)).await;
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
        let beat = self.heartbeat(&id, format!("fitting faces on {}", self.name));
        let looks = tokio::task::spawn_blocking(move || crate::ask::look_many(&faces, &files, &crate::analyze::Opts::default(), threads)).await;
        beat.abort();
        let looks = looks??;
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
            self.put_blob(&id, "fits", &fits).await?;
        }
        let n = checks.len();
        let answered = checks.iter().filter(|c| c.fit.is_some()).count();
        self.post(&format!("/v1/work/{id}/result"), &serde_json::to_value(crate::work::CheckResult { checks })?).await?;
        let (kept, bytes, _) = crate::points::kept();
        Ok(format!("{n} faces checked, {answered} with a reading ({kept} stations in memory, {:.1} GB)", bytes as f64 / 1e9))
    }

    /// The station's preview from the bridge, for grey levels; none if it has none.
    async fn preview(&self, scan_id: &str) -> Option<Vec<u8>> {
        let r = self.http.get(format!("{}/v1/scans/{scan_id}/preview", self.base)).bearer_auth(&self.token).timeout(Duration::from_secs(60)).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.bytes().await.ok().map(|b| b.to_vec())
    }

    /// A save: write the project to its folder under the save directory
    /// (`specs/laptop-offload.md`, part D). Every station's scan file (linked
    /// from the cache when it can be, else copied) and preview, each placed
    /// station's points in the project frame as COPC, the registration and
    /// mission, and last the manifest, listing every file with its checksum.
    /// A station whose scan and pose are as at the last save keeps its cloud.
    /// A station that can't be saved is noted, and the rest are saved.
    async fn save(&mut self, job: &Value) -> anyhow::Result<String> {
        let id = job["job_id"].as_str().context("job_id")?.to_string();
        let project = job["project_id"].as_str().unwrap_or("").to_string();
        let folder = job["folder"].as_str().context("folder")?;
        if folder.is_empty() || folder.len() > 120 || folder.contains(['/', '\\']) || folder.starts_with('.') {
            bail!("not a folder name: {folder:?}");
        }
        let stations: Vec<SaveStation> = serde_json::from_value(job["stations"].clone()).context("stations")?;
        let frame = job["frame_station"].as_u64().unwrap_or(1) as u16;
        let wkt = crate::export::project_wkt(frame);
        let dir = self.save_dir.join(folder);
        for sub in ["scans", "stations"] {
            std::fs::create_dir_all(dir.join(sub)).with_context(|| format!("creating {}", dir.join(sub).display()))?;
        }
        let before: Value = std::fs::read(dir.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        // (path in the folder, whether the last manifest's checksum still holds)
        let mut written: Vec<(String, bool)> = Vec::new();
        let (mut out, mut listed) = (Vec::new(), Vec::new());
        for (k, s) in stations.iter().enumerate() {
            self.progress_or_stop(&id, &format!("saving station {} on {}", s.station, self.name), Some(k as f64 / stations.len().max(1) as f64 * 0.9)).await?;
            let mut done = crate::work::SavedStation { station: s.station, scan_id: s.scan_id.clone(), copc: None, point_count: None, kept: false, note: None };
            let tzf = match self.tzf(&s.scan_id, &project).await {
                Ok(p) => p,
                Err(e) => {
                    done.note = Some(format!("scan file not saved: {e:#}"));
                    listed.push(json!({"station": s.station, "scan_id": s.scan_id, "pose": s.pose, "trusted": s.trusted, "note": done.note}));
                    out.push(done);
                    continue;
                }
            };
            let raw = format!("scans/station-{:02}-{}.tzf", s.station, s.scan_id);
            let fresh = place(&tzf, &dir.join(&raw))?;
            written.push((raw.clone(), !fresh));
            let preview = self.preview(&s.scan_id).await;
            let jpg = format!("scans/station-{:02}-{}.jpg", s.station, s.scan_id);
            if let Some(b) = &preview {
                write_atomic(&dir.join(&jpg), b)?;
                written.push((jpg.clone(), false));
            }
            match s.pose {
                None => done.note = Some("not placed: saved as its scan file only".into()),
                Some(pose) => {
                    let name = format!("stations/station-{:02}{}.copc.laz", s.station, if s.trusted { "" } else { "-unconfirmed" });
                    let last = before["stations"].as_array().and_then(|a| a.iter().find(|e| e["station"] == s.station)).cloned().unwrap_or(Value::Null);
                    let unchanged = last["scan_id"] == s.scan_id.as_str() && last["copc"] == name.as_str() && last["pose"] == serde_json::to_value(pose)? && dir.join(&name).exists();
                    if unchanged {
                        done.kept = true;
                        done.point_count = last["point_count"].as_u64();
                    } else {
                        let (t, w, pv, part) = (tzf.clone(), wkt.clone(), preview.clone(), dir.join(format!("{name}.part")));
                        let p2 = part.clone();
                        match tokio::task::spawn_blocking(move || crate::export::save_station(&t, pv.as_deref(), &pose, &w, &p2, &|| false)).await? {
                            Ok(v) => {
                                std::fs::rename(&part, dir.join(&name))?;
                                done.point_count = v["point_count"].as_u64();
                            }
                            Err(e) => {
                                let _ = std::fs::remove_file(&part);
                                let _ = std::fs::remove_file(dir.join(&name));
                                done.note = Some(format!("cloud not written: {}", failure(&e)));
                            }
                        }
                    }
                    if dir.join(&name).exists() {
                        done.copc = Some(name.clone());
                        written.push((name, done.kept));
                    }
                }
            }
            listed.push(json!({"station": s.station, "scan_id": s.scan_id, "pose": s.pose, "trusted": s.trusted, "copc": done.copc, "point_count": done.point_count, "scan_file": raw, "preview": preview.is_some().then_some(jpg), "note": done.note}));
            out.push(done);
        }
        // A cloud from an earlier save that this one didn't write (the station
        // was confirmed since, or left the registration) would mislead.
        let keep: std::collections::HashSet<&str> = written.iter().map(|w| w.0.as_str()).collect();
        for e in std::fs::read_dir(dir.join("stations"))?.flatten() {
            let rel = format!("stations/{}", e.file_name().to_string_lossy());
            if rel.ends_with(".copc.laz") && !keep.contains(rel.as_str()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        self.progress_or_stop(&id, &format!("checksums on {}", self.name), Some(0.9)).await?;
        write_atomic(&dir.join("registration.json"), &serde_json::to_vec_pretty(&job["registration"])?)?;
        written.push(("registration.json".into(), false));
        if !job["mission"].is_null() {
            write_atomic(&dir.join("mission.json"), &serde_json::to_vec_pretty(&job["mission"])?)?;
            written.push(("mission.json".into(), false));
        }
        let (d, last) = (dir.clone(), before["files"].clone());
        let files = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<crate::work::SavedFile>> {
            written
                .into_iter()
                .map(|(path, reuse)| {
                    let bytes = std::fs::metadata(d.join(&path))?.len();
                    let known = last.as_array().and_then(|a| a.iter().find(|f| f["path"] == path.as_str() && f["bytes"] == bytes)).and_then(|f| f["sha256"].as_str().map(str::to_string));
                    let sha256 = match known.filter(|_| reuse) {
                        Some(h) => h,
                        None => sha256_of(&d.join(&path))?,
                    };
                    Ok(crate::work::SavedFile { path, bytes, sha256 })
                })
                .collect()
        })
        .await??;
        let manifest = json!({
            "format": "x9-bridge project folder, version 1",
            "project_id": project,
            "project_name": job["project_name"],
            "saved_at": crate::util::now_rfc3339(),
            "saved_by": self.name,
            "worker_version": crate::routes::VERSION,
            "bridge_version": job["bridge_version"],
            "frame_station": frame,
            "crs_wkt": wkt,
            "stations": listed,
            "files": files,
        });
        write_atomic(&dir.join("manifest.json"), &serde_json::to_vec_pretty(&manifest)?)?;
        let gb = files.iter().map(|f| f.bytes).sum::<u64>() as f64 / 1e9;
        let placed = out.iter().filter(|s| s.copc.is_some()).count();
        let res = crate::work::SaveResult { folder: dir.display().to_string(), files, stations: out };
        self.post(&format!("/v1/work/{id}/result"), &serde_json::to_value(&res)?).await?;
        Ok(format!("saved to {}: {placed} of {} stations in the project frame, {gb:.1} GB", res.folder, res.stations.len()))
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

/// A station as a save job lists it.
#[derive(serde::Deserialize)]
struct SaveStation {
    station: u16,
    scan_id: String,
    pose: Option<crate::register::Pose>,
    #[serde(default)]
    trusted: bool,
}

/// Put a scan file in the folder: a hard link to the cached file when they
/// share a disk (no second copy, and it outlives the cache's trimming), a
/// copy otherwise. A file already there at the same size is left. Returns
/// whether it wrote.
fn place(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<bool> {
    let len = std::fs::metadata(src)?.len();
    if std::fs::metadata(dst).is_ok_and(|m| m.len() == len) {
        return Ok(false);
    }
    let part = dst.with_extension("tzf.part");
    let _ = std::fs::remove_file(&part);
    if std::fs::hard_link(src, &part).is_err() {
        std::fs::copy(src, &part)?;
    }
    std::fs::rename(&part, dst)?;
    Ok(true)
}

/// Write a file whole or not at all.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let part = std::path::PathBuf::from(format!("{}.part", path.display()));
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, path)
}

fn sha256_of(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn failure(e: &crate::export::Failure) -> String {
    match e {
        crate::export::Failure::Decode(m) => format!("the scan did not decode ({m})"),
        crate::export::Failure::Write(e) => format!("writing it failed ({e})"),
        crate::export::Failure::Cancelled => "stopped".into(),
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
        Worker { http: reqwest::Client::new(), base: base.into(), token: token.into(), name: "laptop".into(), cache: cache_dir(), cache_bytes: 20_000_000_000, prepared: HashMap::new(), checks: AuditCache::new(), save_dir: cache_dir().join("saved") }
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

    /// A worker that claims and runs checks and counts until `stop` is set.
    fn run_checks(mut w: Worker, stop: Arc<std::sync::atomic::AtomicBool>) -> tokio::task::JoinHandle<Worker> {
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match w.claim().await {
                    Ok(Some(job)) if job["kind"] == "check" => {
                        w.check(&job).await.unwrap();
                    }
                    Ok(Some(job)) if job["kind"] == "count" => {
                        w.count(&job).await.unwrap();
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

    // The guard keeps other tests from changing what is kept while this one
    // runs, across its awaits; the tests that also take it are synchronous.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_question_on_the_laptop_decodes_no_station_again() {
        use crate::ask::tests::{yard_bridge, PROJECT};
        let _g = crate::points::CONFIGURED.lock().unwrap_or_else(|e| e.into_inner());
        crate::points::configure(1 << 32, 4);
        let (b, fx) = yard_bridge().await;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let laptop = run_checks(worker(&serve(&b).await, AGENT), stop.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while b.st.work.lock().unwrap().workers().is_empty() {
            assert!(std::time::Instant::now() < deadline, "the worker never showed up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mine = |cache: &std::path::Path| crate::points::DECODED.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|d| d.starts_with(cache)).count();
        let first = crate::ask::on_bridge(&b.st, PROJECT, "east", "agent").await.unwrap();
        assert_eq!(first["checked_on"], "laptop", "{first}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let w = laptop.await.unwrap();
        let cache = w.cache.clone();
        let after_first = mine(&cache);
        assert!(after_first > 0, "the first check decodes its stations");
        // The disk cache marks the files used again on every job; the kept
        // points must still be found.
        stop.store(false, std::sync::atomic::Ordering::Relaxed);
        let laptop = run_checks(w, stop.clone());
        let second = crate::ask::on_bridge(&b.st, PROJECT, "east", "agent").await.unwrap();
        assert_eq!(second["checked_on"], "laptop", "{second}");
        for k in ["verdict", "reason", "stations", "face"] {
            assert_eq!(second[k], first[k], "{k}");
        }
        assert_eq!(mine(&cache), after_first, "the second check decoded a station again");
        let log: Vec<String> = b.st.inner.lock().unwrap().log.iter().map(|e| e.text.clone()).collect();
        assert_eq!(log.iter().filter(|t| t.starts_with("Asked Target east (plumb)") && t.ends_with("checked on laptop")).count(), 2, "{log:#?}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let w = laptop.await.unwrap();
        crate::points::configure(0, 1);
        std::fs::remove_dir_all(&w.cache).unwrap();
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_count_on_the_laptop_finds_what_the_pi_finds() {
        use crate::count::tests::{bound_facade, P};
        let count = format!("/v1/projects/{P}/count");
        // The Pi alone.
        let (pi, fx_pi) = bound_facade().await;
        let (_, alone) = pi.call("POST", &count, Some(AGENT), None).await;
        assert_eq!(alone["counted_on"], "bridge", "{alone}");
        // The same count, with a laptop on the cable.
        let (b, fx) = bound_facade().await;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let laptop = run_checks(worker(&serve(&b).await, AGENT), stop.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while b.st.work.lock().unwrap().workers().is_empty() {
            assert!(std::time::Instant::now() < deadline, "the worker never showed up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (code, on) = b.call("POST", &count, Some(AGENT), None).await;
        assert_eq!(code, axum::http::StatusCode::OK, "{on}");
        assert_eq!((on["counted_on"].as_str(), on["counted_by"].as_str()), (Some("laptop"), Some("agent")), "{on}");
        for k in ["say", "faces", "lines", "proposed", "not_on_schedule"] {
            assert_eq!(on[k], alone[k], "{k}");
        }
        assert_eq!(on["faces"][0]["openings"].as_array().unwrap().len(), 3);
        // The same pictures, to the byte.
        let pics = |b: &Bridge| b.st.inner.lock().unwrap().count_pictures.clone();
        assert_eq!(pics(&b), pics(&pi));
        assert!(!pics(&b).is_empty());
        let (_, again) = b.call("GET", &count, Some(VIEWER), None).await;
        assert_eq!(again["counted_on"], "laptop");
        let log: Vec<String> = b.st.inner.lock().unwrap().log.iter().map(|e| e.text.clone()).collect();
        assert!(log.iter().any(|t| t.starts_with("Counted openings (") && t.contains(" s, counted on laptop): 3 openings found")), "{log:#?}");
        assert!(log.iter().any(|t| t == "Worker laptop took the count"), "{log:#?}");
        // The record says where its count was made, and with whose token.
        let m = json!({"matches": [{"line_id": "W1", "opening_id": "f1-1"}], "confirmed_via": "asked in the chat"});
        assert_eq!(b.call("POST", &format!("/v1/projects/{P}/count/matches"), Some(OPERATOR), Some(m)).await.0, axum::http::StatusCode::OK);
        let (_, r) = b.call("GET", &format!("/v1/projects/{P}/record"), Some(VIEWER), None).await;
        let w1 = r["lines"].as_array().unwrap().iter().find(|l| l["line_id"] == "W1").unwrap();
        assert_eq!((w1["evidence"]["counted_on"].as_str(), w1["evidence"]["counted_by"].as_str()), (Some("laptop"), Some("agent")), "{w1}");
        // After a registration the laptop counts again.
        crate::count::after_registration(&b.st, P).await;
        let n = b.st.inner.lock().unwrap().log.iter().filter(|e| e.text.contains("counted on laptop")).count();
        assert_eq!(n, 2);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let w = laptop.await.unwrap();
        assert!(std::fs::read_dir(&w.cache).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tzf")), "the scan file is kept for next time");
        std::fs::remove_dir_all(&w.cache).unwrap();
        std::fs::remove_dir_all(fx).unwrap();
        std::fs::remove_dir_all(fx_pi).unwrap();
    }

    /// Have the bridge offer a save of the project and take it.
    async fn claim_save(b: &Bridge, w: &Worker, project: &str) -> (String, Value) {
        assert!(w.claim().await.unwrap().is_none(), "seen first");
        let (code, job) = b.call("POST", &format!("/v1/projects/{project}/save"), Some(AGENT), None).await;
        assert_eq!(code, axum::http::StatusCode::ACCEPTED, "{job}");
        let j = w.claim().await.unwrap().expect("the save is on offer");
        assert_eq!((j["job_id"].as_str(), j["kind"].as_str()), (job["job_id"].as_str(), Some("save")));
        (job["job_id"].as_str().unwrap().to_string(), j)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_writes_the_project_folder_on_the_laptop_and_keeps_what_is_unchanged() {
        use crate::ask::tests::{yard_bridge, PROJECT};
        use crate::register::Pose;
        let (b, fx) = yard_bridge().await;
        let pose = Pose { turn_deg: 90.0, xyz_m: [10.0, 0.0, 1.0] };
        let scan = {
            let mut inner = b.st.inner.lock().unwrap();
            let s = &mut inner.registrations.get_mut(PROJECT).unwrap().stations[0];
            s.pose = Some(pose);
            s.scan_id.clone()
        };
        let w = &mut worker(&serve(&b).await, AGENT);
        let (id, job) = claim_save(&b, w, PROJECT).await;
        let summary = w.save(&job).await.unwrap();
        assert!(summary.contains(": 1 of 1 stations in the project frame"), "{summary}");
        let done = wait_job(&b, &id, 5.0).await;
        assert_eq!(done["state"], "succeeded", "{done}");
        let folder = PathBuf::from(done["result"]["folder"].as_str().unwrap());
        assert!(folder.starts_with(&w.save_dir), "{folder:?}");

        // Every file is listed with its size and checksum.
        let manifest: Value = serde_json::from_slice(&std::fs::read(folder.join("manifest.json")).unwrap()).unwrap();
        let mut paths = Vec::new();
        for f in manifest["files"].as_array().unwrap() {
            let p = folder.join(f["path"].as_str().unwrap());
            assert_eq!(std::fs::metadata(&p).unwrap().len(), f["bytes"].as_u64().unwrap(), "{f}");
            assert_eq!(sha256_of(&p).unwrap(), f["sha256"].as_str().unwrap(), "{f}");
            paths.push(f["path"].as_str().unwrap().to_string());
        }
        paths.sort();
        let raw = format!("scans/station-01-{scan}.tzf");
        assert_eq!(paths, ["mission.json", "registration.json", &format!("scans/station-01-{scan}.jpg"), &raw, "stations/station-01.copc.laz"]);
        assert_eq!(std::fs::read(folder.join(&raw)).unwrap(), std::fs::read(fx.join("dome.tzf")).unwrap(), "the scan file as the scanner made it");
        assert_eq!(manifest["frame_station"], 1);
        assert!(manifest["crs_wkt"].as_str().unwrap().contains("X9 project frame (station 1, levelled)"));

        // The cloud is the station's points, moved by its pose into the project frame.
        let copc = folder.join("stations/station-01.copc.laz");
        let mut r = copc_rs::CopcReader::from_path(&copc).unwrap();
        let got: Vec<las::Point> = r.points(copc_rs::LodSelection::All, copc_rs::BoundsSelection::All).unwrap().collect();
        let t = trimble_tzf::Tzf::open(&fx.join("dome.tzf")).unwrap();
        let pts = t.grid_points().unwrap();
        assert_eq!(got.len(), pts.len());
        assert_eq!(manifest["stations"][0]["point_count"].as_u64(), Some(pts.len() as u64));
        let moved: Vec<[f32; 3]> = pts.iter().map(|p| pose.apply([p.point.x, p.point.y, p.point.z])).collect();
        for k in 0..3 {
            let (lo, hi) = got.iter().map(|q| [q.x, q.y, q.z][k]).fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
            let (elo, ehi) = moved.iter().map(|q| q[k] as f64).fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
            assert!((lo - elo).abs() < 1e-3 && (hi - ehi).abs() < 1e-3, "axis {k}: {lo}..{hi} against {elo}..{ehi}");
        }

        // The bridge has the record, and the Pi may drop its copy: the laptop holds it.
        assert_eq!(b.call("GET", &format!("/v1/projects/{PROJECT}/save"), Some(VIEWER), None).await.1["last"]["folder"], folder.to_str().unwrap());
        assert!(log_has(&b, &format!("Saved the project on laptop: {} (1 of 1 stations in the project frame", folder.display())));
        assert!(w.claim().await.unwrap().is_none());
        assert!(b.st.work.lock().unwrap().held().contains(&scan));

        // Saved again unchanged: the cloud is kept, not written again.
        let when = std::fs::metadata(&copc).unwrap().modified().unwrap();
        let (id, job) = claim_save(&b, w, PROJECT).await;
        w.save(&job).await.unwrap();
        let again = wait_job(&b, &id, 5.0).await;
        assert_eq!(again["result"]["stations"][0]["kept"], true, "{again}");
        assert_eq!(std::fs::metadata(&copc).unwrap().modified().unwrap(), when);
        assert!(log_has(&b, "1 unchanged since the last save"));

        // No longer confirmed: written again under a name that says so, and the old one goes.
        b.st.inner.lock().unwrap().registrations.get_mut(PROJECT).unwrap().stations[0].audit = Some(crate::register::Audit { verdict: "doubtful".into(), ..Default::default() });
        let (id, job) = claim_save(&b, w, PROJECT).await;
        w.save(&job).await.unwrap();
        let third = wait_job(&b, &id, 5.0).await;
        assert_eq!(third["result"]["stations"][0]["copc"], "stations/station-01-unconfirmed.copc.laz", "{third}");
        assert!(!copc.exists());
        std::fs::remove_dir_all(&w.cache).unwrap();
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_station_that_cannot_be_saved_is_noted_and_the_rest_are_saved() {
        use crate::register::{Registration, StationPose};
        let b = Bridge::new().await;
        let reg = Registration { project_id: PID.into(), frame_scan_id: None, pairs: vec![], walls: vec![], summary: String::new(), updated_at: String::new(),
                                 stations: vec![StationPose { scan_id: SCAN0.into(), station: 1, pose: None, audit: None }] };
        b.st.inner.lock().unwrap().registrations.insert(PID.into(), reg);
        let w = &mut worker(&serve(&b).await, AGENT);
        let (id, job) = claim_save(&b, w, PID).await;
        // The mock has no scan file to give: the station is noted, the registration still saved.
        w.save(&job).await.unwrap();
        let done = wait_job(&b, &id, 5.0).await;
        assert_eq!(done["state"], "succeeded", "{done}");
        let note = done["result"]["stations"][0]["note"].as_str().unwrap();
        assert!(note.starts_with("scan file not saved: fetching the scan: 502"), "{note}");
        assert!(log_has(&b, "; station 1: scan file not saved"));
        let folder = PathBuf::from(done["result"]["folder"].as_str().unwrap());
        let m: Value = serde_json::from_slice(&std::fs::read(folder.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(m["stations"][0]["note"].as_str(), Some(note));
        assert_eq!(m["files"][0]["path"], "registration.json");
        assert!(!folder.join("mission.json").exists(), "no mission, no file");
        // A folder name that would leave the save directory is refused.
        for bad in ["../out", "a/b", ".hidden", ""] {
            let e = w.save(&json!({"job_id": "job_x", "folder": bad, "stations": []})).await.unwrap_err().to_string();
            assert!(e.starts_with("not a folder name"), "{bad}: {e}");
        }
        std::fs::remove_dir_all(&w.cache).unwrap();
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
        for bad in ["-1", "inf", "NaN", "lots"] {
            let e = run(&args(&["--mem-gb", bad])).await.unwrap_err().to_string();
            assert_eq!(e, "--mem-gb: a number of gigabytes, 0 for none", "{bad}");
        }
        if token_in_env() {
            return;
        }
        let missing = std::env::temp_dir().join(crate::util::new_id("no-token"));
        let e = run(&args(&["--token-file", missing.to_str().unwrap()])).await.unwrap_err().to_string();
        assert!(e.starts_with("reading the token from"), "{e}");
    }

    #[test]
    fn the_default_memory_budget_is_3_gb_or_a_quarter_of_the_computers_memory() {
        assert_eq!(default_mem_gb(Some(16 << 30)), 3.0);
        assert!((default_mem_gb(Some(8_000_000_000)) - 2.0).abs() < 1e-9);
        assert_eq!(default_mem_gb(None), 3.0);
        assert!(physical_memory().is_some_and(|b| b > 1 << 30), "this computer's memory can be read");
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
