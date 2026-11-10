//! Measurements. v1 measures one thing: a window's finished opening, the hole
//! in the wall face, from one levelled close-up scan.
//!
//! Method (port of a Python prototype):
//! 1. Drop points within 0.3 m of the scanner.
//! 2. The wall's direction is the largest vertical plane (RANSAC on a fixed,
//!    evenly spaced subsample). In a tight close-up that plane is often the
//!    glass, so the wall face is the frontmost parallel plane with real
//!    support; glass and frame sit behind it, and a plant in front is not
//!    planar.
//! 3. Lay the wall-face points flat on a 5 mm grid, close small gaps, and take
//!    the largest empty region as the opening.
//! 4. Measure from the opening's middle outward to the first wall cell along
//!    the middle 60 % of rows (width) and columns (height); report medians.

use serde_json::{json, Value};

pub const CELL: f64 = 0.005;
const WALL_TOL: f64 = 0.010;
const FACE_TOL: f64 = 0.008;
const NEAR: f64 = 0.3;
const CLEAN_TOL: f64 = 0.020;
/// Wall wanted beyond each side of the opening, and how often it must be seen.
pub const MARGIN_WANT: f64 = 0.05;
const SIDE_SEEN_WANT: f64 = 0.8;
/// Width of the wall ring around the opening checked for occlusion.
const RING: f64 = 0.10;

/// Wall beyond one side of the opening.
#[derive(Debug, Clone)]
pub struct Side {
    pub name: &'static str,
    /// Share of the measuring rows (columns) with wall on this side.
    pub seen: f64,
    /// Median wall beyond the opening's edge on this side, in metres.
    pub margin_m: f64,
}

impl Side {
    pub fn ok(&self) -> bool {
        self.seen >= SIDE_SEEN_WANT && self.margin_m >= MARGIN_WANT
    }
}
const RANSAC_ITERS: usize = 3000;
const RANSAC_SAMPLE: usize = 60_000;

/// The measured opening and what the evidence image needs.
#[derive(Debug, Clone)]
pub struct Opening {
    pub width_m: f64,
    pub height_m: f64,
    /// Rows (columns) whose width (height) is within 20 mm of the median;
    /// the rest are taken as obstructed (a plant, a blind cord).
    pub width_clean: usize,
    pub height_clean: usize,
    /// Standard deviation of the clean rows (columns).
    pub width_sd_m: f64,
    pub height_sd_m: f64,
    pub width_spread_m: f64,
    pub height_spread_m: f64,
    pub rows_used: usize,
    pub rows_tried: usize,
    pub cols_used: usize,
    pub cols_tried: usize,
    pub wall_rms_m: f64,
    pub wall_points: usize,
    pub points: usize,
    pub distance_m: f64,
    pub normal_xy: [f64; 2],
    /// Wall-face raster (row 0 = lowest), `w` × `h` cells: 0 empty, 1 wall, 2 opening.
    pub raster: Vec<u8>,
    pub w: usize,
    pub h: usize,
    /// The measuring row and column through the opening's middle, and the
    /// runs found on them (start, end) in cells.
    pub mid: (usize, usize),
    /// Left, right, bottom, top.
    pub sides: [Side; 4],
    /// Share of the 10 cm wall ring around the opening with no wall points:
    /// something in front of the wall, which a rescan from here can't fix.
    pub ring_empty: f64,
    pub row_run: Option<(usize, usize)>,
    pub col_run: Option<(usize, usize)>,
}

/// Deterministic generator (xorshift64*), so reruns give the same numbers.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Least-squares line through 2-D points: unit normal and offset (n·p = d),
/// oriented away from the origin (the scanner).
fn fit_line(pts: &[[f64; 2]]) -> Option<([f64; 2], f64)> {
    if pts.len() < 3 {
        return None;
    }
    let n = pts.len() as f64;
    let (mx, my) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + p[0], a.1 + p[1]));
    let (mx, my) = (mx / n, my / n);
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for p in pts {
        let (dx, dy) = (p[0] - mx, p[1] - my);
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    // Direction of least spread = eigenvector of the smaller eigenvalue.
    let tr = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let lmin = tr / 2.0 - ((tr * tr / 4.0) - det).max(0.0).sqrt();
    let (mut nx, mut ny) = if sxy.abs() > 1e-12 { (sxy, lmin - sxx) } else if sxx < syy { (1.0, 0.0) } else { (0.0, 1.0) };
    let len = (nx * nx + ny * ny).sqrt();
    nx /= len;
    ny /= len;
    let mut d = nx * mx + ny * my;
    if d < 0.0 {
        nx = -nx;
        ny = -ny;
        d = -d;
    }
    Some(([nx, ny], d))
}

fn median(v: &mut [f64]) -> f64 {
    percentile(v, 50.0)
}

/// Linear-interpolated percentile (numpy's default).
fn percentile(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let pos = (v.len() - 1) as f64 * q / 100.0;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    v[lo] + (v[hi] - v[lo]) * (pos - lo as f64)
}

/// Measure the opening. `pts` are levelled scanner-frame points in metres.
pub fn opening(all: &[[f64; 3]]) -> Result<Opening, String> {
    let p: Vec<[f64; 3]> = all.iter().copied().filter(|q| (q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt() > NEAR).collect();
    if p.len() < 10_000 {
        return Err(format!("only {} points beyond {NEAR} m; is this a close-up of a wall?", p.len()));
    }
    let xy: Vec<[f64; 2]> = p.iter().map(|q| [q[0], q[1]]).collect();

    // RANSAC for the dominant vertical plane on an evenly spaced subsample.
    let step = (xy.len() / RANSAC_SAMPLE).max(1);
    let sample: Vec<[f64; 2]> = xy.iter().step_by(step).copied().collect();
    let mut rng = Rng(7);
    let mut best = (0usize, [0.0; 2], 0.0);
    for _ in 0..RANSAC_ITERS {
        let (a, b) = (sample[rng.below(sample.len())], sample[rng.below(sample.len())]);
        let t = [b[0] - a[0], b[1] - a[1]];
        let len = (t[0] * t[0] + t[1] * t[1]).sqrt();
        if len < 0.05 {
            continue;
        }
        let n = [-t[1] / len, t[0] / len];
        let d = n[0] * a[0] + n[1] * a[1];
        let cnt = sample.iter().filter(|q| (n[0] * q[0] + n[1] * q[1] - d).abs() < WALL_TOL).count();
        if cnt > best.0 {
            best = (cnt, n, d);
        }
    }
    if best.0 == 0 {
        return Err("no vertical plane found".into());
    }
    let inl: Vec<[f64; 2]> = xy.iter().copied().filter(|q| (best.1[0] * q[0] + best.1[1] * q[1] - best.2).abs() < WALL_TOL).collect();
    let (mut n, mut d) = fit_line(&inl).ok_or("plane fit failed")?;

    // Frontmost parallel plane with real support = the wall face.
    let s: Vec<f64> = xy.iter().map(|q| n[0] * q[0] + n[1] * q[1]).collect();
    let (smin, smax) = s.iter().fold((f64::MAX, f64::MIN), |a, v| (a.0.min(*v), a.1.max(*v)));
    let nb = (((smax - smin) / CELL).ceil() as usize).max(1);
    let mut hist = vec![0usize; nb + 1];
    for v in &s {
        hist[((v - smin) / CELL) as usize] += 1;
    }
    let need = 2000usize.max(p.len() * 3 / 100);
    // A bin past either end counts as empty, so a wall whose points all fall in
    // the first bin (a clean close-up) is still found.
    let peak = |i: usize| hist[i] >= need && (i == 0 || hist[i] >= hist[i - 1]) && (i == nb || hist[i] >= hist[i + 1]);
    if let Some(i) = (0..=nb).find(|&i| peak(i)) {
        let d0 = smin + (i as f64 + 0.5) * CELL;
        let sel: Vec<[f64; 2]> = xy.iter().zip(&s).filter(|(_, v)| (*v - d0).abs() < WALL_TOL).map(|(q, _)| *q).collect();
        if let Some((n2, d2)) = fit_line(&sel) {
            // Keep the original orientation.
            let flip = n2[0] * n[0] + n2[1] * n[1] < 0.0;
            n = if flip { [-n2[0], -n2[1]] } else { n2 };
            d = if flip { -d2 } else { d2 };
        }
    }

    // Wall-face points, laid out flat: u along the wall to the right as seen
    // from the scanner (n points away from it, so right = n × up), z up.
    let t = [n[1], -n[0]];
    let mut face = Vec::new();
    let mut sq = 0.0;
    for q in &p {
        let dist = n[0] * q[0] + n[1] * q[1] - d;
        if dist.abs() < FACE_TOL {
            face.push((t[0] * q[0] + t[1] * q[1], q[2]));
            sq += dist * dist;
        }
    }
    if face.len() < 1000 {
        return Err("too few points on the wall face".into());
    }
    let rms = (sq / face.len() as f64).sqrt();
    let (u0, u1, z0, z1) = face.iter().fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |a, f| (a.0.min(f.0), a.1.max(f.0), a.2.min(f.1), a.3.max(f.1)));
    let w = ((u1 - u0) / CELL).ceil() as usize + 1;
    let h = ((z1 - z0) / CELL).ceil() as usize + 1;
    if w * h > 4_000_000 {
        return Err(format!("wall face is {:.1} × {:.1} m; aim a tighter close-up", w as f64 * CELL, h as f64 * CELL));
    }
    let mut grid = vec![0u8; w * h];
    for f in &face {
        grid[((f.1 - z0) / CELL) as usize * w + ((f.0 - u0) / CELL) as usize] = 1;
    }
    let wall = close(&grid, w, h, 2);

    // Largest empty region (4-connected) = the opening.
    let mut label = vec![0u32; w * h];
    let mut best_lab = (0u32, 0usize);
    let mut next = 0u32;
    let mut stack = Vec::new();
    for start in 0..w * h {
        if wall[start] != 0 || label[start] != 0 {
            continue;
        }
        next += 1;
        let mut area = 0usize;
        label[start] = next;
        stack.push(start);
        while let Some(c) = stack.pop() {
            area += 1;
            let (x, y) = (c % w, c / w);
            let mut go = |nc: usize| {
                if wall[nc] == 0 && label[nc] == 0 {
                    label[nc] = next;
                    stack.push(nc);
                }
            };
            if x > 0 { go(c - 1); }
            if x + 1 < w { go(c + 1); }
            if y > 0 { go(c - w); }
            if y + 1 < h { go(c + w); }
        }
        if area > best_lab.1 {
            best_lab = (next, area);
        }
    }
    if best_lab.1 == 0 {
        return Err("no opening in the wall face".into());
    }
    let lab = best_lab.0;
    let (mut ys, mut xs) = (Vec::with_capacity(best_lab.1), Vec::with_capacity(best_lab.1));
    for (c, l) in label.iter().enumerate() {
        if *l == lab {
            ys.push((c / w) as f64);
            xs.push((c % w) as f64);
        }
    }
    let (rmin, rmax) = (ys.iter().cloned().fold(f64::MAX, f64::min) as usize, ys.iter().cloned().fold(f64::MIN, f64::max) as usize);
    let (cmin, cmax) = (xs.iter().cloned().fold(f64::MAX, f64::min) as usize, xs.iter().cloned().fold(f64::MIN, f64::max) as usize);
    let cy = median(&mut ys) as usize;
    let cx = median(&mut xs) as usize;

    let band = |lo: usize, hi: usize| {
        let k = (hi - lo) as f64;
        let a = lo + (k * 0.2) as usize;
        let b = lo + ((k * 0.8) as usize).max((k * 0.2) as usize + 1);
        a..b
    };
    // From `c` outward to the first wall cell on each side: (start, end) of the run.
    let run = |line: &dyn Fn(usize) -> u8, len: usize, c: usize| -> Option<(usize, usize)> {
        if line(c) != 0 {
            return None;
        }
        let left = (0..c).rev().find(|&i| line(i) != 0)?;
        let right = (c..len).find(|&i| line(i) != 0)?;
        Some((left + 1, right))
    };
    let (mut widths, mut heights) = (Vec::new(), Vec::new());
    let rows = band(rmin, rmax);
    let cols = band(cmin, cmax);
    let (rows_tried, cols_tried) = (rows.len(), cols.len());
    for r in rows {
        if let Some((a, b)) = run(&|i| wall[r * w + i], w, cx) {
            widths.push((b - a) as f64 * CELL);
        }
    }
    for c in cols {
        if let Some((a, b)) = run(&|i| wall[i * w + c], h, cy) {
            heights.push((b - a) as f64 * CELL);
        }
    }
    if widths.is_empty() || heights.is_empty() {
        return Err("the opening runs off the scan: aim the close-up with wall on every side".into());
    }
    let clean = |v: &mut Vec<f64>| -> (usize, f64) {
        let m = median(v);
        let c: Vec<f64> = v.iter().copied().filter(|x| (x - m).abs() <= CLEAN_TOL).collect();
        let mean = c.iter().sum::<f64>() / c.len() as f64;
        let sd = (c.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / c.len() as f64).sqrt();
        (c.len(), sd)
    };
    let (width_clean, width_sd_m) = clean(&mut widths);
    let (height_clean, height_sd_m) = clean(&mut heights);
    // Wall beyond each side, along the same measuring rows and columns.
    let side = |name: &'static str, lines: std::ops::Range<usize>, edge: &dyn Fn(usize) -> Option<usize>| -> Side {
        let n = lines.len().max(1);
        let mut m: Vec<f64> = lines.filter_map(edge).map(|c| c as f64 * CELL).collect();
        Side { name, seen: m.len() as f64 / n as f64, margin_m: if m.is_empty() { 0.0 } else { median(&mut m) } }
    };
    // Margin = wall from the raster's edge up to the opening, on lines whose
    // middle cell is in the opening.
    let open_r = |r: usize| wall[r * w + cx] == 0;
    let open_c = |c: usize| wall[cy * w + c] == 0;
    let left = side("left", band(rmin, rmax), &|r| if open_r(r) { (0..cx).rev().find(|&i| wall[r * w + i] != 0).map(|i| i + 1) } else { None });
    let right = side("right", band(rmin, rmax), &|r| if open_r(r) { (cx..w).find(|&i| wall[r * w + i] != 0).map(|b| w - b) } else { None });
    let bottom = side("bottom", band(cmin, cmax), &|c| if open_c(c) { (0..cy).rev().find(|&i| wall[i * w + c] != 0).map(|i| i + 1) } else { None });
    let top = side("top", band(cmin, cmax), &|c| if open_c(c) { (cy..h).find(|&i| wall[i * w + c] != 0).map(|b| h - b) } else { None });
    // Occlusion: empty cells in a ring just outside the opening's box.
    let ring = (RING / CELL) as usize;
    let (x0, x1) = (cmin.saturating_sub(ring), (cmax + ring).min(w - 1));
    let (y0r, y1r) = (rmin.saturating_sub(ring), (rmax + ring).min(h - 1));
    let (mut ring_cells, mut ring_empty_cells) = (0usize, 0usize);
    for y in y0r..=y1r {
        for x in x0..=x1 {
            if (cmin..=cmax).contains(&x) && (rmin..=rmax).contains(&y) {
                continue;
            }
            ring_cells += 1;
            if wall[y * w + x] == 0 && label[y * w + x] != lab {
                ring_empty_cells += 1;
            }
        }
    }
    let ring_empty = if ring_cells == 0 { 0.0 } else { ring_empty_cells as f64 / ring_cells as f64 };
    let row_run = run(&|i| wall[cy * w + i], w, cx);
    let col_run = run(&|i| wall[i * w + cx], h, cy);
    let mut raster: Vec<u8> = wall.iter().map(|v| (*v != 0) as u8).collect();
    for (c, l) in label.iter().enumerate() {
        if *l == lab {
            raster[c] = 2;
        }
    }
    Ok(Opening {
        width_m: median(&mut widths),
        height_m: median(&mut heights),
        width_clean,
        height_clean,
        width_sd_m,
        height_sd_m,
        width_spread_m: percentile(&mut widths, 90.0) - percentile(&mut widths, 10.0),
        height_spread_m: percentile(&mut heights, 90.0) - percentile(&mut heights, 10.0),
        rows_used: widths.len(),
        rows_tried,
        cols_used: heights.len(),
        cols_tried,
        wall_rms_m: rms,
        wall_points: face.len(),
        points: p.len(),
        distance_m: d.abs(),
        normal_xy: n,
        raster,
        w,
        h,
        mid: (cy, cx),
        sides: [left, right, bottom, top],
        ring_empty,
        row_run,
        col_run,
    })
}

/// Morphological closing (dilate then erode) with a square of radius `r`;
/// cells outside the grid don't erode the wall (as OpenCV's default).
fn close(g: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let pass = |src: &[u8], dilate: bool| -> Vec<u8> {
        let mut tmp = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let (a, b) = (x.saturating_sub(r), (x + r).min(w - 1));
                let row = &src[y * w + a..=y * w + b];
                tmp[y * w + x] = if dilate { row.iter().any(|v| *v != 0) as u8 } else { row.iter().all(|v| *v != 0) as u8 };
            }
        }
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let (a, b) = (y.saturating_sub(r), (y + r).min(h - 1));
                let mut col = (a..=b).map(|yy| tmp[yy * w + x]);
                out[y * w + x] = if dilate { col.any(|v| v != 0) as u8 } else { col.all(|v| v != 0) as u8 };
            }
        }
        out
    };
    pass(&pass(g, true), false)
}

fn check(name: &str, ok: bool, value: f64, unit: &str, detail: String) -> Value {
    json!({"name": name, "ok": ok, "value": value, "unit": unit, "detail": detail})
}

/// The `Measurement` fields other than ids, and the evidence PNG.
pub fn report(o: &Opening) -> (Value, Vec<u8>) {
    let mm = |m: f64| (m * 1000.0).round();
    // One grid cell, or the spread of the unobstructed rows (columns) if larger.
    let unc_w = o.width_sd_m.max(CELL);
    let unc_h = o.height_sd_m.max(CELL);
    let basis = "hole in the wall face, median over the middle 60 % of rows (width) or columns (height) on a 5 mm grid";
    let values = json!([
        {"name": "opening_width", "value_m": (o.width_m * 1000.0).round() / 1000.0, "uncertainty_m": (unc_w * 1000.0).round() / 1000.0, "basis": basis},
        {"name": "opening_height", "value_m": (o.height_m * 1000.0).round() / 1000.0, "uncertainty_m": (unc_h * 1000.0).round() / 1000.0, "basis": basis},
    ]);
    let covered = |used: usize, tried: usize| if tried == 0 { 0.0 } else { used as f64 / tried as f64 };
    let checks = json!([
        check("wall_fit", o.wall_rms_m <= 0.003, mm(o.wall_rms_m * 10.0) / 10.0, "mm",
              format!("wall face fitted at {:.1} mm RMS from {} points (want ≤ 3 mm)", o.wall_rms_m * 1000.0, o.wall_points)),
        check("opening_bounded", covered(o.rows_used, o.rows_tried) >= 0.6 && covered(o.cols_used, o.cols_tried) >= 0.6,
              (covered(o.rows_used, o.rows_tried).min(covered(o.cols_used, o.cols_tried)) * 100.0).round(), "%",
              format!("wall found on both sides in {} of {} rows and {} of {} columns (want ≥ 60 %)", o.rows_used, o.rows_tried, o.cols_used, o.cols_tried)),
        check("margins", o.sides.iter().all(Side::ok),
              (o.sides.iter().map(|s| s.margin_m).fold(f64::MAX, f64::min) * 1000.0).round(), "mm",
              if o.sides.iter().all(Side::ok) {
                  format!("at least {} mm of wall beyond the opening on every side", mm(MARGIN_WANT))
              } else {
                  format!("not enough wall beyond the opening at the {} (want {} mm, seen in ≥ 80 % of lines); a wider close-up fixes this",
                          o.sides.iter().filter(|s| !s.ok()).map(|s| s.name).collect::<Vec<_>>().join(", "), mm(MARGIN_WANT))
              }),
        check("unoccluded", o.ring_empty <= 0.25, (o.ring_empty * 100.0).round(), "%",
              format!("{}% of the 10 cm wall ring around the opening has no wall points (want ≤ 25 %); if high, something stands in front of the wall, and only moving the scanner helps", (o.ring_empty * 100.0).round())),
        check("consistent", covered(o.width_clean, o.rows_used) >= 0.3 && covered(o.height_clean, o.cols_used) >= 0.3,
              (covered(o.width_clean, o.rows_used).min(covered(o.height_clean, o.cols_used)) * 100.0).round(), "%",
              format!("{} of {} rows and {} of {} columns agree with the median to 20 mm; the rest look obstructed (want ≥ 30 %)",
                      o.width_clean, o.rows_used, o.height_clean, o.cols_used)),
    ]);
    let v = json!({
        "kind": "opening",
        "values": values,
        "checks": checks,
        "opening_width_mm": mm(o.width_m) as i64,
        "opening_height_mm": mm(o.height_m) as i64,
        "uncertainty_mm": mm(unc_w.max(unc_h)) as i64,
        "details": {
            "distance_m": (o.distance_m * 1000.0).round() / 1000.0,
            "wall_rms_mm": mm(o.wall_rms_m * 10.0) / 10.0,
            "wall_points": o.wall_points,
            "points": o.points,
            "width_spread_mm": mm(o.width_spread_m),
            "rows_clean": o.width_clean,
            "columns_clean": o.height_clean,
            "height_spread_mm": mm(o.height_spread_m),
            "rows_used": o.rows_used,
            "columns_used": o.cols_used,
            "cell_mm": CELL * 1000.0,
            "sides": o.sides.iter().map(|s| (s.name.to_string(), json!({"seen_pct": (s.seen * 100.0).round(), "margin_mm": mm(s.margin_m), "ok": s.ok()}))).collect::<serde_json::Map<_, _>>(),
            "ring_empty_pct": (o.ring_empty * 100.0).round(),
            "wall_normal_xy": [(o.normal_xy[0] * 1e4).round() / 1e4, (o.normal_xy[1] * 1e4).round() / 1e4],
        },
    });
    (v, evidence_png(o))
}

/// The wall-face raster (wall grey, opening green, the measuring row and
/// column red), upside up. No room image: only the wall plane.
fn evidence_png(o: &Opening) -> Vec<u8> {
    let (w, h) = (o.w, o.h);
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let c = match o.raster[y * w + x] {
                1 => [200, 200, 200],
                2 => [75, 180, 60],
                _ => [24, 24, 24],
            };
            let i = ((h - 1 - y) * w + x) * 3;
            rgb[i..i + 3].copy_from_slice(&c);
        }
    }
    let mut red = |x: usize, y: usize| {
        if x < w && y < h {
            let i = ((h - 1 - y) * w + x) * 3;
            rgb[i..i + 3].copy_from_slice(&[230, 40, 40]);
        }
    };
    let (cy, cx) = o.mid;
    if let Some((a, b)) = o.row_run {
        for x in a..b {
            red(x, cy);
            red(x, cy + 1);
        }
    }
    if let Some((a, b)) = o.col_run {
        for y in a..b {
            red(cx, y);
            red(cx + 1, y);
        }
    }
    png_rgb(w, h, &rgb)
}

/// A minimal PNG writer (8-bit RGB, zlib via miniz_oxide).
pub fn png_rgb(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * w * 3..(y + 1) * w * 3]);
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = crc32fast::Hasher::new();
        c.update(kind);
        c.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&c.finalize().to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wall 2.0 × 1.8 m at 1.2 m from the scanner, with a 1.2 × 1.1 m opening,
    /// glass 12 cm behind it, and a "plant" (scattered points) in front.
    fn synthetic() -> Vec<[f64; 3]> {
        let n = [0.6, -0.8];
        let t = [-n[1], n[0]];
        let d = 1.2;
        let mut pts = Vec::new();
        let mut s = 1u64;
        let mut rnd = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as f64 / (1u64 << 31) as f64) - 0.5
        };
        let at = |u: f64, z: f64, off: f64| [n[0] * (d + off) + t[0] * u, n[1] * (d + off) + t[1] * u, z];
        let mut u: f64 = -1.0;
        while u < 1.0 {
            let mut z: f64 = -0.9;
            while z < 0.9 {
                let inside = u.abs() < 0.6 && z > -0.5 && z < 0.6;
                let off = rnd() * 0.004;
                pts.push(if inside { at(u, z, 0.12 + off) } else { at(u, z, off) });
                z += 0.004;
            }
            u += 0.004;
        }
        for _ in 0..3000 {
            pts.push(at(-0.4 + rnd() * 0.3, -0.2 + rnd() * 0.5, -0.3 + rnd() * 0.2));
        }
        pts
    }

    #[test]
    fn measures_a_synthetic_opening() {
        let o = opening(&synthetic()).unwrap();
        assert!((o.width_m - 1.2).abs() <= 0.011, "width {}", o.width_m);
        assert!((o.height_m - 1.1).abs() <= 0.011, "height {}", o.height_m);
        assert!((o.distance_m - 1.2).abs() < 0.01, "picked the wall face, not the glass: {}", o.distance_m);
        let (v, png) = report(&o);
        assert_eq!(v["values"][0]["name"], "opening_width");
        assert!(v["checks"].as_array().unwrap().iter().all(|c| c["ok"] == true), "{}", v["checks"]);
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn margins_fail_on_the_side_where_the_wall_stops() {
        // The synthetic wall faces away along n = (0.6, -0.8); seen from the
        // scanner, right is n × up = (-0.8, -0.6). Crop it 1 cm beyond the
        // opening on the scanner's right.
        let right = [-0.8, -0.6];
        let pts: Vec<[f64; 3]> = synthetic().into_iter().filter(|q| q[0] * right[0] + q[1] * right[1] < 0.61).collect();
        let o = opening(&pts).unwrap();
        let r = o.sides.iter().find(|s| s.name == "right").unwrap();
        assert!(!r.ok(), "right margin {} m, seen {}", r.margin_m, r.seen);
        assert!(o.sides.iter().filter(|s| s.name != "right").all(Side::ok));
        let (v, _) = report(&o);
        let margins = v["checks"].as_array().unwrap().iter().find(|c| c["name"] == "margins").unwrap();
        assert_eq!(margins["ok"], false);
        assert!(margins["detail"].as_str().unwrap().contains("right"));
    }

    #[test]
    fn same_numbers_every_time() {
        let pts = synthetic();
        let (a, b) = (opening(&pts).unwrap(), opening(&pts).unwrap());
        assert_eq!((a.width_m, a.height_m), (b.width_m, b.height_m));
    }

    #[test]
    fn refuses_a_scan_with_no_wall() {
        assert!(opening(&[[0.1, 0.0, 0.0]; 50]).is_err());
    }

    /// Compare with the Python tool on a real export: MEASURE_PLY=/path/scan.ply
    /// cargo test -p x9-bridge real_ply -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_ply() {
        let path = std::env::var("MEASURE_PLY").expect("MEASURE_PLY");
        let bytes = std::fs::read(&path).unwrap();
        let end = bytes.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
        let head = String::from_utf8_lossy(&bytes[..end]).to_string();
        let n: usize = head.lines().find(|l| l.starts_with("element vertex")).unwrap().split(' ').nth(2).unwrap().parse().unwrap();
        let stride = (bytes.len() - end) / n;
        let pts: Vec<[f64; 3]> = (0..n).map(|i| {
            let o = end + i * stride;
            let f = |k: usize| f32::from_le_bytes(bytes[o + 4 * k..o + 4 * k + 4].try_into().unwrap()) as f64;
            [f(0), f(1), f(2)]
        }).collect();
        let t = std::time::Instant::now();
        let o = opening(&pts).unwrap();
        let (v, _) = report(&o);
        println!("{path}: {} x {} mm (±{} mm), {:?}, {:.2} s", v["opening_width_mm"], v["opening_height_mm"], v["uncertainty_mm"], v["details"], t.elapsed().as_secs_f64());
    }

    #[test]
    fn percentile_matches_numpy() {
        let mut v = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&mut v, 50.0), 2.5);
        assert_eq!(percentile(&mut v, 25.0), 1.75);
    }
}
