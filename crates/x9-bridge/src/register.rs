//! Registration: a project's stations in one frame. The X9 levels every scan,
//! so a station's pose is a turn about vertical and a shift (x, y, z): four
//! numbers, not six. Stations are matched in pairs, then adjusted together:
//!
//! 1. Coarse: wall-like points (in a height band around the scanner, locally
//!    flat, with a horizontal normal; ground and trees drop out) as a plan
//!    image; every turn in 1° steps, the shift by FFT cross-correlation; the
//!    best few turns at least 10° apart, refined to 0.05°.
//! 2. Fine: point-to-plane ICP on 10 cm-thinned points from each guess,
//!    turn and shift together with a shrinking match distance; the guess that
//!    fits best wins. A pair counts when its residual is under 15 mm over at
//!    least 8000 matched flat points.
//! 3. Global: every pair that counts is a constraint, and all stations are
//!    adjusted together (the first fixed) with a robust (Cauchy) loss, so one
//!    wrong pair can't drag the rest and loops close.
//!
//! The project frame is the first station's: x and y as that scanner happened
//! to face (not north), z up, the origin at its centre. From the registered
//! stations come the walls (straight runs in the merged plan) and, for any
//! wall face, how much of its length some station saw and how finely.

use rustfft::{num_complex::Complex, FftPlanner};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use trimble_tzf::GridPoint;

/// Station clouds: one point per 5 cm cube (the centroid; 7.5 cm beyond
/// `NEAR_M`) within `RANGE`.
pub const VOXEL: f32 = 0.05;
/// 60 m: on a large site (240 × 320 m) stations 20–40 m
/// apart share little within 30 m; at 60 m they tie firmly.
pub const RANGE: f32 = 60.0;
/// Matching works on 10 cm-thinned points (15 cm beyond `NEAR_M`).
const ICP_VOXEL: f32 = 0.10;
/// Height band of the coarse plan slice, relative to the scanner.
const SLICE: (f32, f32) = (-1.0, 1.5);
/// Coarse plan cells: 25 cm keeps the 60 m search to a 960² FFT.
const COARSE_CELL: f32 = 0.25;
const HYPOTHESES: usize = 5;
/// Points of the second station ICP iterates on (all are used for the final fit).
const ICP_SAMPLE: usize = 40_000;
pub const GOOD_RMS_M: f64 = 0.015;
pub const GOOD_MATCHES: usize = 8000;
/// A pair may be used from this many matches when it is held firmly and
/// unique: indoors, two stations either side of a doorway share little
/// (2913 matches, fitting within 1 mm by every independent check). Pairs
/// count in the global fit by their matches, and only fits with
/// `GOOD_MATCHES` count as evidence against a station in the audit.
pub const PAIR_MIN_MATCHES: usize = 2000;
/// A pair the global fit leaves further off than this is an outlier.
const OUTLIER_M: f64 = 0.10;
const OUTLIER_DEG: f64 = 0.3;
/// A pair is held firmly when its weakest direction carries at least this
/// share of the matches (a turn counted at a 10 m arm). Below it the fit can
/// slide (along a lone wall, a fence, a corridor) and still look perfect.
pub const CONSTRAINT_MIN: f64 = 0.01;
const TURN_ARM: f64 = 10.0;
/// Two fits differ when this far apart; a different fit is a rival when it
/// also fits and has this share of the best one's matches.
const DISTINCT_M: f64 = 0.3;
const DISTINCT_DEG: f64 = 2.0;
const RIVAL_SHARE: f64 = 0.8;
/// Independent checks between placed stations (`audit`): the share of one
/// station's flat points lying where the other saw straight through, how far
/// its surfaces sit off the other's, and the ground under its tripod.
const FREE_SPACE_BAD: f64 = 0.05;
/// Rival fits: one loses when it puts at least this share in free space and
/// three times as much as the other (correct stations on a recorded
/// survey stay under 0.02).
const FREE_SPACE_RIVAL: f64 = 0.03;
const SURFACE_BAD_MM: f64 = 20.0;
/// A re-fit against a neighbour, from the final poses, may move a station
/// this much before it disagrees (pairs that count are left within 10 mm).
const REFIT_BAD_MM: f64 = 15.0;
const REFIT_BAD_DEG: f64 = 0.08;
/// When judging whether other pairs back a join (not a station's own
/// votes), the millimetre limits grow with the distance between the two
/// stations: a turn the angle limit allows moves a station that far away by
/// its lever arm (0.08° is 1.4 mm per metre). On the admin building, good
/// pairs 22-38 m apart read 25-36 mm with turns under 0.05°. Surfaces get
/// half the slope. A station's own votes keep the fixed limits: with the
/// wider ones, 3 cm planted errors went uncaught (bench, 2026-10-24).
const REFIT_BAD_PER_M: f64 = REFIT_BAD_DEG * std::f64::consts::PI / 180.0 * 1000.0;
const SURFACE_BAD_PER_M: f64 = REFIT_BAD_PER_M / 2.0;

/// The most a fresh fit may move a station `apart_m` from its neighbour (mm).
fn refit_limit_mm(apart_m: f64) -> f64 {
    REFIT_BAD_MM + REFIT_BAD_PER_M * apart_m
}

/// The most a station's surfaces may sit off a neighbour's `apart_m` away (mm).
fn surface_limit_mm(apart_m: f64) -> f64 {
    SURFACE_BAD_MM + SURFACE_BAD_PER_M * apart_m
}
/// Coarse: on grass a far station reads the ground a few centimetres high.
const GROUND_BAD_M: f64 = 0.10;
/// Direction bins of a station's depth map (0.5°).
const NAZ: usize = 720;
const NEL: usize = 360;

// ------------------------------------------------------------------ clouds ---

/// A station's points (scanner frame, levelled), one per 5 cm cube, with the
/// nearest range seen in each (for point spacing) and the scan's angular step.
pub struct Cloud {
    pub step_rad: f32,
    pub pts: Vec<[f32; 3]>,
    pub range: Vec<f32>,
}

const CLOUD_MAGIC: &[u8; 4] = b"X9C1";

fn key(p: [f32; 3], v: f32) -> u64 {
    let k = |x: f32| ((x / v).floor() as i64 + (1 << 20)) as u64 & 0x1F_FFFF;
    (k(p[0]) << 42) | (k(p[1]) << 21) | k(p[2])
}

/// Beyond this horizontal range points are kept in cells 1.5 times as large:
/// the scan is sparser out there anyway, and on a big site they are many.
/// (Twice as large lost the far ties a big site's walk closes its loop with.)
pub const NEAR_M: f32 = 30.0;

/// A point's thinning cell: `v` near, `1.5v` beyond `NEAR_M` (marked by the top
/// bit, so near and far cells never share a key).
fn thin_key(p: [f32; 3], v: f32) -> u64 {
    if p[0].hypot(p[1]) <= NEAR_M {
        key(p, v)
    } else {
        key(p, 1.5 * v) | (1 << 63)
    }
}

impl Cloud {
    pub fn from_grid(pts: &[GridPoint], step_rad: f64) -> Cloud {
        let mut acc: HashMap<u64, ([f64; 3], u32, f32)> = HashMap::with_capacity(pts.len() / 8);
        for g in pts {
            let p = [g.point.x, g.point.y, g.point.z];
            if !p.iter().all(|v| v.is_finite()) {
                continue;
            }
            let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            if r < 0.5 || p[0].hypot(p[1]) > RANGE {
                continue;
            }
            let e = acc.entry(thin_key(p, VOXEL)).or_insert(([0.0; 3], 0, f32::MAX));
            for (sum, v) in e.0.iter_mut().zip(p) {
                *sum += v as f64;
            }
            e.1 += 1;
            e.2 = e.2.min(r);
        }
        let mut v: Vec<(u64, [f32; 3], f32)> =
            acc.into_iter().map(|(k, (s, n, r))| (k, [(s[0] / n as f64) as f32, (s[1] / n as f64) as f32, (s[2] / n as f64) as f32], r)).collect();
        v.sort_unstable_by_key(|x| x.0);
        Cloud { step_rad: step_rad as f32, pts: v.iter().map(|x| x.1).collect(), range: v.iter().map(|x| x.2).collect() }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + self.pts.len() * 16);
        b.extend_from_slice(CLOUD_MAGIC);
        b.extend_from_slice(&(self.pts.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.step_rad.to_le_bytes());
        for p in &self.pts {
            for v in p {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        for r in &self.range {
            b.extend_from_slice(&r.to_le_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Result<Cloud, String> {
        let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        if b.len() < 12 || &b[..4] != CLOUD_MAGIC {
            return Err("not a station cloud".into());
        }
        let n = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        if b.len() != 12 + n * 16 {
            return Err("station cloud is truncated".into());
        }
        let pts = (0..n).map(|i| [f(12 + i * 12), f(16 + i * 12), f(20 + i * 12)]).collect();
        let range = (0..n).map(|i| f(12 + n * 12 + i * 4)).collect();
        Ok(Cloud { step_rad: f(8), pts, range })
    }
}

// --------------------------------------------------------------- kd-tree ---

/// A static 3-D kd-tree (implicit: each range's middle element splits it).
pub struct Tree {
    pts: Vec<[f32; 3]>,
    idx: Vec<u32>,
}

fn d2(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

impl Tree {
    pub fn new(pts: &[[f32; 3]]) -> Tree {
        let mut idx: Vec<u32> = (0..pts.len() as u32).collect();
        fn build(pts: &[[f32; 3]], idx: &mut [u32], depth: usize) {
            if idx.len() <= 1 {
                return;
            }
            let mid = idx.len() / 2;
            let d = depth % 3;
            idx.select_nth_unstable_by(mid, |a, b| pts[*a as usize][d].total_cmp(&pts[*b as usize][d]));
            let (lo, hi) = idx.split_at_mut(mid);
            build(pts, lo, depth + 1);
            build(pts, &mut hi[1..], depth + 1);
        }
        build(pts, &mut idx, 0);
        let sorted = idx.iter().map(|&i| pts[i as usize]).collect();
        Tree { pts: sorted, idx }
    }

    /// The nearest point within `r` (index into the original slice, distance²).
    pub fn nearest(&self, q: [f32; 3], r: f32) -> Option<(usize, f32)> {
        let mut best = (u32::MAX, r * r);
        self.nn(0, self.pts.len(), 0, q, &mut best);
        (best.0 != u32::MAX).then_some((best.0 as usize, best.1))
    }

    fn nn(&self, lo: usize, hi: usize, depth: usize, q: [f32; 3], best: &mut (u32, f32)) {
        if lo >= hi {
            return;
        }
        let mid = (lo + hi) / 2;
        let p = self.pts[mid];
        let dd = d2(p, q);
        if dd < best.1 {
            *best = (self.idx[mid], dd);
        }
        let diff = q[depth % 3] - p[depth % 3];
        let (first, second) = if diff < 0.0 { ((lo, mid), (mid + 1, hi)) } else { ((mid + 1, hi), (lo, mid)) };
        self.nn(first.0, first.1, depth + 1, q, best);
        if diff * diff < best.1 {
            self.nn(second.0, second.1, depth + 1, q, best);
        }
    }

    /// The `K` nearest points (fewer if the tree is smaller): (distance², index).
    pub fn knn<const K: usize>(&self, q: [f32; 3]) -> ([(f32, u32); K], usize) {
        let mut out = [(f32::MAX, 0u32); K];
        let mut n = 0;
        self.kn(0, self.pts.len(), 0, q, &mut out, &mut n);
        (out, n)
    }

    fn kn<const K: usize>(&self, lo: usize, hi: usize, depth: usize, q: [f32; 3], out: &mut [(f32, u32); K], n: &mut usize) {
        if lo >= hi {
            return;
        }
        let mid = (lo + hi) / 2;
        let p = self.pts[mid];
        let dd = d2(p, q);
        if *n < K || dd < out[K - 1].0 {
            let mut i = (*n).min(K - 1);
            while i > 0 && out[i - 1].0 > dd {
                out[i] = out[i - 1];
                i -= 1;
            }
            out[i] = (dd, self.idx[mid]);
            *n = (*n + 1).min(K);
        }
        let diff = q[depth % 3] - p[depth % 3];
        let (first, second) = if diff < 0.0 { ((lo, mid), (mid + 1, hi)) } else { ((mid + 1, hi), (lo, mid)) };
        self.kn(first.0, first.1, depth + 1, q, out, n);
        if *n < K || diff * diff < out[K - 1].0 {
            self.kn(second.0, second.1, depth + 1, q, out, n);
        }
    }
}

/// Eigen-decomposition of a symmetric 3×3 matrix (Jacobi): the eigenvector
/// of the smallest eigenvalue, and that eigenvalue over their sum.
fn smallest_eigen(mut a: [[f64; 3]; 3]) -> ([f64; 3], f64) {
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..12 {
        let off = a[0][1].abs() + a[0][2].abs() + a[1][2].abs();
        if off < 1e-14 {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if a[p][q].abs() < 1e-18 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum().max(0.0) * 2.0 - 1.0;
            let t = t / (theta.abs() + (theta * theta + 1.0).sqrt());
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for row in a.iter_mut() {
                let (akp, akq) = (row[p], row[q]);
                row[p] = c * akp - s * akq;
                row[q] = s * akp + c * akq;
            }
            let (rp, rq) = (a[p], a[q]);
            a[p] = std::array::from_fn(|k| c * rp[k] - s * rq[k]);
            a[q] = std::array::from_fn(|k| s * rp[k] + c * rq[k]);
            for row in v.iter_mut() {
                let (vp, vq) = (row[p], row[q]);
                row[p] = c * vp - s * vq;
                row[q] = s * vp + c * vq;
            }
        }
    }
    let ev = [a[0][0], a[1][1], a[2][2]];
    let i = (0..3).min_by(|&x, &y| ev[x].total_cmp(&ev[y])).unwrap();
    let sum = ev.iter().map(|e| e.max(0.0)).sum::<f64>();
    ([v[0][i], v[1][i], v[2][i]], ev[i].max(0.0) / (sum + 1e-12))
}

/// Eigenvalues and eigenvectors (column `k` of the row-major `n × n` result)
/// of a symmetric matrix, by cyclic Jacobi.
#[allow(clippy::needless_range_loop)]
fn sym_eigen(mut a: Vec<f64>, n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut v = vec![0.0; n * n];
    for i in 0..n {
        v[i * n + i] = 1.0;
    }
    let scale = 1.0 + (0..n * n).map(|i| a[i].abs()).sum::<f64>();
    for _ in 0..60 {
        let off: f64 = (0..n).flat_map(|p| (p + 1..n).map(move |q| (p, q))).map(|(p, q)| a[p * n + q].abs()).sum();
        if off < 1e-14 * scale {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                let apq = a[p * n + q];
                if apq.abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q * n + q] - a[p * n + p]) / (2.0 * apq);
                let sgn = if theta >= 0.0 { 1.0 } else { -1.0 };
                let t = sgn / (theta.abs() + (theta * theta + 1.0).sqrt());
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let (akp, akq) = (a[k * n + p], a[k * n + q]);
                    a[k * n + p] = c * akp - s * akq;
                    a[k * n + q] = s * akp + c * akq;
                }
                for k in 0..n {
                    let (apk, aqk) = (a[p * n + k], a[q * n + k]);
                    a[p * n + k] = c * apk - s * aqk;
                    a[q * n + k] = s * apk + c * aqk;
                }
                for k in 0..n {
                    let (vkp, vkq) = (v[k * n + p], v[k * n + q]);
                    v[k * n + p] = c * vkp - s * vkq;
                    v[k * n + q] = s * vkp + c * vkq;
                }
            }
        }
    }
    ((0..n).map(|i| a[i * n + i]).collect(), v)
}

static THREAD_CAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Use at most `n` threads for registration (0: all cores). The bench sets
/// it so a measurement doesn't take the whole machine.
pub fn set_thread_cap(n: usize) {
    THREAD_CAP.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Run `f` over `0..n` on all cores (or the cap), collecting results in order.
fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let cores = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(1);
    let cap = THREAD_CAP.load(std::sync::atomic::Ordering::Relaxed);
    let threads = if cap > 0 { cores.min(cap) } else { cores }.min(n.max(1));
    let chunk = n.div_ceil(threads.max(1)).max(1);
    let f = &f;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..n).step_by(chunk).map(|lo| s.spawn(move || (lo..(lo + chunk).min(n)).map(f).collect::<Vec<T>>())).collect();
        hs.into_iter().flat_map(|h| h.join().expect("worker thread")).collect()
    })
}

// --------------------------------------------------------------- matching ---

/// A station ready for matching: 10 cm-thinned points, their normals and
/// flatness, a kd-tree, and its wall slice (x, y).
pub struct Prepared {
    /// The scan's angular step (for point spacing).
    pub step_rad: f32,
    pub pts: Vec<[f32; 3]>,
    pub range: Vec<f32>,
    pub normal: Vec<[f32; 3]>,
    pub planar: Vec<bool>,
    pub wall: Vec<bool>,
    tree: Tree,
    slice: Vec<[f32; 2]>,
    /// Nearest return per direction (`NAZ × NEL` bins, 0: none): the space
    /// before it is what the scanner saw through.
    depth: Vec<f32>,
}

/// A direction's bin in a depth map: (azimuth, elevation).
fn depth_bin(p: [f32; 3]) -> (usize, usize) {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let az = p[1].atan2(p[0]).rem_euclid(TAU);
    let el = p[2].atan2(p[0].hypot(p[1])) + FRAC_PI_2;
    (((az / TAU * NAZ as f32) as usize).min(NAZ - 1), ((el / PI * NEL as f32) as usize).min(NEL - 1))
}

impl Prepared {
    /// Roughly how much memory it holds.
    pub fn approx_bytes(&self) -> usize {
        let n = self.pts.len();
        n * (12 + 4 + 12 + 1 + 1) + self.tree.pts.len() * 12 + self.tree.idx.len() * 4 + self.slice.len() * 8 + self.depth.len() * 4
    }
}

pub fn prepare(c: &Cloud) -> Prepared {
    let mut seen = HashSet::with_capacity(c.pts.len() / 4);
    let (mut pts, mut range) = (Vec::new(), Vec::new());
    for (p, r) in c.pts.iter().zip(&c.range) {
        if seen.insert(thin_key(*p, ICP_VOXEL)) {
            pts.push(*p);
            range.push(*r);
        }
    }
    let tree = Tree::new(&pts);
    let nc: Vec<([f32; 3], f32)> = par_map(pts.len(), |i| {
        let (nb, n) = tree.knn::<12>(pts[i]);
        if n < 6 {
            return ([0.0, 0.0, 1.0], 1.0);
        }
        let mut m = [0f64; 3];
        for &(_, j) in &nb[..n] {
            for k in 0..3 {
                m[k] += pts[j as usize][k] as f64 / n as f64;
            }
        }
        let mut cov = [[0f64; 3]; 3];
        for &(_, j) in &nb[..n] {
            let d = [pts[j as usize][0] as f64 - m[0], pts[j as usize][1] as f64 - m[1], pts[j as usize][2] as f64 - m[2]];
            for a in 0..3 {
                for b in 0..3 {
                    cov[a][b] += d[a] * d[b];
                }
            }
        }
        let (v, curv) = smallest_eigen(cov);
        ([v[0] as f32, v[1] as f32, v[2] as f32], curv as f32)
    });
    let normal: Vec<[f32; 3]> = nc.iter().map(|x| x.0).collect();
    let planar = nc.iter().map(|x| x.1 < 0.05).collect();
    let wall: Vec<bool> = nc.iter().map(|x| x.0[2].abs() < 0.25 && x.1 < 0.03).collect();
    let slice = pts
        .iter()
        .zip(&wall)
        .filter(|(p, w)| **w && p[2] > SLICE.0 && p[2] < SLICE.1)
        .map(|(p, _)| [p[0], p[1]])
        .collect();
    // From the 5 cm cloud: denser in angle than the matching points.
    let mut depth = vec![0f32; NAZ * NEL];
    for p in &c.pts {
        let (i, j) = depth_bin(*p);
        let d = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        let e = &mut depth[j * NAZ + i];
        *e = if *e == 0.0 { d } else { e.min(d) };
    }
    Prepared { step_rad: c.step_rad, pts, range, normal, planar, wall, tree, slice, depth }
}

/// How B sits in A: `B_in_A = R(turn) b + shift`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pair {
    pub a: String,
    pub b: String,
    pub turn_deg: f64,
    pub shift_m: [f64; 3],
    /// None when ICP found too few matches.
    pub rms_mm: Option<f64>,
    pub matches: usize,
    pub overlap: f64,
    pub coarse_score: f64,
    /// Fits well enough, held firmly and unique: a constraint.
    pub good: bool,
    /// The share of the matches holding the fit's weakest direction (0: free
    /// to slide; under 0.01 is too weak).
    #[serde(default)]
    pub constraint: f64,
    /// That direction in words, when it is too weak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weak: Option<String>,
    /// A different fit is as good (symmetric or repeated walls).
    #[serde(default)]
    pub ambiguous: bool,
    /// How rival fits were told apart ("free space").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    /// Why it is not a constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
    /// After the global fit: how far this pair is left from its own fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_deg: Option<f64>,
    #[serde(default)]
    pub outlier: bool,
}

struct Coarse {
    n: usize,
    m: usize,
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    ifft: std::sync::Arc<dyn rustfft::Fft<f32>>,
}

impl Coarse {
    fn new() -> Coarse {
        let n = (2.0 * RANGE / COARSE_CELL) as usize;
        let m = 2 * n;
        let mut planner = FftPlanner::new();
        Coarse { n, m, fft: planner.plan_fft_forward(m), ifft: planner.plan_fft_inverse(m) }
    }

    /// The slice turned by `theta` as a blurred plan image (`n × n`).
    fn raster(&self, xy: &[[f32; 2]], theta: f32) -> Vec<f32> {
        let n = self.n;
        let (c, s) = (theta.cos(), theta.sin());
        let mut img = vec![0f32; n * n];
        for p in xy {
            let (x, y) = (c * p[0] - s * p[1], s * p[0] + c * p[1]);
            let (i, j) = (((x + RANGE) / COARSE_CELL).floor(), ((y + RANGE) / COARSE_CELL).floor());
            if i >= 0.0 && j >= 0.0 && (i as usize) < n && (j as usize) < n {
                let v = &mut img[j as usize * n + i as usize];
                *v = (*v + 1.0).min(3.0);
            }
        }
        // Gaussian blur, σ = 1 cell, separable.
        let k = [0.0044, 0.054, 0.242, 0.3991, 0.242, 0.054, 0.0044];
        let mut tmp = vec![0f32; n * n];
        for r in 0..n {
            for c in 0..n {
                let mut acc = 0.0;
                for (t, w) in k.iter().enumerate() {
                    let cc = c as i64 + t as i64 - 3;
                    if cc >= 0 && (cc as usize) < n {
                        acc += w * img[r * n + cc as usize];
                    }
                }
                tmp[r * n + c] = acc;
            }
        }
        for r in 0..n {
            for c in 0..n {
                let mut acc = 0.0;
                for (t, w) in k.iter().enumerate() {
                    let rr = r as i64 + t as i64 - 3;
                    if rr >= 0 && (rr as usize) < n {
                        acc += w * tmp[rr as usize * n + c];
                    }
                }
                img[r * n + c] = acc;
            }
        }
        img
    }

    /// 2-D FFT of an `n × n` image zero-padded to `m × m`, transposed.
    fn spectrum(&self, img: &[f32]) -> Vec<Complex<f32>> {
        let (n, m) = (self.n, self.m);
        let mut rows = vec![Complex::new(0.0, 0.0); n * m];
        for r in 0..n {
            for c in 0..n {
                rows[r * m + c].re = img[r * n + c];
            }
        }
        self.fft.process(&mut rows);
        let mut t = vec![Complex::new(0.0, 0.0); m * m];
        for r in 0..n {
            for c in 0..m {
                t[c * m + r] = rows[r * m + c];
            }
        }
        self.fft.process(&mut t);
        t
    }

    /// Correlation peak of A's spectrum with B's image: (score, shift x, y in m).
    fn score(&self, fa: &[Complex<f32>], na: f32, b: &[f32]) -> (f32, [f32; 2]) {
        let m = self.m;
        let nb = b.iter().map(|v| v * v).sum::<f32>().sqrt();
        let fb = self.spectrum(b);
        let mut p: Vec<Complex<f32>> = fa.iter().zip(&fb).map(|(x, y)| x * y.conj()).collect();
        self.ifft.process(&mut p);
        let mut t = vec![Complex::new(0.0, 0.0); m * m];
        for r in 0..m {
            for c in 0..m {
                t[c * m + r] = p[r * m + c];
            }
        }
        self.ifft.process(&mut t);
        let (k, v) = t.iter().enumerate().max_by(|a, b| a.1.re.total_cmp(&b.1.re)).map(|(k, v)| (k, v.re)).unwrap_or((0, 0.0));
        let (iy, ix) = (k / m, k % m);
        let wrap = |i: usize| if i >= self.n { i as f32 - m as f32 } else { i as f32 };
        (v / ((m * m) as f32) / (na * nb + 1e-9), [wrap(ix) * COARSE_CELL, wrap(iy) * COARSE_CELL])
    }

    /// The best few (turn, shift, score) taking B's slice onto A's.
    fn hypotheses(&self, a: &[[f32; 2]], b: &[[f32; 2]]) -> Vec<(f32, [f32; 2], f32)> {
        let ia = self.raster(a, 0.0);
        let na = ia.iter().map(|v| v * v).sum::<f32>().sqrt();
        let fa = self.spectrum(&ia);
        let at = |theta: f32| self.score(&fa, na, &self.raster(b, theta));
        let sweep: Vec<(f32, [f32; 2])> = par_map(360, |d| at((d as f32).to_radians()));
        let mut order: Vec<usize> = (0..360).collect();
        order.sort_by(|x, y| sweep[*y].0.total_cmp(&sweep[*x].0));
        let mut picked: Vec<usize> = Vec::new();
        for d in order {
            if picked.iter().all(|e| {
                let g = (d as i64 - *e as i64).abs();
                g.min(360 - g) >= 10
            }) {
                picked.push(d);
            }
            if picked.len() == HYPOTHESES {
                break;
            }
        }
        picked
            .into_iter()
            .map(|d| {
                let mut best = (sweep[d].0, sweep[d].1, (d as f32).to_radians());
                for step in [0.25f32, 0.05] {
                    let centre = best.2;
                    for k in -4..=4 {
                        let th = centre + (k as f32 * step).to_radians();
                        let (s, t) = at(th);
                        if s > best.0 {
                            best = (s, t, th);
                        }
                    }
                }
                (best.2, best.1, best.0)
            })
            .collect()
    }
}

/// Solve a small dense system `A x = b` (Gaussian elimination, partial pivoting).
fn solve_dense(mut a: Vec<f64>, mut b: Vec<f64>, n: usize) -> Option<Vec<f64>> {
    for c in 0..n {
        let p = (c..n).max_by(|&i, &j| a[i * n + c].abs().total_cmp(&a[j * n + c].abs()))?;
        if a[p * n + c].abs() < 1e-15 {
            return None;
        }
        if p != c {
            for k in 0..n {
                a.swap(p * n + k, c * n + k);
            }
            b.swap(p, c);
        }
        for r in c + 1..n {
            let f = a[r * n + c] / a[c * n + c];
            if f != 0.0 {
                for k in c..n {
                    a[r * n + k] -= f * a[c * n + k];
                }
                b[r] -= f * b[c];
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let s: f64 = (r + 1..n).map(|k| a[r * n + k] * x[k]).sum();
        x[r] = (b[r] - s) / a[r * n + r];
    }
    Some(x)
}

/// Point-to-plane ICP, 4 DOF, from a guess: (turn, shift, rms m, overlap, matches).
/// An ICP result: B in A (turn rad, x, y, z), RMS (m), overlap, matches, and
/// the information the matches carry (4 × 4, a turn at a 10 m arm).
#[derive(Clone, Copy)]
struct Fit {
    x: P4,
    rms: f64,
    overlap: f64,
    matches: usize,
    info: [f64; 16],
}

fn icp(a: &Prepared, b: &Prepared, theta: f64, t: [f64; 3]) -> Option<Fit> {
    let mut x = [theta, t[0], t[1], t[2]];
    let every = (b.pts.len() / ICP_SAMPLE).max(1);
    let sample: Vec<[f32; 3]> = b.pts.iter().step_by(every).copied().collect();
    let moved = |x: &[f64; 4], p: [f32; 3]| {
        let (c, s) = (x[0].cos(), x[0].sin());
        let (px, py) = (p[0] as f64, p[1] as f64);
        [c * px - s * py + x[1], s * px + c * py + x[2], p[2] as f64 + x[3]]
    };
    for dmax in [1.0, 0.5, 0.25, 0.12, 0.08, 0.06, 0.05, 0.05] {
        for _ in 0..4 {
            let mut ata = [0f64; 16];
            let mut atb = [0f64; 4];
            let mut used = 0;
            for &p in &sample {
                let q = moved(&x, p);
                let Some((j, _)) = a.tree.nearest([q[0] as f32, q[1] as f32, q[2] as f32], dmax as f32) else { continue };
                if !a.planar[j] {
                    continue;
                }
                let (aq, n) = (a.pts[j], a.normal[j]);
                let nn = [n[0] as f64, n[1] as f64, n[2] as f64];
                let r = (q[0] - aq[0] as f64) * nn[0] + (q[1] - aq[1] as f64) * nn[1] + (q[2] - aq[2] as f64) * nn[2];
                // d(R p)/d turn about the moved point: (-(y - ty), x - tx, 0).
                let (rx, ry) = (q[0] - x[1], q[1] - x[2]);
                let jrow = [nn[0] * -ry + nn[1] * rx, nn[0], nn[1], nn[2]];
                let w = 1.0 / (r.abs() / (dmax / 3.0)).max(1.0);
                for i in 0..4 {
                    for k in 0..4 {
                        ata[i * 4 + k] += w * jrow[i] * jrow[k];
                    }
                    atb[i] -= w * jrow[i] * r;
                }
                used += 1;
            }
            if used < 200 {
                return None;
            }
            let dx = solve_dense(ata.to_vec(), atb.to_vec(), 4)?;
            for i in 0..4 {
                x[i] += dx[i];
            }
            if dx.iter().all(|d| d.abs() < 1e-5) {
                break;
            }
        }
    }
    // Final fit over all of B's points, and what holds it.
    let (mut ss, mut matches) = (0f64, 0usize);
    let mut info = [0f64; 16];
    for &p in &b.pts {
        let q = moved(&x, p);
        let Some((j, _)) = a.tree.nearest([q[0] as f32, q[1] as f32, q[2] as f32], 0.05) else { continue };
        if !a.planar[j] {
            continue;
        }
        let (aq, n) = (a.pts[j], a.normal[j]);
        let nn = [n[0] as f64, n[1] as f64, n[2] as f64];
        let r = (q[0] - aq[0] as f64) * nn[0] + (q[1] - aq[1] as f64) * nn[1] + (q[2] - aq[2] as f64) * nn[2];
        ss += r * r;
        matches += 1;
        let (rx, ry) = (q[0] - x[1], q[1] - x[2]);
        let jrow = [(nn[0] * -ry + nn[1] * rx) / TURN_ARM, nn[0], nn[1], nn[2]];
        for i in 0..4 {
            for k in 0..4 {
                info[i * 4 + k] += jrow[i] * jrow[k];
            }
        }
    }
    (matches > 0).then(|| Fit { x, rms: (ss / matches as f64).sqrt(), overlap: matches as f64 / b.pts.len().max(1) as f64, matches, info })
}

/// The fit's weakest direction: its share of the matches, and in words.
fn weakest(f: &Fit) -> (f64, String) {
    let (vals, vecs) = sym_eigen(f.info.to_vec(), 4);
    let k = (0..4).min_by(|&a, &b| vals[a].total_cmp(&vals[b])).unwrap_or(0);
    let v: Vec<f64> = (0..4).map(|i| vecs[i * 4 + k]).collect();
    let share = vals[k].max(0.0) / f.matches.max(1) as f64;
    let words = if v[0].abs() >= 0.7 {
        "it can turn about vertical".to_string()
    } else if v[3].abs() >= 0.7 {
        "it can slide up and down".to_string()
    } else {
        let h = v[1].hypot(v[2]).max(1e-9);
        format!("it can slide along ({:.2}, {:.2}) in the first station's frame", v[1] / h, v[2] / h)
    };
    (share, words)
}

/// Share of B's wall points that land, by `rel` (B in A), where A saw
/// straight through: nearer than A's nearest return within 1° of that
/// direction (at least three directions with returns), by 0.3 m or 5%. Walls only: ground seen at a grazing angle
/// turns a few centimetres of grass into tens of centimetres of range. None
/// when too few can be judged.
fn free_space(a: &Prepared, b: &Prepared, rel: P4) -> Option<f64> {
    let (s, c) = rel[0].sin_cos();
    let idx: Vec<usize> = (0..b.pts.len()).filter(|&i| b.wall[i]).collect();
    let every = (idx.len() / 20_000).max(1);
    let (mut tested, mut through) = (0usize, 0usize);
    for &i in idx.iter().step_by(every) {
        let p = b.pts[i];
        let (px, py) = (p[0] as f64, p[1] as f64);
        let q = [(c * px - s * py + rel[1]) as f32, (s * px + c * py + rel[2]) as f32, (p[2] as f64 + rel[3]) as f32];
        let d = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt();
        if d < 1.0 {
            continue;
        }
        let (ia, ie) = depth_bin(q);
        let (mut m, mut known) = (f32::MAX, 0);
        for de in -2i64..=2 {
            for da in -2i64..=2 {
                let e = ie as i64 + de;
                if !(0..NEL as i64).contains(&e) {
                    continue;
                }
                let v = a.depth[e as usize * NAZ + (ia as i64 + da).rem_euclid(NAZ as i64) as usize];
                if v > 0.0 {
                    m = m.min(v);
                    known += 1;
                }
            }
        }
        if known < 3 {
            continue;
        }
        tested += 1;
        if d < m - 0.3f32.max(0.05 * d) {
            through += 1;
        }
    }
    (tested >= 500).then(|| through as f64 / tested as f64)
}

/// Point-to-plane distances (m) of B's flat points to A's surfaces, by `rel`,
/// where both surfaces face the same way (so corners and bushes don't pair
/// up different surfaces).
fn surface_gaps(a: &Prepared, b: &Prepared, rel: P4) -> Vec<f32> {
    let (s, c) = rel[0].sin_cos();
    let idx: Vec<usize> = (0..b.pts.len()).filter(|&i| b.planar[i]).collect();
    let every = (idx.len() / 10_000).max(1);
    let mut out = Vec::new();
    for &i in idx.iter().step_by(every) {
        let p = b.pts[i];
        let (px, py) = (p[0] as f64, p[1] as f64);
        let q = [(c * px - s * py + rel[1]) as f32, (s * px + c * py + rel[2]) as f32, (p[2] as f64 + rel[3]) as f32];
        let Some((j, _)) = a.tree.nearest(q, 0.15) else { continue };
        if !a.planar[j] {
            continue;
        }
        let (aq, n) = (a.pts[j], a.normal[j]);
        let nb = b.normal[i];
        let nb = [c as f32 * nb[0] - s as f32 * nb[1], s as f32 * nb[0] + c as f32 * nb[1], nb[2]];
        if (n[0] * nb[0] + n[1] * nb[1] + n[2] * nb[2]).abs() < 0.9 {
            continue;
        }
        out.push(((q[0] - aq[0]) * n[0] + (q[1] - aq[1]) * n[1] + (q[2] - aq[2]) * n[2]).abs());
    }
    out
}

/// How far the ground A saw around B's tripod is from B's own ground (m):
/// flat, level points 1.5–4 m out from B (the X9 can't see right under
/// itself), compared cell by cell (0.5 m) so a slope doesn't count; the
/// median over the cells both saw.
fn ground_gap(a: &Prepared, b: &Prepared, rel: P4) -> Option<f64> {
    let (s, c) = rel[0].sin_cos();
    let (cx, cy) = (rel[1], rel[2]);
    let level = |k: usize, p: &Prepared| p.planar[k] && p.normal[k][2].abs() > 0.95;
    let mut cells: HashMap<(i32, i32), (Vec<f32>, Vec<f32>)> = HashMap::new();
    for (k, p) in b.pts.iter().enumerate() {
        let r = p[0].hypot(p[1]);
        if !(1.5..=4.0).contains(&r) || p[2] > -0.5 || !level(k, b) {
            continue;
        }
        let (x, y) = (c * p[0] as f64 - s * p[1] as f64 + cx, s * p[0] as f64 + c * p[1] as f64 + cy);
        cells.entry(((x / 0.5).floor() as i32, (y / 0.5).floor() as i32)).or_default().0.push((p[2] as f64 + rel[3]) as f32);
    }
    for (k, p) in a.pts.iter().enumerate() {
        let r = (p[0] as f64 - cx).hypot(p[1] as f64 - cy);
        if !(1.5..=4.0).contains(&r) || !level(k, a) {
            continue;
        }
        if let Some(e) = cells.get_mut(&(((p[0] as f64) / 0.5).floor() as i32, ((p[1] as f64) / 0.5).floor() as i32)) {
            e.1.push(p[2]);
        }
    }
    let med = |v: &mut Vec<f32>| {
        v.sort_by(|x, y| x.total_cmp(y));
        v[v.len() / 2]
    };
    let mut diffs: Vec<f32> = cells
        .into_values()
        .filter(|(o, t)| o.len() >= 3 && t.len() >= 3)
        .map(|(mut o, mut t)| med(&mut t) - med(&mut o))
        .filter(|d| d.abs() < 0.5)
        .collect();
    (diffs.len() >= 5).then(|| med(&mut diffs) as f64)
}

/// Match B against A (ids for the record): a coarse search, then ICP.
pub fn pair(a_id: &str, a: &Prepared, b_id: &str, b: &Prepared) -> Pair {
    let coarse = Coarse::new();
    let mut tried: Vec<(Fit, f32)> = Vec::new();
    if a.slice.len() >= 50 && b.slice.len() >= 50 {
        for (theta, t, sc) in coarse.hypotheses(&a.slice, &b.slice) {
            if let Some(f) = icp(a, b, theta as f64, [t[0] as f64, t[1] as f64, 0.0]) {
                tried.push((f, sc));
            }
        }
    }
    judge_pair(a_id, a, b_id, b, tried)
}

/// Match B against A from a guess of B in A (turn rad, x, y, z): ICP only.
pub fn pair_from(a_id: &str, a: &Prepared, b_id: &str, b: &Prepared, guess: [f64; 4]) -> Pair {
    let tried = icp(a, b, guess[0], [guess[1], guess[2], guess[3]]).map(|f| (f, f32::NAN)).into_iter().collect();
    judge_pair(a_id, a, b_id, b, tried)
}

fn fits(f: &Fit) -> bool {
    f.rms <= GOOD_RMS_M && f.matches >= PAIR_MIN_MATCHES
}

/// A fit strong enough to count as evidence in the audit.
fn firm(f: &Fit) -> bool {
    f.rms <= GOOD_RMS_M && f.matches >= GOOD_MATCHES && weakest(f).0 >= CONSTRAINT_MIN
}

fn distinct(x: &Fit, y: &Fit) -> bool {
    let dm = ((x.x[1] - y.x[1]).powi(2) + (x.x[2] - y.x[2]).powi(2) + (x.x[3] - y.x[3]).powi(2)).sqrt();
    wrap(x.x[0] - y.x[0]).to_degrees().abs() > DISTINCT_DEG || dm > DISTINCT_M
}

/// Pick the pair's fit and say whether it counts: it must fit, be held
/// firmly in every direction, and be the only good fit, or the one free space
/// allows (a rival that puts points where the other station saw through loses).
fn judge_pair(a_id: &str, a: &Prepared, b_id: &str, b: &Prepared, mut tried: Vec<(Fit, f32)>) -> Pair {
    let key = |f: &Fit| (fits(f), if fits(f) { f.matches as f64 } else { -f.rms });
    tried.sort_by(|x, y| key(&y.0).partial_cmp(&key(&x.0)).unwrap_or(std::cmp::Ordering::Equal));
    let r3 = |v: f64| (v * 1000.0).round() / 1000.0;
    let Some(&best) = tried.first() else {
        return Pair { a: a_id.into(), b: b_id.into(), turn_deg: 0.0, shift_m: [0.0; 3], rms_mm: None, matches: 0, overlap: 0.0,
                      coarse_score: 0.0, good: false, constraint: 0.0, weak: None, ambiguous: false, resolved_by: None,
                      rejected: Some("too few matches".into()), left_mm: None, left_deg: None, outlier: false };
    };
    let mut rivals: Vec<(Fit, f32)> = vec![best];
    if fits(&best.0) {
        for &(f, sc) in &tried[1..] {
            if fits(&f) && f.matches as f64 >= RIVAL_SHARE * best.0.matches as f64 && rivals.iter().all(|r| distinct(&r.0, &f)) {
                rivals.push((f, sc));
            }
        }
    }
    let (mut chosen, mut ambiguous, mut resolved_by) = (best, false, None);
    if rivals.len() > 1 {
        let mut scored: Vec<((Fit, f32), f64)> = rivals
            .iter()
            .map(|r| (*r, free_space(a, b, r.0.x).unwrap_or(0.0).max(free_space(b, a, invert(r.0.x)).unwrap_or(0.0))))
            .collect();
        scored.sort_by(|x, y| x.1.total_cmp(&y.1));
        if scored[1].1 >= FREE_SPACE_RIVAL && scored[0].1 * 3.0 <= scored[1].1 {
            chosen = scored[0].0;
            resolved_by = Some(format!("free space ({} rival fits)", rivals.len()));
        } else {
            ambiguous = true;
        }
    }
    let (f, sc) = chosen;
    let (constraint, weak) = weakest(&f);
    let rejected = if f.rms > GOOD_RMS_M {
        Some(format!("fit {:.0} mm (needs {:.0} or less)", f.rms * 1000.0, GOOD_RMS_M * 1000.0))
    } else if f.matches < PAIR_MIN_MATCHES {
        Some(format!("{} matched points (needs {PAIR_MIN_MATCHES})", f.matches))
    } else if constraint < CONSTRAINT_MIN {
        Some(format!("weakly held: {weak}"))
    } else if ambiguous {
        Some(format!("ambiguous: {} different fits are as good and free space can't tell them apart", rivals.len()))
    } else {
        None
    };
    Pair {
        a: a_id.into(),
        b: b_id.into(),
        turn_deg: r3(f.x[0].to_degrees().rem_euclid(360.0)),
        shift_m: [f.x[1], f.x[2], f.x[3]].map(r3),
        rms_mm: Some((f.rms * 10_000.0).round() / 10.0),
        matches: f.matches,
        overlap: r3(f.overlap),
        coarse_score: if sc.is_nan() { 0.0 } else { r3(sc as f64) },
        good: rejected.is_none(),
        constraint: (constraint * 10_000.0).round() / 10_000.0,
        weak: (constraint < CONSTRAINT_MIN).then_some(weak),
        ambiguous,
        resolved_by,
        rejected,
        left_mm: None,
        left_deg: None,
        outlier: false,
    }
}

// ----------------------------------------------------------------- global ---

/// A station's pose in the project frame: turn about vertical and position.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Pose {
    pub turn_deg: f64,
    pub xyz_m: [f64; 3],
}

impl Pose {
    pub fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let (s, c) = self.turn_deg.to_radians().sin_cos();
        let (x, y) = (p[0] as f64, p[1] as f64);
        [(c * x - s * y + self.xyz_m[0]) as f32, (s * x + c * y + self.xyz_m[1]) as f32, (p[2] as f64 + self.xyz_m[2]) as f32]
    }
}

type P4 = [f64; 4];

fn compose(pa: P4, rel: P4) -> P4 {
    let (s, c) = pa[0].sin_cos();
    [pa[0] + rel[0], pa[1] + c * rel[1] - s * rel[2], pa[2] + s * rel[1] + c * rel[2], pa[3] + rel[3]]
}

fn invert(rel: P4) -> P4 {
    let th = -rel[0];
    let (s, c) = th.sin_cos();
    [th, -(c * rel[1] - s * rel[2]), -(s * rel[1] + c * rel[2]), -rel[3]]
}

fn relative(pa: P4, pb: P4) -> P4 {
    let (s, c) = pa[0].sin_cos();
    let (dx, dy) = (pb[1] - pa[1], pb[2] - pa[2]);
    [pb[0] - pa[0], c * dx + s * dy, -s * dx + c * dy, pb[3] - pa[3]]
}

fn wrap(a: f64) -> f64 {
    (a + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
}

/// Stations tied together by pairs that count, each group solved in its own
/// frame (its lowest station's). The project frame is the largest group's
/// (on a tie, the one holding the lowest station); stations in other groups
/// wait to be tied in.
pub struct Groups {
    group: Vec<usize>,
    pose: Vec<P4>,
    root: Vec<usize>,
    size: Vec<usize>,
    main: usize,
}

impl Groups {
    /// Poses in the project frame; `None` outside the main group.
    pub fn poses(&self) -> Vec<Option<Pose>> {
        (0..self.group.len()).map(|k| (self.group[k] == self.main).then(|| to_pose(self.pose[k]))).collect()
    }
    /// The station whose scanner frame is the project frame.
    pub fn frame(&self) -> usize {
        self.root.get(self.main).copied().unwrap_or(0)
    }
    fn in_main(&self, k: usize) -> bool {
        self.group[k] == self.main
    }
    fn same(&self, a: usize, b: usize) -> bool {
        self.group[a] == self.group[b]
    }
    fn alone(&self, k: usize) -> bool {
        self.size[self.group[k]] == 1
    }
    /// Groups other than the main one with two or more stations.
    fn others(&self) -> Vec<Vec<usize>> {
        (0..self.root.len())
            .filter(|&c| c != self.main && self.size[c] > 1)
            .map(|c| (0..self.group.len()).filter(|&k| self.group[k] == c).collect())
            .collect()
    }
}

fn to_pose(p: P4) -> Pose {
    Pose { turn_deg: (p[0].to_degrees().rem_euclid(360.0) * 1000.0).round() / 1000.0, xyz_m: [p[1], p[2], p[3]].map(|v| (v * 1000.0).round() / 1000.0) }
}

type Constraint = (usize, usize, P4, f64, usize);

/// Solve one group from its root (fixed): a spanning tree, strongest pairs
/// first, then Gauss–Newton with a Cauchy loss, turns weighted so 0.1 rad ~ 1 m.
fn solve_group(n: usize, good: &[Constraint], root: usize) -> Vec<Option<P4>> {
    let mut pose: Vec<Option<P4>> = vec![None; n];
    pose[root] = Some([0.0; 4]);
    loop {
        let mut grew = false;
        for &(a, b, rel, _, _) in good {
            match (pose[a], pose[b]) {
                (Some(pa), None) => {
                    pose[b] = Some(compose(pa, rel));
                    grew = true;
                }
                (None, Some(pb)) => {
                    pose[a] = Some(compose(pb, invert(rel)));
                    grew = true;
                }
                _ => {}
            }
        }
        if !grew {
            break;
        }
    }
    let free: Vec<usize> = (0..n).filter(|&k| k != root && pose[k].is_some()).collect();
    let slot = |k: usize| free.iter().position(|&f| f == k);
    let mut x: Vec<f64> = free.iter().flat_map(|&k| pose[k].unwrap()).collect();
    let cons: Vec<&Constraint> = good.iter().filter(|g| pose[g.0].is_some() && pose[g.1].is_some()).collect();
    let get = |x: &[f64], k: usize| -> P4 {
        match slot(k) {
            None => [0.0; 4],
            Some(s) => [x[4 * s], x[4 * s + 1], x[4 * s + 2], x[4 * s + 3]],
        }
    };
    const SCALE: f64 = 0.05;
    let m = 4 * free.len();
    for _ in 0..50 {
        if m == 0 {
            break;
        }
        let mut h = vec![0f64; m * m];
        let mut g = vec![0f64; m];
        for &&(a, b, o, w, _) in &cons {
            let (pa, pb) = (get(&x, a), get(&x, b));
            let rel = relative(pa, pb);
            let mut e = [wrap(rel[0] - o[0]), rel[1] - o[1], rel[2] - o[2], rel[3] - o[3]];
            let unit = [10.0 * w, w, w, w];
            for k in 0..4 {
                e[k] *= unit[k];
            }
            // Jacobians of rel by A's and B's (turn, x, y, z).
            let (s, c) = pa[0].sin_cos();
            let (dx, dy) = (pb[1] - pa[1], pb[2] - pa[2]);
            let ja: [[f64; 4]; 4] = [[-1.0, 0.0, 0.0, 0.0], [-s * dx + c * dy, -c, -s, 0.0], [-c * dx - s * dy, s, -c, 0.0], [0.0, 0.0, 0.0, -1.0]];
            let jb: [[f64; 4]; 4] = [[1.0, 0.0, 0.0, 0.0], [0.0, c, s, 0.0], [0.0, -s, c, 0.0], [0.0, 0.0, 0.0, 1.0]];
            for k in 0..4 {
                let rw = 1.0 / (1.0 + (e[k] / SCALE).powi(2));
                let mut row: Vec<(usize, f64)> = Vec::with_capacity(8);
                for (who, jac) in [(a, &ja), (b, &jb)] {
                    if let Some(sl) = slot(who) {
                        for (q, j) in jac[k].iter().enumerate() {
                            row.push((4 * sl + q, j * unit[k]));
                        }
                    }
                }
                for &(i, vi) in &row {
                    g[i] -= rw * vi * e[k];
                    for &(j, vj) in &row {
                        h[i * m + j] += rw * vi * vj;
                    }
                }
            }
        }
        for i in 0..m {
            h[i * m + i] += 1e-9;
        }
        let Some(dx) = solve_dense(h, g, m) else { break };
        for i in 0..m {
            x[i] += dx[i];
        }
        if dx.iter().all(|d| d.abs() < 1e-7) {
            break;
        }
    }
    (0..n).map(|k| if k == root { Some([0.0; 4]) } else { slot(k).map(|_| get(&x, k)) }).collect()
}

/// Group the stations by the pairs that fit, solve each group, and mark each
/// pair's leftover and outliers.
pub fn solve_groups(ids: &[String], pairs: &mut [Pair]) -> Groups {
    let n = ids.len();
    let at = |id: &str| ids.iter().position(|x| x == id);
    let obs = |p: &Pair| -> P4 { [p.turn_deg.to_radians(), p.shift_m[0], p.shift_m[1], p.shift_m[2]] };
    let mut good: Vec<Constraint> = pairs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.good)
        .filter_map(|(k, p)| Some((at(&p.a)?, at(&p.b)?, obs(p), (p.matches as f64 / GOOD_MATCHES as f64).sqrt(), k)))
        .collect();
    good.sort_by(|x, y| pairs[y.4].matches.cmp(&pairs[x.4].matches));
    // Groups: connected by the pairs that count.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], k: usize) -> usize {
        let mut r = k;
        while parent[r] != r {
            r = parent[r];
        }
        parent[k] = r;
        r
    }
    for &(a, b, _, _, _) in &good {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra.max(rb)] = ra.min(rb);
        }
    }
    let rootk: Vec<usize> = (0..n).map(|k| find(&mut parent, k)).collect();
    let mut root: Vec<usize> = rootk.clone();
    root.sort();
    root.dedup();
    let group: Vec<usize> = rootk.iter().map(|r| root.iter().position(|x| x == r).unwrap_or(0)).collect();
    let size: Vec<usize> = (0..root.len()).map(|c| group.iter().filter(|&&g| g == c).count()).collect();
    let main = (0..root.len()).max_by(|&x, &y| size[x].cmp(&size[y]).then(root[y].cmp(&root[x]))).unwrap_or(0);
    let mut pose = vec![[0.0; 4]; n];
    for &r in &root {
        let solved = solve_group(n, &good, r);
        for k in 0..n {
            if rootk[k] == r {
                pose[k] = solved[k].unwrap_or([0.0; 4]);
            }
        }
    }
    for &(a, b, o, _, k) in &good {
        let rel = relative(pose[a], pose[b]);
        let mm = ((rel[1] - o[1]).powi(2) + (rel[2] - o[2]).powi(2) + (rel[3] - o[3]).powi(2)).sqrt() * 1000.0;
        let deg = wrap(rel[0] - o[0]).to_degrees();
        pairs[k].left_mm = Some((mm * 10.0).round() / 10.0);
        pairs[k].left_deg = Some((deg * 1000.0).round() / 1000.0);
        pairs[k].outlier = mm > OUTLIER_M * 1000.0 || deg.abs() > OUTLIER_DEG;
    }
    Groups { group, pose, root, size, main }
}

/// Poses in the project frame (see `solve_groups`).
#[cfg(test)]
pub fn solve(ids: &[String], pairs: &mut [Pair]) -> Vec<Option<Pose>> {
    if ids.is_empty() {
        return vec![];
    }
    solve_groups(ids, pairs).poses()
}

impl Pose {
    fn p4(&self) -> P4 {
        [self.turn_deg.to_radians(), self.xyz_m[0], self.xyz_m[1], self.xyz_m[2]]
    }
}

/// Full searches a station tied to nothing may spend per run, nearest in
/// number first (the rest wait for later runs: pairs tried are kept).
const NEWCOMER_TRIES: usize = 6;
/// Full searches a separate group may spend per run trying to join another.
const JOIN_TRIES: usize = 4;

/// Bring the stations in `ids` (station order) into groups and the project
/// frame, keeping `pairs` already matched:
/// 1. A station tied to nothing searches the stations nearest in number
///    until one fits; the rest of that station's group is then matched from
///    the predicted pose (ICP only, no search).
/// 2. Each group outside the main one spends a few searches trying to tie
///    to another: the stations next in number, alternating with the group's
///    last stations against the first ones (a walk coming back to its start);
///    groups join when one fits.
/// 3. Stations that joined the main group this run are matched from their
///    poses against its stations within reach (ICP only), closing loops.
///
/// `say` hears each search (station, against).
pub fn register_all(ids: &[String], prep: &[&Prepared], mut pairs: Vec<Pair>, say: &dyn Fn(usize, usize)) -> (Vec<Pair>, Groups) {
    // Pairs judged under older rules are matched again: those from before the
    // checks existed (constraint 0), and rejections today's rules would accept.
    pairs.retain(|p| ids.contains(&p.a) && ids.contains(&p.b) && !(p.good && p.constraint == 0.0) && !stale_rejection(p));
    let n = ids.len();
    let mut g = solve_groups(ids, &mut pairs);
    if n < 2 {
        return (pairs, g);
    }
    let was_main: Vec<bool> = (0..n).map(|k| g.in_main(k)).collect();
    let tried = |pairs: &[Pair], a: usize, b: usize| pairs.iter().any(|p| (p.a == ids[a] && p.b == ids[b]) || (p.a == ids[b] && p.b == ids[a]));
    let rel_of = |p: &Pair| -> P4 { [p.turn_deg.to_radians(), p.shift_m[0], p.shift_m[1], p.shift_m[2]] };
    let reach = |pa: P4, pb: P4| (pb[1] - pa[1]).hypot(pb[2] - pa[2]) <= 1.5 * RANGE as f64;

    // 1. Stations tied to nothing.
    for b in 0..n {
        if !g.alone(b) || g.in_main(b) {
            continue;
        }
        let mut cands: Vec<usize> = (0..n).filter(|&a| a != b && !tried(&pairs, a, b)).collect();
        cands.sort_by_key(|&a| ((a as i64 - b as i64).abs(), a));
        let mut found: Option<(usize, P4)> = None;
        let mut searches = 0;
        for &a in &cands {
            let p = match found {
                None => {
                    if searches >= NEWCOMER_TRIES {
                        continue;
                    }
                    searches += 1;
                    say(b, a);
                    pair(&ids[a], prep[a], &ids[b], prep[b])
                }
                Some((a0, pb)) => {
                    if !g.same(a, a0) || !reach(g.pose[a], pb) {
                        continue;
                    }
                    pair_from(&ids[a], prep[a], &ids[b], prep[b], relative(g.pose[a], pb))
                }
            };
            if p.good && found.is_none() {
                found = Some((a, compose(g.pose[a], rel_of(&p))));
            }
            pairs.push(p);
        }
        if found.is_some() {
            g = solve_groups(ids, &mut pairs);
        }
    }

    // 2. Separate groups try to join another.
    for members in g.others() {
        let outside: Vec<usize> = (0..n).filter(|a| !members.contains(a)).collect();
        let all: Vec<(usize, usize)> = members
            .iter()
            .flat_map(|&b| outside.iter().map(move |&a| (b, a)))
            .filter(|&(b, a)| !tried(&pairs, a, b))
            .collect();
        // Two ways a walk meets itself: the next station in number, and the
        // walk's end coming back to its start. Alternate between them.
        let mut by_number = all.clone();
        by_number.sort_by_key(|&(b, a)| ((a as i64 - b as i64).abs(), a, b));
        let mut back_to_start = all;
        back_to_start.sort_by_key(|&(b, a)| (a, std::cmp::Reverse(b)));
        let mut cands: Vec<(usize, usize)> = Vec::new();
        for (x, y) in by_number.iter().zip(&back_to_start) {
            for c in [x, y] {
                if !cands.contains(c) {
                    cands.push(*c);
                }
            }
        }
        for &(b, a) in cands.iter().take(JOIN_TRIES) {
            say(b, a);
            let p = pair(&ids[a], prep[a], &ids[b], prep[b]);
            let joined = p.good;
            pairs.push(p);
            if joined {
                break;
            }
        }
    }
    g = solve_groups(ids, &mut pairs);

    // 3. Loop closures for stations new to the main group.
    let fresh: Vec<usize> = (0..n).filter(|&k| g.in_main(k) && !was_main[k]).collect();
    let mut todo: Vec<(usize, usize)> = fresh
        .iter()
        .flat_map(|&b| (0..n).filter(move |&a| a != b).map(move |a| (b, a)))
        .filter(|&(b, a)| g.in_main(a) && !tried(&pairs, a, b) && reach(g.pose[a], g.pose[b]))
        .collect();
    todo.sort();
    todo.dedup_by(|x, y| (x.0 == y.1 && x.1 == y.0) || x == y);
    let mut added = false;
    for (b, a) in todo {
        if tried(&pairs, a, b) {
            continue;
        }
        pairs.push(pair_from(&ids[a], prep[a], &ids[b], prep[b], relative(g.pose[a], g.pose[b])));
        added = true;
    }
    if added {
        g = solve_groups(ids, &mut pairs);
    }
    (pairs, g)
}

/// A pair rejected only for a bar that has since been lowered: it fitted, was
/// held firmly and unique, but had fewer matches than the old bar asked.
fn stale_rejection(p: &Pair) -> bool {
    !p.good
        && !p.ambiguous
        && p.weak.is_none()
        && p.constraint >= CONSTRAINT_MIN
        && p.rms_mm.is_some_and(|r| r <= GOOD_RMS_M * 1000.0)
        && p.matches >= PAIR_MIN_MATCHES
}

// ------------------------------------------------------------------- audit ---

/// A station's independent checks and verdict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Audit {
    /// `confirmed`: tied by two or more pairs that count, and no independent
    /// check disagrees; `single`: one such tie; `frame`: the only station;
    /// `doubtful`: an independent check disagrees with most of its
    /// neighbours, or it is tied only to stations in doubt; `grouped`: tied
    /// to stations in a separate group, not yet to the main one, or in a
    /// block joined to the main one by one pair only; `ambiguous`: only rival fits; `unplaced`.
    pub verdict: String,
    /// The stations its counting pairs tie it to (scan ids).
    pub ties: Vec<String>,
    /// Its flat points' distance to its neighbours' surfaces (median, p90, mm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_mm: Option<[f64; 2]>,
    /// The worst share of points lying where a neighbour saw through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_space: Option<f64>,
    /// The worst gap between its ground and a neighbour's around its tripod (mm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ground_mm: Option<f64>,
    /// The largest move a firm re-fit against a neighbour asks for (mm, °).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refit: Option<[f64; 2]>,
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl Audit {
    /// Counts towards coverage.
    pub fn trusted(&self) -> bool {
        matches!(self.verdict.as_str(), "confirmed" | "single" | "frame")
    }
}

/// Check every placed station against every neighbour it overlaps, under the
/// final poses: a fresh fit from those poses (how far it wants to move the
/// station), where each saw straight through, how its surfaces sit on
/// theirs, and the ground under its tripod. Each check votes among the
/// neighbours it has evidence for; a station is doubtful when any check finds
/// it off against more than half of those. `names` label stations in the
/// reasons.
/// One pair's independent checks (see `audit`): surface gaps (m), free
/// space, ground gap (m), and the fresh fit's move (mm, °).
#[derive(Clone, Default)]
pub struct PairChecks {
    gaps: Vec<f32>,
    free_space: Option<f64>,
    ground: Option<f64>,
    refit: Option<[f64; 2]>,
}

/// Checks already made, by pair of scan ids, with the relative pose they
/// were made at: reused while that pose stays put (within 1 mm and 0.001°),
/// so a new station only checks what changed.
pub type AuditCache = HashMap<(String, String), ([f64; 4], PairChecks)>;

pub fn audit(ids: &[String], names: &[String], prep: &[&Prepared], poses: &[Option<Pose>], pairs: &[Pair], cache: &mut AuditCache) -> Vec<Audit> {
    let n = ids.len();
    let placed: Vec<usize> = (0..n).filter(|&k| poses[k].is_some()).collect();
    let near = |i: usize, j: usize| {
        let (p, q) = (poses[i].expect("placed"), poses[j].expect("placed"));
        (p.xyz_m[0] - q.xyz_m[0]).hypot(p.xyz_m[1] - q.xyz_m[1]) < 2.0 * RANGE as f64
    };
    let jobs: Vec<(usize, usize)> = placed.iter().flat_map(|&i| placed.iter().filter(move |&&j| j > i).map(move |&j| (i, j))).filter(|&(i, j)| near(i, j)).collect();
    let rel_of = |i: usize, j: usize| relative(poses[i].expect("placed").p4(), poses[j].expect("placed").p4());
    let same = |x: &[f64; 4], y: &[f64; 4]| (x[0] - y[0]).abs() < 1e-3f64.to_radians() && (1..4).all(|k| (x[k] - y[k]).abs() < 1e-3);
    let todo: Vec<usize> = (0..jobs.len())
        .filter(|&k| {
            let (i, j) = jobs[k];
            cache.get(&(ids[i].clone(), ids[j].clone())).is_none_or(|(at, _)| !same(at, &rel_of(i, j)))
        })
        .collect();
    let fresh: Vec<PairChecks> = par_map(todo.len(), |t| {
        let (i, j) = jobs[todo[t]];
        let rel = rel_of(i, j);
        // Only a fit that is good and firmly held is evidence.
        let refit = icp(prep[i], prep[j], rel[0], [rel[1], rel[2], rel[3]]).filter(firm).map(|f| {
            let mm = ((f.x[1] - rel[1]).powi(2) + (f.x[2] - rel[2]).powi(2) + (f.x[3] - rel[3]).powi(2)).sqrt() * 1000.0;
            [(mm * 10.0).round() / 10.0, (wrap(f.x[0] - rel[0]).to_degrees().abs() * 1000.0).round() / 1000.0]
        });
        let mut gaps = surface_gaps(prep[i], prep[j], rel);
        gaps.extend(surface_gaps(prep[j], prep[i], invert(rel)));
        let fs = [free_space(prep[i], prep[j], rel), free_space(prep[j], prep[i], invert(rel))].into_iter().flatten().reduce(f64::max);
        let gr = [ground_gap(prep[i], prep[j], rel), ground_gap(prep[j], prep[i], invert(rel))].into_iter().flatten().reduce(|x, y| if y.abs() > x.abs() { y } else { x });
        PairChecks { gaps, free_space: fs, ground: gr, refit }
    });
    for (t, c) in todo.iter().zip(fresh) {
        let (i, j) = jobs[*t];
        cache.insert((ids[i].clone(), ids[j].clone()), (rel_of(i, j), c));
    }
    let checks: Vec<PairChecks> = jobs.iter().map(|&(i, j)| cache[&(ids[i].clone(), ids[j].clone())].1.clone()).collect();
    let pct = |v: &mut Vec<f32>| -> Option<[f64; 2]> {
        if v.len() < 300 {
            return None;
        }
        v.sort_by(|x, y| x.total_cmp(y));
        let at = |q: f64| ((v[((v.len() - 1) as f64 * q) as usize] as f64) * 10_000.0).round() / 10.0;
        Some([at(0.5), at(0.9)])
    };
    let mut out: Vec<Audit> = (0..n)
        .map(|k| {
            let mut a = Audit::default();
            if poses[k].is_none() {
                // Tied to stations that are themselves outside the main group: a separate group.
                let mates: Vec<String> = pairs
                    .iter()
                    .filter(|p| p.good && !p.outlier && (p.a == ids[k] || p.b == ids[k]))
                    .filter_map(|p| {
                        let o = if p.a == ids[k] { &p.b } else { &p.a };
                        let j = ids.iter().position(|x| x == o)?;
                        poses[j].is_none().then(|| names[j].clone())
                    })
                    .collect();
                if !mates.is_empty() {
                    a.verdict = "grouped".into();
                    a.ties = pairs
                        .iter()
                        .filter(|p| p.good && !p.outlier && (p.a == ids[k] || p.b == ids[k]))
                        .map(|p| if p.a == ids[k] { p.b.clone() } else { p.a.clone() })
                        .collect();
                    a.reasons.push(format!("in a separate group with station{} {}; it doesn't tie to the main group yet", if mates.len() == 1 { "" } else { "s" }, mates.join(", ")));
                    return a;
                }
                let rival = pairs.iter().any(|p| p.ambiguous && (p.a == ids[k] || p.b == ids[k]));
                a.verdict = if rival { "ambiguous" } else { "unplaced" }.into();
                a.reasons.push(if rival {
                    "it fits in more than one place (symmetric or repeated walls), and free space can't tell which".into()
                } else {
                    "it shares too little firmly held wall with the placed stations".into()
                });
                return a;
            }
            for p in pairs.iter().filter(|p| p.good && !p.outlier) {
                let other = if p.a == ids[k] { &p.b } else if p.b == ids[k] { &p.a } else { continue };
                if ids.iter().position(|x| x == other).is_some_and(|o| poses[o].is_some()) && !a.ties.contains(other) {
                    a.ties.push(other.clone());
                }
            }
            // Per check: (neighbours with evidence, neighbours it disagrees with).
            let mut votes = [(0usize, 0usize); 4];
            let mut gaps = Vec::new();
            for (q, &(i, j)) in jobs.iter().enumerate() {
                if i != k && j != k {
                    continue;
                }
                let other = &names[if i == k { j } else { i }];
                let PairChecks { gaps: g, free_space: fs, ground: gr, refit } = &checks[q];
                let mut g = g.clone();
                let mut vote = |c: usize, bad: bool, why: String| {
                    votes[c].0 += 1;
                    if bad {
                        votes[c].1 += 1;
                        a.reasons.push(why);
                    }
                };
                if let Some([mm, deg]) = *refit {
                    let w = a.refit.unwrap_or([0.0, 0.0]);
                    a.refit = Some([w[0].max(mm), w[1].max(deg)]);
                    vote(0, mm > REFIT_BAD_MM || deg > REFIT_BAD_DEG, format!("a fresh fit against station {other} moves it {mm:.0} mm and {deg:.2}°"));
                }
                if let Some(f) = *fs {
                    a.free_space = Some(a.free_space.unwrap_or(0.0).max((f * 1000.0).round() / 1000.0));
                    vote(1, f > FREE_SPACE_BAD, format!("{:.0}% of its wall points lie where station {other} saw straight through", f * 100.0));
                }
                if let Some([p50, _]) = pct(&mut g) {
                    vote(2, p50 > SURFACE_BAD_MM, format!("its surfaces sit {p50:.0} mm off station {other}'s"));
                }
                if let Some(d) = *gr {
                    let mm = (d * 1000.0).round();
                    if a.ground_mm.is_none_or(|w| mm.abs() > w.abs()) {
                        a.ground_mm = Some(mm);
                    }
                    vote(3, d.abs() > GROUND_BAD_M, format!("the ground under it is {mm:.0} mm off station {other}'s"));
                }
                gaps.extend(g);
            }
            // A strict majority: with three stations, the one wrong station
            // doesn't drag a neighbour down with it.
            let disagree = votes.iter().any(|&(n, bad)| bad * 2 > n);
            a.surface_mm = pct(&mut gaps);
            a.verdict = if disagree {
                "doubtful"
            } else {
                match a.ties.len() {
                    0 => "frame",
                    1 => "single",
                    _ => "confirmed",
                }
            }
            .into();
            a
        })
        .collect();
    let evidence: Evidence = jobs
        .iter()
        .zip(&checks)
        .filter_map(|(&(i, j), c)| {
            let rel = rel_of(i, j);
            pair_evidence(c, rel[1].hypot(rel[2])).map(|ok| ((i, j), ok))
        })
        .collect();
    settle(ids, names, &mut out, &evidence);
    out
}

/// Verdicts that depend on the whole tie graph, not one station's checks.
///
/// A block of stations tied among themselves vouches for itself: each has
/// two or more ties and its neighbours agree with it, because they moved
/// with it. So:
///
/// 1. **A block joined to the rest by one tie only** (a bridge in the tie
///    graph, with two or more stations on each side) is `grouped`, unless
///    other station pairs across the join back it up (`JOIN_BACKING`): a
///    wrong tie between repeated facades moves the whole block where it
///    overlaps nothing. The side whose own checks hold up is kept (more
///    stations trusted before this pass, then the larger side).
/// 2. **Ties only count to trusted stations.** A station is `confirmed` with
///    two or more ties to trusted stations, `single` with one, and
///    `doubtful` with none.
/// 3. **Trusted stations hang together.** Trusted stations that reach the
///    largest trusted block only through stations in doubt are `doubtful`,
///    unless pairs between the two blocks back them up the same way.
///
/// 2 and 3 repeat until nothing changes.
/// What the audit's checks say about a pair of stations under their current
/// poses, keyed by (lower, higher) index: `true` when there is geometric
/// evidence (a firm refit or a surface comparison) and no check disagrees,
/// `false` when a check disagrees. Pairs with no evidence are left out.
type Evidence = HashMap<(usize, usize), bool>;

fn pair_evidence(c: &PairChecks, apart_m: f64) -> Option<bool> {
    let surface_mm = (c.gaps.len() >= 300).then(|| {
        let mut g = c.gaps.clone();
        g.sort_by(|x, y| x.total_cmp(y));
        g[g.len() / 2] as f64 * 1000.0
    });
    let bad = c.refit.is_some_and(|[mm, deg]| mm > refit_limit_mm(apart_m) || deg > REFIT_BAD_DEG)
        || c.free_space.is_some_and(|f| f > FREE_SPACE_BAD)
        || surface_mm.is_some_and(|mm| mm > surface_limit_mm(apart_m))
        || c.ground.is_some_and(|d| d.abs() > GROUND_BAD_M);
    if bad {
        Some(false)
    } else if c.refit.is_some() || surface_mm.is_some() {
        Some(true)
    } else {
        None
    }
}

/// A join between two sets of stations counts as checked when at least this
/// many pairs across it, other than the tie that makes it, agree, and they
/// outnumber those that disagree (a strict majority, as in the audit's own
/// votes: far pairs sometimes read just over a limit). A wrong join between
/// repeated facades puts the two sides where they overlap nothing, so no
/// pair across it has evidence.
const JOIN_BACKING: usize = 2;

fn settle(ids: &[String], names: &[String], audits: &mut [Audit], evidence: &Evidence) {
    let n = audits.len();
    let index: HashMap<&str, usize> = ids.iter().enumerate().map(|(k, s)| (s.as_str(), k)).collect();
    let placed = |a: &Audit| matches!(a.verdict.as_str(), "confirmed" | "single" | "doubtful" | "frame");
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (k, a) in audits.iter().enumerate() {
        if !placed(a) {
            continue;
        }
        for o in &a.ties {
            if let Some(&j) = index.get(o.as_str()) {
                if j > k && placed(&audits[j]) {
                    edges.push((k, j));
                }
            }
        }
    }
    // Stations reached from `from` without crossing edge `skip`.
    let reach = |from: usize, skip: Option<usize>| -> Vec<bool> {
        let mut seen = vec![false; n];
        let mut stack = vec![from];
        seen[from] = true;
        while let Some(u) = stack.pop() {
            for (e, &(a, b)) in edges.iter().enumerate() {
                if Some(e) == skip {
                    continue;
                }
                let v = if a == u { b } else if b == u { a } else { continue };
                if !seen[v] {
                    seen[v] = true;
                    stack.push(v);
                }
            }
        }
        seen
    };
    let label = |ks: &[usize]| ks.iter().map(|&k| names[k].clone()).collect::<Vec<_>>().join(", ");
    // Pairs across a join (one side to the other) that agree and disagree,
    // leaving out the tie `skip` that makes the join.
    let backing = |xs: &[usize], ys: &[usize], skip: Option<(usize, usize)>| -> (usize, usize) {
        let (mut agree, mut disagree) = (0, 0);
        for &x in xs {
            for &y in ys {
                let key = (x.min(y), x.max(y));
                if Some(key) == skip {
                    continue;
                }
                match evidence.get(&key) {
                    Some(true) => agree += 1,
                    Some(false) => disagree += 1,
                    None => {}
                }
            }
        }
        (agree, disagree)
    };
    let backed = |(agree, disagree): (usize, usize)| agree >= JOIN_BACKING && agree > disagree;
    // Which side of a join to keep: the one whose own checks hold up (more
    // trusted stations before this pass), then the larger, then the one
    // holding the lowest station.
    let pre: Vec<bool> = audits.iter().map(Audit::trusted).collect();
    let standing = |s: &[usize]| (s.iter().filter(|&&k| pre[k]).count(), s.len(), std::cmp::Reverse(s.iter().min().copied()));
    let mut weak = vec![false; n];
    for (e, &(a, b)) in edges.iter().enumerate() {
        let whole = reach(a, None);
        let side_a = reach(a, Some(e));
        if side_a[b] {
            continue; // another path: not a bridge
        }
        let sa: Vec<usize> = (0..n).filter(|&k| side_a[k]).collect();
        let sb: Vec<usize> = (0..n).filter(|&k| whole[k] && !side_a[k]).collect();
        if sa.len() < 2 || sb.len() < 2 {
            continue; // a lone station on one pair is `single`, checked by the audit
        }
        let (small, big) = if standing(&sa) < standing(&sb) { (sa, sb) } else { (sb, sa) };
        let (inner, outer) = if small.contains(&a) { (a, b) } else { (b, a) };
        let across = backing(&small, &big, Some((a.min(b), a.max(b))));
        if backed(across) {
            continue; // other overlapping stations agree with the join
        }
        let check = match across {
            (0, 0) => "no other pair across it has evidence".to_string(),
            (ag, d) => format!("of the other pairs across it {ag} agree and {d} disagree"),
        };
        for &k in &small {
            if weak[k] {
                continue;
            }
            weak[k] = true;
            audits[k].verdict = "grouped".into();
            audits[k].reasons.push(format!(
                "its block (stations {}) joins the other {} stations by one tie only ({}–{}), and {check}; a station overlapping both sides is needed",
                label(&small), big.len(), names[inner], names[outer]
            ));
        }
    }
    // Ties count only to trusted stations.
    loop {
        let trusted: Vec<bool> = audits.iter().map(Audit::trusted).collect();
        let mut changed = false;
        for a in audits.iter_mut() {
            if !matches!(a.verdict.as_str(), "confirmed" | "single") {
                continue;
            }
            let good = a.ties.iter().filter(|o| index.get(o.as_str()).is_some_and(|&j| trusted[j])).count();
            let v = match good {
                0 => "doubtful",
                1 => "single",
                _ => "confirmed",
            };
            if v != a.verdict {
                if v == "doubtful" {
                    let doubted: Vec<usize> = a.ties.iter().filter_map(|o| index.get(o.as_str()).copied()).collect();
                    a.reasons.push(format!("it is tied only to stations that are themselves in doubt ({})", label(&doubted)));
                }
                a.verdict = v.into();
                changed = true;
            }
        }
        // Blocks of trusted stations, through trusted ties only.
        let trusted: Vec<bool> = audits.iter().map(Audit::trusted).collect();
        let mut block = vec![usize::MAX; n];
        let mut sizes: Vec<usize> = Vec::new();
        for s in 0..n {
            if !trusted[s] || block[s] != usize::MAX {
                continue;
            }
            let id = sizes.len();
            let (mut stack, mut size) = (vec![s], 0);
            block[s] = id;
            while let Some(u) = stack.pop() {
                size += 1;
                for &(a, b) in &edges {
                    let v = if a == u { b } else if b == u { a } else { continue };
                    if trusted[v] && block[v] == usize::MAX {
                        block[v] = id;
                        stack.push(v);
                    }
                }
            }
            sizes.push(size);
        }
        // The largest block (on a tie, the one found first: the lowest station).
        if let Some(main) = (0..sizes.len()).max_by(|&x, &y| sizes[x].cmp(&sizes[y]).then(y.cmp(&x))) {
            let members = |b: usize| (0..n).filter(|&k| block[k] == b).collect::<Vec<_>>();
            let main_members = members(main);
            let held: Vec<bool> = (0..sizes.len()).map(|b| b == main || backed(backing(&members(b), &main_members, None))).collect();
            for k in 0..n {
                if trusted[k] && !held[block[k]] {
                    // The stations in doubt this block hangs on.
                    let via: Vec<usize> = (0..n)
                        .filter(|&j| !trusted[j] && edges.iter().any(|&(x, y)| (x == j && block[y] == block[k]) || (y == j && block[x] == block[k])))
                        .collect();
                    audits[k].verdict = "doubtful".into();
                    audits[k].reasons.push(format!("it reaches the other trusted stations only through stations in doubt ({})", label(&via)));
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

// -------------------------------------------------------------- the record ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationPose {
    pub scan_id: String,
    pub station: u16,
    /// None: not placed (no pair with a placed station fits).
    pub pose: Option<Pose>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<Audit>,
}

/// A project's registration as the bridge keeps it (`registrations.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub project_id: String,
    /// The frame: the first station's scan.
    pub frame_scan_id: Option<String>,
    pub stations: Vec<StationPose>,
    pub pairs: Vec<Pair>,
    pub walls: Vec<Wall>,
    pub summary: String,
    pub updated_at: String,
}

impl Registration {
    /// Trusted stations' scan ids by station number: placed, and confirmed,
    /// single or the frame. Only these count towards coverage.
    pub fn station_scans(&self) -> HashMap<u16, String> {
        self.stations
            .iter()
            .filter(|s| s.pose.is_some() && s.audit.as_ref().is_none_or(Audit::trusted))
            .map(|s| (s.station, s.scan_id.clone()))
            .collect()
    }

    /// The station whose scanner frame is the project frame.
    pub fn frame_station(&self) -> u16 {
        self.frame_scan_id
            .as_ref()
            .and_then(|f| self.stations.iter().find(|s| &s.scan_id == f))
            .or(self.stations.first())
            .map(|s| s.station)
            .unwrap_or(1)
    }

    /// "12 of 12 stations in one frame; 26 pairs agree within 9 mm"
    pub fn summarise(&mut self) {
        let placed = self.stations.iter().filter(|s| s.pose.is_some()).count();
        let used: Vec<&Pair> = self.pairs.iter().filter(|p| p.good && !p.outlier && p.left_mm.is_some()).collect();
        let worst = used.iter().filter_map(|p| p.left_mm).fold(0f64, f64::max);
        let rms = used.iter().filter_map(|p| p.rms_mm).fold(0f64, f64::max);
        let s = |u: usize| if u == 1 { "" } else { "s" };
        let mut summary = match (self.stations.len(), used.len()) {
            (0, _) => "No full domes in this project yet.".into(),
            (1, _) => "One station: it is the project frame.".into(),
            (n, 0) => format!("{placed} of {n} stations in one frame; no pairs fit yet."),
            // A chain: each pair is the only tie, so there is nothing to cross-check yet.
            (n, u) if u < placed => format!("{placed} of {n} stations in one frame; {u} pair{} fit within {rms:.0} mm (no loop to cross-check yet).", s(u)),
            (n, u) => format!("{placed} of {n} stations in one frame; {u} pair{} agree within {worst:.0} mm.", s(u)),
        };
        let count = |v: &str| self.stations.iter().filter(|s| s.audit.as_ref().is_some_and(|a| a.verdict == v)).count();
        let verdicts: Vec<String> = ["confirmed", "single", "doubtful", "grouped", "ambiguous", "unplaced"]
            .iter()
            .filter(|v| count(v) > 0)
            .map(|v| format!("{} {v}", count(v)))
            .collect();
        if self.stations.len() > 1 && !verdicts.is_empty() {
            summary = format!("{} Stations: {}.", summary, verdicts.join(", "));
        }
        self.summary = summary;
    }

    pub fn to_json(&self) -> Value {
        let number = |id: &str| self.stations.iter().find(|s| s.scan_id == id).map(|s| s.station);
        let frame = self.frame_station();
        json!({
            "project_id": self.project_id,
            "frame": format!("station {frame}'s scanner: x and y as it happened to face (not north), z up, the origin at its centre; metres"),
            "summary": self.summary,
            "updated_at": self.updated_at,
            "stations": self.stations.iter().map(|s| {
                let a = s.audit.clone().unwrap_or_default();
                json!({
                    "station": s.station, "scan_id": s.scan_id, "placed": s.pose.is_some(),
                    "turn_deg": s.pose.map(|p| p.turn_deg), "xyz_m": s.pose.map(|p| p.xyz_m),
                    "verdict": a.verdict, "counts_for_coverage": s.pose.is_some() && a.trusted(),
                    "tied_to": a.ties.iter().filter_map(|t| number(t)).collect::<Vec<_>>(),
                    "checks": {"surface_mm": a.surface_mm, "free_space": a.free_space, "ground_mm": a.ground_mm},
                    "reasons": a.reasons,
                })
            }).collect::<Vec<_>>(),
            "pairs": self.pairs.iter().filter(|p| p.good).map(|p| json!({
                "stations": [number(&p.a), number(&p.b)], "rms_mm": p.rms_mm, "matches": p.matches,
                "constraint": p.constraint, "resolved_by": p.resolved_by, "left_mm": p.left_mm, "outlier": p.outlier,
            })).collect::<Vec<_>>(),
            "rejected_pairs": self.pairs.iter().filter(|p| p.weak.is_some() || p.ambiguous).map(|p| json!({
                "stations": [number(&p.a), number(&p.b)], "rms_mm": p.rms_mm, "matches": p.matches, "why": p.rejected,
            })).collect::<Vec<_>>(),
            "pairs_tried": self.pairs.len(),
            "walls": self.walls,
            "plan_url": format!("/v1/projects/{}/registration/plan", self.project_id),
        })
    }
}

/// What a mission needs of the registration: walls by id, and judging faces.
pub struct Geo {
    pub walls: Vec<Wall>,
    pub merged: Option<std::sync::Arc<Merged>>,
    pub scans: HashMap<u16, String>,
}

impl Geo {
    pub fn none() -> Geo {
        Geo { walls: vec![], merged: None, scans: HashMap::new() }
    }
    pub fn wall(&self, id: &str) -> Option<Face> {
        self.walls.iter().find(|w| w.wall_id == id).map(|w| Face { from: w.from, to: w.to })
    }
    /// The face's coverage, when the project is registered.
    pub fn judge(&self, f: &Face) -> Option<Value> {
        self.merged.as_ref().map(|m| face_coverage(m, f, &self.scans))
    }
}

// ------------------------------------------------------------------ merged ---

/// Vertical surfaces of every registered station in the project frame (10 cm
/// points): position, point spacing there (mm) and which station saw it.
#[derive(Default)]
pub struct Merged {
    pub pts: Vec<[f32; 3]>,
    pub spacing_mm: Vec<f32>,
    pub station: Vec<u16>,
    /// Every 10 cm point in plan (x, y), for drawing the ground and clutter.
    pub plan: Vec<[f32; 2]>,
    /// The ground: the lowest level surface seen in each 1 m plan cell.
    pub ground: HashMap<(i32, i32), f32>,
}

/// Without a ground grid (merged before walls were classified).
const MERGED_MAGIC_V1: &[u8; 4] = b"X9M1";
const MERGED_MAGIC: &[u8; 4] = b"X9M2";

impl Merged {
    pub fn add(&mut self, p: &Prepared, pose: &Pose, station: u16) {
        let step_rad = p.step_rad;
        for i in 0..p.pts.len() {
            let w = pose.apply(p.pts[i]);
            self.plan.push([w[0], w[1]]);
            // Level and below the scanner: ground (or a floor, a step, a table top
            // below eye level; the lowest per cell wins).
            if p.planar[i] && p.normal[i][2].abs() > 0.95 && p.pts[i][2] < -0.5 {
                let e = self.ground.entry((w[0].floor() as i32, w[1].floor() as i32)).or_insert(f32::MAX);
                *e = e.min(w[2]);
            }
            if p.wall[i] {
                self.pts.push(w);
                self.spacing_mm.push(p.range[i] * step_rad * 1000.0);
                self.station.push(station);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(16 + self.pts.len() * 18 + self.plan.len() * 8 + self.ground.len() * 12);
        b.extend_from_slice(MERGED_MAGIC);
        b.extend_from_slice(&(self.pts.len() as u32).to_le_bytes());
        b.extend_from_slice(&(self.plan.len() as u32).to_le_bytes());
        b.extend_from_slice(&(self.ground.len() as u32).to_le_bytes());
        for (i, p) in self.pts.iter().enumerate() {
            for v in p {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&self.spacing_mm[i].to_le_bytes());
            b.extend_from_slice(&self.station[i].to_le_bytes());
        }
        for p in &self.plan {
            b.extend_from_slice(&p[0].to_le_bytes());
            b.extend_from_slice(&p[1].to_le_bytes());
        }
        let mut cells: Vec<(&(i32, i32), &f32)> = self.ground.iter().collect();
        cells.sort_by_key(|c| *c.0);
        for ((x, y), z) in cells {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
            b.extend_from_slice(&z.to_le_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Result<Merged, String> {
        let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let v2 = b.len() >= 16 && &b[..4] == MERGED_MAGIC;
        if !v2 && (b.len() < 12 || &b[..4] != MERGED_MAGIC_V1) {
            return Err("not a merged cloud".into());
        }
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let (n, np, ng, head) = if v2 { (u(4), u(8), u(12), 16) } else { (u(4), u(8), 0, 12) };
        if b.len() != head + n * 18 + np * 8 + ng * 12 {
            return Err("merged cloud is truncated".into());
        }
        let mut m = Merged::default();
        for i in 0..n {
            let o = head + i * 18;
            m.pts.push([f(o), f(o + 4), f(o + 8)]);
            m.spacing_mm.push(f(o + 12));
            m.station.push(u16::from_le_bytes([b[o + 16], b[o + 17]]));
        }
        let o = head + n * 18;
        m.plan = (0..np).map(|i| [f(o + i * 8), f(o + i * 8 + 4)]).collect();
        let o = o + np * 8;
        let i32at = |i: usize| i32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        m.ground = (0..ng).map(|k| ((i32at(o + k * 12), i32at(o + k * 12 + 4)), f(o + k * 12 + 8))).collect();
        Ok(m)
    }
}

// ------------------------------------------------------------------- walls ---

/// A straight run of wall in plan (project frame, metres).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wall {
    pub wall_id: String,
    pub from: [f64; 2],
    pub to: [f64; 2],
    pub length_m: f64,
    /// Height seen (bottom and top, project frame), from points tight to the line.
    pub z_m: [f64; 2],
    /// Station numbers that saw it.
    pub seen_by: Vec<u16>,
    /// `building` (flat and at least 2.4 m above the ground), `low wall`
    /// (flat, lower: a garden wall, a fence, a trimmed hedge face) or
    /// `vegetation` (rough: hedges and shrubs against a wall, trees).
    #[serde(default)]
    pub kind: String,
    /// Share of the points near the line (15 cm) that lie within 2 cm of it.
    #[serde(default)]
    pub flatness: f64,
    /// How far its top rises above the ground near it (m), when the ground is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_m: Option<f64>,
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545F4914F6CDD1D) % n.max(1) as u64) as usize
    }
}

const WALL_TOL: f32 = 0.04;
const WALL_GAP: f32 = 0.6;
const WALL_MIN: f32 = 1.5;
const MAX_WALLS: usize = 60;

/// Straight walls in the merged vertical points, longest first.
pub fn walls(m: &Merged) -> Vec<Wall> {
    // One point per 5 cm plan cell (keeping a station that saw it and its height span).
    // Plan cell → (a point, lowest and highest z, stations seen by, as bits).
    type Cell = ([f32; 2], f32, f32, u64);
    let mut cells: HashMap<(i32, i32), Cell> = HashMap::new();
    for (i, p) in m.pts.iter().enumerate() {
        let k = ((p[0] / 0.05).floor() as i32, (p[1] / 0.05).floor() as i32);
        let e = cells.entry(k).or_insert(([p[0], p[1]], p[2], p[2], 0));
        e.1 = e.1.min(p[2]);
        e.2 = e.2.max(p[2]);
        e.3 |= 1u64 << (m.station[i].min(63));
    }
    // A plan cell is wall if its points span at least 0.5 m of height.
    let mut pts: Vec<Cell> = cells.into_values().filter(|c| c.2 - c.1 >= 0.5).collect();
    pts.sort_by(|a, b| a.0[0].total_cmp(&b.0[0]).then(a.0[1].total_cmp(&b.0[1])));
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::new();
    let mut alive: Vec<bool> = vec![true; pts.len()];
    let mut misses = 0;
    while out.len() < MAX_WALLS && misses < 8 {
        let idx: Vec<usize> = (0..pts.len()).filter(|&i| alive[i]).collect();
        if idx.len() < 40 {
            break;
        }
        let mut best: Option<([f32; 2], [f32; 2], usize)> = None;
        for _ in 0..600 {
            let i = idx[rng.below(idx.len())];
            let j = idx[rng.below(idx.len())];
            let (a, b) = (pts[i].0, pts[j].0);
            let d = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            if !(0.5..=8.0).contains(&d) {
                continue;
            }
            let dir = [(b[0] - a[0]) / d, (b[1] - a[1]) / d];
            let nrm = [-dir[1], dir[0]];
            let count = idx.iter().filter(|&&k| ((pts[k].0[0] - a[0]) * nrm[0] + (pts[k].0[1] - a[1]) * nrm[1]).abs() < WALL_TOL).count();
            if best.is_none_or(|b| count > b.2) {
                best = Some((a, nrm, count));
            }
        }
        let Some((a, nrm, count)) = best else { break };
        if count < 30 {
            break;
        }
        // Refit to the inliers (principal direction), then split at gaps.
        let inl: Vec<usize> = idx.iter().copied().filter(|&k| ((pts[k].0[0] - a[0]) * nrm[0] + (pts[k].0[1] - a[1]) * nrm[1]).abs() < WALL_TOL).collect();
        let n = inl.len() as f32;
        let c = inl.iter().fold([0f32; 2], |s, &k| [s[0] + pts[k].0[0] / n, s[1] + pts[k].0[1] / n]);
        let (mut sxx, mut sxy, mut syy) = (0f32, 0f32, 0f32);
        for &k in &inl {
            let (dx, dy) = (pts[k].0[0] - c[0], pts[k].0[1] - c[1]);
            sxx += dx * dx;
            sxy += dx * dy;
            syy += dy * dy;
        }
        let ang = 0.5 * (2.0 * sxy).atan2(sxx - syy);
        let dir = [ang.cos(), ang.sin()];
        let nrm = [-dir[1], dir[0]];
        let mut on: Vec<(f32, usize)> = idx
            .iter()
            .copied()
            .filter(|&k| ((pts[k].0[0] - c[0]) * nrm[0] + (pts[k].0[1] - c[1]) * nrm[1]).abs() < WALL_TOL)
            .map(|k| ((pts[k].0[0] - c[0]) * dir[0] + (pts[k].0[1] - c[1]) * dir[1], k))
            .collect();
        on.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut kept = false;
        let mut start = 0;
        for e in 1..=on.len() {
            if e < on.len() && on[e].0 - on[e - 1].0 <= WALL_GAP {
                continue;
            }
            let run = &on[start..e];
            let len = run.last().map(|l| l.0).unwrap_or(0.0) - run[0].0;
            // At least half the run's 5 cm cells hold wall.
            if len >= WALL_MIN && run.len() as f32 >= 0.5 * len / 0.05 {
                let (u0, u1) = (run[0].0, run.last().unwrap().0);
                let p = |u: f32| [((c[0] + dir[0] * u) * 100.0).round() as f64 / 100.0, ((c[1] + dir[1] * u) * 100.0).round() as f64 / 100.0];
                let zlo = run.iter().map(|r| pts[r.1].1).fold(f32::MAX, f32::min);
                let zhi = run.iter().map(|r| pts[r.1].2).fold(f32::MIN, f32::max);
                let mask = run.iter().fold(0u64, |s, r| s | pts[r.1].3);
                out.push(Wall {
                    wall_id: String::new(),
                    from: p(u0),
                    to: p(u1),
                    length_m: ((u1 - u0) as f64 * 100.0).round() / 100.0,
                    z_m: [(zlo as f64 * 100.0).round() / 100.0, (zhi as f64 * 100.0).round() / 100.0],
                    seen_by: (0..64u16).filter(|b| mask & (1u64 << b) != 0).collect(),
                    kind: String::new(),
                    flatness: 0.0,
                    height_m: None,
                });
                kept = true;
            }
            start = e;
        }
        for &(_, k) in &on {
            alive[k] = false;
        }
        misses = if kept { 0 } else { misses + 1 };
    }
    for w in out.iter_mut() {
        classify(m, w);
    }
    // Buildings first, then low walls, then vegetation; longest first in each.
    let rank = |k: &str| match k {
        "building" => 0,
        "low wall" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| rank(&a.kind).cmp(&rank(&b.kind)).then(b.length_m.total_cmp(&a.length_m)));
    for (i, w) in out.iter_mut().enumerate() {
        w.wall_id = format!("w{}", i + 1);
    }
    out
}

/// A building wall is flat and rises this far above the ground.
const BUILDING_HEIGHT_M: f64 = 2.4;
/// Below this flatness a run is vegetation (hedges, shrubs, trees).
const FLAT_MIN: f64 = 0.55;

/// Measure a wall run from the points near its line: how flat it is (share
/// of points within 15 cm that lie within 2 cm, for the station that sees it
/// most sharply: stations' small disagreements and far, noisy views would
/// smear a flat wall), its height from points within 4 cm (so a tree over it
/// doesn't count), and its top above the ground near it. Then name its kind.
fn classify(m: &Merged, w: &mut Wall) {
    let (a, b) = ([w.from[0] as f32, w.from[1] as f32], [w.to[0] as f32, w.to[1] as f32]);
    let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt().max(1e-3);
    let dir = [(b[0] - a[0]) / len, (b[1] - a[1]) / len];
    let nrm = [-dir[1], dir[0]];
    let mut per: HashMap<u16, Vec<f32>> = HashMap::new();
    let mut zs: Vec<f32> = Vec::new();
    for (i, p) in m.pts.iter().enumerate() {
        let (dx, dy) = (p[0] - a[0], p[1] - a[1]);
        let u = dx * dir[0] + dy * dir[1];
        if u < 0.0 || u > len {
            continue;
        }
        let signed = dx * nrm[0] + dy * nrm[1];
        let d = signed.abs();
        if d > 0.15 {
            continue;
        }
        // Each station is judged against its own line (its median offset), so
        // the stations' small disagreements don't count as roughness.
        per.entry(m.station[i]).or_default().push(signed);
        if d <= 0.04 {
            zs.push(p[2]);
        }
    }
    let best = per
        .values_mut()
        .filter(|v| v.len() >= 40)
        .map(|v| {
            v.sort_by(|x, y| x.total_cmp(y));
            let mid = v[v.len() / 2];
            v.iter().filter(|d| (**d - mid).abs() <= 0.02).count() as f64 / v.len() as f64
        })
        .fold(None, |b: Option<f64>, f| Some(b.map_or(f, |b: f64| b.max(f))));
    w.flatness = (best.unwrap_or(0.0) * 1000.0).round() / 1000.0;
    if !zs.is_empty() {
        zs.sort_by(|x, y| x.total_cmp(y));
        let at = |q: f64| zs[((zs.len() - 1) as f64 * q) as usize] as f64;
        w.z_m = [(at(0.02) * 100.0).round() / 100.0, (at(0.98) * 100.0).round() / 100.0];
    }
    // The ground near it: the lowest level cell within 2 m of the run.
    let mut ground = f32::MAX;
    let steps = (len / 1.0).ceil() as i32;
    for k in 0..=steps {
        let t = (k as f32).min(len);
        let (x, y) = (a[0] + dir[0] * t, a[1] + dir[1] * t);
        for gx in -2..=2 {
            for gy in -2..=2 {
                if let Some(z) = m.ground.get(&(x.floor() as i32 + gx, y.floor() as i32 + gy)) {
                    ground = ground.min(*z);
                }
            }
        }
    }
    w.height_m = (ground < f32::MAX && !zs.is_empty()).then(|| ((w.z_m[1] - ground as f64) * 100.0).round() / 100.0);
    w.kind = if w.flatness < FLAT_MIN {
        "vegetation"
    } else if w.height_m.is_none_or(|h| h >= BUILDING_HEIGHT_M) {
        "building"
    } else {
        "low wall"
    }
    .into();
}

// ------------------------------------------------------------------- faces ---

/// A wall face in plan: the stretch from `from` to `to` (project frame, m).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Face {
    pub from: [f64; 2],
    pub to: [f64; 2],
}

/// Points within this of the face's line count as on it.
const FACE_TOL: f32 = 0.15;
/// The face is judged in columns this wide; a column is seen when its points
/// span at least `COLUMN_SEEN` of height (windows leave the wall around them).
const COLUMN: f32 = 0.5;
const COLUMN_SEEN: f32 = 1.0;
/// Share of the face's length that must be seen for it to count as covered.
pub const FACE_COVERED: f64 = 0.8;

/// How much of a face the registered stations saw: share of its length,
/// spacing there, which stations, and the unseen stretches.
pub fn face_coverage(m: &Merged, f: &Face, station_scan: &HashMap<u16, String>) -> Value {
    let (a, b) = ([f.from[0] as f32, f.from[1] as f32], [f.to[0] as f32, f.to[1] as f32]);
    let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
    if len < 0.2 {
        return json!({"error": "the face is shorter than 20 cm"});
    }
    let dir = [(b[0] - a[0]) / len, (b[1] - a[1]) / len];
    let nrm = [-dir[1], dir[0]];
    let ncol = (len / COLUMN).ceil().max(1.0) as usize;
    let mut col: Vec<(f32, f32)> = vec![(f32::MAX, f32::MIN); ncol];
    let mut per_station: HashMap<u16, HashSet<usize>> = HashMap::new();
    let mut spacing: Vec<f32> = Vec::new();
    let mut zs: Vec<f32> = Vec::new();
    for (i, p) in m.pts.iter().enumerate() {
        // Only stations that count (see `Registration::station_scans`).
        if !station_scan.contains_key(&m.station[i]) {
            continue;
        }
        let (dx, dy) = (p[0] - a[0], p[1] - a[1]);
        let u = dx * dir[0] + dy * dir[1];
        if u < 0.0 || u > len || (dx * nrm[0] + dy * nrm[1]).abs() > FACE_TOL {
            continue;
        }
        let c = ((u / COLUMN) as usize).min(ncol - 1);
        col[c].0 = col[c].0.min(p[2]);
        col[c].1 = col[c].1.max(p[2]);
        per_station.entry(m.station[i]).or_default().insert(c);
        spacing.push(m.spacing_mm[i]);
        zs.push(p[2]);
    }
    let seen: Vec<bool> = col.iter().map(|(lo, hi)| hi - lo >= COLUMN_SEEN).collect();
    let share = seen.iter().filter(|s| **s).count() as f64 / ncol as f64;
    // Unseen stretches, as plan points.
    let at = |u: f32| [((a[0] + dir[0] * u) * 100.0).round() as f64 / 100.0, ((a[1] + dir[1] * u) * 100.0).round() as f64 / 100.0];
    let mut gaps = Vec::new();
    let mut i = 0;
    while i < ncol {
        if seen[i] {
            i += 1;
            continue;
        }
        let j = (i..ncol).find(|&k| seen[k]).unwrap_or(ncol);
        gaps.push(json!({"from": at(i as f32 * COLUMN), "to": at((j as f32 * COLUMN).min(len)), "length_m": (((j - i) as f32 * COLUMN).min(len) * 10.0).round() / 10.0}));
        i = j;
    }
    spacing.sort_by(|x, y| x.total_cmp(y));
    zs.sort_by(|x, y| x.total_cmp(y));
    let pct = |v: &[f32], q: f64| v.get(((v.len() as f64 - 1.0) * q).round() as usize).map(|x| (*x as f64 * 10.0).round() / 10.0);
    let mut stations: Vec<(u16, usize)> = per_station.iter().map(|(s, c)| (*s, c.iter().filter(|k| seen[**k]).count())).filter(|x| x.1 > 0).collect();
    stations.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
    json!({
        "length_m": (len as f64 * 100.0).round() / 100.0,
        "seen_share": (share * 1000.0).round() / 1000.0,
        "covered": share >= FACE_COVERED,
        "height_m": [pct(&zs, 0.02), pct(&zs, 0.98)],
        "spacing_mm": {"p50": pct(&spacing, 0.5), "p90": pct(&spacing, 0.9)},
        "stations": stations.iter().map(|(s, c)| json!({"station": s, "scan_id": station_scan.get(s), "seen_share": ((*c as f64 / ncol as f64) * 1000.0).round() / 1000.0})).collect::<Vec<_>>(),
        "unseen": gaps,
    })
}

// ------------------------------------------------------------------ picture ---

/// What to draw on the plan besides the points.
pub struct PlanMarks<'a> {
    /// Number, pose, and whether it counts (others are drawn amber, "5?").
    pub stations: &'a [(u16, Pose, bool)],
    pub walls: &'a [Wall],
    /// Target faces: id, face, colour.
    pub faces: Vec<(String, Face, [u8; 3])>,
}

/// The registered plan: every point grey, vertical surfaces coloured by
/// spacing, walls with their ids, stations numbered, a 5 m grid. PNG bytes.
pub fn plan_png(m: &Merged, marks: &PlanMarks) -> Vec<u8> {
    use crate::station::Canvas;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in m.plan.iter().chain(m.pts.iter().map(|p| [p[0], p[1]]).collect::<Vec<_>>().iter()) {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    if x0 > x1 {
        (x0, y0, x1, y1) = (-5.0, -5.0, 5.0, 5.0);
    }
    let (x0, y0, x1, y1) = (x0 - 2.0, y0 - 2.0, x1 + 2.0, y1 + 2.0);
    let ppm = (1800.0 / (x1 - x0).max(y1 - y0)).clamp(8.0, 40.0);
    let (w, h) = (((x1 - x0) * ppm) as usize + 1, ((y1 - y0) * ppm) as usize + 1);
    let to_px = |x: f32, y: f32| ((x - x0) * ppm, (y1 - y) * ppm);
    let mut c = Canvas::new(w, h, [17, 24, 39]);
    // 5 m grid.
    let mut g = (x0 / 5.0).ceil() * 5.0;
    while g < x1 {
        let (px, _) = to_px(g, 0.0);
        c.line((px, 0.0), (px, h as f32), [38, 46, 64], 1.0);
        g += 5.0;
    }
    let mut g = (y0 / 5.0).ceil() * 5.0;
    while g < y1 {
        let (_, py) = to_px(0.0, g);
        c.line((0.0, py), (w as f32, py), [38, 46, 64], 1.0);
        g += 5.0;
    }
    for p in &m.plan {
        let (px, py) = to_px(p[0], p[1]);
        c.put(px as i64, py as i64, [90, 96, 110], 0.35);
    }
    for (i, p) in m.pts.iter().enumerate() {
        let (px, py) = to_px(p[0], p[1]);
        let col = crate::view::class_colour(crate::view::class_of(m.spacing_mm[i] as f64));
        c.put(px as i64, py as i64, col, 1.0);
    }
    let font = crate::station::font();
    for wl in marks.walls {
        let (a, b) = (to_px(wl.from[0] as f32, wl.from[1] as f32), to_px(wl.to[0] as f32, wl.to[1] as f32));
        // A wall a target sits on is drawn as the target.
        if marks.faces.iter().any(|(_, f, _)| f.from == wl.from && f.to == wl.to) {
            continue;
        }
        let long = wl.length_m >= 3.0;
        c.line(a, b, [226, 232, 240], if long { 2.5 } else { 1.0 });
        if long {
            c.text(&font, 16.0, ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0 - 10.0), 0.5, &wl.wall_id, [226, 232, 240]);
        }
    }
    for (id, f, col) in &marks.faces {
        let (a, b) = (to_px(f.from[0] as f32, f.from[1] as f32), to_px(f.to[0] as f32, f.to[1] as f32));
        c.line(a, b, *col, 5.0);
        c.text(&font, 18.0, ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0 + 8.0), 0.5, id, *col);
    }
    for (n, pose, counts) in marks.stations {
        let (px, py) = to_px(pose.xyz_m[0] as f32, pose.xyz_m[1] as f32);
        let col = if *counts { [255, 255, 255] } else { [251, 191, 36] };
        c.ellipse((px, py), (7.0, 7.0), col, 1.0, true);
        // A tick for the way that station's scanner faced (its x axis).
        let t = pose.turn_deg.to_radians() as f32;
        c.line((px, py), (px + 16.0 * t.cos(), py - 16.0 * t.sin()), col, 2.0);
        let label = if *counts { n.to_string() } else { format!("{n}?") };
        c.text(&font, 20.0, (px + 12.0, py - 32.0), 0.0, &label, col);
    }
    c.text(&font, 16.0, (12.0, h as f32 - 34.0), 0.0, "grid 5 m · project frame (station 1's), not north", [203, 213, 225]);
    c.png()
}

// -------------------------------------------------------------- offline run ---

/// `x9-bridge register [--perturb N:dx,dy,deg] [--truth poses.json] OUT_DIR
/// STATION.tzf …`: register scan files offline (the first is the frame), as
/// the bridge does, one station more each time, then audit. Writes
/// poses.json, pairs.json, audit.json, walls.json and plan.png to OUT_DIR,
/// with timings. `--perturb` moves station N (1-based) after the solve, a
/// planted wrong placement the audit should catch; `--truth` compares the
/// poses with known ones (a poses.json from an earlier run).
pub fn cli(args: &[String]) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut perturb: Option<(usize, [f64; 3])> = None;
    let mut truth: Option<Vec<Option<Pose>>> = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--perturb" => {
                let v = it.next().context("--perturb N:dx,dy,deg")?;
                let (n, d) = v.split_once(':').context("--perturb N:dx,dy,deg")?;
                let d: Vec<f64> = d.split(',').map(str::parse).collect::<Result<_, _>>()?;
                anyhow::ensure!(d.len() == 3, "--perturb N:dx,dy,deg");
                perturb = Some((n.parse()?, [d[0], d[1], d[2]]));
            }
            "--truth" => truth = Some(serde_json::from_slice(&std::fs::read(it.next().context("--truth poses.json")?)?)?),
            _ => rest.push(a),
        }
    }
    let [out, files @ ..] = &rest[..] else { anyhow::bail!("usage: x9-bridge register [--perturb N:dx,dy,deg] [--truth poses.json] OUT_DIR STATION.tzf …") };
    std::fs::create_dir_all(out)?;
    let t0 = std::time::Instant::now();
    let mut clouds = Vec::new();
    for f in files {
        let t = trimble_tzf::Tzf::open(std::path::Path::new(f.as_str())).with_context(|| format!("opening {f}"))?;
        let meta = t.scan_meta().with_context(|| format!("{f}: metadata"))?;
        let pts = t.grid_points().with_context(|| format!("{f}: decoding"))?;
        clouds.push(Cloud::from_grid(&pts, crate::view::step_rad(&meta)));
    }
    eprintln!("decoded {} stations in {:.1} s", files.len(), t0.elapsed().as_secs_f64());
    let prep: Vec<Prepared> = clouds.iter().map(prepare).collect();
    for (k, (c, p)) in clouds.iter().zip(&prep).enumerate() {
        let far = p.pts.iter().filter(|q| q[0].hypot(q[1]) > 30.0).count();
        eprintln!("  file {}: cloud {} k points; prepared {} k ({} k beyond 30 m), ~{} MB", k + 1, c.pts.len() / 1000, p.pts.len() / 1000, far / 1000, p.approx_bytes() >> 20);
    }
    // Station numbers from the file names (station_07.tzf, 7.tzf), else their order.
    let number = |i: usize, f: &str| -> u16 {
        let stem = std::path::Path::new(f).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let digits: String = stem.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
        digits.parse().unwrap_or(i as u16 + 1)
    };
    let ids: Vec<String> = files.iter().enumerate().map(|(i, f)| number(i, f).to_string()).collect();
    let (mut pairs, mut poses): (Vec<Pair>, Vec<Option<Pose>>) = (Vec::new(), Vec::new());
    for k in 1..=files.len() {
        let t = std::time::Instant::now();
        let refs: Vec<&Prepared> = prep[..k].iter().collect();
        let (p2, groups) = register_all(&ids[..k], &refs, pairs, &|_, _| {});
        pairs = p2;
        poses = groups.poses();
        eprintln!("station {}: {} ({:.1} s)", ids[k - 1], if poses[k - 1].is_some() { "placed" } else { "NOT placed" }, t.elapsed().as_secs_f64());
    }
    for p in pairs.iter().filter(|p| p.rms_mm.is_some()) {
        eprintln!("  pair {}-{}: rms {:?} mm, {} matches, constraint {:.4}{}{}", p.a, p.b, p.rms_mm, p.matches, p.constraint,
                  p.resolved_by.as_deref().map(|r| format!(", resolved by {r}")).unwrap_or_default(),
                  p.rejected.as_deref().map(|r| format!(" — not used: {r}")).unwrap_or_default());
    }
    if let Some((n, [dx, dy, deg])) = perturb {
        if let Some(Some(p)) = poses.get_mut(n - 1) {
            p.xyz_m[0] += dx;
            p.xyz_m[1] += dy;
            p.turn_deg += deg;
            eprintln!("planted: station {n} moved by ({dx}, {dy}) m and {deg}°");
        }
    }
    let t = std::time::Instant::now();
    let refs: Vec<&Prepared> = prep.iter().collect();
    let mut cache = AuditCache::new();
    let audits = audit(&ids, &ids, &refs, &poses, &pairs, &mut cache);
    eprintln!("audit in {:.1} s", t.elapsed().as_secs_f64());
    // Every pair the audit checked, with how far apart the two stations are:
    // the evidence behind each verdict, for tuning the checks.
    let mut checked: Vec<Value> = cache
        .iter()
        .map(|((a, b), (rel, c))| {
            let mut g = c.gaps.clone();
            g.sort_by(|x, y| x.total_cmp(y));
            json!({
                "a": a, "b": b,
                "apart_m": (rel[1].hypot(rel[2]) * 100.0).round() / 100.0,
                "refit": c.refit,
                "surface_mm": (g.len() >= 300).then(|| (g[g.len() / 2] as f64 * 10_000.0).round() / 10.0),
                "surface_points": g.len(),
                "free_space": c.free_space,
                "ground_mm": c.ground.map(|d| (d * 1000.0).round()),
            })
        })
        .collect();
    checked.sort_by(|x, y| x["apart_m"].as_f64().unwrap_or(0.0).total_cmp(&y["apart_m"].as_f64().unwrap_or(0.0)));
    for (k, a) in audits.iter().enumerate() {
        let off = truth.as_ref().and_then(|tr| Some((tr.get(k)?.as_ref()?, poses[k].as_ref()?))).map(|(t, p)| {
            let mm = ((t.xyz_m[0] - p.xyz_m[0]).powi(2) + (t.xyz_m[1] - p.xyz_m[1]).powi(2) + (t.xyz_m[2] - p.xyz_m[2]).powi(2)).sqrt() * 1000.0;
            format!(", {mm:.0} mm from truth")
        });
        eprintln!("station {:>2}: {:<9} ties {:?} surface {:?} mm, refit {:?}, free space {:?}, ground {:?} mm{}{}", ids[k], a.verdict, a.ties, a.surface_mm, a.refit,
                  a.free_space, a.ground_mm, off.unwrap_or_default(),
                  if a.reasons.is_empty() { String::new() } else { format!("\n             {}", a.reasons.join("; ")) });
    }
    let mut merged = Merged::default();
    let mut stations = Vec::new();
    for (k, p) in poses.iter().enumerate() {
        if let Some(p) = p {
            let n: u16 = ids[k].parse().unwrap_or(k as u16 + 1);
            merged.add(&prep[k], p, n);
            stations.push((n, *p, audits[k].trusted()));
        }
    }
    let ws = walls(&merged);
    for w in &ws {
        eprintln!("  {:<4} {:<10} {:>6.1} m long, flatness {:.2}, height {:?} m, z {:?}, seen by {:?}", w.wall_id, w.kind, w.length_m, w.flatness, w.height_m, w.z_m, w.seen_by);
    }
    std::fs::write(format!("{out}/poses.json"), serde_json::to_vec_pretty(&poses)?)?;
    std::fs::write(format!("{out}/pairs.json"), serde_json::to_vec_pretty(&pairs)?)?;
    std::fs::write(format!("{out}/audit.json"), serde_json::to_vec_pretty(&audits)?)?;
    std::fs::write(format!("{out}/checks.json"), serde_json::to_vec_pretty(&checked)?)?;
    std::fs::write(format!("{out}/walls.json"), serde_json::to_vec_pretty(&ws)?)?;
    // For `ask`: the same merged cloud and trusted stations the server judges faces on.
    std::fs::write(format!("{out}/merged.bin"), merged.to_bytes())?;
    let listed: Vec<Value> = files
        .iter()
        .enumerate()
        .map(|(k, f)| json!({"station": ids[k].parse::<u16>().unwrap_or(k as u16 + 1), "file": f, "pose": poses[k], "trusted": poses[k].is_some() && audits[k].trusted()}))
        .collect();
    std::fs::write(format!("{out}/stations.json"), serde_json::to_vec_pretty(&listed)?)?;
    std::fs::write(format!("{out}/plan.png"), plan_png(&merged, &PlanMarks { stations: &stations, walls: &ws, faces: vec![] }))?;
    let map = crate::minimap::Minimap {
        title: format!("{} stations placed (offline)", stations.len()),
        walls: ws.clone(),
        stations: stations.clone(),
        faces: vec![],
        notes: poses.iter().enumerate().filter(|(_, p)| p.is_none()).map(|(k, _)| format!("Station {}: not placed", ids[k])).collect(),
    };
    std::fs::write(format!("{out}/minimap.svg"), map.svg())?;
    std::fs::write(format!("{out}/minimap.png"), map.png())?;
    eprintln!("{} of {} placed, {} trusted; total {:.1} s", poses.iter().flatten().count(), poses.len(),
              audits.iter().filter(|a| a.trusted()).count(), t0.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kd_tree_matches_brute_force() {
        let mut r = Rng(7);
        let pts: Vec<[f32; 3]> = (0..2000).map(|_| [r.below(1000) as f32 / 100.0, r.below(1000) as f32 / 100.0, r.below(300) as f32 / 100.0]).collect();
        let t = Tree::new(&pts);
        for q in pts.iter().step_by(97) {
            let q = [q[0] + 0.03, q[1] - 0.02, q[2] + 0.01];
            let brute = (0..pts.len()).min_by(|&a, &b| d2(pts[a], q).total_cmp(&d2(pts[b], q))).unwrap();
            assert_eq!(t.nearest(q, 1.0).unwrap().0, brute);
            let (nb, n) = t.knn::<5>(q);
            let mut all: Vec<f32> = pts.iter().map(|p| d2(*p, q)).collect();
            all.sort_by(|a, b| a.total_cmp(b));
            assert_eq!(n, 5);
            for k in 0..5 {
                assert!((nb[k].0 - all[k]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn smallest_eigenvector_of_a_flat_patch_is_its_normal() {
        let (v, curv) = smallest_eigen([[4.0, 0.0, 0.0], [0.0, 0.01, 0.0], [0.0, 0.0, 3.0]]);
        assert!(v[1].abs() > 0.999 && curv < 0.01);
    }

    /// Audits from (station, verdict, ties) as the per-station checks left them.
    fn audits(rows: &[(&str, &str, &[&str])]) -> (Vec<String>, Vec<Audit>) {
        let ids = rows.iter().map(|r| r.0.to_string()).collect();
        let a = rows
            .iter()
            .map(|r| Audit { verdict: r.1.into(), ties: r.2.iter().map(|s| s.to_string()).collect(), ..Default::default() })
            .collect();
        (ids, a)
    }

    fn verdicts(a: &[Audit]) -> Vec<&str> {
        a.iter().map(|a| a.verdict.as_str()).collect()
    }

    #[test]
    fn a_block_joined_by_one_pair_is_not_confirmed() {
        // 1-2-3-4 tied in a ring; 5 and 6 tied to each other and to the ring
        // through 4-5 only; 7 hangs off 1 alone.
        let (ids, mut a) = audits(&[
            ("1", "confirmed", &["2", "4", "7"]),
            ("2", "confirmed", &["1", "3"]),
            ("3", "confirmed", &["2", "4"]),
            ("4", "confirmed", &["3", "1", "5"]),
            ("5", "confirmed", &["4", "6"]),
            ("6", "single", &["5"]),
            ("7", "single", &["1"]),
        ]);
        settle(&ids, &ids, &mut a, &Evidence::new());
        assert_eq!(verdicts(&a), ["confirmed", "confirmed", "confirmed", "confirmed", "grouped", "grouped", "single"]);
        assert!(a[4].reasons[0].contains("stations 5, 6") && a[4].reasons[0].contains("(5–4)"), "{:?}", a[4].reasons);
    }

    #[test]
    fn the_smaller_side_is_marked_even_when_it_holds_the_frame() {
        // Station 1 (the frame) and 2 form the small side; 3, 4, 5 the big one.
        let (ids, mut a) = audits(&[
            ("1", "single", &["2"]),
            ("2", "confirmed", &["1", "3"]),
            ("3", "confirmed", &["2", "4", "5"]),
            ("4", "confirmed", &["3", "5"]),
            ("5", "confirmed", &["3", "4"]),
        ]);
        settle(&ids, &ids, &mut a, &Evidence::new());
        assert_eq!(verdicts(&a), ["grouped", "grouped", "confirmed", "confirmed", "confirmed"]);
    }

    #[test]
    fn the_side_whose_checks_hold_up_is_kept_over_the_larger() {
        // 1-3 hold up; 4-7 are larger but mostly doubtful already. They join
        // through 3-4 only.
        let (ids, mut a) = audits(&[
            ("1", "confirmed", &["2", "3"]),
            ("2", "confirmed", &["1", "3"]),
            ("3", "confirmed", &["1", "2", "4"]),
            ("4", "confirmed", &["3", "5", "6"]),
            ("5", "doubtful", &["4", "6"]),
            ("6", "doubtful", &["4", "5", "7"]),
            ("7", "doubtful", &["6"]),
        ]);
        settle(&ids, &ids, &mut a, &Evidence::new());
        assert_eq!(verdicts(&a), ["confirmed", "confirmed", "confirmed", "grouped", "grouped", "grouped", "grouped"]);
    }

    #[test]
    fn ties_count_only_to_trusted_stations() {
        // 4 is doubtful. 8 is tied only to 4. 5 and 7 are tied to each other
        // and to 4, so they reach the rest only through 4. 6 is tied to 4 and 1.
        let (ids, mut a) = audits(&[
            ("1", "confirmed", &["2", "3", "6"]),
            ("2", "confirmed", &["1", "3", "4"]),
            ("3", "confirmed", &["1", "2", "4"]),
            ("4", "doubtful", &["2", "3", "5", "6", "7", "8"]),
            ("5", "confirmed", &["4", "7"]),
            ("6", "confirmed", &["4", "1"]),
            ("7", "confirmed", &["5", "4"]),
            ("8", "single", &["4"]),
        ]);
        settle(&ids, &ids, &mut a, &Evidence::new());
        assert_eq!(verdicts(&a), ["confirmed", "confirmed", "confirmed", "doubtful", "doubtful", "single", "doubtful", "doubtful"]);
        for k in [4, 6, 7] {
            assert!(a[k].reasons.iter().any(|r| r.contains("in doubt")), "{:?}", a[k].reasons);
        }
        assert!(a[4].reasons.iter().any(|r| r.contains("in doubt (4)")), "names the station it hangs on: {:?}", a[4].reasons);
    }

    #[test]
    fn a_one_tie_join_backed_by_other_pairs_is_kept() {
        // As in the first test: 5 and 6 join the ring through 4-5 only.
        let rows: &[(&str, &str, &[&str])] = &[
            ("1", "confirmed", &["2", "4"]),
            ("2", "confirmed", &["1", "3"]),
            ("3", "confirmed", &["2", "4"]),
            ("4", "confirmed", &["3", "1", "5"]),
            ("5", "confirmed", &["4", "6"]),
            ("6", "single", &["5"]),
        ];
        // Two other pairs across the join agree: kept.
        let (ids, mut a) = audits(rows);
        let ev: Evidence = [((2, 4), true), ((3, 5), true), ((0, 1), true)].into();
        settle(&ids, &ids, &mut a, &ev);
        assert_eq!(verdicts(&a), ["confirmed", "confirmed", "confirmed", "confirmed", "confirmed", "single"]);
        // One agrees: not enough.
        let (ids, mut a) = audits(rows);
        settle(&ids, &ids, &mut a, &[((2, 4), true)].into());
        assert_eq!(verdicts(&a)[4..], ["grouped", "grouped"]);
        assert!(a[4].reasons[0].contains("1 agree and 0 disagree"), "{:?}", a[4].reasons);
        // Two agree and one disagrees: a majority, kept.
        let (ids, mut a) = audits(rows);
        settle(&ids, &ids, &mut a, &[((2, 4), true), ((3, 5), true), ((0, 5), false)].into());
        assert_eq!(verdicts(&a)[4..], ["confirmed", "single"]);
        // Two agree and two disagree: not kept.
        let (ids, mut a) = audits(rows);
        settle(&ids, &ids, &mut a, &[((2, 4), true), ((3, 5), true), ((0, 5), false), ((1, 4), false)].into());
        assert_eq!(verdicts(&a)[4..], ["grouped", "grouped"]);
        assert!(a[4].reasons[0].contains("2 agree and 2 disagree"), "{:?}", a[4].reasons);
        // The tie itself doesn't count as its own backing.
        let (ids, mut a) = audits(rows);
        settle(&ids, &ids, &mut a, &[((3, 4), true), ((2, 4), true)].into());
        assert_eq!(verdicts(&a)[4..], ["grouped", "grouped"]);
    }

    #[test]
    fn a_trusted_block_behind_a_doubtful_station_is_kept_when_backed() {
        // 5 and 7 reach the rest only through doubtful 4, but 5-1 and 7-2 agree.
        let rows: &[(&str, &str, &[&str])] = &[
            ("1", "confirmed", &["2", "3"]),
            ("2", "confirmed", &["1", "3", "4"]),
            ("3", "confirmed", &["1", "2", "4"]),
            ("4", "doubtful", &["2", "3", "5", "7"]),
            ("5", "confirmed", &["4", "7"]),
            ("7", "confirmed", &["5", "4"]),
        ];
        let (ids, mut a) = audits(rows);
        settle(&ids, &ids, &mut a, &[((0, 4), true), ((1, 5), true)].into());
        assert_eq!(verdicts(&a), ["confirmed", "confirmed", "confirmed", "doubtful", "single", "single"]);
    }

    #[test]
    fn limits_grow_with_the_lever_arm() {
        assert_eq!(refit_limit_mm(0.0), REFIT_BAD_MM);
        assert!((REFIT_BAD_PER_M - 1.396).abs() < 0.001, "0.08° is 1.4 mm per metre");
        // The admin building's corner pairs, all right: 14-15 (22 m, 36 mm) and 14-16 (38 m, 25 mm).
        assert!(refit_limit_mm(22.3) > 35.8 && refit_limit_mm(38.5) > 24.7);
        // Near pairs stay strict: a 3 cm move 5 m away still disagrees.
        assert!(refit_limit_mm(5.0) < 30.0 && surface_limit_mm(5.0) < 30.0);
        assert!((surface_limit_mm(38.4) - (20.0 + 0.698 * 38.4)).abs() < 0.1);
    }

    #[test]
    fn pair_evidence_needs_geometry_and_no_disagreement() {
        let ok = PairChecks { refit: Some([3.0, 0.01]), ..Default::default() };
        assert_eq!(pair_evidence(&ok, 10.0), Some(true));
        let off = PairChecks { refit: Some([40.0, 0.01]), ..Default::default() };
        assert_eq!(pair_evidence(&off, 10.0), Some(false));
        assert_eq!(pair_evidence(&off, 22.0), Some(true), "40 mm is within the lever arm of a station 22 m away");
        let ground_only = PairChecks { ground: Some(0.01), ..Default::default() };
        assert_eq!(pair_evidence(&ground_only, 10.0), None, "ground alone is not enough to back a join");
        let seen_through = PairChecks { refit: Some([3.0, 0.01]), free_space: Some(0.2), ..Default::default() };
        assert_eq!(pair_evidence(&seen_through, 10.0), Some(false));
        let surface = PairChecks { gaps: vec![0.004; 400], ..Default::default() };
        assert_eq!(pair_evidence(&surface, 10.0), Some(true));
        let far_surface = PairChecks { gaps: vec![0.0315; 400], ..Default::default() };
        assert_eq!(pair_evidence(&far_surface, 5.0), Some(false));
        assert_eq!(pair_evidence(&far_surface, 38.4), Some(true), "the admin corner pair 13-15 reads 31.5 mm at 38 m");
    }

    #[test]
    fn a_well_tied_site_is_left_alone() {
        let (ids, mut a) = audits(&[
            ("1", "confirmed", &["2", "3"]),
            ("2", "confirmed", &["1", "3", "4"]),
            ("3", "confirmed", &["1", "2", "4"]),
            ("4", "confirmed", &["2", "3"]),
            ("5", "unplaced", &[]),
        ]);
        let before = verdicts(&a).iter().map(|s| s.to_string()).collect::<Vec<_>>();
        settle(&ids, &ids, &mut a, &Evidence::new());
        assert_eq!(verdicts(&a), before);
    }

    #[test]
    fn poses_compose_and_invert() {
        let a = [0.3, 1.0, -2.0, 0.1];
        let rel = [1.1, 4.0, 2.0, -0.2];
        let b = compose(a, rel);
        let back = relative(a, b);
        for k in 0..4 {
            assert!((back[k] - rel[k]).abs() < 1e-12);
        }
        let ab = compose(b, invert(rel));
        for k in 0..4 {
            assert!((ab[k] - a[k]).abs() < 1e-12);
        }
    }

    /// A room-like L of walls seen from two stations: the global solve recovers
    /// the second station from a pair, and a wrong third pair can't drag it.
    #[test]
    fn global_fit_is_robust_to_a_wrong_pair() {
        let ids: Vec<String> = ["s1", "s2", "s3"].iter().map(|s| s.to_string()).collect();
        let truth2 = [0.5f64, 3.0, 1.0, 0.1];
        let truth3 = [1.2f64, -2.0, 4.0, 0.0];
        let mk = |a: &str, b: &str, rel: P4, good: bool| Pair {
            a: a.into(), b: b.into(), turn_deg: rel[0].to_degrees(), shift_m: [rel[1], rel[2], rel[3]], rms_mm: Some(10.0),
            matches: 20000, overlap: 0.3, coarse_score: 0.5, good, constraint: 0.2, weak: None, ambiguous: false, resolved_by: None,
            rejected: None, left_mm: None, left_deg: None, outlier: false,
        };
        let rel23 = relative(truth2, truth3);
        let mut wrong = rel23;
        wrong[1] += 2.0;
        let mut pairs = vec![mk("s1", "s2", truth2, true), mk("s1", "s3", truth3, true), mk("s2", "s3", rel23, true), mk("s2", "s3", wrong, true)];
        let poses = solve(&ids, &mut pairs);
        let p2 = poses[1].unwrap();
        assert!((p2.xyz_m[0] - 3.0).abs() < 0.01 && (p2.turn_deg - 0.5f64.to_degrees()).abs() < 0.05);
        assert!(pairs[3].outlier && !pairs[0].outlier);
    }

    #[test]
    fn a_wall_is_found_and_its_face_judged() {
        let mut m = Merged::default();
        // A 10 m wall along x at y = 5, 3 m tall, seen from station 1 over x 0..8.
        for i in 0..200 {
            for k in 0..30 {
                let x = i as f32 * 0.05;
                if x < 8.0 {
                    m.pts.push([x, 5.0, -1.5 + k as f32 * 0.1]);
                    m.spacing_mm.push(5.0);
                    m.station.push(1);
                }
            }
        }
        let ws = walls(&m);
        assert_eq!(ws.len(), 1);
        assert!((ws[0].length_m - 7.95).abs() < 0.1 && (ws[0].from[1] - 5.0).abs() < 0.02, "{:?}", ws[0]);
        let face = Face { from: [0.0, 5.0], to: [10.0, 5.0] };
        let scans: HashMap<u16, String> = [(1u16, "scan-1".to_string())].into();
        let v = face_coverage(&m, &face, &scans);
        assert_eq!(v["seen_share"], 0.8);
        assert_eq!(v["covered"], true);
        assert_eq!(v["stations"][0]["scan_id"], "scan-1");
        assert_eq!(v["unseen"][0]["to"], json!([10.0, 5.0]));
    }

    #[test]
    fn no_stations_no_panic() {
        let (pairs, g) = register_all(&[], &[], vec![], &|_, _| {});
        assert!(pairs.is_empty() && g.poses().is_empty() && g.frame() == 0);
    }

    #[test]
    fn rejections_under_an_old_bar_are_matched_again() {
        let old = Pair {
            a: "1".into(), b: "2".into(), turn_deg: 116.773, shift_m: [-3.188, 2.204, -0.007], rms_mm: Some(7.4), matches: 2913,
            overlap: 0.18, coarse_score: 0.426, good: false, constraint: 0.0124, weak: None, ambiguous: false, resolved_by: None,
            rejected: Some("2913 matched points (needs 8000)".into()), left_mm: None, left_deg: None, outlier: false,
        };
        assert!(stale_rejection(&old));
        assert!(!stale_rejection(&Pair { matches: 1500, ..old.clone() }));
        assert!(!stale_rejection(&Pair { rms_mm: Some(21.0), ..old.clone() }));
        assert!(!stale_rejection(&Pair { ambiguous: true, ..old.clone() }));
        assert!(!stale_rejection(&Pair { weak: Some("it can slide".into()), constraint: 0.002, ..old.clone() }));
        assert!(!stale_rejection(&Pair { good: true, ..old }));
    }

    #[test]
    fn jacobi_finds_every_eigenpair() {
        let a = vec![4.0, 1.0, 0.5, 0.0, 1.0, 3.0, 0.2, 0.1, 0.5, 0.2, 2.0, 0.3, 0.0, 0.1, 0.3, 1.0];
        let (vals, vecs) = sym_eigen(a.clone(), 4);
        for k in 0..4 {
            for i in 0..4 {
                let av: f64 = (0..4).map(|j| a[i * 4 + j] * vecs[j * 4 + k]).sum();
                assert!((av - vals[k] * vecs[i * 4 + k]).abs() < 1e-9);
            }
        }
    }

    type Rect = ([f32; 3], [f32; 3]);

    /// The six faces of an axis-aligned box.
    fn boxed(lo: [f32; 3], hi: [f32; 3]) -> Vec<Rect> {
        (0..3)
            .flat_map(|k| {
                [lo[k], hi[k]].map(|v| {
                    let (mut a, mut b) = (lo, hi);
                    a[k] = v;
                    b[k] = v;
                    (a, b)
                })
            })
            .collect()
    }

    /// Points 5 cm apart on axis-aligned rectangles, as a scanner at `at`
    /// turned by `turn` (rad) gets them, within range. No occlusion: these
    /// are rooms seen from inside.
    fn scene(rects: &[Rect], at: [f32; 3], turn: f32) -> Cloud {
        let (s, c) = turn.sin_cos();
        let (mut pts, mut range) = (Vec::new(), Vec::new());
        for &(lo, hi) in rects {
            let steps = |k: usize| ((hi[k] - lo[k]) / 0.05).round() as usize;
            for i in 0..=steps(0) {
                for j in 0..=steps(1) {
                    for k in 0..=steps(2) {
                        let w = [lo[0] + i as f32 * 0.05, lo[1] + j as f32 * 0.05, lo[2] + k as f32 * 0.05];
                        let d = [w[0] - at[0], w[1] - at[1], w[2] - at[2]];
                        if d[0].hypot(d[1]) > RANGE {
                            continue;
                        }
                        pts.push([c * d[0] + s * d[1], -s * d[0] + c * d[1], d[2]]);
                        range.push((d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt());
                    }
                }
            }
        }
        Cloud { step_rad: 0.0005, pts, range }
    }

    /// A 12 × 8 m room, floor 1.5 m below the scanner, ceiling 1.2 m above.
    fn room() -> Vec<Rect> {
        boxed([-6.0, -4.0, -1.5], [6.0, 4.0, 1.2])
    }

    #[test]
    fn a_corridor_is_weakly_held_along_its_length() {
        let walls = vec![([-40.0, -1.5, -1.5], [40.0, -1.5, 1.0]), ([-40.0, 1.5, -1.5], [40.0, 1.5, 1.0]),
                         ([-40.0, -1.5, -1.5], [40.0, 1.5, -1.5]), ([-40.0, -1.5, 1.0], [40.0, 1.5, 1.0])];
        let (a, b) = (prepare(&scene(&walls, [0.0, 0.0, 0.0], 0.0)), prepare(&scene(&walls, [4.0, 0.0, 0.0], 0.0)));
        let p = pair_from("a", &a, "b", &b, [0.0, 3.0, 0.0, 0.0]);
        assert!(!p.good && p.constraint < CONSTRAINT_MIN, "{p:?}");
        assert!(p.weak.as_deref().is_some_and(|w| w.contains("slide along (1.00,") || w.contains("slide along (-1.00,")), "{p:?}");
    }

    #[test]
    fn a_symmetric_room_is_ambiguous() {
        let r = room();
        let (a, b) = (prepare(&scene(&r, [1.0, 0.5, 0.0], 0.0)), prepare(&scene(&r, [-2.0, -1.0, 0.0], 0.3)));
        let p = pair("a", &a, "b", &b);
        assert!(p.ambiguous && !p.good, "{p:?}");
        assert!(p.rejected.as_deref().is_some_and(|r| r.contains("ambiguous")), "{p:?}");
    }

    /// A partition breaks the room's symmetry: the one right fit is found.
    #[test]
    fn a_partition_breaks_the_symmetry() {
        let mut r = room();
        r.push(([3.0, -4.0, -1.5], [3.0, -1.0, 0.5]));
        let (a, b) = (prepare(&scene(&r, [1.0, 0.5, 0.0], 0.0)), prepare(&scene(&r, [-2.0, -1.0, 0.0], 0.3)));
        let p = pair("a", &a, "b", &b);
        assert!(p.good, "{p:?}");
        assert!((p.turn_deg - 0.3f64.to_degrees()).abs() < 0.1, "{p:?}");
        assert!((p.shift_m[0] + 3.0).abs() < 0.02 && (p.shift_m[1] + 1.5).abs() < 0.02 && p.shift_m[2].abs() < 0.02, "{p:?}");
    }

    /// Three stations in a room with a partition: placed right they are all
    /// confirmed; one moved by 0.3 m (or turned 2°) is doubtful, alone.
    #[test]
    fn the_audit_finds_a_misplaced_station() {
        let mut r = room();
        r.push(([3.0, -4.0, -1.5], [3.0, -1.0, 0.5]));
        r.extend(boxed([-5.0, 2.0, -1.5], [-4.0, 3.0, -0.5]));
        let at = [([1.0, 0.5, 0.0], 0.0), ([-2.0, -1.0, 0.0], 0.3), ([-3.0, 1.5, 0.1], -1.0)];
        let prep: Vec<Prepared> = at.iter().map(|(p, t)| prepare(&scene(&r, *p, *t))).collect();
        let refs: Vec<&Prepared> = prep.iter().collect();
        let ids: Vec<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
        let (pairs, groups) = register_all(&ids, &refs, vec![], &|_, _| {});
        let poses = groups.poses();
        assert!(poses.iter().all(Option::is_some), "{pairs:?}");
        let mut cache = AuditCache::new();
        let mut verdicts = |poses: &[Option<Pose>]| audit(&ids, &ids, &refs, poses, &pairs, &mut cache).into_iter().map(|a| a.verdict).collect::<Vec<_>>();
        assert_eq!(verdicts(&poses), ["confirmed", "confirmed", "confirmed"]);
        let mut moved = poses.clone();
        moved[1].as_mut().unwrap().xyz_m[0] += 0.3;
        // The other two keep one trusted tie each (to one another): single, still trusted.
        assert_eq!(verdicts(&moved), ["single", "doubtful", "single"]);
        let mut turned = poses.clone();
        turned[2].as_mut().unwrap().turn_deg += 2.0;
        assert_eq!(verdicts(&turned), ["single", "single", "doubtful"]);
    }

    #[test]
    fn clouds_round_trip() {
        let c = Cloud { step_rad: 0.001, pts: vec![[1.0, 2.0, 3.0], [-1.0, 0.5, 0.0]], range: vec![3.7, 1.1] };
        let d = Cloud::from_bytes(&c.to_bytes()).unwrap();
        assert_eq!(d.pts, c.pts);
        assert_eq!(d.range, c.range);
        let mut m = Merged::default();
        m.pts.push([1.0, 2.0, 3.0]);
        m.spacing_mm.push(4.0);
        m.station.push(3);
        m.plan.push([5.0, 6.0]);
        let n = Merged::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!((n.pts, n.station, n.plan), (m.pts, m.station, m.plan));
    }
}
