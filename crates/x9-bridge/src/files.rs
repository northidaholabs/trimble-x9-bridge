//! Scan files and previews, fetched from the scanner once and cached on disk.
//! Both are immutable once the scan exists, but a scan can be deleted: every
//! request asks the scanner whether it still lists the scan before serving
//! the cache, and a delete evicts the scan's cached files.

use crate::problem::ApiError;
use crate::state::AppState;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Tzf,
    Preview,
}

impl FileKind {
    const ALL: [FileKind; 2] = [FileKind::Tzf, FileKind::Preview];

    fn ext(self) -> &'static str {
        match self {
            FileKind::Tzf => "tzf",
            FileKind::Preview => "jpg",
        }
    }
}

/// Scan ids are the scanner's UUIDs; refuse anything else so a path can't
/// escape the cache directory.
pub(crate) fn safe_id(id: &str) -> Result<&str, ApiError> {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        Ok(id)
    } else {
        Err(ApiError::not_found("scan"))
    }
}

fn cache_path(data_dir: &Path, id: &str, kind: FileKind) -> PathBuf {
    data_dir.join("scans").join(format!("{id}.{}", kind.ext()))
}

/// Remove a scan's cached file and preview, its viewer and coverage files, and
/// any export or measurement made from it. Missing files are fine.
pub async fn evict(st: &AppState, scan_id: &str) {
    let Ok(id) = safe_id(scan_id) else { return };
    for kind in FileKind::ALL {
        let _ = tokio::fs::remove_file(cache_path(&st.data_dir, id, kind)).await;
    }
    st.inner.lock().unwrap().next_stations.remove(id);
    if let Ok(mut d) = tokio::fs::read_dir(st.data_dir.join("views")).await {
        while let Ok(Some(e)) = d.next_entry().await {
            if e.file_name().to_string_lossy().starts_with(&format!("{id}.")) || e.file_name().to_string_lossy().starts_with(&format!("{id}-")) {
                let _ = tokio::fs::remove_file(e.path()).await;
            }
        }
    }
    let exports: Vec<PathBuf> = {
        let mut inner = st.inner.lock().unwrap();
        let ids: Vec<String> = inner.exports.iter().filter(|(_, v)| v["scan_id"] == id).map(|(k, _)| k.clone()).collect();
        ids.iter()
            .filter_map(|e| {
                inner.exports.remove(e);
                inner.export_files.remove(e)
            })
            .collect()
    };
    let evidence: Vec<PathBuf> = {
        let mut inner = st.inner.lock().unwrap();
        let ids: Vec<String> = inner.measurements.iter().filter(|(_, v)| v["scan_id"] == id).map(|(k, _)| k.clone()).collect();
        ids.iter()
            .filter_map(|m| {
                inner.measurements.remove(m);
                inner.measurement_files.remove(m)
            })
            .collect()
    };
    for p in evidence {
        let _ = tokio::fs::remove_file(p).await;
    }
    for p in exports {
        let _ = tokio::fs::remove_file(p).await;
    }
}

/// Deletes a partial download unless it was moved into place.
struct PartFile(PathBuf, bool);

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

const MB: u64 = 1 << 20;

/// The scan cache's budget in bytes (`specs/laptop-offload.md`, part C):
/// `BRIDGE_CACHE_MB`, or else what leaves 1 GB free on the data disk when
/// first asked (the cache's own files included), at most 4 GB.
fn budget(st: &AppState) -> u64 {
    if let Some(mb) = std::env::var("BRIDGE_CACHE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        return mb * MB;
    }
    static DEFAULT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *DEFAULT.get_or_init(|| {
        let held: u64 = cached(&st.data_dir).iter().map(|f| f.bytes).sum();
        free_bytes(&st.data_dir).map_or(4096 * MB, |free| (free + held).saturating_sub(1024 * MB)).min(4096 * MB)
    })
}

#[cfg(unix)]
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `s` a zeroed statvfs the call fills in.
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}

/// A file in the scan cache.
#[derive(Debug, Clone)]
struct Cached {
    path: PathBuf,
    scan_id: String,
    bytes: u64,
    modified: std::time::SystemTime,
}

/// The cache's files, partial downloads left out.
fn cached(data_dir: &Path) -> Vec<Cached> {
    let Ok(dir) = std::fs::read_dir(data_dir.join("scans")) else { return vec![] };
    dir.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (id, ext) = name.split_once('.')?;
            if ext.contains("part") {
                return None;
            }
            let m = e.metadata().ok()?;
            Some(Cached { path: e.path(), scan_id: id.to_string(), bytes: m.len(), modified: m.modified().ok()? })
        })
        .collect()
}

/// Scan files being read: while it lives, the cache never drops them.
pub struct Reading<'a> {
    st: &'a AppState,
    ids: Vec<String>,
}

/// Mark scan files as being read, from before they are fetched until the
/// guard is dropped (a fetch of one may make room by dropping another).
pub fn reading(st: &AppState, ids: impl IntoIterator<Item = String>) -> Reading<'_> {
    let ids: Vec<String> = ids.into_iter().collect();
    let mut inner = st.inner.lock().unwrap();
    for id in &ids {
        *inner.reading.entry(id.clone()).or_default() += 1;
    }
    Reading { st, ids }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let mut inner = self.st.inner.lock().unwrap();
        for id in &self.ids {
            if let Some(n) = inner.reading.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    inner.reading.remove(id);
                }
            }
        }
    }
}

/// Make room for `incoming` bytes of scan `incoming_id` within `budget`.
/// First the cached scan files of other projects, oldest first: never one of
/// the incoming scan's project, of a project with a job running or
/// registering (it may be reading them), or of a scan in no registration
/// while any job runs. Then, any project's files a connected laptop holds
/// whose 5 cm clouds are prepared, oldest first (`specs/laptop-offload.md`,
/// part D). Never a file being read (`reading`). A dropped file is fetched
/// again if it is needed: the scanner keeps every scan. When nothing more may
/// be dropped and the cache is still over, the field log says so once.
/// Returns the bytes dropped.
pub async fn make_room(st: &AppState, incoming_id: &str, incoming: u64, budget: u64) -> u64 {
    let mut files = cached(&st.data_dir);
    let mut used: u64 = files.iter().map(|f| f.bytes).sum();
    if used + incoming <= budget {
        st.inner.lock().unwrap().cache_noted = false;
        return 0;
    }
    let (project_of, busy, any_job) = {
        let inner = st.inner.lock().unwrap();
        let project_of: std::collections::HashMap<String, String> = inner
            .registrations
            .iter()
            .flat_map(|(p, r)| r.stations.iter().map(move |s| (s.scan_id.clone(), p.clone())))
            .collect();
        let mut busy: std::collections::HashSet<String> = inner.jobs.iter().filter(|j| !j.job.state.is_final()).map(|j| j.project_id.clone()).collect();
        busy.extend(inner.registering.keys().cloned());
        let any_job = inner.jobs.iter().any(|j| !j.job.state.is_final()) || !inner.registering.is_empty();
        (project_of, busy, any_job)
    };
    let mine = project_of.get(incoming_id).cloned();
    let reading: std::collections::HashSet<String> = st.inner.lock().unwrap().reading.keys().cloned().collect();
    let keep = |f: &Cached| {
        f.scan_id == incoming_id
            || reading.contains(&f.scan_id)
            || match project_of.get(&f.scan_id) {
                Some(p) => Some(p) == mine.as_ref() || busy.contains(p),
                None => any_job || mine.is_none(),
            }
    };
    files.retain(|f| !keep(f));
    files.sort_by_key(|f| f.modified);
    let (mut dropped, mut n) = (0u64, 0usize);
    for f in files {
        if used + incoming <= budget {
            break;
        }
        if tokio::fs::remove_file(&f.path).await.is_ok() {
            used -= f.bytes;
            dropped += f.bytes;
            n += 1;
        }
    }
    if n > 0 {
        st.log("note", format!("Made room in the scan cache: dropped {n} file{} ({} MB) of other projects, oldest first", if n == 1 { "" } else { "s" }, dropped / MB), "bridge", None, &[]);
    }
    if used + incoming > budget {
        // The second tier: what the laptop holds, once the Pi needs only the
        // station's prepared cloud.
        let held = st.work.lock().unwrap().held();
        let prepared = st.data_dir.join("registration");
        let mut second: Vec<Cached> = cached(&st.data_dir)
            .into_iter()
            .filter(|f| f.scan_id != incoming_id && !reading.contains(&f.scan_id) && held.contains(&f.scan_id))
            .filter(|f| prepared.join(crate::worker::cloud_name(&f.scan_id)).exists())
            .collect();
        second.sort_by_key(|f| f.modified);
        let (mut freed, mut k) = (0u64, 0usize);
        for f in second {
            if used + incoming <= budget {
                break;
            }
            if tokio::fs::remove_file(&f.path).await.is_ok() {
                used -= f.bytes;
                freed += f.bytes;
                k += 1;
            }
        }
        if k > 0 {
            st.log("note", format!("Made room in the scan cache: dropped {k} file{} ({} MB) the laptop holds; their stations stay registered", if k == 1 { "" } else { "s" }, freed / MB), "bridge", mine.as_deref(), &[]);
        }
        dropped += freed;
    }
    if used + incoming > budget {
        let first = !std::mem::replace(&mut st.inner.lock().unwrap().cache_noted, true);
        if first {
            st.log(
                "check",
                format!("The scan cache needs {} MB, over its {} MB budget, and holds nothing it may drop. Connect the laptop to take scans off the Pi.", (used + incoming) / MB, budget / MB),
                "bridge", mine.as_deref(), &[],
            );
        }
    } else {
        st.inner.lock().unwrap().cache_noted = false;
    }
    dropped
}

/// Local path of a scan's file or preview, downloading it on first use. The
/// scanner must still list the scan, cached or not; if it doesn't, any cached
/// copy is evicted and the scan is `not_found`.
pub async fn scan_file(st: &AppState, scan_id: &str, kind: FileKind) -> Result<PathBuf, ApiError> {
    let id = safe_id(scan_id)?;
    let scans = st.scanner.scans().await?;
    let Some(s) = scans.iter().find(|s| s.scan_uuid.as_deref() == Some(id)) else {
        evict(st, id).await;
        return Err(ApiError::not_found("scan"));
    };
    let path = cache_path(&st.data_dir, id, kind);
    if path.exists() {
        return Ok(path);
    }
    let tzf_id = s.tzf_id.or_else(|| s.tzf.as_ref().and_then(|t| t.id)).ok_or_else(|| ApiError::not_found("scan file"))?;
    let rel = match kind {
        FileKind::Tzf => format!("tzfs/{tzf_id}/file"),
        FileKind::Preview => format!("tzfs/{tzf_id}/preview"),
    };
    let dir = st.data_dir.join("scans");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| ApiError::cache(format!("cache directory: {e}")))?;
    if kind == FileKind::Tzf {
        let incoming = s.tzf.as_ref().and_then(|t| t.file_size).unwrap_or(0);
        make_room(st, id, incoming, budget(st)).await;
    }
    // Dropped (and deleted) if the download fails or its caller gives up.
    let mut part = PartFile(dir.join(format!("{id}.{}.part-{}", kind.ext(), crate::util::new_id("dl"))), false);
    st.scanner.download(&rel, &part.0).await?;
    tokio::fs::rename(&part.0, &path).await.map_err(|e| ApiError::cache(format!("cache: {e}")))?;
    part.1 = true;
    Ok(path)
}

/// What a viewer build produces.
#[derive(Debug, Clone, Copy)]
pub enum ViewPart {
    /// Thinned points for drawing, at most about this many.
    Points(usize),
    /// The coverage summary (JSON).
    Coverage,
    /// The coverage plan (PNG).
    Image,
}

/// Points the viewer gets unless it asks for another number.
pub const VIEW_POINTS: usize = 2_000_000;

/// Local path of a scan's viewer points, coverage summary or coverage image,
/// built from the scan file on first use and cached. The scan must still be
/// on the scanner (as for `scan_file`). Builds run one at a time.
pub async fn view_file(st: &AppState, scan_id: &str, part: ViewPart) -> Result<PathBuf, ApiError> {
    let id = safe_id(scan_id)?;
    let _reading = reading(st, [id.to_string()]);
    let tzf = scan_file(st, id, FileKind::Tzf).await?;
    let dir = st.data_dir.join("views");
    let points = |max: usize| dir.join(format!("{id}-{max}.x9v"));
    let (cov, img) = (dir.join(format!("{id}.coverage.json")), dir.join(format!("{id}.coverage.png")));
    let (want, max) = match part {
        ViewPart::Points(m) => (points(m), m),
        ViewPart::Coverage => (cov.clone(), VIEW_POINTS),
        ViewPart::Image => (img.clone(), VIEW_POINTS),
    };
    if want.exists() {
        return Ok(want);
    }
    let _one = st.view_lock.lock().await;
    if want.exists() {
        return Ok(want);
    }
    let preview = match scan_file(st, id, FileKind::Preview).await {
        Ok(p) => tokio::fs::read(p).await.ok(),
        Err(_) => None,
    };
    let built = tokio::task::spawn_blocking(move || -> Result<crate::view::Built, String> {
        let t = trimble_tzf::Tzf::open(&tzf).map_err(|e| format!("open: {e}"))?;
        let meta = t.scan_meta().map_err(|e| format!("metadata: {e}"))?;
        let pts = t.grid_points().map_err(|e| format!("decode: {e}"))?;
        let img = preview.as_deref().and_then(|j| crate::aim::jpeg_luma(j).ok());
        let grey = crate::export::Grey::new(&meta, img);
        Ok(crate::view::build(&pts, &meta, &|p| grey.of(p), max))
    })
    .await
    .map_err(|e| ApiError::cache(format!("viewer build: {e}")))?
    .map_err(|e| ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "decode_failed", "Could not decode the scan").detail(e))?;
    tokio::fs::create_dir_all(&dir).await.map_err(|e| ApiError::cache(format!("views directory: {e}")))?;
    let mut cov_json = built.coverage;
    cov_json["scan_id"] = serde_json::json!(id);
    let cov_bytes = serde_json::to_vec_pretty(&cov_json).unwrap_or_default();
    for (path, bytes) in [(points(max), &built.view), (cov, &cov_bytes), (img, &built.image)] {
        let tmp = path.with_extension(format!("part-{}", crate::util::new_id("v")));
        tokio::fs::write(&tmp, bytes).await.map_err(|e| ApiError::cache(format!("views: {e}")))?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| ApiError::cache(format!("views: {e}")))?;
    }
    Ok(want)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------- the cache's budget (part C) ---

    fn reg(project: &str, scans: &[&str]) -> crate::register::Registration {
        crate::register::Registration {
            project_id: project.into(), frame_scan_id: None, pairs: vec![], walls: vec![], summary: String::new(), updated_at: String::new(),
            stations: scans.iter().enumerate().map(|(k, id)| crate::register::StationPose { scan_id: id.to_string(), station: k as u16 + 1, pose: None, audit: None }).collect(),
        }
    }

    /// A cached scan file of `kb` KB, last modified `age` seconds ago.
    fn put(b: &crate::testkit::Bridge, id: &str, kb: usize, age: u64) {
        let dir = b.st.data_dir.join("scans");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.tzf"));
        std::fs::write(&path, vec![0u8; kb * 1024]).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
    }

    fn left(b: &crate::testkit::Bridge) -> Vec<String> {
        let mut v: Vec<String> = cached(&b.st.data_dir).into_iter().map(|f| f.scan_id).collect();
        v.sort();
        v
    }

    fn notes(b: &crate::testkit::Bridge, start: &str) -> usize {
        b.st.inner.lock().unwrap().log.iter().filter(|e| e.text.starts_with(start)).count()
    }

    const KB: u64 = 1024;

    #[tokio::test]
    async fn under_budget_nothing_is_dropped() {
        let b = crate::testkit::Bridge::new().await;
        put(&b, "a1", 100, 50);
        put(&b, "b1", 100, 90);
        assert_eq!(make_room(&b.st, "a2", 100 * KB, 300 * KB).await, 0);
        assert_eq!(left(&b), ["a1", "b1"]);
    }

    #[tokio::test]
    async fn other_projects_go_first_oldest_first_and_this_project_stays() {
        let b = crate::testkit::Bridge::new().await;
        {
            let mut inner = b.st.inner.lock().unwrap();
            inner.registrations.insert("A".into(), reg("A", &["a1", "a2", "a3"]));
            inner.registrations.insert("B".into(), reg("B", &["b1", "b2", "b3"]));
        }
        put(&b, "a1", 100, 500); // the oldest of all, but this project's
        put(&b, "a2", 100, 10);
        put(&b, "b1", 100, 300);
        put(&b, "b2", 100, 200);
        put(&b, "b3", 100, 100);
        // 500 KB held, 100 KB coming for a3, 400 KB allowed: two of B's go, the oldest two.
        assert_eq!(make_room(&b.st, "a3", 100 * KB, 400 * KB).await, 200 * KB);
        assert_eq!(left(&b), ["a1", "a2", "b3"]);
        assert_eq!(notes(&b, "Made room in the scan cache: dropped 2 files"), 1);
        assert_eq!(notes(&b, "The scan cache needs"), 0);
    }

    #[tokio::test]
    async fn a_busy_project_keeps_its_files_and_the_log_says_so_once() {
        let b = crate::testkit::Bridge::new().await;
        {
            let mut inner = b.st.inner.lock().unwrap();
            inner.registrations.insert("A".into(), reg("A", &["a1", "a2"]));
            inner.registrations.insert("B".into(), reg("B", &["b1"]));
            inner.registering.insert("B".into(), "job_1".into()); // B is being registered: it may be reading b1
        }
        put(&b, "a1", 100, 50);
        put(&b, "b1", 100, 500);
        assert_eq!(make_room(&b.st, "a2", 100 * KB, 150 * KB).await, 0);
        assert_eq!(left(&b), ["a1", "b1"]);
        assert_eq!(notes(&b, "The scan cache needs 0 MB, over its 0 MB budget, and holds nothing it may drop. Connect the laptop"), 1);
        // Said once, not on every fetch.
        make_room(&b.st, "a2", 100 * KB, 150 * KB).await;
        assert_eq!(notes(&b, "The scan cache needs"), 1);
        // Once B is done, its file may go; back under budget, the note may come again later.
        b.st.inner.lock().unwrap().registering.clear();
        assert_eq!(make_room(&b.st, "a2", 100 * KB, 250 * KB).await, 100 * KB);
        assert!(!b.st.inner.lock().unwrap().cache_noted);
    }

    #[tokio::test]
    async fn a_file_of_no_known_project_stays_while_any_job_runs() {
        let b = crate::testkit::Bridge::new().await;
        b.st.inner.lock().unwrap().registrations.insert("A".into(), reg("A", &["a1", "a2"]));
        put(&b, "loose", 100, 500);
        put(&b, "a1", 100, 50);
        b.st.inner.lock().unwrap().registering.insert("A".into(), "job_1".into());
        assert_eq!(make_room(&b.st, "a2", 100 * KB, 150 * KB).await, 0, "a job is running: the loose file may be its");
        b.st.inner.lock().unwrap().registering.clear();
        assert_eq!(make_room(&b.st, "a2", 100 * KB, 150 * KB).await, 100 * KB);
        assert_eq!(left(&b), ["a1"]);
        // A scan of no known project keeps every file of no known project (they may be its
        // siblings); another project's file may still go.
        put(&b, "loose", 100, 500);
        assert_eq!(make_room(&b.st, "stray", 100 * KB, 150 * KB).await, 100 * KB);
        assert_eq!(left(&b), ["loose"]);
    }

    /// A station whose 5 cm cloud is prepared on the Pi.
    fn prepared(b: &crate::testkit::Bridge, id: &str) {
        let dir = b.st.data_dir.join("registration");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::worker::cloud_name(id)), b"cloud").unwrap();
    }

    #[tokio::test]
    async fn this_projects_files_the_laptop_holds_go_once_their_clouds_are_prepared() {
        let b = crate::testkit::Bridge::new().await;
        {
            let mut inner = b.st.inner.lock().unwrap();
            inner.registrations.insert("A".into(), reg("A", &["a1", "a2", "a3", "a4", "a5"]));
            inner.registering.insert("A".into(), "job_1".into()); // busy: the first tier keeps all of A
        }
        put(&b, "a1", 100, 500); // held and prepared, but being read
        put(&b, "a2", 100, 400); // held and prepared: goes first
        put(&b, "a3", 100, 300); // held, not prepared: the Pi still needs it
        put(&b, "a4", 100, 200); // prepared, not held
        for id in ["a1", "a2", "a4"] {
            prepared(&b, id);
        }
        // No laptop: nothing may go.
        assert_eq!(make_room(&b.st, "a5", 100 * KB, 350 * KB).await, 0);
        // The laptop says what it holds.
        {
            let mut w = b.st.work.lock().unwrap();
            w.claim("laptop", "agent");
            w.note_holds("laptop", ["a1", "a2", "a3"].map(String::from).into());
        }
        let _r = reading(&b.st, ["a1".to_string()]);
        assert_eq!(make_room(&b.st, "a5", 100 * KB, 350 * KB).await, 100 * KB);
        assert_eq!(left(&b), ["a1", "a3", "a4"]);
        assert_eq!(notes(&b, "Made room in the scan cache: dropped 1 file (0 MB) the laptop holds; their stations stay registered"), 1);
        // Still over, with nothing more it may drop: the note says so.
        assert_eq!(make_room(&b.st, "a5", 100 * KB, 250 * KB).await, 0);
        assert_eq!(notes(&b, "The scan cache needs"), 1);
        // Once read, a1 may go too.
        drop(_r);
        assert!(b.st.inner.lock().unwrap().reading.is_empty());
        assert_eq!(make_room(&b.st, "a5", 100 * KB, 250 * KB).await, 100 * KB);
        assert_eq!(left(&b), ["a3", "a4"]);
    }

    #[tokio::test]
    async fn a_file_being_read_stays_even_in_the_first_tier() {
        let b = crate::testkit::Bridge::new().await;
        b.st.inner.lock().unwrap().registrations.insert("B".into(), reg("B", &["b1", "b2"]));
        b.st.inner.lock().unwrap().registrations.insert("A".into(), reg("A", &["a1"]));
        put(&b, "b1", 100, 500);
        put(&b, "b2", 100, 400);
        let r1 = reading(&b.st, ["b1".to_string()]);
        let r2 = reading(&b.st, ["b1".to_string()]);
        assert_eq!(make_room(&b.st, "a1", 100 * KB, 150 * KB).await, 100 * KB);
        assert_eq!(left(&b), ["b1"]);
        drop(r1);
        assert_eq!(b.st.inner.lock().unwrap().reading.get("b1"), Some(&1), "two readers, one left");
        drop(r2);
        assert_eq!(make_room(&b.st, "a1", 100 * KB, 50 * KB).await, 100 * KB);
    }

    #[test]
    fn ids_cannot_escape_the_cache() {
        assert!(safe_id("5b5314cf-76b8-4f59-a445-ceee1e6df4c3").is_ok());
        for bad in ["", "../etc/passwd", "a/b", "x".repeat(65).as_str()] {
            assert_eq!(safe_id(bad).unwrap_err().code, "not_found", "{bad:?}");
        }
    }

    #[test]
    fn part_file_is_removed_unless_kept() {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-part"));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.part"), dir.join("b.part"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        drop(PartFile(a.clone(), false));
        drop(PartFile(b.clone(), true));
        assert!(!a.exists() && b.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    // ------------------------------------------------- the cache on the mock ---

    use crate::testkit::{fast_mock, Bridge};
    use crate::worker::tests::{dead_base, junk_fixtures, SCAN0};
    use serde_json::json;

    /// A scan id the mock does not list.
    const GONE: &str = "0000aaaa-1111-2222-3333-444455556666";

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"cached").unwrap();
    }

    #[tokio::test]
    async fn a_scan_file_and_preview_are_downloaded_once_then_served_from_the_cache() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let tzf = scan_file(&b.st, SCAN0, FileKind::Tzf).await.unwrap();
        assert_eq!(tzf, b.dir.join(format!("data/scans/{SCAN0}.tzf")));
        assert_eq!(std::fs::read(&tzf).unwrap(), b"not a real scan file");
        let jpg = scan_file(&b.st, SCAN0, FileKind::Preview).await.unwrap();
        assert!(jpg.to_string_lossy().ends_with(".jpg"));
        // Cached: the scanner's copy is not fetched again.
        std::fs::write(&tzf, b"from the cache").unwrap();
        let again = scan_file(&b.st, SCAN0, FileKind::Tzf).await.unwrap();
        assert_eq!(std::fs::read(again).unwrap(), b"from the cache");
        // No partial downloads left.
        assert_eq!(std::fs::read_dir(b.dir.join("data/scans")).unwrap().count(), 2);
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test]
    async fn a_failed_download_leaves_no_partial_file() {
        // The mock has no file to serve.
        let b = Bridge::new().await;
        let e = scan_file(&b.st, SCAN0, FileKind::Tzf).await.unwrap_err();
        assert_eq!(e.code, "scanner_error", "{e:?}");
        assert_eq!(std::fs::read_dir(b.dir.join("data/scans")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_scan_the_scanner_no_longer_lists_is_not_found_and_evicted() {
        let b = Bridge::new().await;
        let cached = cache_path(&b.st.data_dir, GONE, FileKind::Tzf);
        touch(&cached);
        let e = scan_file(&b.st, GONE, FileKind::Tzf).await.unwrap_err();
        assert_eq!(e.code, "not_found");
        assert!(!cached.exists());
        // Ids that could leave the cache never reach the scanner.
        assert_eq!(scan_file(&b.st, "../grants.json", FileKind::Tzf).await.unwrap_err().code, "not_found");
    }

    #[tokio::test]
    async fn an_unreachable_scanner_serves_nothing_even_from_the_cache() {
        let b = Bridge::on_scanner(&dead_base().await);
        touch(&cache_path(&b.st.data_dir, SCAN0, FileKind::Tzf));
        let e = scan_file(&b.st, SCAN0, FileKind::Tzf).await.unwrap_err();
        assert_eq!(e.code, "scanner_disconnected");
    }

    #[tokio::test]
    async fn an_unwritable_cache_directory_is_a_cache_error() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        std::fs::write(b.dir.join("data/scans"), b"in the way").unwrap();
        let e = scan_file(&b.st, SCAN0, FileKind::Tzf).await.unwrap_err();
        assert_eq!(e.code, "cache_error");
        assert!(e.detail.as_deref().unwrap().starts_with("cache directory"), "{e:?}");
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test]
    async fn a_scan_listed_without_a_file_id_has_no_file() {
        use axum::{routing::get, Json, Router};
        let app = Router::new().route("/api/v1/scans", get(|| async { Json(json!([{"scanUUID": SCAN0}])) }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let b = Bridge::on_scanner(&format!("http://{addr}/api/v1"));
        let e = scan_file(&b.st, SCAN0, FileKind::Preview).await.unwrap_err();
        assert_eq!((e.code, e.title.as_str()), ("not_found", "No such scan file"), "{e:?}");
    }

    #[tokio::test]
    async fn evicting_a_scan_removes_its_files_exports_and_measurements_only() {
        let b = Bridge::with_mock(fast_mock()).await;
        let d = &b.st.data_dir;
        let mine = [
            cache_path(d, GONE, FileKind::Tzf),
            cache_path(d, GONE, FileKind::Preview),
            d.join(format!("views/{GONE}-2000000.x9v")),
            d.join(format!("views/{GONE}.coverage.json")),
            d.join("exports/exp_1.ply"),
            d.join("measurements/msr_1.png"),
        ];
        let others = [cache_path(d, SCAN0, FileKind::Tzf), d.join(format!("views/{SCAN0}.coverage.png")), d.join("exports/exp_2.ply")];
        for p in mine.iter().chain(&others) {
            touch(p);
        }
        {
            let mut inner = b.st.inner.lock().unwrap();
            inner.exports.insert("exp_1".into(), json!({"scan_id": GONE}));
            inner.export_files.insert("exp_1".into(), mine[4].clone());
            inner.exports.insert("exp_2".into(), json!({"scan_id": SCAN0}));
            inner.export_files.insert("exp_2".into(), others[2].clone());
            inner.measurements.insert("msr_1".into(), json!({"scan_id": GONE}));
            inner.measurement_files.insert("msr_1".into(), mine[5].clone());
            inner.next_stations.insert(GONE.into(), (json!({}), vec![]));
        }
        evict(&b.st, GONE).await;
        for p in &mine {
            assert!(!p.exists(), "{p:?}");
        }
        for p in &others {
            assert!(p.exists(), "{p:?}");
        }
        let inner = b.st.inner.lock().unwrap();
        assert!(!inner.exports.contains_key("exp_1") && inner.exports.contains_key("exp_2"));
        assert!(inner.measurements.is_empty() && inner.measurement_files.is_empty());
        assert!(inner.next_stations.is_empty());
        drop(inner);
        // A bad id is ignored.
        evict(&b.st, "../data").await;
        assert!(b.st.data_dir.exists());
    }

    #[tokio::test]
    async fn a_viewer_build_of_a_junk_scan_file_fails_to_decode() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        for part in [ViewPart::Points(VIEW_POINTS), ViewPart::Coverage, ViewPart::Image] {
            let e = view_file(&b.st, SCAN0, part).await.unwrap_err();
            assert_eq!(e.code, "decode_failed", "{part:?}");
            assert!(e.detail.as_deref().unwrap().starts_with("open: "), "{e:?}");
        }
        assert!(!b.st.data_dir.join("views").exists());
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test]
    async fn built_viewer_files_are_served_from_the_cache() {
        let (fx, cfg) = junk_fixtures();
        let b = Bridge::with_mock(cfg).await;
        let views = b.st.data_dir.join("views");
        let built = [
            (ViewPart::Points(50_000), views.join(format!("{SCAN0}-50000.x9v"))),
            (ViewPart::Coverage, views.join(format!("{SCAN0}.coverage.json"))),
            (ViewPart::Image, views.join(format!("{SCAN0}.coverage.png"))),
        ];
        for (_, p) in &built {
            touch(p);
        }
        for (part, p) in built {
            assert_eq!(view_file(&b.st, SCAN0, part).await.unwrap(), p);
        }
        // Ids are checked first; scans the scanner dropped are not served.
        assert_eq!(view_file(&b.st, "nope/..", ViewPart::Coverage).await.unwrap_err().code, "not_found");
        assert_eq!(view_file(&b.st, GONE, ViewPart::Coverage).await.unwrap_err().code, "not_found");
        std::fs::remove_dir_all(fx).unwrap();
    }
}
