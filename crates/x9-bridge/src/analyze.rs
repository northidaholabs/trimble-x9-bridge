//! `x9-bridge analyze`: deliverables from a registered survey.
//!
//! Takes the stations a `register` run placed (its poses, audit and walls)
//! and goes back to each station's full-resolution points, keeping only what
//! lies near a wall or on the ground, one station at a time. From those:
//!
//! - **Walls:** for each building wall, a best-fit plane; how far it leans
//!   from plumb (whole wall and per metre-wide strip); how flat it is
//!   (deviation from the plane, and from a straight line over a
//!   straightedge length, against a tolerance); its top above the ground;
//!   and an elevation heat map.
//! - **Ground or floor:** a 10 cm height grid of the lowest level surface
//!   (cars and other objects dropped); its departure from a best-fit plane
//!   with a heat map; where water would stand (low spots, with depth, area
//!   and volume); contours; and cross-sections along lines you give.
//! - **A report:** `report.html` with the pictures, `walls.csv`,
//!   `low_spots.csv`, `plan.dxf` (walls, stations, low spots), and
//!   `analysis.json`. `ground.f32` + `ground.json` hold the height grid, so
//!   `x9-bridge analyze --compare A B OUT` can measure cut and fill between
//!   two visits.
//!
//! Only stations the audit trusts are used. Numbers are as good as the
//! registration and the scanner: a wall's plane mixes every station that saw
//! it, so a few millimetres of registration mismatch show up as roughness.

use crate::register::{Pose, Wall};
use crate::station::Canvas;
use anyhow::Context;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BinaryHeap, HashMap};
use std::fmt::Write as _;

/// Points this close to a wall's line (m) belong to it.
const WALL_BAND_M: f64 = 0.30;
/// Beyond this from the wall's plane a point is a feature (an opening, a
/// relief, a downpipe), not the surface.
const FEATURE_M: f64 = 0.05;
/// Points farther from their scanner than this are left out (m).
const WALL_RANGE_M: f64 = 35.0;
const GROUND_RANGE_M: f64 = 30.0;
/// A ground cell needs this many returns: sparse far hits on grass are noise.
const GROUND_MIN_HITS: u16 = 4;
/// The height grid's cell (m).
const CELL: f64 = 0.10;
/// Ground cells this far above their surroundings are objects (cars, bins).
const OBJECT_M: f32 = 0.25;
/// A low spot: at least this deep (m) and this large (m²).
const POND_DEPTH_M: f32 = 0.010;
const POND_AREA_M2: f64 = 0.5;

const BG: [u8; 3] = [17, 24, 39];
const TEXT: [u8; 3] = [226, 232, 240];
const DIM: [u8; 3] = [148, 163, 184];
const FEATURE: [u8; 3] = [100, 106, 120];

#[derive(Clone, Debug)]
pub struct Opts {
    pub straightedge_m: f64,
    pub tolerance_mm: f64,
    pub sections: Vec<[f64; 4]>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { straightedge_m: 3.0, tolerance_mm: 6.0, sections: vec![] }
    }
}

// ------------------------------------------------------------------ walls ---

/// One wall's points, gridded on its face: `u` along the wall from its start,
/// `z` up, each cell holding the mean offset from the wall's line (positive
/// toward the side the stations stood on).
struct WallGrid {
    wall: Wall,
    from: [f64; 2],
    dir: [f64; 2],
    nrm: [f64; 2],
    u0: f64,
    z0: f64,
    cell: f64,
    nu: usize,
    nz: usize,
    sum: Vec<f32>,
    cnt: Vec<u32>,
    bbox: [f64; 4],
}

impl WallGrid {
    fn new(w: &Wall, stations: &[[f64; 2]]) -> WallGrid {
        let (dx, dy) = (w.to[0] - w.from[0], w.to[1] - w.from[1]);
        let len = dx.hypot(dy).max(1e-6);
        let dir = [dx / len, dy / len];
        let mut nrm = [-dir[1], dir[0]];
        // Facing the stations that saw it.
        let side: f64 = stations.iter().map(|s| ((s[0] - w.from[0]) * nrm[0] + (s[1] - w.from[1]) * nrm[1]).signum()).sum();
        if side < 0.0 {
            nrm = [-nrm[0], -nrm[1]];
        }
        let (u0, z0) = (-0.2, w.z_m[0] - 0.3);
        let (ulen, zlen) = (len + 0.4, (w.z_m[1] - w.z_m[0]) + 0.6);
        // About half a million cells at most, 2 cm or more.
        let cell = (ulen * zlen / 500_000.0).sqrt().max(0.02);
        let (nu, nz) = ((ulen / cell).ceil() as usize, (zlen / cell).ceil() as usize);
        let b = WALL_BAND_M + 0.2;
        let bbox = [w.from[0].min(w.to[0]) - b, w.from[1].min(w.to[1]) - b, w.from[0].max(w.to[0]) + b, w.from[1].max(w.to[1]) + b];
        WallGrid { wall: w.clone(), from: w.from, dir, nrm, u0, z0, cell, nu, nz, sum: vec![0.0; nu * nz], cnt: vec![0; nu * nz], bbox }
    }

    fn add(&mut self, p: [f64; 3]) {
        if p[0] < self.bbox[0] || p[0] > self.bbox[2] || p[1] < self.bbox[1] || p[1] > self.bbox[3] {
            return;
        }
        let (rx, ry) = (p[0] - self.from[0], p[1] - self.from[1]);
        let d = rx * self.nrm[0] + ry * self.nrm[1];
        if d.abs() > WALL_BAND_M {
            return;
        }
        let u = rx * self.dir[0] + ry * self.dir[1] - self.u0;
        let z = p[2] - self.z0;
        if u < 0.0 || z < 0.0 {
            return;
        }
        let (iu, iz) = ((u / self.cell) as usize, (z / self.cell) as usize);
        if iu >= self.nu || iz >= self.nz {
            return;
        }
        let k = iz * self.nu + iu;
        self.sum[k] += d as f32;
        self.cnt[k] += 1;
    }

    /// Cells with data: (u along the wall from its start, z, offset), metres.
    fn cells(&self) -> Vec<(f64, f64, f64)> {
        let mut out = Vec::new();
        for iz in 0..self.nz {
            for iu in 0..self.nu {
                let k = iz * self.nu + iu;
                if self.cnt[k] > 0 {
                    out.push((self.u0 + (iu as f64 + 0.5) * self.cell, self.z0 + (iz as f64 + 0.5) * self.cell, (self.sum[k] / self.cnt[k] as f32) as f64));
                }
            }
        }
        out
    }
}

/// Least squares for `d = a + b·u + c·z` over `(u, z, d)`.
fn fit_plane(pts: &[(f64, f64, f64)]) -> Option<[f64; 3]> {
    if pts.len() < 10 {
        return None;
    }
    let n = pts.len() as f64;
    let (mu, mz, md) = pts.iter().fold((0.0, 0.0, 0.0), |a, p| (a.0 + p.0 / n, a.1 + p.1 / n, a.2 + p.2 / n));
    let (mut suu, mut szz, mut suz, mut sud, mut szd) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(u, z, d) in pts {
        let (u, z, d) = (u - mu, z - mz, d - md);
        suu += u * u;
        szz += z * z;
        suz += u * z;
        sud += u * d;
        szd += z * d;
    }
    let det = suu * szz - suz * suz;
    if det.abs() < 1e-12 {
        return None;
    }
    let b = (sud * szz - szd * suz) / det;
    let c = (szd * suu - sud * suz) / det;
    Some([md - b * mu - c * mz, b, c])
}

/// Least squares for `y = a + b·x`.
fn fit_line(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    if pts.len() < 3 {
        return None;
    }
    let n = pts.len() as f64;
    let (mx, my) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0 / n, a.1 + p.1 / n));
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for &(x, y) in pts {
        sxx += (x - mx) * (x - mx);
        sxy += (x - mx) * (y - my);
    }
    if sxx < 1e-12 {
        return None;
    }
    let b = sxy / sxx;
    Some((my - b * mx, b))
}

/// Fit, drop what sits beyond `FEATURE_M` of the fit, fit again.
fn robust_plane(cells: &[(f64, f64, f64)]) -> Option<[f64; 3]> {
    let near: Vec<_> = cells.iter().copied().filter(|c| c.2.abs() <= 0.15).collect();
    let mut f = fit_plane(&near)?;
    for _ in 0..3 {
        let keep: Vec<_> = cells.iter().copied().filter(|c| (c.2 - (f[0] + f[1] * c.0 + f[2] * c.1)).abs() <= FEATURE_M).collect();
        f = fit_plane(&keep)?;
    }
    Some(f)
}

/// Windows of `len` metres along a profile of `(position, residual)` (sorted,
/// cells `step` apart), starting every half metre, with at least 60% of their
/// cells: the largest departure from the window's own straight line.
fn straightedge(profile: &[(f64, f64)], len: f64, step: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    if profile.is_empty() {
        return out;
    }
    let (lo, hi) = (profile[0].0, profile[profile.len() - 1].0);
    let need = ((len / step) * 0.6) as usize;
    let mut s = lo;
    while s + len <= hi + 1e-9 {
        let w: Vec<(f64, f64)> = profile.iter().copied().filter(|p| p.0 >= s && p.0 < s + len).collect();
        if w.len() >= need.max(3) {
            if let Some((a, b)) = fit_line(&w) {
                let gap = w.iter().map(|p| (p.1 - (a + b * p.0)).abs()).fold(0.0, f64::max);
                out.push((s + len / 2.0, gap));
            }
        }
        s += 0.5;
    }
    out
}

#[derive(Serialize, Clone, Debug)]
pub struct WallResult {
    pub wall_id: String,
    pub kind: String,
    pub length_m: f64,
    pub seen_by: Vec<u16>,
    /// Share of the wall's face with points (%).
    pub covered_pct: f64,
    /// Share of the covered face more than 50 mm off the plane: openings,
    /// reliefs, things in front (%).
    pub features_pct: f64,
    /// The plane's lean (mm per metre of height; positive: the top leans
    /// toward the side the stations stood on) and what it adds up to over
    /// the surface's height.
    pub lean_mm_per_m: Option<f64>,
    pub out_of_plumb_mm: Option<f64>,
    /// The worst metre-wide strip: its lean and where along the wall (m).
    pub worst_strip_mm_per_m: Option<f64>,
    pub worst_strip_at_m: Option<f64>,
    /// Departure of the surface from the plane (mm).
    pub flat_rms_mm: Option<f64>,
    pub flat_p95_mm: Option<f64>,
    /// Straight-line windows (horizontal and vertical) within tolerance (%),
    /// and the worst one.
    pub straightedge_windows: usize,
    pub straightedge_ok_pct: Option<f64>,
    pub straightedge_worst_mm: Option<f64>,
    /// The surface's top (99th percentile) above the ground at its foot (m).
    pub top_above_ground_m: Option<f64>,
    pub image: String,
}

fn percentile(v: &mut [f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[((v.len() - 1) as f64 * q).round() as usize])
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Blue (negative) through near-white to red (positive), for `t` in -1..1.
fn diverging(t: f64) -> [u8; 3] {
    let t = t.clamp(-1.0, 1.0);
    let (lo, mid, hi) = ([37.0, 99.0, 235.0], [241.0, 245.0, 249.0], [220.0, 38.0, 38.0]);
    let (a, b, s) = if t < 0.0 { (mid, lo, -t) } else { (mid, hi, t) };
    [0, 1, 2].map(|k| (a[k] + (b[k] - a[k]) * s) as u8)
}

fn analyze_wall(g: &WallGrid, ground: Option<&Ground>, opts: &Opts, img: &str) -> (WallResult, Vec<u8>) {
    let cells = g.cells();
    let w = &g.wall;
    let total = ((w.length_m / g.cell) * ((w.z_m[1] - w.z_m[0]) / g.cell)).max(1.0);
    let in_face = cells.iter().filter(|c| c.0 >= 0.0 && c.0 <= w.length_m && c.1 >= w.z_m[0] && c.1 <= w.z_m[1]).count() as f64;
    let plane = robust_plane(&cells);
    let res = |c: &(f64, f64, f64)| plane.map(|f| c.2 - (f[0] + f[1] * c.0 + f[2] * c.1));
    let surface: Vec<(f64, f64, f64)> = cells.iter().filter_map(|c| res(c).filter(|r| r.abs() <= FEATURE_M).map(|r| (c.0, c.1, r))).collect();
    let features = cells.len() - surface.len();
    let (zlo, zhi) = {
        let mut zs: Vec<f64> = surface.iter().map(|s| s.1).collect();
        (percentile(&mut zs, 0.01).unwrap_or(0.0), percentile(&mut zs, 0.99).unwrap_or(0.0))
    };
    let lean = plane.map(|f| f[2]);
    // Strips a metre wide with at least 2 m of surface height.
    let mut worst: Option<(f64, f64)> = None;
    let mut u = 0.0;
    while u < w.length_m {
        let strip: Vec<(f64, f64)> = surface.iter().filter(|s| s.0 >= u && s.0 < u + 1.0).map(|s| (s.1, s.2)).collect();
        let span = strip.iter().map(|s| s.0).fold(f64::MIN, f64::max) - strip.iter().map(|s| s.0).fold(f64::MAX, f64::min);
        if strip.len() >= 30 && span >= 2.0 {
            if let (Some((_, b)), Some(c)) = (fit_line(&strip), lean) {
                let total_lean = b + c;
                if worst.is_none_or(|(wl, _)| total_lean.abs() > wl.abs()) {
                    worst = Some((total_lean, u + 0.5));
                }
            }
        }
        u += 1.0;
    }
    let mut abs_r: Vec<f64> = surface.iter().map(|s| s.2.abs()).collect();
    let rms = (!surface.is_empty()).then(|| (surface.iter().map(|s| s.2 * s.2).sum::<f64>() / surface.len() as f64).sqrt());
    // Straight-line windows along rows and columns, every ~10 cm.
    let mut gaps: Vec<f64> = Vec::new();
    let stride = ((0.10 / g.cell).round() as usize).max(1);
    let mut rows: HashMap<usize, Vec<(f64, f64)>> = HashMap::new();
    let mut cols: HashMap<usize, Vec<(f64, f64)>> = HashMap::new();
    for s in &surface {
        let (iu, iz) = (((s.0 - g.u0) / g.cell) as usize, ((s.1 - g.z0) / g.cell) as usize);
        if iz % stride == 0 {
            rows.entry(iz).or_default().push((s.0, s.2));
        }
        if iu % stride == 0 {
            cols.entry(iu).or_default().push((s.1, s.2));
        }
    }
    for line in rows.values_mut().chain(cols.values_mut()) {
        line.sort_by(|a, b| a.0.total_cmp(&b.0));
        gaps.extend(straightedge(line, opts.straightedge_m, g.cell).into_iter().map(|(_, gap)| gap));
    }
    let tol = opts.tolerance_mm / 1000.0;
    let ok = gaps.iter().filter(|&&x| x <= tol).count();
    let top = ground.and_then(|gr| {
        // Ground in front of the wall, along it.
        let mut zs = Vec::new();
        let mut a = 0.0;
        while a <= w.length_m {
            for off in [0.5, 1.0, 1.5] {
                let p = [g.from[0] + g.dir[0] * a + g.nrm[0] * off, g.from[1] + g.dir[1] * a + g.nrm[1] * off];
                if let Some(z) = gr.at(p) {
                    zs.push(z as f64);
                }
            }
            a += 0.5;
        }
        percentile(&mut zs, 0.5).map(|foot| zhi - foot)
    });
    let r = WallResult {
        wall_id: w.wall_id.clone(),
        kind: w.kind.clone(),
        length_m: r2(w.length_m),
        seen_by: w.seen_by.clone(),
        covered_pct: r1(100.0 * in_face / total).min(100.0),
        features_pct: r1(100.0 * features as f64 / cells.len().max(1) as f64),
        lean_mm_per_m: lean.map(|c| r1(c * 1000.0)),
        out_of_plumb_mm: lean.map(|c| r1(c * (zhi - zlo) * 1000.0)),
        worst_strip_mm_per_m: worst.map(|w| r1(w.0 * 1000.0)),
        worst_strip_at_m: worst.map(|w| r1(w.1)),
        flat_rms_mm: rms.map(|v| r1(v * 1000.0)),
        flat_p95_mm: percentile(&mut abs_r, 0.95).map(|v| r1(v * 1000.0)),
        straightedge_windows: gaps.len(),
        straightedge_ok_pct: (!gaps.is_empty()).then(|| r1(100.0 * ok as f64 / gaps.len() as f64)),
        straightedge_worst_mm: gaps.iter().cloned().fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x)))).map(|v| r1(v * 1000.0)),
        top_above_ground_m: top.map(r2),
        image: img.to_string(),
    };
    (r.clone(), wall_png(g, plane, &r, opts))
}

/// The wall's face seen from the stations' side: colour by departure from
/// the plane, grey for features, dark where nothing was seen.
fn wall_png(g: &WallGrid, plane: Option<[f64; 3]>, r: &WallResult, opts: &Opts) -> Vec<u8> {
    let scale = (1400.0 / g.nu as f64).min(700.0 / g.nz as f64).min(4.0);
    let (iw, ih) = (((g.nu as f64) * scale).ceil() as usize, ((g.nz as f64) * scale).ceil() as usize);
    let (top, bottom) = (46usize, 64usize);
    let (w, h) = (iw.max(760) + 24, ih + top + bottom);
    let mut cv = Canvas::new(w, h, BG);
    let range = (opts.tolerance_mm * 3.0 / 1000.0).max(0.006);
    // Seen from the stations' side (looking along -nrm), the viewer's right
    // is (-nrm.y, nrm.x): draw `u` reversed when the wall runs the other way.
    let flip = g.dir[0] * g.nrm[1] - g.dir[1] * g.nrm[0] > 0.0;
    for py in 0..ih {
        for px in 0..iw {
            let iu = ((px as f64 + 0.5) / scale) as usize;
            let iz = g.nz - 1 - (((py as f64 + 0.5) / scale) as usize).min(g.nz - 1);
            let iu = if flip { g.nu - 1 - iu.min(g.nu - 1) } else { iu.min(g.nu - 1) };
            let k = iz * g.nu + iu;
            if g.cnt[k] == 0 {
                continue;
            }
            let d = (g.sum[k] / g.cnt[k] as f32) as f64;
            let (u, z) = (g.u0 + (iu as f64 + 0.5) * g.cell, g.z0 + (iz as f64 + 0.5) * g.cell);
            let c = match plane {
                Some(f) => {
                    let res = d - (f[0] + f[1] * u + f[2] * z);
                    if res.abs() > FEATURE_M {
                        FEATURE
                    } else {
                        diverging(res / range)
                    }
                }
                None => FEATURE,
            };
            cv.put((12 + px) as i64, (top + py) as i64, c, 1.0);
        }
    }
    let font = crate::station::font();
    let lean = r.out_of_plumb_mm.map(|v| format!("leans {v:+.0} mm over its height")).unwrap_or_else(|| "lean unknown".into());
    let title = format!(
        "Wall {} · {:.1} m long · {} · flatness RMS {} mm · {}",
        r.wall_id,
        r.length_m,
        r.top_above_ground_m.map(|t| format!("top {t:.1} m above ground")).unwrap_or_else(|| "height unknown".into()),
        r.flat_rms_mm.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into()),
        lean
    );
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &title, TEXT);
    // Legend.
    let ly = (top + ih + 14) as i64;
    for i in 0..200 {
        let t = i as f64 / 199.0 * 2.0 - 1.0;
        for y in 0..12 {
            cv.put(12 + i, ly + y, diverging(t), 1.0);
        }
    }
    let mm = range * 1000.0;
    cv.text(&font, 12.0, (12.0, ly as f32 + 16.0), 0.0, &format!("-{mm:.0} mm"), DIM);
    cv.text(&font, 12.0, (212.0, ly as f32 + 16.0), 1.0, &format!("+{mm:.0} mm"), DIM);
    for y in 0..12 {
        for x in 0..14 {
            cv.put(230 + x, ly + y, FEATURE, 1.0);
        }
    }
    cv.text(&font, 12.0, (250.0, ly as f32 - 2.0), 0.0, "more than 50 mm off: openings, reliefs, things in front", DIM);
    cv.text(&font, 12.0, (250.0, ly as f32 + 16.0), 0.0, "red: proud of the plane toward the viewer · blue: set back · seen from the stations' side", DIM);
    cv.png()
}

// ----------------------------------------------------------------- ground ---

/// The lowest few heights seen in a cell.
#[derive(Clone, Copy)]
struct Lowest {
    v: [f32; 8],
    n: u8,
    hits: u16,
}

impl Lowest {
    fn new() -> Lowest {
        Lowest { v: [f32::MAX; 8], n: 0, hits: 0 }
    }
    fn add(&mut self, z: f32) {
        self.hits = self.hits.saturating_add(1);
        let n = self.n as usize;
        if n < 8 {
            self.v[n] = z;
            self.n += 1;
            self.v[..n + 1].sort_by(|a, b| a.total_cmp(b));
        } else if z < self.v[7] {
            self.v[7] = z;
            self.v.sort_by(|a, b| a.total_cmp(b));
        }
    }
    /// The mean of the heights within 3 cm of the lowest: one stray low
    /// return doesn't decide the cell.
    fn height(&self) -> Option<f32> {
        let n = self.n as usize;
        if n < 2 || self.hits < GROUND_MIN_HITS {
            return None;
        }
        // A lone return well below the rest is a stray, not the ground.
        let base = if self.v[1] - self.v[0] > 0.03 { self.v[1] } else { self.v[0] };
        let close: Vec<f32> = self.v[..n].iter().copied().filter(|z| *z >= base && *z <= base + 0.03).collect();
        Some(close.iter().sum::<f32>() / close.len() as f32)
    }
}

/// A height grid (NaN: unknown).
#[derive(Clone, Debug)]
pub struct Ground {
    pub x0: f64,
    pub y0: f64,
    pub cell: f64,
    pub w: usize,
    pub h: usize,
    pub z: Vec<f32>,
}

impl Ground {
    fn from_cells(cells: &HashMap<(i32, i32), Lowest>, cell: f64) -> Option<Ground> {
        let known: Vec<((i32, i32), f32)> = cells.iter().filter_map(|(k, l)| l.height().map(|z| (*k, z))).collect();
        if known.is_empty() {
            return None;
        }
        let (mut ix0, mut iy0, mut ix1, mut iy1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for ((x, y), _) in &known {
            ix0 = ix0.min(*x);
            iy0 = iy0.min(*y);
            ix1 = ix1.max(*x);
            iy1 = iy1.max(*y);
        }
        let (w, h) = ((ix1 - ix0 + 1) as usize, (iy1 - iy0 + 1) as usize);
        let mut z = vec![f32::NAN; w * h];
        for ((x, y), v) in known {
            z[(y - iy0) as usize * w + (x - ix0) as usize] = v;
        }
        let mut g = Ground { x0: ix0 as f64 * cell, y0: iy0 as f64 * cell, cell, w, h, z };
        g.drop_objects();
        g.fill_small_holes();
        Some(g)
    }

    fn idx(&self, p: [f64; 2]) -> Option<usize> {
        let (fx, fy) = ((p[0] - self.x0) / self.cell, (p[1] - self.y0) / self.cell);
        if fx < 0.0 || fy < 0.0 || fx >= self.w as f64 || fy >= self.h as f64 {
            return None;
        }
        Some(fy as usize * self.w + fx as usize)
    }

    pub fn at(&self, p: [f64; 2]) -> Option<f32> {
        self.idx(p).map(|i| self.z[i]).filter(|z| z.is_finite())
    }

    /// Cells well above the ground around them (1 m blocks, 5 m across) are
    /// tops of things, not ground.
    fn drop_objects(&mut self) {
        let k = (1.0 / self.cell).round() as usize;
        let (bw, bh) = (self.w.div_ceil(k), self.h.div_ceil(k));
        let mut block = vec![f32::NAN; bw * bh];
        for by in 0..bh {
            for bx in 0..bw {
                let mut v: Vec<f32> = Vec::new();
                for y in by * k..((by + 1) * k).min(self.h) {
                    for x in bx * k..((bx + 1) * k).min(self.w) {
                        let z = self.z[y * self.w + x];
                        if z.is_finite() {
                            v.push(z);
                        }
                    }
                }
                if v.len() >= 3 {
                    v.sort_by(|a, b| a.total_cmp(b));
                    block[by * bw + bx] = v[v.len() / 5];
                }
            }
        }
        let mut local = vec![f32::NAN; bw * bh];
        for by in 0..bh {
            for bx in 0..bw {
                let mut v: Vec<f32> = Vec::new();
                for y in by.saturating_sub(2)..(by + 3).min(bh) {
                    for x in bx.saturating_sub(2)..(bx + 3).min(bw) {
                        let z = block[y * bw + x];
                        if z.is_finite() {
                            v.push(z);
                        }
                    }
                }
                if !v.is_empty() {
                    v.sort_by(|a, b| a.total_cmp(b));
                    local[by * bw + bx] = v[v.len() / 2];
                }
            }
        }
        for y in 0..self.h {
            for x in 0..self.w {
                let l = local[(y / k) * bw + x / k];
                let z = &mut self.z[y * self.w + x];
                if z.is_finite() && l.is_finite() && *z > l + OBJECT_M {
                    *z = f32::NAN;
                }
            }
        }
    }

    /// Unknown cells with most of their neighbours known take their mean.
    fn fill_small_holes(&mut self) {
        for _ in 0..2 {
            let src = self.z.clone();
            for y in 1..self.h.saturating_sub(1) {
                for x in 1..self.w.saturating_sub(1) {
                    if src[y * self.w + x].is_finite() {
                        continue;
                    }
                    let n: Vec<f32> = [(0, 1), (1, 0), (0, -1), (-1, 0), (1, 1), (1, -1), (-1, 1), (-1, -1)]
                        .iter()
                        .map(|(dx, dy)| src[(y as i64 + dy) as usize * self.w + (x as i64 + dx) as usize])
                        .filter(|z| z.is_finite())
                        .collect();
                    if n.len() >= 5 {
                        self.z[y * self.w + x] = n.iter().sum::<f32>() / n.len() as f32;
                    }
                }
            }
        }
    }

    fn known(&self) -> usize {
        self.z.iter().filter(|z| z.is_finite()).count()
    }

    /// Water standing on the surface: for each cell, how far below the level
    /// water would fill to before spilling out over known ground (priority
    /// flood from the edges of what was seen; unknown cells are edges too).
    pub fn ponding(&self) -> Vec<f32> {
        #[derive(PartialEq)]
        struct Item(f32, usize);
        impl Eq for Item {}
        impl Ord for Item {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering {
                o.0.total_cmp(&self.0).then(o.1.cmp(&self.1))
            }
        }
        impl PartialOrd for Item {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(o))
            }
        }
        let (w, h) = (self.w, self.h);
        let mut filled = vec![f32::NAN; w * h];
        let mut heap = BinaryHeap::new();
        let nb = |i: usize| {
            let (x, y) = ((i % w) as i64, (i / w) as i64);
            [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)]
                .into_iter()
                .map(move |(dx, dy)| (x + dx, y + dy))
                .filter(move |(a, b)| *a >= 0 && *b >= 0 && (*a as usize) < w && (*b as usize) < h)
                .map(move |(a, b)| b as usize * w + a as usize)
        };
        for (i, &zi) in self.z.iter().enumerate() {
            if !zi.is_finite() {
                continue;
            }
            let (x, y) = (i % w, i / w);
            let edge = x == 0 || y == 0 || x == w - 1 || y == h - 1 || nb(i).any(|j| !self.z[j].is_finite());
            if edge {
                filled[i] = zi;
                heap.push(Item(zi, i));
            }
        }
        while let Some(Item(level, i)) = heap.pop() {
            for j in nb(i) {
                if self.z[j].is_finite() && !filled[j].is_finite() {
                    filled[j] = self.z[j].max(level);
                    heap.push(Item(filled[j], j));
                }
            }
        }
        (0..w * h).map(|i| if self.z[i].is_finite() && filled[i].is_finite() { filled[i] - self.z[i] } else { 0.0 }).collect()
    }

    fn to_bytes(&self) -> Vec<u8> {
        self.z.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn header(&self) -> Value {
        json!({"x0": self.x0, "y0": self.y0, "cell": self.cell, "w": self.w, "h": self.h, "format": "little-endian f32, row-major from (x0, y0), NaN unknown"})
    }

    fn load(dir: &str) -> anyhow::Result<Ground> {
        let h: Value = serde_json::from_slice(&std::fs::read(format!("{dir}/ground.json")).with_context(|| format!("{dir}/ground.json"))?)?;
        let b = std::fs::read(format!("{dir}/ground.f32"))?;
        let (w, hh) = (h["w"].as_u64().unwrap_or(0) as usize, h["h"].as_u64().unwrap_or(0) as usize);
        anyhow::ensure!(b.len() == w * hh * 4, "{dir}/ground.f32 is not {w}×{hh}");
        Ok(Ground {
            x0: h["x0"].as_f64().unwrap_or(0.0),
            y0: h["y0"].as_f64().unwrap_or(0.0),
            cell: h["cell"].as_f64().unwrap_or(CELL),
            w,
            h: hh,
            z: b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect(),
        })
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Pond {
    pub at: [f64; 2],
    pub area_m2: f64,
    pub max_depth_mm: f64,
    pub volume_l: f64,
}

/// Connected wet cells (4-neighbour) deep and large enough to matter.
pub fn ponds(g: &Ground, depth: &[f32]) -> Vec<Pond> {
    let (w, h) = (g.w, g.h);
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let a = g.cell * g.cell;
    for s in 0..w * h {
        if seen[s] || depth[s] < 0.005 {
            continue;
        }
        let mut stack = vec![s];
        seen[s] = true;
        let (mut n, mut vol, mut maxd, mut cx, mut cy) = (0usize, 0.0f64, 0.0f32, 0.0, 0.0);
        while let Some(i) = stack.pop() {
            n += 1;
            vol += depth[i] as f64 * a;
            maxd = maxd.max(depth[i]);
            cx += (i % w) as f64;
            cy += (i / w) as f64;
            let (x, y) = (i % w, i / w);
            for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                if nx < w && ny < h {
                    let j = ny * w + nx;
                    if !seen[j] && depth[j] >= 0.005 {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if maxd >= POND_DEPTH_M && n as f64 * a >= POND_AREA_M2 {
            out.push(Pond {
                at: [r2(g.x0 + (cx / n as f64 + 0.5) * g.cell), r2(g.y0 + (cy / n as f64 + 0.5) * g.cell)],
                area_m2: r2(n as f64 * a),
                max_depth_mm: r1(maxd as f64 * 1000.0),
                volume_l: r1(vol * 1000.0),
            });
        }
    }
    out.sort_by(|a, b| b.volume_l.total_cmp(&a.volume_l));
    out
}

/// A plane through the ground (z = a + b·x + c·y), robust to slopes and kerbs.
fn ground_plane(g: &Ground) -> Option<[f64; 3]> {
    let mut pts: Vec<(f64, f64, f64)> = Vec::new();
    let stride = ((g.w * g.h) / 200_000).max(1);
    for i in (0..g.w * g.h).step_by(stride) {
        if g.z[i].is_finite() {
            pts.push((g.x0 + ((i % g.w) as f64 + 0.5) * g.cell, g.y0 + ((i / g.w) as f64 + 0.5) * g.cell, g.z[i] as f64));
        }
    }
    let mut f = fit_plane(&pts)?;
    for _ in 0..3 {
        let keep: Vec<_> = pts.iter().copied().filter(|p| (p.2 - (f[0] + f[1] * p.0 + f[2] * p.1)).abs() <= 0.05).collect();
        f = fit_plane(&keep)?;
    }
    Some(f)
}

/// A picture of the grid in plan: `color(i)` per known cell.
struct PlanView {
    scale: f64,
    w: usize,
    h: usize,
    top: usize,
}

impl PlanView {
    fn of(g: &Ground) -> PlanView {
        let scale = (1400.0 / g.w as f64).min(1100.0 / g.h as f64).min(6.0);
        PlanView { scale, w: (g.w as f64 * scale).ceil() as usize + 24, h: (g.h as f64 * scale).ceil() as usize + 46 + 70, top: 46 }
    }
    fn px(&self, g: &Ground, p: [f64; 2]) -> (f32, f32) {
        let x = (p[0] - g.x0) / g.cell * self.scale + 12.0;
        let y = self.top as f64 + (g.h as f64 - (p[1] - g.y0) / g.cell) * self.scale;
        (x as f32, y as f32)
    }
    fn paint(&self, g: &Ground, cv: &mut Canvas, color: impl Fn(usize) -> Option<[u8; 3]>) {
        let (iw, ih) = ((g.w as f64 * self.scale).ceil() as usize, (g.h as f64 * self.scale).ceil() as usize);
        for py in 0..ih {
            for px in 0..iw {
                let x = ((px as f64 + 0.5) / self.scale) as usize;
                let y = g.h - 1 - (((py as f64 + 0.5) / self.scale) as usize).min(g.h - 1);
                if let Some(c) = color(y * g.w + x.min(g.w - 1)) {
                    cv.put((12 + px) as i64, (self.top + py) as i64, c, 1.0);
                }
            }
        }
    }
    fn overlay(&self, g: &Ground, cv: &mut Canvas, walls: &[Wall], stations: &[(u16, [f64; 2])]) {
        for w in walls.iter().filter(|w| w.kind == "building") {
            cv.line(self.px(g, w.from), self.px(g, w.to), [255, 255, 255], 2.0);
        }
        let font = crate::station::font();
        for (n, p) in stations {
            let (x, y) = self.px(g, *p);
            cv.ellipse((x, y), (4.0, 4.0), [255, 255, 255], 1.0, true);
            cv.text(&font, 12.0, (x + 6.0, y - 20.0), 0.0, &n.to_string(), TEXT);
        }
    }
}

/// Earth tones by height, `t` in 0..1.
fn terrain(t: f64) -> [u8; 3] {
    let stops = [(0.0, [40.0, 80.0, 140.0]), (0.25, [60.0, 140.0, 120.0]), (0.5, [150.0, 180.0, 90.0]), (0.75, [210.0, 170.0, 90.0]), (1.0, [240.0, 235.0, 225.0])];
    let t = t.clamp(0.0, 1.0);
    for k in 1..stops.len() {
        if t <= stops[k].0 {
            let (a, b) = (stops[k - 1], stops[k]);
            let s = (t - a.0) / (b.0 - a.0);
            return [0, 1, 2].map(|i| (a.1[i] + (b.1[i] - a.1[i]) * s) as u8);
        }
    }
    [240, 235, 225]
}

fn nice_step(range: f64) -> f64 {
    for s in [0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0] {
        if range / s <= 25.0 {
            return s;
        }
    }
    5.0
}

fn ground_png(g: &Ground, walls: &[Wall], stations: &[(u16, [f64; 2])]) -> (Vec<u8>, f64) {
    let v = PlanView::of(g);
    let mut cv = Canvas::new(v.w, v.h, BG);
    let mut zs: Vec<f64> = g.z.iter().filter(|z| z.is_finite()).map(|z| *z as f64).collect();
    let lo = percentile(&mut zs, 0.02).unwrap_or(0.0);
    let hi = percentile(&mut zs, 0.98).unwrap_or(1.0).max(lo + 0.01);
    let step = nice_step(hi - lo);
    let shade = |i: usize| -> f64 {
        let (x, y) = (i % g.w, i / g.w);
        let z = |xx: usize, yy: usize| g.z[yy * g.w + xx];
        if x == 0 || y == 0 || x + 1 >= g.w || y + 1 >= g.h {
            return 1.0;
        }
        let (dx, dy) = (z(x + 1, y) - z(x - 1, y), z(x, y + 1) - z(x, y - 1));
        if !dx.is_finite() || !dy.is_finite() {
            return 1.0;
        }
        (1.0 - (dx as f64 - dy as f64) / (2.0 * g.cell) * 0.8).clamp(0.6, 1.25)
    };
    v.paint(g, &mut cv, |i| {
        let z = g.z[i];
        if !z.is_finite() {
            return None;
        }
        let x = i % g.w;
        // Contours where the band changes to the right or above.
        let band = |zz: f32| (zz as f64 / step).floor();
        let edge = (x + 1 < g.w && g.z[i + 1].is_finite() && band(g.z[i + 1]) != band(z)) || (i + g.w < g.z.len() && g.z[i + g.w].is_finite() && band(g.z[i + g.w]) != band(z));
        if edge {
            return Some([30, 30, 30]);
        }
        let c = terrain((z as f64 - lo) / (hi - lo));
        let s = shade(i);
        Some(c.map(|k| (k as f64 * s).clamp(0.0, 255.0) as u8))
    });
    v.overlay(g, &mut cv, walls, stations);
    let font = crate::station::font();
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &format!("Ground and floors · contours every {} mm · {:.2} m from lowest to highest (2-98%)", step * 1000.0, hi - lo), TEXT);
    let ly = (v.h - 56) as i64;
    for i in 0..200 {
        for y in 0..12 {
            cv.put(12 + i, ly + y, terrain(i as f64 / 199.0), 1.0);
        }
    }
    cv.text(&font, 12.0, (12.0, ly as f32 + 16.0), 0.0, &format!("{lo:.2} m"), DIM);
    cv.text(&font, 12.0, (212.0, ly as f32 + 16.0), 1.0, &format!("{hi:.2} m"), DIM);
    cv.text(&font, 12.0, (230.0, ly as f32 - 2.0), 0.0, "white lines: building walls · dots: stations · cars and other objects left out · project frame, not north", DIM);
    (cv.png(), step)
}

/// Each cell's height against the mean of the known cells within `radius`
/// (metres) around it: bumps and dips with the site's slopes taken out.
/// NaN where unknown or with too little around it.
fn local_departure(g: &Ground, radius: f64) -> Vec<f32> {
    let (w, h) = (g.w, g.h);
    let r = (radius / g.cell).round() as usize;
    // Integral images of height and count.
    let mut s = vec![0.0f64; (w + 1) * (h + 1)];
    let mut c = vec![0u32; (w + 1) * (h + 1)];
    for y in 0..h {
        for x in 0..w {
            let z = g.z[y * w + x];
            let (zv, cv) = if z.is_finite() { (z as f64, 1) } else { (0.0, 0) };
            let k = (y + 1) * (w + 1) + x + 1;
            s[k] = zv + s[k - 1] + s[k - (w + 1)] - s[k - (w + 1) - 1];
            c[k] = cv + c[k - 1] + c[k - (w + 1)] - c[k - (w + 1) - 1];
        }
    }
    let area = ((2 * r + 1) * (2 * r + 1)) as u32;
    let mut out = vec![f32::NAN; w * h];
    for y in 0..h {
        for x in 0..w {
            let z = g.z[y * w + x];
            if !z.is_finite() {
                continue;
            }
            let (x0, y0, x1, y1) = (x.saturating_sub(r), y.saturating_sub(r), (x + r + 1).min(w), (y + r + 1).min(h));
            let at = |xx: usize, yy: usize| yy * (w + 1) + xx;
            let n = c[at(x1, y1)] + c[at(x0, y0)] - c[at(x0, y1)] - c[at(x1, y0)];
            // At least a third of the window seen, so the mean is a surface.
            if n * 3 < area {
                continue;
            }
            let sum = s[at(x1, y1)] + s[at(x0, y0)] - s[at(x0, y1)] - s[at(x1, y0)];
            out[y * w + x] = z - (sum / n as f64) as f32;
        }
    }
    out
}

fn flatness_png(g: &Ground, dep: &[f32], slope_pct: Option<f64>, walls: &[Wall], stations: &[(u16, [f64; 2])], opts: &Opts) -> Vec<u8> {
    let v = PlanView::of(g);
    let mut cv = Canvas::new(v.w, v.h, BG);
    let range = (opts.tolerance_mm * 3.0 / 1000.0).max(0.006);
    v.paint(g, &mut cv, |i| {
        if !g.z[i].is_finite() {
            return None;
        }
        let d = dep[i];
        Some(if !d.is_finite() {
            [55, 60, 70]
        } else if (d as f64).abs() > FEATURE_M {
            FEATURE
        } else {
            diverging(d as f64 / range)
        })
    });
    v.overlay(g, &mut cv, walls, stations);
    let font = crate::station::font();
    let slope = slope_pct.map(|s| format!(" · the site falls {s}% overall")).unwrap_or_default();
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &format!("Flatness: height against the ground within {:.1} m around it{slope}", opts.straightedge_m / 2.0), TEXT);
    let ly = (v.h - 56) as i64;
    for i in 0..200 {
        let t = i as f64 / 199.0 * 2.0 - 1.0;
        for y in 0..12 {
            cv.put(12 + i, ly + y, diverging(t), 1.0);
        }
    }
    let mm = range * 1000.0;
    cv.text(&font, 12.0, (12.0, ly as f32 + 16.0), 0.0, &format!("-{mm:.0} mm"), DIM);
    cv.text(&font, 12.0, (212.0, ly as f32 + 16.0), 1.0, &format!("+{mm:.0} mm"), DIM);
    cv.text(&font, 12.0, (230.0, ly as f32 - 2.0), 0.0, "red: a bump · blue: a dip · grey: more than 50 mm (kerbs, steps, beds) · darker: too little seen around it", DIM);
    cv.png()
}

fn ponds_png(g: &Ground, depth: &[f32], ponds: &[Pond], walls: &[Wall], stations: &[(u16, [f64; 2])]) -> Vec<u8> {
    let v = PlanView::of(g);
    let mut cv = Canvas::new(v.w, v.h, BG);
    v.paint(g, &mut cv, |i| {
        let z = g.z[i];
        if !z.is_finite() {
            return None;
        }
        let d = depth[i];
        if d >= 0.002 {
            let t = (d / 0.05).min(1.0);
            Some([(90.0 * (1.0 - t)) as u8, (170.0 * (1.0 - t) + 60.0 * t) as u8, (230.0 - 40.0 * t) as u8])
        } else {
            Some([70, 74, 82])
        }
    });
    v.overlay(g, &mut cv, walls, stations);
    let font = crate::station::font();
    for (k, p) in ponds.iter().take(15).enumerate() {
        let (x, y) = v.px(g, p.at);
        cv.text(&font, 12.0, (x, y - 8.0), 0.5, &format!("L{}", k + 1), [255, 255, 255]);
    }
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &format!("Where water would stand · {} low spots of {} m² or more and {} mm or deeper", ponds.len(), POND_AREA_M2, POND_DEPTH_M * 1000.0), TEXT);
    let ly = (v.h - 56) as i64;
    cv.text(&font, 12.0, (12.0, ly as f32), 0.0, "light to dark blue: 2 to 50 mm deep · grey: drains away · numbered by volume · a drain the scan didn't see would empty a low spot", DIM);
    cv.png()
}

#[derive(Serialize, Clone, Debug)]
pub struct Section {
    pub from: [f64; 2],
    pub to: [f64; 2],
    pub image: String,
    pub csv: String,
    pub walls_crossed: Vec<String>,
}

fn section(g: &Ground, walls: &[WallResult], wall_defs: &[Wall], s: [f64; 4], k: usize) -> (Section, Vec<u8>, String) {
    let (a, b) = ([s[0], s[1]], [s[2], s[3]]);
    let len = (b[0] - a[0]).hypot(b[1] - a[1]).max(0.01);
    let n = (len / g.cell).ceil() as usize;
    let mut prof: Vec<(f64, Option<f32>)> = Vec::new();
    let mut csv = String::from("distance_m,x,y,ground_z_m\n");
    for i in 0..=n {
        let t = i as f64 / n as f64;
        let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        let z = g.at(p);
        let _ = writeln!(csv, "{:.2},{:.2},{:.2},{}", t * len, p[0], p[1], z.map(|z| format!("{z:.3}")).unwrap_or_default());
        prof.push((t * len, z));
    }
    // Walls the line crosses: where, and their top.
    let mut crossed: Vec<(String, f64, f64, f64)> = Vec::new();
    for w in wall_defs.iter().filter(|w| w.kind == "building") {
        let (p, r) = (a, [b[0] - a[0], b[1] - a[1]]);
        let (q, sv) = (w.from, [w.to[0] - w.from[0], w.to[1] - w.from[1]]);
        let den = r[0] * sv[1] - r[1] * sv[0];
        if den.abs() < 1e-9 {
            continue;
        }
        let t = ((q[0] - p[0]) * sv[1] - (q[1] - p[1]) * sv[0]) / den;
        let u = ((q[0] - p[0]) * r[1] - (q[1] - p[1]) * r[0]) / den;
        if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
            let top = w.z_m[1];
            let foot = walls.iter().find(|x| x.wall_id == w.wall_id).and_then(|x| x.top_above_ground_m).map(|h| top - h).unwrap_or(w.z_m[0]);
            crossed.push((w.wall_id.clone(), t * len, foot, top));
        }
    }
    let zs: Vec<f64> = prof.iter().filter_map(|p| p.1.map(|z| z as f64)).chain(crossed.iter().flat_map(|c| [c.2, c.3])).collect();
    let (zlo, zhi) = (zs.iter().cloned().fold(f64::MAX, f64::min), zs.iter().cloned().fold(f64::MIN, f64::max));
    let (zlo, zhi) = if zlo > zhi { (0.0, 1.0) } else { (zlo - 0.2, zhi + 0.2) };
    let (cw, ch, l, t, bm) = (1400usize, 520usize, 60.0f64, 46.0f64, 50.0f64);
    // Distance and height each fill the picture: heights are usually stretched.
    let sx = (cw as f64 - l - 20.0) / len;
    let sy = (ch as f64 - t - bm) / (zhi - zlo);
    let exag = sy / sx;
    let px = |d: f64, z: f64| ((l + d * sx) as f32, (ch as f64 - bm - (z - zlo) * sy) as f32);
    let mut cv = Canvas::new(cw, ch, BG);
    let font = crate::station::font();
    let step = nice_step(zhi - zlo).max(0.1);
    let mut gz = (zlo / step).ceil() * step;
    while gz <= zhi {
        let (x0, y) = px(0.0, gz);
        let (x1, _) = px(len, gz);
        cv.line((x0, y), (x1, y), [40, 48, 64], 1.0);
        cv.text(&font, 11.0, (6.0, y - 8.0), 0.0, &format!("{gz:.1}"), DIM);
        gz += step;
    }
    for w in prof.windows(2) {
        if let (Some(z0), Some(z1)) = (w[0].1, w[1].1) {
            cv.line(px(w[0].0, z0 as f64), px(w[1].0, z1 as f64), [150, 180, 90], 2.0);
        }
    }
    for (id, d, foot, top) in &crossed {
        cv.line(px(*d, *foot), px(*d, *top), [255, 255, 255], 3.0);
        let (x, y) = px(*d, *top);
        cv.text(&font, 12.0, (x, y - 22.0), 0.5, id, TEXT);
    }
    let stretch = if exag > 1.5 { format!(" · heights stretched ×{exag:.0}") } else { String::new() };
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &format!("Section {}: ({:.1}, {:.1}) to ({:.1}, {:.1}), {:.1} m · heights in m{stretch} · white: building walls", k + 1, a[0], a[1], b[0], b[1], len), TEXT);
    cv.text(&font, 12.0, (l as f32, ch as f32 - 26.0), 0.0, "0 m", DIM);
    cv.text(&font, 12.0, (cw as f32 - 20.0, ch as f32 - 26.0), 1.0, &format!("{len:.1} m"), DIM);
    let sec = Section { from: a, to: b, image: format!("section-{}.png", k + 1), csv: format!("section-{}.csv", k + 1), walls_crossed: crossed.iter().map(|c| c.0.clone()).collect() };
    (sec, cv.png(), csv)
}

// ----------------------------------------------------------------- report ---

fn dxf(walls: &[Wall], stations: &[(u16, [f64; 2])], ponds: &[Pond]) -> String {
    let mut s = String::from("0\nSECTION\n2\nENTITIES\n");
    for w in walls {
        let layer = match w.kind.as_str() {
            "building" => "WALLS",
            "low wall" => "LOW_WALLS",
            _ => "VEGETATION",
        };
        let _ = write!(s, "0\nLINE\n8\n{layer}\n10\n{:.3}\n20\n{:.3}\n30\n0.0\n11\n{:.3}\n21\n{:.3}\n31\n0.0\n", w.from[0], w.from[1], w.to[0], w.to[1]);
    }
    for (n, p) in stations {
        let _ = write!(s, "0\nPOINT\n8\nSTATIONS\n10\n{:.3}\n20\n{:.3}\n30\n0.0\n", p[0], p[1]);
        let _ = write!(s, "0\nTEXT\n8\nSTATIONS\n10\n{:.3}\n20\n{:.3}\n30\n0.0\n40\n0.5\n1\n{n}\n", p[0] + 0.3, p[1] + 0.3);
    }
    for (k, p) in ponds.iter().enumerate() {
        let r = (p.area_m2 / std::f64::consts::PI).sqrt();
        let _ = write!(s, "0\nCIRCLE\n8\nLOW_SPOTS\n10\n{:.3}\n20\n{:.3}\n30\n0.0\n40\n{r:.3}\n", p.at[0], p.at[1]);
        let _ = write!(s, "0\nTEXT\n8\nLOW_SPOTS\n10\n{:.3}\n20\n{:.3}\n30\n0.0\n40\n0.4\n1\nL{}\n", p.at[0] + r, p.at[1], k + 1);
    }
    s.push_str("0\nENDSEC\n0\nEOF\n");
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn opt(v: Option<f64>, unit: &str) -> String {
    v.map(|v| format!("{v}{unit}")).unwrap_or_else(|| "–".into())
}

fn report_html(summary: &Value, walls: &[WallResult], ponds: &[Pond], sections: &[Section], opts: &Opts) -> String {
    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Survey report</title>
<style>body{{font:15px/1.5 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;margin:24px auto;max-width:1200px;padding:0 16px;color:#1f2328;background:#fff}}
h1{{font-size:24px}}h2{{font-size:19px;margin-top:32px;border-bottom:1px solid #d0d7de}}table{{border-collapse:collapse;width:100%;font-size:13px}}
th,td{{border:1px solid #d0d7de;padding:4px 6px;text-align:right}}th:first-child,td:first-child{{text-align:left}}img{{max-width:100%;border-radius:6px}}
.note{{color:#57606a}}.bad{{color:#cf222e;font-weight:600}}
@media (prefers-color-scheme:dark){{body{{background:#0d1117;color:#e6edf3}}th,td{{border-color:#30363d}}.note{{color:#8b949e}}h2{{border-color:#30363d}}}}</style>
<h1>Survey report</h1>
<p class="note">{} stations used: those the registration audit trusts. Project frame of station {}: x and y in metres, not north. Flatness tolerance {} mm over a {} m straight line. Made by x9-bridge {}.</p>"#,
        summary["stations_used"], summary["frame_station"], opts.tolerance_mm, opts.straightedge_m, env!("CARGO_PKG_VERSION")
    );
    let _ = write!(h, "<h2>Walls</h2><p class=note>Building walls only. <b>Lean</b>: the best-fit plane's tilt over the wall's height, positive when the top leans toward the side the stations stood on (outward, for a facade scanned from outside). <b>Flatness</b>: the surface's departure from that plane, leaving out openings and reliefs more than 50 mm off. <b>Straight-line check</b>: windows of {} m along rows and columns, each within {} mm of its own straight line.</p>", opts.straightedge_m, opts.tolerance_mm);
    h.push_str("<table><tr><th>Wall</th><th>Length m</th><th>Top above ground m</th><th>Lean mm</th><th>Worst strip mm/m</th><th>Flat RMS mm</th><th>Flat p95 mm</th><th>Straight-line OK %</th><th>Worst mm</th><th>Openings etc. %</th><th>Covered %</th><th>Seen by</th></tr>");
    for w in walls {
        let ok_class = if w.straightedge_ok_pct.is_some_and(|p| p < 90.0) { " class=bad" } else { "" };
        let _ = write!(
            h,
            "<tr><td><a href=\"#{id}\">{id}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td{ok_class}>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            w.length_m,
            opt(w.top_above_ground_m, ""),
            opt(w.out_of_plumb_mm, ""),
            opt(w.worst_strip_mm_per_m, ""),
            opt(w.flat_rms_mm, ""),
            opt(w.flat_p95_mm, ""),
            opt(w.straightedge_ok_pct, ""),
            opt(w.straightedge_worst_mm, ""),
            w.features_pct,
            w.covered_pct,
            w.seen_by.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" "),
            id = esc(&w.wall_id)
        );
    }
    h.push_str("</table>");
    if let Some(top) = summary["building_height_m"].as_f64() {
        let _ = write!(h, "<p>Highest wall top above the ground at its foot: <b>{top} m</b> (the top of what was seen from the ground: a parapet or roof edge, not a roof behind it).</p>");
    }
    for w in walls {
        let _ = write!(h, "<h3 id=\"{id}\">Wall {id}</h3><img src=\"{}\" alt=\"Wall {id} heat map\">", w.image, id = esc(&w.wall_id));
    }
    h.push_str("<h2>Ground and floors</h2><p class=note>The lowest level surface in each 10 cm cell; cars and other things more than 25 cm above their surroundings are left out.</p>");
    h.push_str("<img src=\"ground.png\" alt=\"Ground heights with contours\"><img src=\"flatness.png\" alt=\"Ground bumps and dips against their surroundings\">");
    if let Some(f) = summary.get("ground_flatness") {
        let _ = write!(h, "<p>Height against the ground within {} m around it (bumps and dips within 50 mm): RMS {} mm, 95% within {} mm. Straight-line check, {} m windows: {}% within {} mm. The site falls {}% overall.</p>", opts.straightedge_m / 2.0, f["rms_mm"], f["p95_mm"], opts.straightedge_m, f["straightedge_ok_pct"], opts.tolerance_mm, f["slope_pct"]);
    }
    let _ = write!(h, "<h2>Low spots</h2><img src=\"low_spots.png\" alt=\"Where water would stand\"><p class=note>Where water would stand before spilling over the ground around it, if no drain takes it. A drain the scan didn't see empties a low spot; check them on site.</p>");
    h.push_str("<table><tr><th>Low spot</th><th>x m</th><th>y m</th><th>Area m²</th><th>Deepest mm</th><th>Volume litres</th></tr>");
    for (k, p) in ponds.iter().enumerate() {
        let _ = write!(h, "<tr><td>L{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", k + 1, p.at[0], p.at[1], p.area_m2, p.max_depth_mm, p.volume_l);
    }
    h.push_str("</table>");
    if !sections.is_empty() {
        h.push_str("<h2>Sections</h2>");
        for s in sections {
            let _ = write!(h, "<img src=\"{}\" alt=\"Section\"><p class=note>Data: <a href=\"{}\">{}</a></p>", s.image, s.csv, s.csv);
        }
    }
    h.push_str("<h2>Files</h2><p><a href=walls.csv>walls.csv</a> · <a href=low_spots.csv>low_spots.csv</a> · <a href=plan.dxf>plan.dxf</a> (walls, stations, low spots; metres) · <a href=analysis.json>analysis.json</a> · ground.f32 + ground.json (the height grid)</p>");
    h
}

// -------------------------------------------------------------------- cli ---

const USAGE: &str = "usage: x9-bridge analyze [--straightedge M] [--tolerance-mm MM] [--section x1,y1,x2,y2]… REGISTER_DIR OUT_DIR STATION.tzf…\n       x9-bridge analyze --compare [--min-change-m M] BEFORE_DIR AFTER_DIR OUT_DIR";

pub fn cli(args: &[String]) -> anyhow::Result<()> {
    let mut opts = Opts::default();
    let mut rest: Vec<String> = Vec::new();
    let mut compare = false;
    let mut min_change = 0.02f32;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().context(USAGE);
        match a.as_str() {
            "--straightedge" => opts.straightedge_m = val()?.parse()?,
            "--tolerance-mm" => opts.tolerance_mm = val()?.parse()?,
            "--section" => {
                let v: Vec<f64> = val()?.split(',').map(str::parse).collect::<Result<_, _>>()?;
                anyhow::ensure!(v.len() == 4, "--section x1,y1,x2,y2");
                opts.sections.push([v[0], v[1], v[2], v[3]]);
            }
            "--compare" => compare = true,
            "--min-change-m" => min_change = val()?.parse()?,
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            _ => rest.push(a.clone()),
        }
    }
    if compare {
        let [a, b, out] = &rest[..] else { anyhow::bail!(USAGE) };
        return compare_cli(a, b, out, min_change);
    }
    let [reg, out, files @ ..] = &rest[..] else { anyhow::bail!(USAGE) };
    anyhow::ensure!(!files.is_empty(), USAGE);
    run(reg, out, files, &opts)
}

pub fn run(reg: &str, out: &str, files: &[String], opts: &Opts) -> anyhow::Result<()> {
    let poses: Vec<Option<Pose>> = serde_json::from_slice(&std::fs::read(format!("{reg}/poses.json")).with_context(|| format!("{reg}/poses.json"))?)?;
    let audit: Vec<Value> = serde_json::from_slice(&std::fs::read(format!("{reg}/audit.json"))?)?;
    let walls: Vec<Wall> = serde_json::from_slice(&std::fs::read(format!("{reg}/walls.json"))?)?;
    anyhow::ensure!(poses.len() == files.len(), "{} poses but {} station files: give the files in the order `register` had them", poses.len(), files.len());
    std::fs::create_dir_all(out)?;
    let number = |i: usize, f: &str| -> u16 {
        let stem = std::path::Path::new(f).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let digits: String = stem.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
        digits.parse().unwrap_or(i as u16 + 1)
    };
    let trusted = |k: usize| audit.get(k).and_then(|a| a["verdict"].as_str()).is_some_and(|v| matches!(v, "confirmed" | "single" | "frame"));
    let used: Vec<usize> = (0..files.len()).filter(|&k| poses[k].is_some() && trusted(k)).collect();
    let stations: Vec<(u16, [f64; 2])> = used.iter().map(|&k| (number(k, &files[k]), poses[k].map(|p| [p.xyz_m[0], p.xyz_m[1]]).unwrap())).collect();
    let station_xy: HashMap<u16, [f64; 2]> = stations.iter().copied().collect();
    let mut grids: Vec<WallGrid> = walls
        .iter()
        .filter(|w| w.kind == "building")
        .map(|w| {
            let seen: Vec<[f64; 2]> = w.seen_by.iter().filter_map(|s| station_xy.get(s).copied()).collect();
            WallGrid::new(w, &seen)
        })
        .collect();
    let mut cells: HashMap<(i32, i32), Lowest> = HashMap::new();
    let t0 = std::time::Instant::now();
    for &k in &used {
        let f = &files[k];
        let pose = poses[k].unwrap();
        let t = trimble_tzf::Tzf::open(std::path::Path::new(f)).with_context(|| format!("opening {f}"))?;
        let pts = t.grid_points().with_context(|| format!("{f}: decoding"))?;
        let mut n = 0usize;
        for g in &pts {
            let p = [g.point.x, g.point.y, g.point.z];
            if !p.iter().all(|v| v.is_finite()) {
                continue;
            }
            let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() as f64;
            if r < 0.5 {
                continue;
            }
            let w = pose.apply(p);
            let w = [w[0] as f64, w[1] as f64, w[2] as f64];
            if r <= WALL_RANGE_M {
                for gr in grids.iter_mut() {
                    gr.add(w);
                }
            }
            // Below the scanner: a candidate for the ground or floor.
            if r <= GROUND_RANGE_M && p[2] < -0.5 {
                cells.entry(((w[0] / CELL).floor() as i32, (w[1] / CELL).floor() as i32)).or_insert_with(Lowest::new).add(w[2] as f32);
                n += 1;
            }
        }
        eprintln!("  station {}: {} k points, {} k on or below the ground ({:.0} s)", number(k, f), pts.len() / 1000, n / 1000, t0.elapsed().as_secs_f64());
    }
    let ground = Ground::from_cells(&cells, CELL);
    drop(cells);
    // Walls.
    let mut results = Vec::new();
    for g in &grids {
        let img = format!("wall-{}.png", g.wall.wall_id);
        let (r, png) = analyze_wall(g, ground.as_ref(), opts, &img);
        std::fs::write(format!("{out}/{img}"), png)?;
        eprintln!(
            "  wall {:<4} {:>5.1} m: lean {} mm, flat RMS {} mm, straight-line ok {}%, top {} m",
            r.wall_id,
            r.length_m,
            opt(r.out_of_plumb_mm, ""),
            opt(r.flat_rms_mm, ""),
            opt(r.straightedge_ok_pct, ""),
            opt(r.top_above_ground_m, "")
        );
        results.push(r);
    }
    grids.clear();
    // Ground.
    let mut ponds_found = Vec::new();
    let mut sections = Vec::new();
    let mut ground_flat = Value::Null;
    if let Some(g) = &ground {
        std::fs::write(format!("{out}/ground.f32"), g.to_bytes())?;
        std::fs::write(format!("{out}/ground.json"), serde_json::to_vec_pretty(&g.header())?)?;
        let (png, _) = ground_png(g, &walls, &stations);
        std::fs::write(format!("{out}/ground.png"), png)?;
        let slope = ground_plane(g).map(|p| r2(p[1].hypot(p[2]) * 100.0));
        let dep = local_departure(g, opts.straightedge_m / 2.0);
        std::fs::write(format!("{out}/flatness.png"), flatness_png(g, &dep, slope, &walls, &stations, opts))?;
        ground_flat = ground_flatness(g, &dep, slope, opts);
        let depth = g.ponding();
        ponds_found = ponds(g, &depth);
        std::fs::write(format!("{out}/low_spots.png"), ponds_png(g, &depth, &ponds_found, &walls, &stations))?;
        for (k, s) in opts.sections.iter().enumerate() {
            let (sec, png, csv) = section(g, &results, &walls, *s, k);
            std::fs::write(format!("{out}/{}", sec.image), png)?;
            std::fs::write(format!("{out}/{}", sec.csv), csv)?;
            sections.push(sec);
        }
    }
    // Files.
    let mut csv = String::from("wall_id,kind,length_m,top_above_ground_m,out_of_plumb_mm,lean_mm_per_m,worst_strip_mm_per_m,worst_strip_at_m,flat_rms_mm,flat_p95_mm,straightedge_windows,straightedge_ok_pct,straightedge_worst_mm,features_pct,covered_pct,seen_by\n");
    let o = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
    for r in &results {
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.wall_id,
            r.kind,
            r.length_m,
            o(r.top_above_ground_m),
            o(r.out_of_plumb_mm),
            o(r.lean_mm_per_m),
            o(r.worst_strip_mm_per_m),
            o(r.worst_strip_at_m),
            o(r.flat_rms_mm),
            o(r.flat_p95_mm),
            r.straightedge_windows,
            o(r.straightedge_ok_pct),
            o(r.straightedge_worst_mm),
            r.features_pct,
            r.covered_pct,
            r.seen_by.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" ")
        );
    }
    std::fs::write(format!("{out}/walls.csv"), csv)?;
    let mut pcsv = String::from("low_spot,x_m,y_m,area_m2,max_depth_mm,volume_l\n");
    for (k, p) in ponds_found.iter().enumerate() {
        let _ = writeln!(pcsv, "L{},{},{},{},{},{}", k + 1, p.at[0], p.at[1], p.area_m2, p.max_depth_mm, p.volume_l);
    }
    std::fs::write(format!("{out}/low_spots.csv"), pcsv)?;
    std::fs::write(format!("{out}/plan.dxf"), dxf(&walls, &stations, &ponds_found))?;
    let height = results.iter().filter_map(|r| r.top_above_ground_m).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
    let frame = poses.iter().position(|p| p.is_some_and(|p| p.xyz_m == [0.0, 0.0, 0.0] && p.turn_deg == 0.0)).map(|k| number(k, &files[k]));
    let summary = json!({
        "stations_used": used.len(),
        "frame_station": frame,
        "building_height_m": height,
        "walls": results.len(),
        "low_spots": ponds_found.len(),
        "ground_cells": ground.as_ref().map(|g| g.known()),
        "ground_flatness": ground_flat,
        "straightedge_m": opts.straightedge_m,
        "tolerance_mm": opts.tolerance_mm,
    });
    let mut summary_out = summary.clone();
    if summary_out["ground_flatness"].is_null() {
        summary_out.as_object_mut().unwrap().remove("ground_flatness");
    }
    std::fs::write(format!("{out}/report.html"), report_html(&summary_out, &results, &ponds_found, &sections, opts))?;
    std::fs::write(
        format!("{out}/analysis.json"),
        serde_json::to_vec_pretty(&json!({"version": env!("CARGO_PKG_VERSION"), "summary": summary, "walls": results, "low_spots": ponds_found, "sections": sections}))?,
    )?;
    eprintln!("wrote {out}/report.html: {} walls, {} low spots, building height {} m ({:.0} s)", results.len(), ponds_found.len(), opt(height, ""), t0.elapsed().as_secs_f64());
    Ok(())
}

fn ground_flatness(g: &Ground, dep: &[f32], slope_pct: Option<f64>, opts: &Opts) -> Value {
    let mut abs_r = Vec::new();
    let mut sq = 0.0;
    let mut rows: Vec<Vec<(f64, f64)>> = vec![Vec::new(); g.h];
    let mut cols: Vec<Vec<(f64, f64)>> = vec![Vec::new(); g.w];
    for i in 0..g.w * g.h {
        let (z, d) = (g.z[i], dep[i]);
        if !z.is_finite() || !d.is_finite() || (d as f64).abs() > FEATURE_M {
            continue;
        }
        let (x, y) = (g.x0 + ((i % g.w) as f64 + 0.5) * g.cell, g.y0 + ((i / g.w) as f64 + 0.5) * g.cell);
        abs_r.push((d as f64).abs());
        sq += (d as f64) * (d as f64);
        // The straight-line check runs on the heights themselves: it takes
        // out its own slope.
        rows[i / g.w].push((x, z as f64));
        cols[i % g.w].push((y, z as f64));
    }
    if abs_r.is_empty() {
        return Value::Null;
    }
    let n = abs_r.len();
    let mut gaps = Vec::new();
    for line in rows.iter().step_by(3).chain(cols.iter().step_by(3)) {
        gaps.extend(straightedge(line, opts.straightedge_m, g.cell).into_iter().map(|x| x.1));
    }
    let tol = opts.tolerance_mm / 1000.0;
    let ok = gaps.iter().filter(|&&x| x <= tol).count();
    json!({
        "rms_mm": r1((sq / n as f64).sqrt() * 1000.0),
        "p95_mm": percentile(&mut abs_r, 0.95).map(|v| r1(v * 1000.0)),
        "straightedge_windows": gaps.len(),
        "straightedge_ok_pct": (!gaps.is_empty()).then(|| r1(100.0 * ok as f64 / gaps.len() as f64)),
        "slope_pct": slope_pct,
    })
}

// ---------------------------------------------------------------- compare ---

/// Cut and fill between two height grids of the same site (same frame),
/// counting cells that changed by `min_change` metres or more: summed over a
/// whole site, a scan's millimetres of noise would otherwise add up to cubic
/// metres. Also returns the net change over every cell, threshold or not.
pub fn cut_fill(a: &Ground, b: &Ground, min_change: f32) -> (f64, f64, f64, usize, Vec<(usize, f32)>) {
    let (mut cut, mut fill, mut net, mut n) = (0.0, 0.0, 0.0, 0usize);
    let mut diff = Vec::new();
    let area = b.cell * b.cell;
    for i in 0..b.w * b.h {
        let zb = b.z[i];
        if !zb.is_finite() {
            continue;
        }
        let p = [b.x0 + ((i % b.w) as f64 + 0.5) * b.cell, b.y0 + ((i / b.w) as f64 + 0.5) * b.cell];
        let Some(za) = a.at(p) else { continue };
        let d = zb - za;
        n += 1;
        net += d as f64 * area;
        if d >= min_change {
            fill += d as f64 * area;
        } else if d <= -min_change {
            cut -= d as f64 * area;
        }
        diff.push((i, d));
    }
    (cut, fill, net, n, diff)
}

fn compare_cli(a_dir: &str, b_dir: &str, out: &str, min_change: f32) -> anyhow::Result<()> {
    let (a, b) = (Ground::load(a_dir)?, Ground::load(b_dir)?);
    std::fs::create_dir_all(out)?;
    let (cut, fill, net, n, diff) = cut_fill(&a, &b, min_change);
    // Only changes above the noise count toward the change map.
    let v = PlanView::of(&b);
    let mut cv = Canvas::new(v.w, v.h, BG);
    let dmap: HashMap<usize, f32> = diff.iter().copied().collect();
    v.paint(&b, &mut cv, |i| dmap.get(&i).map(|d| if d.abs() < min_change { [70, 74, 82] } else { diverging(*d as f64 / 0.5) }));
    let font = crate::station::font();
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &format!("Change in ground height · fill {fill:.2} m³ · cut {cut:.2} m³ · over {:.0} m² seen both times", n as f64 * b.cell * b.cell), TEXT);
    let ly = (v.h - 56) as i64;
    for i in 0..200 {
        let t = i as f64 / 199.0 * 2.0 - 1.0;
        for y in 0..12 {
            cv.put(12 + i, ly + y, diverging(t), 1.0);
        }
    }
    cv.text(&font, 12.0, (12.0, ly as f32 + 16.0), 0.0, "-0.5 m", DIM);
    cv.text(&font, 12.0, (212.0, ly as f32 + 16.0), 1.0, "+0.5 m", DIM);
    cv.text(&font, 12.0, (230.0, ly as f32 - 2.0), 0.0, &format!("red: higher after (fill) · blue: lower (cut) · grey: changed less than {:.0} mm", min_change * 1000.0), DIM);
    std::fs::write(format!("{out}/change.png"), cv.png())?;
    let rep = json!({"fill_m3": r2(fill), "cut_m3": r2(cut), "counted_from_mm": r1(min_change as f64 * 1000.0), "net_all_cells_m3": r2(net), "compared_m2": r1(n as f64 * b.cell * b.cell), "before": a_dir, "after": b_dir});
    std::fs::write(format!("{out}/change.json"), serde_json::to_vec_pretty(&rep)?)?;
    eprintln!("fill {fill:.3} m³, cut {cut:.3} m³ (cells changed {:.0} mm or more); net over every cell {net:.3} m³; {:.0} m² seen both times", min_change * 1000.0, n as f64 * b.cell * b.cell);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_ground(w: usize, h: usize, z: f32) -> Ground {
        Ground { x0: 0.0, y0: 0.0, cell: 0.1, w, h, z: vec![z; w * h] }
    }

    #[test]
    fn a_plane_and_its_lean_are_found() {
        // A wall face leaning 5 mm per metre toward the viewer, with an opening.
        let mut cells = Vec::new();
        for iu in 0..200 {
            for iz in 0..100 {
                let (u, z) = (iu as f64 * 0.05, iz as f64 * 0.05);
                let opening = (3.0..4.0).contains(&u) && (1.0..2.5).contains(&z);
                cells.push((u, z, if opening { -0.20 } else { 0.01 + 0.005 * z }));
            }
        }
        let f = robust_plane(&cells).unwrap();
        assert!((f[2] - 0.005).abs() < 1e-6 && f[1].abs() < 1e-6, "{f:?}");
    }

    #[test]
    fn straightedge_finds_a_bump() {
        let flat: Vec<(f64, f64)> = (0..100).map(|i| (i as f64 * 0.05, 0.0)).collect();
        assert!(straightedge(&flat, 3.0, 0.05).iter().all(|w| w.1 < 1e-9));
        let bumped: Vec<(f64, f64)> = (0..100).map(|i| (i as f64 * 0.05, if i == 50 { 0.01 } else { 0.0 })).collect();
        let worst = straightedge(&bumped, 3.0, 0.05).iter().map(|w| w.1).fold(0.0, f64::max);
        assert!(worst > 0.009 && worst < 0.011, "{worst}");
    }

    #[test]
    fn a_dip_holds_water_and_a_slope_does_not() {
        // Sloping 1% in x: drains.
        let mut g = flat_ground(60, 40, 0.0);
        for y in 0..40 {
            for x in 0..60 {
                g.z[y * 60 + x] = 0.01 * x as f32 * 0.1;
            }
        }
        assert!(ponds(&g, &g.ponding()).is_empty());
        // A bowl 2 m across and 30 mm deep in the middle of a flat area.
        let mut g = flat_ground(60, 60, 0.0);
        for y in 0..60 {
            for x in 0..60 {
                let r = ((x as f64 - 30.0).hypot(y as f64 - 30.0)) * 0.1;
                if r < 1.0 {
                    g.z[y * 60 + x] = -(0.03 * (1.0 - r * r)) as f32;
                }
            }
        }
        let p = ponds(&g, &g.ponding());
        assert_eq!(p.len(), 1, "{p:?}");
        // Volume of a paraboloid bowl: π r² d / 2 = 47 litres.
        assert!((p[0].volume_l - 47.1).abs() < 3.0, "{p:?}");
        assert!((p[0].max_depth_mm - 30.0).abs() < 1.5);
    }

    #[test]
    fn unknown_cells_let_water_out() {
        // The same bowl, but a gap in the scan runs to its middle.
        let mut g = flat_ground(60, 60, 0.0);
        for y in 0..60 {
            for x in 0..60 {
                let r = ((x as f64 - 30.0).hypot(y as f64 - 30.0)) * 0.1;
                if r < 1.0 {
                    g.z[y * 60 + x] = -(0.03 * (1.0 - r * r)) as f32;
                }
            }
        }
        g.z[30 * 60 + 30] = f32::NAN;
        let d = g.ponding();
        assert!(ponds(&g, &d).iter().all(|p| p.max_depth_mm < 30.0), "a hole may be a drain");
    }

    #[test]
    fn cars_are_not_ground() {
        let mut cells: HashMap<(i32, i32), Lowest> = HashMap::new();
        for y in 0..100 {
            for x in 0..100 {
                let mut l = Lowest::new();
                // A car roof 1.4 m up over a 2 × 4 m patch the scanner couldn't see under.
                let z = if (40..60).contains(&x) && (30..70).contains(&y) { 1.4 } else { 0.0 };
                for dz in [0.0, 0.002, 0.0, 0.002] {
                    l.add(z + dz);
                }
                cells.insert((x, y), l);
            }
        }
        let g = Ground::from_cells(&cells, 0.1).unwrap();
        assert!(g.at([5.0, 5.0]).is_none_or(|z| z < 0.5), "the roof is gone: {:?}", g.at([5.0, 5.0]));
        assert_eq!(g.at([1.0, 1.0]), Some(0.001));
    }

    #[test]
    fn a_planted_stockpile_has_its_volume() {
        let a = flat_ground(100, 100, 0.0);
        let mut b = a.clone();
        // A cone 3 m across and 1 m high: π r² h / 3 = 2.356 m³.
        for y in 0..100 {
            for x in 0..100 {
                let r = ((x as f64 + 0.5 - 50.0).hypot(y as f64 + 0.5 - 50.0)) * 0.1;
                if r < 1.5 {
                    b.z[y * 100 + x] = (1.0 - r / 1.5) as f32;
                }
            }
        }
        let (cut, fill, net, n, _) = cut_fill(&a, &b, 0.0);
        assert_eq!(n, 10_000);
        assert!(cut < 1e-9 && (fill - 2.356).abs() < 0.03 && (net - fill).abs() < 1e-9, "fill {fill}");
        // Noise doesn't add up once small changes are left out.
        let mut noisy = b.clone();
        for (k, z) in noisy.z.iter_mut().enumerate() {
            *z += if k % 2 == 0 { 0.003 } else { -0.003 };
        }
        let (cut, fill, _, _, _) = cut_fill(&a, &noisy, 0.02);
        assert!(cut < 1e-9 && (fill - 2.356).abs() < 0.05, "fill {fill}, cut {cut}");
    }

    #[test]
    fn local_flatness_ignores_a_slope_and_finds_a_bump() {
        // A 5% slope with a 20 mm bump 0.3 m across.
        let mut g = flat_ground(80, 80, 0.0);
        for y in 0..80 {
            for x in 0..80 {
                let bump = ((x as f64 - 40.0).hypot(y as f64 - 40.0)) * 0.1 < 0.15;
                g.z[y * 80 + x] = (0.05 * x as f64 * 0.1 + if bump { 0.02 } else { 0.0 }) as f32;
            }
        }
        let d = local_departure(&g, 1.5);
        assert!(d[20 * 80 + 40].abs() < 0.001, "the slope is not a dip: {}", d[20 * 80 + 40]);
        assert!(d[40 * 80 + 40] > 0.019, "the bump stands out: {}", d[40 * 80 + 40]);
    }

    #[test]
    fn the_lowest_returns_decide_a_cell() {
        let mut l = Lowest::new();
        for z in [0.50, 0.012, 0.010, 0.011, 0.30, -0.4] {
            l.add(z);
        }
        // -0.4 alone is a stray return; the cell is the 1 cm cluster.
        let h = l.height().unwrap();
        assert!((h - 0.011).abs() < 0.002, "{h}");
    }
}
