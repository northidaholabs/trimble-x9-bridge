//! A station's points as the measurements read them: decoded from its scan
//! file, kept in memory when there is room, and decoded several at a time
//! when a job needs many.
//!
//! Decoding a full-resolution station is most of what a question check costs
//! (about 1.3 s each on a laptop; a wall seen by 7 stations is 9 s of it), and
//! a check after every registration reads the same stations again. The
//! laptop worker therefore keeps decoded stations in memory, least recently
//! used dropped first, within a budget it is given (`x9-bridge worker
//! --mem-gb`), and decodes the stations a job needs on several cores at once.
//!
//! The bridge itself keeps nothing and decodes one station at a time, as
//! before: on the Pi memory is short. Either way the points come back in the
//! same order with the same values, so every sum, and so every reading, is
//! the same as when each station is decoded on its own.

use anyhow::Context;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A station's points that a measurement can use: finite and at least 0.5 m
/// from the scanner, in the station's frame, in the scan file's order.
pub(crate) struct Points {
    pub(crate) pts: Vec<[f32; 3]>,
    /// Every point in the file, used or not.
    pub(crate) total: usize,
}

/// The points a measurement uses, in the file's order.
fn usable(grid: &[trimble_tzf::GridPoint]) -> impl Iterator<Item = [f32; 3]> + '_ {
    grid.iter().map(|g| [g.point.x, g.point.y, g.point.z]).filter(|p| p.iter().all(|v| v.is_finite()) && range(*p) >= 0.5)
}

impl Points {
    /// Only for keeping: it holds a copy of the usable points.
    pub(crate) fn from_grid(grid: &[trimble_tzf::GridPoint]) -> Points {
        let mut pts = Vec::with_capacity(grid.len());
        pts.extend(usable(grid));
        pts.shrink_to_fit();
        Points { pts, total: grid.len() }
    }

    /// What it holds in memory.
    fn bytes(&self) -> u64 {
        (self.pts.capacity() * std::mem::size_of::<[f32; 3]>()) as u64 + 64
    }
}

/// The point's distance from the scanner, as the measurements compute it.
pub(crate) fn range(p: [f32; 3]) -> f64 {
    (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() as f64
}

/// The scan files decoded, in order (tests look for their own).
#[cfg(test)]
pub(crate) static DECODED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn decode(f: &Path) -> anyhow::Result<Vec<trimble_tzf::GridPoint>> {
    let t = trimble_tzf::Tzf::open(f).with_context(|| format!("opening {}", f.display()))?;
    let grid = t.grid_points().with_context(|| format!("{}: decoding", f.display()))?;
    #[cfg(test)]
    DECODED.lock().unwrap_or_else(|e| e.into_inner()).push(f.to_path_buf());
    Ok(grid)
}

/// A scan file on disk, by what changes when it is replaced: the worker
/// writes a new file and renames it into place, which gives it a new inode.
/// Not its modified time: the worker sets that whenever it uses the file, to
/// keep its disk cache's least recently used order.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    len: u64,
    dev: u64,
    ino: u64,
}

impl Key {
    fn of(f: &Path) -> Option<Key> {
        let m = std::fs::metadata(f).ok()?;
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, m.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64);
        Some(Key { path: f.to_path_buf(), len: m.len(), dev, ino })
    }
}

/// A station ready to walk: its kept points, or, with nothing kept, the
/// decoded grid itself, walked as the bridge always has without a copy.
pub(crate) enum Station {
    Kept(Arc<Points>),
    Grid(Vec<trimble_tzf::GridPoint>),
}

impl Station {
    /// Every point in the file, used or not.
    pub(crate) fn total(&self) -> usize {
        match self {
            Station::Kept(p) => p.total,
            Station::Grid(g) => g.len(),
        }
    }

    /// Walk the usable points as `analyze::station_points` always has: the
    /// point in the station's frame, in the project's, and its range.
    pub(crate) fn walk(&self, pose: &crate::register::Pose, mut each: impl FnMut([f32; 3], [f64; 3], f64)) {
        let mut one = |q: [f32; 3], r: f64| {
            let w = pose.apply(q);
            each(q, [w[0] as f64, w[1] as f64, w[2] as f64], r);
        };
        match self {
            Station::Kept(p) => p.pts.iter().for_each(|&q| one(q, range(q))),
            // One pass, each range worked out once, as the bridge always has.
            Station::Grid(g) => {
                for x in g {
                    let q = [x.point.x, x.point.y, x.point.z];
                    if !q.iter().all(|v| v.is_finite()) {
                        continue;
                    }
                    let r = range(q);
                    if r >= 0.5 {
                        one(q, r);
                    }
                }
            }
        }
    }
}

/// Decoded stations kept within `budget` bytes, the least recently used
/// dropped first.
struct Memory {
    budget: u64,
    used: u64,
    tick: u64,
    kept: HashMap<Key, (Arc<Points>, u64)>,
}

impl Memory {
    fn new() -> Memory {
        Memory { budget: 0, used: 0, tick: 0, kept: HashMap::new() }
    }

    fn get(&mut self, k: &Key) -> Option<Arc<Points>> {
        self.tick += 1;
        let tick = self.tick;
        self.kept.get_mut(k).map(|(p, used)| {
            *used = tick;
            p.clone()
        })
    }

    fn put(&mut self, k: Key, p: Arc<Points>) {
        let size = p.bytes();
        if size > self.budget || self.kept.contains_key(&k) {
            return;
        }
        while self.used + size > self.budget {
            let Some(oldest) = self.kept.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) else { break };
            if let Some((q, _)) = self.kept.remove(&oldest) {
                self.used -= q.bytes();
            }
        }
        self.tick += 1;
        self.used += size;
        self.kept.insert(k, (p, self.tick));
    }

    fn set_budget(&mut self, budget: u64) {
        self.budget = budget;
        while self.used > budget {
            let Some(oldest) = self.kept.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) else { break };
            if let Some((q, _)) = self.kept.remove(&oldest) {
                self.used -= q.bytes();
            }
        }
    }
}

static MEMORY: std::sync::LazyLock<Mutex<Memory>> = std::sync::LazyLock::new(|| Mutex::new(Memory::new()));
static BUDGET: AtomicU64 = AtomicU64::new(0);
static THREADS: AtomicUsize = AtomicUsize::new(1);

/// Keep up to `budget` bytes of decoded stations, and decode up to `threads`
/// stations at once. The bridge leaves both at none and one; the laptop
/// worker sets them from `--mem-gb` and its cores.
pub fn configure(budget: u64, threads: usize) {
    BUDGET.store(budget, Ordering::Relaxed);
    THREADS.store(threads.max(1), Ordering::Relaxed);
    MEMORY.lock().unwrap().set_budget(budget);
}

/// What is kept now: stations, bytes, and the budget.
pub fn kept() -> (usize, u64, u64) {
    let m = MEMORY.lock().unwrap();
    (m.kept.len(), m.used, m.budget)
}

/// The station: from memory, or decoded. With a budget it is kept (when
/// there is room); without one, the decoded grid is handed over as it is.
pub(crate) fn get(f: &Path) -> anyhow::Result<Station> {
    let key = if BUDGET.load(Ordering::Relaxed) > 0 { Key::of(f) } else { None };
    let Some(k) = key else { return Ok(Station::Grid(decode(f)?)) };
    if let Some(p) = MEMORY.lock().unwrap().get(&k) {
        return Ok(Station::Kept(p));
    }
    let p = Arc::new(Points::from_grid(&decode(f)?));
    MEMORY.lock().unwrap().put(k, p.clone());
    Ok(Station::Kept(p))
}

/// Keep points already decoded for another reason (registration decodes the
/// newest station), so the next check finds them.
pub(crate) fn remember(f: &Path, grid: &[trimble_tzf::GridPoint]) {
    if BUDGET.load(Ordering::Relaxed) == 0 {
        return;
    }
    if let Some(k) = Key::of(f) {
        MEMORY.lock().unwrap().put(k, Arc::new(Points::from_grid(grid)));
    }
}

/// Hand each file's points to `visit`, in the files' order. Up to the
/// configured number of stations are decoded at once; with one, a station is
/// decoded, visited and let go before the next, as the bridge always has.
pub(crate) fn each_station(files: &[&Path], mut visit: impl FnMut(usize, &Station) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let threads = THREADS.load(Ordering::Relaxed).max(1);
    let mut k = 0;
    for batch in files.chunks(threads) {
        let loaded: Vec<anyhow::Result<Station>> = if batch.len() == 1 {
            vec![get(batch[0])]
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = batch.iter().map(|f| s.spawn(move || get(f))).collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("decoding a station panicked")))).collect()
            })
        };
        for p in loaded {
            let p = p?;
            visit(k, &p)?;
            k += 1;
        }
    }
    Ok(())
}


/// Tests that change what is kept take this, so they don't change it under
/// each other.
#[cfg(test)]
pub(crate) static CONFIGURED: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openings::tests::{facade_scans, scene};

    fn facade(tag: &str) -> Vec<(u16, crate::register::Pose, PathBuf)> {
        let dir = crate::export::tests::scratch(tag);
        facade_scans(&scene(), &dir, &[([-2.0, -1.0, 0.0], 0.0), ([-3.0, 2.0, 0.0], 0.2), ([-1.5, 0.5, 0.0], -0.1)])
    }

    /// Every point each station gives, as the measurements walk them.
    fn walked(files: &[(u16, crate::register::Pose, PathBuf)]) -> Vec<(usize, [f32; 3], [u64; 3], u64)> {
        let paths: Vec<&Path> = files.iter().map(|f| f.2.as_path()).collect();
        let mut out = vec![];
        each_station(&paths, |i, p| {
            p.walk(&files[i].1, |q, w, r| out.push((i, q, w.map(f64::to_bits), r.to_bits())));
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn kept_or_not_and_decoded_together_or_not_the_points_and_readings_are_the_same() {
        let _g = CONFIGURED.lock().unwrap_or_else(|e| e.into_inner());
        let files = facade("points-same");
        let wall = crate::ask::face_wall("front", &crate::register::Face { from: [5.0, -5.0], to: [5.0, 5.0] }, [-1.5, 1.5], vec![1, 2, 3]);
        let mut seen = vec![];
        for (budget, threads) in [(0, 1), (1 << 32, 3), (1 << 32, 3), (1 << 32, 2)] {
            configure(budget, threads);
            let fits = crate::analyze::measure_faces_with(std::slice::from_ref(&wall), &files, &crate::analyze::Opts::default(), 2).unwrap();
            seen.push((walked(&files), fits[0].to_bytes()));
        }
        configure(0, 1);
        assert!(!seen[0].0.is_empty());
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "the points or the fit changed with what is kept");
    }

    fn kept_arc(f: &Path) -> Option<Arc<Points>> {
        match get(f).unwrap() {
            Station::Kept(p) => Some(p),
            Station::Grid(_) => None,
        }
    }

    #[test]
    fn a_kept_station_is_read_from_memory_and_a_replaced_file_is_decoded_again() {
        let _g = CONFIGURED.lock().unwrap_or_else(|e| e.into_inner());
        let files = facade("points-kept");
        let f = files[0].2.as_path();
        configure(0, 1);
        assert!(kept_arc(f).is_none(), "nothing is kept without a budget: the grid is walked as it is");
        configure(1 << 32, 2);
        let of_f = || DECODED.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|d| d.as_path() == f).count();
        let before = of_f();
        let a = kept_arc(f).unwrap();
        assert!(Arc::ptr_eq(&a, &kept_arc(f).unwrap()), "a kept station isn't decoded again");
        assert_eq!(a.total, crate::analyze::station_points(f, &files[0].1, |_, _, _| {}).unwrap());
        assert_eq!(a.bytes(), (a.pts.capacity() * 12) as u64 + 64);
        assert_eq!(a.pts.capacity(), a.pts.len(), "kept points hold no spare room");
        // Used again with its time set (as the worker's disk cache does): still kept.
        std::fs::File::options().write(true).open(f).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
        assert!(Arc::ptr_eq(&a, &kept_arc(f).unwrap()), "touching the file isn't replacing it");
        assert_eq!(of_f() - before, 1, "decoded once while kept");
        // Replaced as the worker replaces a file (written beside it, renamed in): decoded again.
        let part = f.with_extension("tzf.part");
        std::fs::copy(&files[1].2, &part).unwrap();
        std::fs::rename(&part, f).unwrap();
        assert!(!Arc::ptr_eq(&a, &kept_arc(f).unwrap()));
        // Registration's decode is kept for the check after it.
        let g = files[2].2.as_path();
        let grid = trimble_tzf::Tzf::open(g).unwrap().grid_points().unwrap();
        remember(g, &grid);
        let r = kept_arc(g).unwrap();
        assert!(Arc::ptr_eq(&r, &kept_arc(g).unwrap()) && r.total == grid.len());
        let (n, used, budget) = kept();
        assert!(n >= 2 && used > 0 && budget == 1 << 32);
        configure(0, 1);
        remember(g, &grid); // without a budget, nothing
        assert_eq!(kept().0, 0);
    }

    #[test]
    fn counts_are_the_same_kept_or_not_and_decoded_together_or_not() {
        let _g = CONFIGURED.lock().unwrap_or_else(|e| e.into_inner());
        let files = facade("points-count");
        let spec = crate::count::FaceSpec {
            face_id: "front".into(),
            label: "Front".into(),
            face: crate::register::Face { from: [5.0, -5.0], to: [5.0, 5.0] },
            height_m: Some([-1.0, 1.0]),
            seen_share: 1.0,
            stations: vec![1, 2, 3],
        };
        let mut seen = vec![];
        for (budget, threads) in [(0, 1), (0, 3), (1 << 32, 3), (1 << 32, 3)] {
            configure(budget, threads);
            let reads = crate::count::measure(std::slice::from_ref(&spec), &files).unwrap();
            seen.push(reads.iter().map(|(found, png)| (serde_json::to_string(found).unwrap(), png.clone())).collect::<Vec<_>>());
        }
        configure(0, 1);
        assert!(seen[0][0].0.contains("openings") && seen[0][0].1.is_some(), "{:?}", seen[0][0].0);
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "the count changed with what is kept");
    }

    #[test]
    fn a_station_that_cannot_be_read_stops_the_walk_with_its_error() {
        let _g = CONFIGURED.lock().unwrap_or_else(|e| e.into_inner());
        let files = facade("points-missing");
        let missing = files[0].2.with_file_name("nowhere.tzf");
        for threads in [1, 3] {
            configure(1 << 32, threads);
            let paths = [files[1].2.as_path(), missing.as_path(), files[2].2.as_path()];
            let mut visited = vec![];
            let e = each_station(&paths, |i, _| {
                visited.push(i);
                Ok(())
            })
            .unwrap_err();
            assert!(format!("{e:#}").contains("nowhere.tzf"), "{e:#}");
            assert_eq!(visited, vec![0], "stations are visited in order, up to the one that failed");
        }
        configure(0, 1);
    }

    fn pts(n: usize) -> Arc<Points> {
        Arc::new(Points { pts: vec![[1.0, 2.0, 3.0]; n], total: n })
    }
    fn key(name: &str) -> Key {
        Key { path: PathBuf::from(name), len: 1, dev: 1, ino: 1 }
    }

    #[test]
    fn memory_keeps_within_its_budget_dropping_the_least_recently_used() {
        let mut m = Memory::new();
        let one = pts(100).bytes();
        m.set_budget(one * 2);
        m.put(key("a"), pts(100));
        m.put(key("b"), pts(100));
        assert!(m.get(&key("a")).is_some()); // a is now the more recent
        m.put(key("c"), pts(100));
        assert!(m.get(&key("b")).is_none(), "b was the least recently used");
        assert!(m.get(&key("a")).is_some() && m.get(&key("c")).is_some());
        assert!(m.used <= m.budget);
        // Too big for the budget: not kept, nothing dropped for it.
        m.put(key("big"), pts(1000));
        assert!(m.get(&key("big")).is_none() && m.kept.len() == 2);
        // A smaller budget drops the oldest until it fits.
        m.set_budget(one);
        assert_eq!(m.kept.len(), 1);
        m.set_budget(0);
        assert!(m.kept.is_empty() && m.used == 0);
    }

    #[test]
    fn a_new_projects_stations_push_out_the_old_ones_first() {
        let mut m = Memory::new();
        let one = pts(10).bytes();
        m.set_budget(one * 3);
        for s in ["old-1", "old-2", "old-3"] {
            m.put(key(s), pts(10));
        }
        for s in ["new-1", "new-2"] {
            m.put(key(s), pts(10));
            assert!(m.get(&key(s)).is_some());
        }
        assert!(m.get(&key("old-1")).is_none() && m.get(&key("old-2")).is_none());
        assert!(m.get(&key("old-3")).is_some(), "the last old station stays while there is room");
    }

    #[test]
    fn a_replaced_file_is_another_file() {
        let dir = std::env::temp_dir().join(format!("x9-points-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.tzf");
        std::fs::write(&f, b"one").unwrap();
        let a = Key::of(&f).unwrap();
        // Its time set, as when the worker uses it again: the same file.
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
        assert!(a == Key::of(&f).unwrap());
        // Another file renamed into its place, the same length: another file.
        let part = dir.join("s.tzf.part");
        std::fs::write(&part, b"two").unwrap();
        std::fs::rename(&part, &f).unwrap();
        assert!(a != Key::of(&f).unwrap());
        assert!(Key::of(&dir.join("missing.tzf")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
