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
//! registration: a wall's plane mixes every station that saw it, so a few
//! millimetres of registration mismatch show up as roughness.

use crate::register::{Pose, Wall};
use crate::station::Canvas;
use anyhow::Context;
use serde::{Deserialize, Serialize};
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
    /// A mission (`missions.json` entry) whose questions open the report.
    pub questions: Option<String>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { straightedge_m: 3.0, tolerance_mm: 6.0, sections: vec![], questions: None }
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

#[derive(Serialize, Deserialize, Clone, Debug)]
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
    /// The height plumb is measured over (m): the surface's 1st to 99th
    /// percentile of height.
    pub plumb_over_m: Option<f64>,
    pub image: String,
    /// The gate's word on plumb and flatness (`answer`, `not_yet`,
    /// `does_not_fit`) and, when not an answer, why: the numbers are gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The on-site gate (`ask::gate`), once, on every wall of a finished survey:
/// a wall under 80% covered, under 4 m, or rougher than 15 mm RMS keeps its
/// place and coverage and loses its plumb and flatness numbers.
pub fn gate_walls(walls: &mut [WallResult]) {
    use crate::ask::Gate;
    for w in walls {
        let (verdict, reason) = match crate::ask::gate(w.covered_pct / 100.0, w.length_m, w.flat_rms_mm) {
            Gate::Pass => ("answer", None),
            Gate::NotYet(r) => ("not_yet", Some(r)),
            Gate::DoesNotFit(r) => ("does_not_fit", Some(r)),
        };
        withhold(w, verdict, reason);
    }
}

/// Give a measured wall its verdict; anything but an answer loses its plumb
/// and flatness numbers.
pub fn withhold(w: &mut WallResult, verdict: &str, reason: Option<String>) {
    {
        w.verdict = Some(verdict.into());
        if reason.is_some() {
            w.lean_mm_per_m = None;
            w.out_of_plumb_mm = None;
            w.worst_strip_mm_per_m = None;
            w.worst_strip_at_m = None;
            w.flat_rms_mm = None;
            w.flat_p95_mm = None;
            w.straightedge_ok_pct = None;
            w.straightedge_worst_mm = None;
            w.plumb_over_m = None;
        }
        w.reason = reason;
    }
}

/// "not yet", "does not fit"
fn said(verdict: &str) -> String {
    verdict.replace('_', " ")
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

/// Measure one wall's grid; the plane comes back for drawing it.
fn measure_wall(g: &WallGrid, ground: Option<&Ground>, opts: &Opts, img: &str) -> (WallResult, Option<[f64; 3]>) {
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
        plumb_over_m: lean.map(|_| r2(zhi - zlo)),
        image: img.to_string(),
        verdict: None,
        reason: None,
    };
    (r, plane)
}

/// The wall image's title: its numbers only when the gate let them through.
fn wall_title(r: &WallResult) -> String {
    let top = r.top_above_ground_m.map(|t| format!("top {t:.1} m above ground")).unwrap_or_else(|| "height unknown".into());
    if let (Some(v), Some(_)) = (&r.verdict, &r.reason) {
        return format!("Wall {} · {:.1} m long · {top} · plumb and flatness: {}", r.wall_id, r.length_m, said(v));
    }
    let lean = r.out_of_plumb_mm.map(|v| format!("leans {v:+.0} mm over its height")).unwrap_or_else(|| "lean unknown".into());
    format!("Wall {} · {:.1} m long · {top} · flatness RMS {} mm · {lean}", r.wall_id, r.length_m, r.flat_rms_mm.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into()))
}

/// The colour scale's ends: in millimetres only when the numbers are given.
/// Otherwise the colours show where the face stands proud or sits back, and
/// no figure can be read off them.
pub(crate) fn legend_ends(r: &WallResult, mm: f64) -> Option<(String, String)> {
    r.reason.is_none().then(|| (format!("-{mm:.0} mm"), format!("+{mm:.0} mm")))
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
    cv.text(&font, 15.0, (12.0, 10.0), 0.0, &wall_title(r), TEXT);
    // Legend.
    let ly = (top + ih + 14) as i64;
    for i in 0..200 {
        let t = i as f64 / 199.0 * 2.0 - 1.0;
        for y in 0..12 {
            cv.put(12 + i, ly + y, diverging(t), 1.0);
        }
    }
    if let Some((lo, hi)) = legend_ends(r, range * 1000.0) {
        cv.text(&font, 12.0, (12.0, ly as f32 + 16.0), 0.0, &lo, DIM);
        cv.text(&font, 12.0, (212.0, ly as f32 + 16.0), 1.0, &hi, DIM);
    }
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

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn opt(v: Option<f64>, unit: &str) -> String {
    v.map(|v| format!("{v}{unit}")).unwrap_or_else(|| "–".into())
}

fn report_html(summary: &Value, walls: &[WallResult], ponds: &[Pond], sections: &[Section], opts: &Opts, questions: &str) -> String {
    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Scan report</title>
<style>body{{font:15px/1.5 -apple-system,Segoe UI,Helvetica,Arial,sans-serif;margin:24px auto;max-width:1200px;padding:0 16px;color:#1f2328;background:#fff}}
h1{{font-size:24px}}h2{{font-size:19px;margin-top:32px;border-bottom:1px solid #d0d7de}}table{{border-collapse:collapse;width:100%;font-size:13px}}
th,td{{border:1px solid #d0d7de;padding:4px 6px;text-align:right}}th:first-child,td:first-child{{text-align:left}}img{{max-width:100%;border-radius:6px}}
.note{{color:#57606a}}.bad{{color:#cf222e;font-weight:600}}.question{{border:1px solid #d0d7de;border-radius:6px;padding:4px 12px;margin:12px 0}}.question h3{{margin:6px 0}}.verdict{{font-weight:600;text-transform:uppercase;font-size:12px;letter-spacing:.04em;margin-right:6px}}.flag{{color:#cf222e}}.declined{{color:#57606a}}
@media (prefers-color-scheme:dark){{body{{background:#0d1117;color:#e6edf3}}th,td{{border-color:#30363d}}.note{{color:#8b949e}}h2{{border-color:#30363d}}}}</style>
<h1>Scan report</h1>
<p class="note">{} stations used: those the registration audit trusts. Project frame of station {}: x and y in metres, not north. Flatness tolerance {} mm over a {} m straight line. Made by x9-bridge {}.</p>"#,
        summary["stations_used"], summary["frame_station"], opts.tolerance_mm, opts.straightedge_m, env!("CARGO_PKG_VERSION")
    );
    h.push_str(questions);
    let _ = write!(h, "<h2>Walls</h2><p class=note>Building walls only. <b>Lean</b>: the best-fit plane's tilt over the wall's height, positive when the top leans toward the side the stations stood on (outward, for a facade scanned from outside). <b>Flatness</b>: the surface's departure from that plane, leaving out openings and reliefs more than 50 mm off. <b>Straight-line check</b>: windows of {} m along rows and columns, each within {} mm of its own straight line. A wall less than {:.0}% covered, shorter than {:.0} m, or rougher than {:.0} mm RMS against one plane gives the reason instead of these numbers: not seen enough yet, or not one flat face.</p>", opts.straightedge_m, opts.tolerance_mm, crate::register::FACE_COVERED * 100.0, crate::ask::MIN_LENGTH_M, crate::ask::MAX_RMS_MM);
    h.push_str("<table><tr><th>Wall</th><th>Length m</th><th>Top above ground m</th><th>Lean mm</th><th>Worst strip mm/m</th><th>Flat RMS mm</th><th>Flat p95 mm</th><th>Straight-line OK %</th><th>Worst mm</th><th>Openings etc. %</th><th>Covered %</th><th>Seen by</th></tr>");
    for w in walls {
        if let (Some(v), Some(why)) = (&w.verdict, &w.reason) {
            let _ = write!(
                h,
                "<tr><td><a href=\"#{id}\">{id}</a></td><td>{}</td><td>{}</td><td colspan=6 class=note style=\"text-align:left\"><b>{}</b>: {}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                w.length_m,
                opt(w.top_above_ground_m, ""),
                said(v),
                esc(why),
                w.features_pct,
                w.covered_pct,
                w.seen_by.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" "),
                id = esc(&w.wall_id)
            );
            continue;
        }
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

const USAGE: &str = "usage: x9-bridge analyze [--straightedge M] [--tolerance-mm MM] [--section x1,y1,x2,y2]… [--questions MISSION.json] REGISTER_DIR OUT_DIR STATION.tzf…\n       x9-bridge analyze --compare [--min-change-m M] BEFORE_DIR AFTER_DIR OUT_DIR";

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
            "--questions" => opts.questions = Some(val()?),
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

/// Every point of a station file at full resolution: `(scanner frame,
/// project frame, range)`, past half a metre. Returns how many were decoded.
pub(crate) fn station_points(f: &std::path::Path, pose: &Pose, mut each: impl FnMut([f32; 3], [f64; 3], f64)) -> anyhow::Result<usize> {
    let t = trimble_tzf::Tzf::open(f).with_context(|| format!("opening {}", f.display()))?;
    let pts = t.grid_points().with_context(|| format!("{}: decoding", f.display()))?;
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
        each(p, [w[0] as f64, w[1] as f64, w[2] as f64], r);
    }
    Ok(pts.len())
}

/// A face measured as `run` measures a wall, kept for drawing once the gate
/// has had its say (`png` draws `result` as it then stands).
pub struct FaceFit {
    pub result: WallResult,
    grid: WallGrid,
    plane: Option<[f64; 3]>,
}

impl FaceFit {
    pub fn png(&self, opts: &Opts) -> Vec<u8> {
        wall_png(&self.grid, self.plane, &self.result, opts)
    }

    /// The fit as bytes, so a worker can send it back and the bridge draw its
    /// evidence exactly as when it fits itself: the result and plane as JSON,
    /// then the grid's sums and counts, all deflated.
    pub fn to_bytes(&self) -> Vec<u8> {
        let g = &self.grid;
        let head = serde_json::to_vec(&serde_json::json!({
            "result": self.result, "plane": self.plane, "wall": g.wall, "from": g.from, "dir": g.dir, "nrm": g.nrm,
            "u0": g.u0, "z0": g.z0, "cell": g.cell, "nu": g.nu, "nz": g.nz, "bbox": g.bbox,
        }))
        .expect("a fit serializes");
        let mut raw = Vec::with_capacity(12 + head.len() + g.sum.len() * 8);
        raw.extend_from_slice(FIT_MAGIC);
        raw.extend_from_slice(&(head.len() as u32).to_le_bytes());
        raw.extend_from_slice(&head);
        for v in &g.sum {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        for v in &g.cnt {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        miniz_oxide::deflate::compress_to_vec(&raw, 6)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<FaceFit, String> {
        let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(bytes, 64 << 20).map_err(|e| format!("not a deflated fit: {e:?}"))?;
        if raw.len() < 8 || &raw[..4] != FIT_MAGIC {
            return Err("not a fit".into());
        }
        let n = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
        let head: Value = serde_json::from_slice(raw.get(8..8 + n).ok_or("fit header cut short")?).map_err(|e| format!("fit header: {e}"))?;
        fn get<T: serde::de::DeserializeOwned>(head: &Value, k: &str) -> Result<T, String> {
            serde_json::from_value(head[k].clone()).map_err(|e| format!("fit {k}: {e}"))
        }
        let (nu, nz): (usize, usize) = (get(&head, "nu")?, get(&head, "nz")?);
        let cells = nu.checked_mul(nz).filter(|c| *c <= 4_000_000).ok_or("fit grid too large")?;
        let body = &raw[8 + n..];
        if body.len() != cells * 8 {
            return Err(format!("fit grid: {} bytes for {cells} cells", body.len()));
        }
        let sum = body[..cells * 4].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        let cnt = body[cells * 4..].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        let grid = WallGrid {
            wall: get(&head, "wall")?, from: get(&head, "from")?, dir: get(&head, "dir")?, nrm: get(&head, "nrm")?,
            u0: get(&head, "u0")?, z0: get(&head, "z0")?, cell: get(&head, "cell")?, nu, nz, sum, cnt, bbox: get(&head, "bbox")?,
        };
        Ok(FaceFit { result: get(&head, "result")?, grid, plane: get(&head, "plane")? })
    }
}

const FIT_MAGIC: &[u8; 4] = b"XFF1";

/// Walls measured from the full-resolution points of the stations that saw
/// them (`files`: station number, pose, file), without the ground. Each file
/// is read once, one at a time (decoding is what needs the memory), and only
/// if one of the walls lists its station; its points go to every wall whose
/// `seen_by` holds that station. Then up to `threads` walls are fitted at once
/// (fitting is what needs the cores). A wall's fit doesn't depend on the
/// others: it takes points only from its own stations, in the order of
/// `files`, so fitting several together gives each the fit it gets alone.
pub fn measure_faces_with(walls: &[Wall], files: &[(u16, Pose, std::path::PathBuf)], opts: &Opts, threads: usize) -> anyhow::Result<Vec<FaceFit>> {
    let xy: HashMap<u16, [f64; 2]> = files.iter().map(|(n, p, _)| (*n, [p.xyz_m[0], p.xyz_m[1]])).collect();
    let mut grids: Vec<WallGrid> = walls.iter().map(|w| WallGrid::new(w, &w.seen_by.iter().filter_map(|s| xy.get(s).copied()).collect::<Vec<_>>())).collect();
    for (n, pose, f) in files {
        let mine: Vec<usize> = (0..walls.len()).filter(|&k| walls[k].seen_by.contains(n)).collect();
        if mine.is_empty() {
            continue;
        }
        station_points(f, pose, |_, w, r| {
            if r <= WALL_RANGE_M {
                for &k in &mine {
                    grids[k].add(w);
                }
            }
        })?;
    }
    let fit = |grid: &WallGrid| measure_wall(grid, None, opts, &format!("wall-{}.png", grid.wall.wall_id));
    let threads = threads.clamp(1, grids.len().max(1));
    if threads == 1 {
        return Ok(grids
            .into_iter()
            .map(|grid| {
                let (result, plane) = fit(&grid);
                FaceFit { result, grid, plane }
            })
            .collect());
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    type Fitted = std::sync::Mutex<Option<(WallResult, Option<[f64; 3]>)>>;
    let done: Vec<Fitted> = grids.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(grid) = grids.get(k) else { break };
                *done[k].lock().unwrap() = Some(fit(grid));
            });
        }
    });
    Ok(grids
        .into_iter()
        .zip(done)
        .map(|(grid, d)| {
            let (result, plane) = d.into_inner().unwrap().expect("every wall is fitted");
            FaceFit { result, grid, plane }
        })
        .collect())
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
        let mut n = 0usize;
        let total = station_points(std::path::Path::new(f), &pose, |p, w, r| {
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
        })?;
        eprintln!("  station {}: {} k points, {} k on or below the ground ({:.0} s)", number(k, f), total / 1000, n / 1000, t0.elapsed().as_secs_f64());
    }
    let ground = Ground::from_cells(&cells, CELL);
    drop(cells);
    // Walls.
    let mut results = Vec::new();
    let mut planes = Vec::new();
    for g in &grids {
        let (r, plane) = measure_wall(g, ground.as_ref(), opts, &format!("wall-{}.png", g.wall.wall_id));
        results.push(r);
        planes.push(plane);
    }
    gate_walls(&mut results);
    for ((g, r), plane) in grids.iter().zip(&results).zip(&planes) {
        std::fs::write(format!("{out}/{}", r.image), wall_png(g, *plane, r, opts))?;
        eprintln!(
            "  wall {:<4} {:>5.1} m: lean {} mm, flat RMS {} mm, straight-line ok {}%, top {} m",
            r.wall_id,
            r.length_m,
            opt(r.out_of_plumb_mm, ""),
            opt(r.flat_rms_mm, ""),
            opt(r.straightedge_ok_pct, ""),
            opt(r.top_above_ground_m, "")
        );
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
    let csv = walls_csv(&results);
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
    let (questions_html, questions) = match &opts.questions {
        Some(m) => mission_questions(reg, out, m, &walls, &used.iter().map(|&k| (number(k, &files[k]), poses[k].unwrap(), files[k].clone().into())).collect::<Vec<_>>(), opts)?,
        None => (String::new(), Value::Null),
    };
    std::fs::write(format!("{out}/report.html"), report_html(&summary_out, &results, &ponds_found, &sections, opts, &questions_html))?;
    std::fs::write(
        format!("{out}/analysis.json"),
        serde_json::to_vec_pretty(&json!({"version": env!("CARGO_PKG_VERSION"), "summary": summary, "questions": questions, "walls": results, "low_spots": ponds_found, "sections": sections}))?,
    )?;
    eprintln!("wrote {out}/report.html: {} walls, {} low spots, building height {} m ({:.0} s)", results.len(), ponds_found.len(), opt(height, ""), t0.elapsed().as_secs_f64());
    Ok(())
}

/// The mission's questions, each judged again on this registration
/// (`merged.bin` in `reg`) from the stations' files: the report's opening
/// section and its JSON. Evidence images go to `out`.
fn mission_questions(reg: &str, out: &str, mission: &str, walls: &[Wall], files: &[(u16, Pose, std::path::PathBuf)], opts: &Opts) -> anyhow::Result<(String, Value)> {
    use crate::ask::{evidence, gate, look, questions_html, report_drawing, Asked, Gate};
    let m: crate::mission::Mission = serde_json::from_slice(&std::fs::read(mission).with_context(|| mission.to_string())?).with_context(|| format!("{mission}: a mission"))?;
    let merged = crate::register::Merged::from_bytes(&std::fs::read(format!("{reg}/merged.bin")).with_context(|| format!("{reg}/merged.bin: run `x9-bridge register` again"))?)
        .map_err(anyhow::Error::msg)?;
    let scans: HashMap<u16, String> = files.iter().map(|(n, _, _)| (*n, n.to_string())).collect();
    let mut asked = Vec::new();
    for t in m.targets.iter().filter(|t| t.question.is_some()) {
        let (Some(f), Some(q)) = (t.face, &t.question) else { continue };
        let coverage = crate::register::face_coverage(&merged, &f, &scans);
        let (c, fit) = look(&t.target_id, &f, &coverage, files, opts)?;
        let again = gate(c.seen_share, c.length_m, c.fit.as_ref().map(|r| r.flat_rms_mm));
        let image = match fit {
            Some(mut fit) => match report_drawing(q, &again) {
                Some(drawn) => {
                    let name = format!("question-{}.png", t.target_id);
                    std::fs::write(format!("{out}/{name}"), evidence(&mut fit, &drawn, opts))?;
                    Some(name)
                }
                None => None,
            },
            None => None,
        };
        asked.push(Asked {
            target_id: t.target_id.clone(),
            name: t.name.clone(),
            declined: (t.status == crate::mission::Status::Declined).then(|| t.note.clone()),
            on_site: q.clone(),
            again_reading: if again == Gate::Pass { c.fit.clone() } else { None },
            again,
            image,
            tolerance: t.tolerance.clone(),
            face: f,
            runs_on_m: crate::ask::runs_on(&f, walls),
        });
    }
    let json: Vec<Value> = asked
        .iter()
        .map(|a| {
            json!({"target_id": a.target_id, "name": a.name, "on_site": crate::ask::public(&a.target_id, &a.on_site, a.tolerance.as_ref()), "declined": a.declined,
                   "final": match &a.again { Gate::Pass => json!({"verdict": "answer", "answer": a.again_reading.as_ref().map(|r| crate::ask::reading_said(r, a.tolerance.as_ref()))}), Gate::NotYet(r) => json!({"verdict": "not_yet", "reason": r}),
                                             Gate::DoesNotFit(r) => json!({"verdict": "does_not_fit", "reason": r}) },
                   "face": crate::ask::face_said(&a.face, (a.face.to[0] - a.face.from[0]).hypot(a.face.to[1] - a.face.from[1]), walls), "image": a.image})
        })
        .collect();
    Ok((questions_html(&asked), json!(json)))
}

fn walls_csv(results: &[WallResult]) -> String {
    let mut csv = String::from("wall_id,kind,length_m,top_above_ground_m,out_of_plumb_mm,lean_mm_per_m,worst_strip_mm_per_m,worst_strip_at_m,flat_rms_mm,flat_p95_mm,straightedge_windows,straightedge_ok_pct,straightedge_worst_mm,features_pct,covered_pct,seen_by,verdict,reason\n");
    let o = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
    for r in results {
        let _ = writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},\"{}\"",
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
            r.seen_by.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" "),
            r.verdict.as_deref().unwrap_or(""),
            r.reason.as_deref().unwrap_or("").replace('"', "\"\"")
        );
    }
    csv
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

    fn measured(id: &str, length_m: f64, covered_pct: f64, rms: f64, plumb: f64) -> WallResult {
        WallResult {
            wall_id: id.into(), kind: "building".into(), length_m, seen_by: vec![1, 2], covered_pct, features_pct: 3.0,
            lean_mm_per_m: Some(plumb + 0.2), out_of_plumb_mm: Some(plumb), worst_strip_mm_per_m: Some(plumb + 0.1), worst_strip_at_m: Some(1.5),
            flat_rms_mm: Some(rms), flat_p95_mm: Some(rms * 2.0), straightedge_windows: 40, straightedge_ok_pct: Some(plumb - 0.6),
            straightedge_worst_mm: Some(plumb + 0.3), top_above_ground_m: Some(5.2), image: format!("wall-{id}.png"), verdict: None, reason: None,
            plumb_over_m: Some(4.0),
        }
    }

    /// Criterion 2, the report: a wall that isn't an answer shows its verdict, not numbers.
    #[test]
    fn the_report_shows_numbers_only_for_answers() {
        let mut walls = vec![measured("w1", 10.0, 92.0, 8.0, 6.4), measured("w2", 3.0, 87.0, 8.0, 104.0), measured("w3", 9.0, 70.0, 8.0, 104.0),
                             measured("w4", 9.0, 95.0, 22.0, 104.0)];
        gate_walls(&mut walls);
        let v: Vec<Option<&str>> = walls.iter().map(|w| w.verdict.as_deref()).collect();
        assert_eq!(v, [Some("answer"), Some("does_not_fit"), Some("not_yet"), Some("does_not_fit")]);
        for w in &walls[1..] {
            assert!(w.out_of_plumb_mm.is_none() && w.flat_rms_mm.is_none() && w.straightedge_ok_pct.is_none() && w.worst_strip_mm_per_m.is_none(), "{w:?}");
            assert!(w.reason.as_deref().is_some_and(|r| !r.is_empty()));
            // Where it is and how much was seen stay: they aren't the answer.
            assert!(w.length_m > 0.0 && w.covered_pct > 0.0 && w.top_above_ground_m.is_some());
        }
        let summary = json!({"stations_used": 2, "frame_station": 1});
        let h = report_html(&summary, &walls, &[], &[], &Opts::default(), "");
        let csv = walls_csv(&walls);
        for text in [&h, &csv] {
            assert!(!text.contains("103.") && !text.contains("104"), "{text}");
        }
        assert!(h.contains("6.4") && h.contains("not yet") && h.contains("does not fit"), "{h}");
        let rows: Vec<&str> = csv.lines().collect();
        assert!(rows[0].ends_with(",verdict,reason"), "{}", rows[0]);
        assert!(rows[1].contains(",6.4,") && rows[1].contains(",answer,"), "{}", rows[1]);
        assert!(rows[3].contains(",not_yet,"), "{}", rows[3]);
        assert!(wall_title(&walls[0]).contains("+6 mm"));
        // The evidence drawing marks the verdict first, so a face that doesn't fit loses its scale.
        let mut drawn = measured("w9", 10.0, 92.0, 22.0, 104.0);
        let rough = crate::ask::Question { verdict: Some(crate::ask::Verdict::DoesNotFit), reason: "rough".into(), ..crate::ask::Question::new("plumb") };
        crate::ask::mark(&mut drawn, &rough);
        assert!(legend_ends(&drawn, 18.0).is_none() && drawn.out_of_plumb_mm.is_none() && drawn.plumb_over_m.is_none());
        let mut kept = measured("w9", 10.0, 92.0, 8.0, 6.4);
        crate::ask::mark(&mut kept, &crate::ask::Question { verdict: Some(crate::ask::Verdict::Answer), ..crate::ask::Question::new("plumb") });
        assert!(legend_ends(&kept, 18.0).is_some() && kept.plumb_over_m.is_some());
        // Only an answer's heat map has a millimetre scale.
        assert_eq!(legend_ends(&walls[0], 18.0), Some(("-18 mm".to_string(), "+18 mm".to_string())));
        for w in &walls[1..] {
            assert_eq!(legend_ends(w, 18.0), None);
        }
        for w in &walls[1..] {
            let t = wall_title(w);
            assert!(!t.contains("104") && !t.contains("flatness RMS") && t.contains(w.verdict.as_deref().unwrap().replace('_', " ").as_str()), "{t}");
        }
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

    // ------------------------------------------- a synthetic surveyed yard ---

    use crate::export::tests::{scratch, tzf_bytes};

    /// A yard seen from a scanner 1.5 m above its floor: walls at x = -6 and
    /// 5, y = -4 and 6, up to 3 m above the scanner, no roof; a 2 cm dip
    /// 1.5 m square at (2, 2) and a 0.5 m high box (a parked thing) at (-3, 3).
    fn yard(va: f64, ha: f64) -> Option<f64> {
        let d = [va.sin() * ha.cos(), va.sin() * ha.sin(), va.cos()];
        let (lo, hi) = ([-6.0, -4.0], [5.0, 6.0]);
        let wall = (0..2).map(|k| if d[k] > 0.0 { hi[k] / d[k] } else if d[k] < 0.0 { lo[k] / d[k] } else { f64::MAX }).fold(f64::MAX, f64::min);
        if d[2] >= 0.0 {
            return (wall * d[2] <= 3.0).then_some(wall);
        }
        let over = |t: f64, x: [f64; 2], y: [f64; 2]| (x[0]..=x[1]).contains(&(d[0] * t)) && (y[0]..=y[1]).contains(&(d[1] * t));
        let top = -1.0 / d[2];
        if top < wall && over(top, [-3.5, -2.5], [2.5, 3.5]) {
            return Some(top);
        }
        let dip = -1.52 / d[2];
        if dip < wall && over(dip, [1.25, 2.75], [1.25, 2.75]) {
            return Some(dip);
        }
        Some(wall.min(-1.5 / d[2]))
    }

    fn yard_scan(cols: usize) -> Vec<u8> {
        let step = std::f64::consts::TAU / cols as f64;
        tzf_bytes(((std::f64::consts::PI - 0.45) / step) as usize, cols, 0.4, 0.0, step, yard)
    }

    fn wall_def(id: &str, from: [f64; 2], to: [f64; 2], kind: &str) -> Wall {
        Wall { wall_id: id.into(), from, to, length_m: (to[0] - from[0]).hypot(to[1] - from[1]), z_m: [-1.5, 3.0], seen_by: vec![1], kind: kind.into(), flatness: 0.9, height_m: None }
    }

    /// The east wall's 10 cm points, as a registration merges them.
    fn east_wall_merged() -> crate::register::Merged {
        let mut m = crate::register::Merged::default();
        for iy in 0..=100 {
            for iz in 0..=43 {
                m.pts.push([5.0, -4.0 + iy as f32 * 0.1, -1.4 + iz as f32 * 0.1]);
                m.spacing_mm.push(10.0);
                m.station.push(1);
            }
        }
        m
    }

    /// A `register` output for the yard: station 1 (the frame) trusted,
    /// station 2 not placed; four walls of every kind; a merged cloud; and a
    /// mission asking whether the east wall is plumb.
    fn yard_survey(dir: &std::path::Path) -> (String, Vec<String>, String) {
        let reg = dir.join("reg");
        std::fs::create_dir_all(&reg).unwrap();
        let w = |p: &str, v: Value| std::fs::write(reg.join(p), serde_json::to_vec(&v).unwrap()).unwrap();
        w("poses.json", json!([{"turn_deg": 0.0, "xyz_m": [0.0, 0.0, 0.0]}, null]));
        w("audit.json", json!([{"verdict": "frame"}, {"verdict": "unplaced"}]));
        let walls = vec![
            wall_def("w1", [5.0, -4.0], [5.0, 6.0], "building"),
            wall_def("w2", [-6.0, 6.0], [5.0, 6.0], "building"),
            wall_def("w3", [-6.0, -4.0], [-6.0, 6.0], "low wall"),
            wall_def("w4", [-6.0, -4.0], [5.0, -4.0], "vegetation"),
        ];
        w("walls.json", json!(walls));
        std::fs::write(reg.join("merged.bin"), east_wall_merged().to_bytes()).unwrap();
        let f1 = dir.join("station_01.tzf");
        std::fs::write(&f1, yard_scan(1440)).unwrap();
        let reading = json!({"out_of_plumb_mm": 0.5, "lean_mm_per_m": 0.1, "flat_rms_mm": 0.4, "over_m": 4.2, "stations": [1]});
        let mission = json!({"mission_id": "m1", "project_id": "p", "goal": "Is the yard's east wall plumb?", "kind": "custom", "done_when": "answered",
            "state": "active", "created_by": "crew", "created_at": "2026-11-10T10:00:00Z", "targets": [
                {"target_id": "east", "name": "East wall", "face": {"from": [5.0, -4.0], "to": [5.0, 6.0]},
                 "question": {"kind": "plumb", "verdict": "answer", "answer": reading, "stations": [1]},
                 "tolerance": {"words": "1/4 inch in 10 feet", "mm_per_m": 2.0}},
                {"target_id": "north", "name": "North wall", "status": "declined", "note": "behind a hedge", "face": {"from": [-6.0, 6.0], "to": [5.0, 6.0]},
                 "question": {"kind": "plumb", "verdict": "does_not_fit", "reason": "rough"}},
                {"target_id": "gate", "name": "The gate"}]});
        let m = dir.join("mission.json");
        std::fs::write(&m, serde_json::to_vec(&mission).unwrap()).unwrap();
        let files = vec![f1.to_string_lossy().into_owned(), dir.join("station_02.tzf").to_string_lossy().into_owned()];
        (reg.to_string_lossy().into_owned(), files, m.to_string_lossy().into_owned())
    }

    #[test]
    fn a_surveyed_yard_gets_its_walls_ground_low_spot_sections_and_questions() {
        let dir = scratch("x9-analyze-yard");
        let (reg, files, mission) = yard_survey(&dir);
        let out = dir.join("out").to_string_lossy().into_owned();
        let mut args: Vec<String> = ["--straightedge", "2", "--tolerance-mm", "5", "--section", "0,0,6,0.5", "--section", "0,-5,0.5,7",
                                     "--section", "5.5,-3,5.5,5", "--questions", mission.as_str(), reg.as_str(), out.as_str()].iter().map(|s| s.to_string()).collect();
        args.extend(files);
        cli(&args).unwrap();
        for f in ["report.html", "analysis.json", "walls.csv", "low_spots.csv", "plan.dxf", "ground.f32", "ground.json", "ground.png", "flatness.png",
                  "low_spots.png", "wall-w1.png", "wall-w2.png", "section-1.png", "section-1.csv", "section-3.csv", "question-east.png"] {
            assert!(std::path::Path::new(&format!("{out}/{f}")).is_file(), "{f} missing");
        }
        let a: Value = serde_json::from_slice(&std::fs::read(format!("{out}/analysis.json")).unwrap()).unwrap();
        let s = &a["summary"];
        assert_eq!((s["stations_used"].as_u64(), s["frame_station"].as_u64(), s["walls"].as_u64()), (Some(1), Some(1), Some(2)), "{s}");
        assert!(s["ground_cells"].as_u64().unwrap() > 1000 && s["ground_flatness"]["rms_mm"].is_number(), "{s}");
        // The wall stands 4.5 m above the floor at its foot.
        let w1 = &a["walls"][0];
        assert_eq!(w1["wall_id"], "w1");
        assert!((w1["top_above_ground_m"].as_f64().unwrap() - 4.5).abs() < 0.1, "{w1}");
        // The dip holds water: about 2 m², 2 cm deep.
        let spots = a["low_spots"].as_array().unwrap();
        assert!(!spots.is_empty(), "{a}");
        let dip = &spots[0];
        assert!((dip["at"][0].as_f64().unwrap() - 2.0).abs() < 0.3 && (dip["at"][1].as_f64().unwrap() - 2.0).abs() < 0.3, "{dip}");
        assert!((dip["max_depth_mm"].as_f64().unwrap() - 20.0).abs() < 4.0, "{dip}");
        // Sections: the first crosses the east wall, the second the north one, the third runs along a wall.
        let crossed: Vec<Value> = a["sections"].as_array().unwrap().iter().map(|x| x["walls_crossed"].clone()).collect();
        assert_eq!(crossed, [json!(["w1"]), json!(["w2"]), json!([])]);
        // The east wall is plumb, judged again from the final registration; the north one wasn't seen.
        let q = a["questions"].as_array().unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0]["final"]["verdict"], "answer", "{}", q[0]);
        assert!(q[0]["final"]["answer"]["out_of_plumb_mm"].as_f64().unwrap().abs() < 2.0, "{}", q[0]);
        assert_eq!(q[1]["final"]["verdict"], "not_yet");
        assert_eq!(q[1]["declined"], "behind a hedge");
        // The parked box is not ground.
        let g = Ground::load(&out).unwrap();
        assert!(g.at([-3.0, 3.0]).is_none_or(|z| z < -1.3), "{:?}", g.at([-3.0, 3.0]));
        assert!((g.at([1.0, -1.0]).unwrap() + 1.5).abs() < 0.01);
        let html = std::fs::read_to_string(format!("{out}/report.html")).unwrap();
        assert!(html.contains("The questions") && html.contains("Sections") && html.contains("Highest wall top"), "report");
        let dxf = std::fs::read_to_string(format!("{out}/plan.dxf")).unwrap();
        for layer in ["WALLS", "LOW_WALLS", "VEGETATION", "STATIONS", "LOW_SPOTS"] {
            assert!(dxf.contains(&format!("\n8\n{layer}\n")), "{layer}");
        }
        assert!(dxf.ends_with("0\nENDSEC\n0\nEOF\n"));
        let csv = std::fs::read_to_string(format!("{out}/low_spots.csv")).unwrap();
        assert!(csv.lines().nth(1).is_some_and(|l| l.starts_with("L1,")), "{csv}");

        // A second visit with a heap tipped near the dip: cut and fill between the two.
        let after = dir.join("after");
        std::fs::create_dir_all(&after).unwrap();
        let mut b = g.clone();
        for y in 0..b.h {
            for x in 0..b.w {
                let p = [b.x0 + (x as f64 + 0.5) * b.cell, b.y0 + (y as f64 + 0.5) * b.cell];
                if (p[0] - 0.0).hypot(p[1] - 2.0) < 0.5 && b.z[y * b.w + x].is_finite() {
                    b.z[y * b.w + x] += 0.3;
                }
            }
        }
        std::fs::write(after.join("ground.f32"), b.to_bytes()).unwrap();
        std::fs::write(after.join("ground.json"), serde_json::to_vec(&b.header()).unwrap()).unwrap();
        let change = dir.join("change").to_string_lossy().into_owned();
        cli(&["--compare".into(), "--min-change-m".into(), "0.05".into(), out.clone(), after.to_string_lossy().into_owned(), change.clone()]).unwrap();
        let c: Value = serde_json::from_slice(&std::fs::read(format!("{change}/change.json")).unwrap()).unwrap();
        assert!((c["fill_m3"].as_f64().unwrap() - 0.3 * std::f64::consts::PI * 0.25).abs() < 0.03, "{c}");
        assert_eq!(c["cut_m3"], 0.0);
        assert!(std::path::Path::new(&format!("{change}/change.png")).is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_command_line_says_how_to_use_it() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        cli(&a(&["--help"])).unwrap();
        let bads: [&[&str]; 5] = [&[], &["reg"], &["reg", "out"], &["--compare", "a", "b"], &["--straightedge"]];
        for bad in bads {
            assert!(cli(&a(bad)).unwrap_err().to_string().contains("usage: x9-bridge analyze"), "{bad:?}");
        }
        assert!(cli(&a(&["--section", "1,2,3", "r", "o", "f"])).unwrap_err().to_string().contains("--section x1,y1,x2,y2"));
        assert!(cli(&a(&["--tolerance-mm", "fine", "r", "o", "f"])).is_err());
        assert!(cli(&a(&["--section", "1,2,x,4", "r", "o", "f"])).is_err());
        // A registration folder that isn't one, or files that don't match it.
        let dir = scratch("x9-analyze-cli");
        let reg = dir.to_string_lossy().into_owned();
        let (reg, out, cmp) = (reg.as_str(), format!("{reg}/out"), format!("{reg}/c"));
        let (out, cmp) = (out.as_str(), cmp.as_str());
        assert!(format!("{:#}", cli(&a(&[reg, out, "a.tzf"])).unwrap_err()).contains("poses.json"));
        std::fs::write(dir.join("poses.json"), "[null, null]").unwrap();
        std::fs::write(dir.join("audit.json"), "[]").unwrap();
        std::fs::write(dir.join("walls.json"), "[]").unwrap();
        assert!(cli(&a(&[reg, out, "a.tzf"])).unwrap_err().to_string().contains("2 poses but 1 station files"));
        // Nothing trusted: an empty report, no ground.
        cli(&a(&[reg, out, "a.tzf", "b.tzf"])).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(dir.join("out/analysis.json")).unwrap()).unwrap();
        assert_eq!((v["summary"]["stations_used"].as_u64(), v["summary"]["ground_cells"].clone()), (Some(0), Value::Null));
        // Comparing needs both height grids, whole.
        assert!(format!("{:#}", cli(&a(&["--compare", reg, reg, cmp])).unwrap_err()).contains("ground.json"));
        std::fs::write(dir.join("ground.json"), r#"{"x0": 0, "y0": 0, "cell": 0.1, "w": 3, "h": 2}"#).unwrap();
        std::fs::write(dir.join("ground.f32"), [0u8; 20]).unwrap();
        assert!(cli(&a(&["--compare", reg, reg, cmp])).unwrap_err().to_string().contains("is not 3×2"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_station_that_saw_none_of_the_walls_is_not_read() {
        let w = wall_def("w1", [5.0, -4.0], [5.0, 6.0], "building");
        let pose = Pose { turn_deg: 0.0, xyz_m: [0.0, 0.0, 0.0] };
        let fits = measure_faces_with(&[w], &[(2, pose, "/nonexistent/station_02.tzf".into())], &Opts::default(), 1).unwrap();
        assert_eq!(fits.len(), 1);
        assert!(fits[0].result.lean_mm_per_m.is_none() && fits[0].result.covered_pct == 0.0);
        assert!(!fits[0].png(&Opts::default()).is_empty());
        // One that did see it, but whose file isn't a scan, is an error.
        let w = wall_def("w1", [5.0, -4.0], [5.0, 6.0], "building");
        assert!(measure_faces_with(&[w], &[(1, pose, "/nonexistent/station_01.tzf".into())], &Opts::default(), 1).is_err());
    }

    #[test]
    fn small_helpers_say_what_they_mean() {
        assert_eq!(said("does_not_fit"), "does not fit");
        assert_eq!(esc("<a & b>"), "&lt;a &amp; b&gt;");
        assert_eq!((opt(Some(1.5), " m"), opt(None, " m")), ("1.5 m".to_string(), "–".to_string()));
        assert_eq!(percentile(&mut Vec::<f64>::new(), 0.5), None);
        assert_eq!(percentile(&mut [3.0, 1.0, 2.0], 0.5), Some(2.0));
        assert_eq!(diverging(-5.0), [37, 99, 235]);
        assert_eq!(diverging(5.0), [220, 38, 38]);
        assert_eq!(terrain(-1.0), [40, 80, 140]);
        assert_eq!(terrain(2.0), [240, 235, 225]);
        assert_eq!((nice_step(0.05), nice_step(3.0), nice_step(500.0)), (0.005, 0.2, 5.0));
        assert_eq!((r1(1.26), r2(1.256)), (1.3, 1.26));
        assert!(fit_line(&[(0.0, 0.0), (0.0, 1.0), (0.0, 2.0)]).is_none(), "a vertical line has no slope");
        assert!(fit_plane(&[(1.0, 1.0, 0.0); 12]).is_none(), "points on one spot fit no plane");
        assert!(straightedge(&[], 3.0, 0.05).is_empty());
        // Too few returns, or one stray far below the rest.
        let mut l = Lowest::new();
        l.add(0.0);
        assert_eq!(l.height(), None);
        for z in [0.0, 0.01, 0.02, 0.0, 0.01, 0.0, 0.01, 0.02, 0.01, -0.5] {
            l.add(z);
        }
        assert!(l.height().unwrap().abs() < 0.02);
        assert!(Ground::from_cells(&HashMap::new(), 0.1).is_none());
        // Walls from the ground's foot: a ground with nothing ahead of a wall gives no height.
        let g = flat_ground(10, 10, 0.0);
        assert_eq!(g.known(), 100);
        assert!(g.at([-1.0, 0.0]).is_none() && g.at([0.5, 0.5]) == Some(0.0));
        assert!(ground_plane(&g).is_some());
        let sloped = Ground { z: (0..100).map(|i| (i % 10) as f32 * 0.01).collect(), ..g.clone() };
        let p = ground_plane(&sloped).unwrap();
        assert!((p[1] - 0.1).abs() < 1e-6, "{p:?}");
        assert!(ground_flatness(&Ground { z: vec![f32::NAN; 100], ..g.clone() }, &[f32::NAN; 100], None, &Opts::default()).is_null());
    }

    #[test]
    fn a_wall_seen_from_its_far_side_is_drawn_mirrored_and_titled_by_its_verdict() {
        // Station on the wall's other side: its normal flips, and the face is drawn from there.
        let w = wall_def("w9", [0.0, 0.0], [6.0, 0.0], "building");
        let mut g = WallGrid::new(&w, &[[3.0, -4.0]]);
        assert!(g.nrm[1] < 0.0);
        for iu in 0..300 {
            for iz in 0..300 {
                let (u, z) = (iu as f64 * 0.02, -1.5 + iz as f64 * 0.015);
                g.add([u, -(0.002 * (z + 1.5)), z]);
            }
        }
        g.add([100.0, 0.0, 0.0]);
        g.add([3.0, 2.0, 0.0]);
        g.add([3.0, 0.0, -9.0]);
        let ground = flat_ground(80, 80, -1.5);
        let ground = Ground { x0: -1.0, y0: -6.0, ..ground };
        let (r, plane) = measure_wall(&g, Some(&ground), &Opts::default(), "wall-w9.png");
        assert!(plane.is_some() && r.covered_pct > 90.0, "{r:?}");
        assert!((r.lean_mm_per_m.unwrap() - 2.0).abs() < 0.2, "{r:?}");
        assert!((r.top_above_ground_m.unwrap() - 4.5).abs() < 0.1, "{r:?}");
        assert!(r.straightedge_windows > 0 && r.worst_strip_at_m.is_some());
        let mut rs = vec![r.clone()];
        gate_walls(&mut rs);
        assert_eq!(rs[0].verdict.as_deref(), Some("answer"));
        assert!(wall_title(&rs[0]).contains("flatness RMS"));
        let png = wall_png(&g, plane, &rs[0], &Opts::default());
        assert_eq!(&png[1..4], b"PNG");
        withhold(&mut rs[0], "not_yet", Some("settling".into()));
        assert!(wall_title(&rs[0]).ends_with("plumb and flatness: not yet"));
        assert!(!wall_png(&g, None, &rs[0], &Opts::default()).is_empty());
    }
}
