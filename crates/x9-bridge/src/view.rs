//! What the browser viewer and the agent see of a scan: a thinned point set
//! for drawing, and its coverage. Coverage is the point spacing the scan
//! reached (range × angular step, at full resolution) and a plan-view map of
//! what the scanner saw and where it was blocked: shadows are floor cells
//! nearer than the farthest return in their direction that got no points at
//! any height, i.e. something stood in the way. Shadows are assessed only on
//! scans that swept the whole horizon (full domes): an area scan's narrow
//! window leaves most of the room unlooked-at, which is not a shadow.

use serde_json::{json, Value};
use trimble_tzf::{GridPoint, ScanMeta};

/// Spacing classes, finest first: name and the largest spacing (mm) in it.
pub const CLASSES: [(&str, f64); 4] = [("fine", 6.0), ("good", 12.0), ("coarse", 25.0), ("sparse", f64::INFINITY)];

/// Plan-view cell (m), the radius it covers (m), and the blind circle under
/// the tripod (m) that is never called a shadow.
const PLAN_CELL: f64 = 0.10;
const PLAN_RADIUS: f64 = 25.0;
const TRIPOD_BLIND: f64 = 1.0;
/// Directions (azimuth bins) for the shadow test, and how much nearer than
/// the farthest return a cell must be to count.
const AZ_BINS: usize = 720;
const SHADOW_REACH: f64 = 0.9;
/// Share of directions with returns for a scan to count as all-round.
const ALL_ROUND: f64 = 0.9;

/// Plan cell codes, as drawn in the image.
const NONE: u8 = 0;
const SHADOW: u8 = 1;
/// Seen cells are `SEEN_BASE + (3 - class)`: sparse 2 … fine 5.
const SEEN_BASE: u8 = 2;

/// The binary view format (`X9V1`): a 16-byte header — magic, point count
/// (u32), stride (u32), full-resolution angular step (f32, rad) — then
/// `n × [x, y, z]` f32, `n` grey levels (u8) and `n` spacing classes (u8,
/// 0 fine … 3 sparse). Little-endian throughout.
pub const MAGIC: &[u8; 4] = b"X9V1";

pub struct Built {
    pub view: Vec<u8>,
    pub coverage: Value,
    pub image: Vec<u8>,
}

pub fn class_of(spacing_mm: f64) -> u8 {
    CLASSES.iter().position(|(_, max)| spacing_mm <= *max).unwrap_or(CLASSES.len() - 1) as u8
}

/// The scan's full-resolution angular step (the coarser of the two axes).
pub fn step_rad(meta: &ScanMeta) -> f64 {
    meta.angle_step_rad[0].abs().max(meta.angle_step_rad[1].abs())
}

/// Grid stride that keeps about `max_points` of `n` (every `k`th row and column).
pub fn stride_for(n: usize, max_points: usize) -> u32 {
    let mut k = 1u32;
    while n / ((k * k) as usize) > max_points.max(1) {
        k += 1;
    }
    k
}

pub fn build(pts: &[GridPoint], meta: &ScanMeta, grey: &dyn Fn(&GridPoint) -> u8, max_points: usize) -> Built {
    let step = step_rad(meta);
    let spacing = |p: &GridPoint| {
        let (x, y, z) = (p.point.x as f64, p.point.y as f64, p.point.z as f64);
        (x * x + y * y + z * z).sqrt() * step * 1000.0
    };

    // Thinned points for drawing.
    let k = stride_for(pts.len(), max_points);
    let keep: Vec<&GridPoint> = pts.iter().filter(|p| p.row % k == 0 && p.col % k == 0).collect();
    let mut view = Vec::with_capacity(16 + keep.len() * 14);
    view.extend_from_slice(MAGIC);
    view.extend_from_slice(&(keep.len() as u32).to_le_bytes());
    view.extend_from_slice(&k.to_le_bytes());
    view.extend_from_slice(&(step as f32).to_le_bytes());
    for p in &keep {
        for v in [p.point.x, p.point.y, p.point.z] {
            view.extend_from_slice(&v.to_le_bytes());
        }
    }
    view.extend(keep.iter().map(|p| grey(p)));
    view.extend(keep.iter().map(|p| class_of(spacing(p))));

    // Spacing at full resolution.
    let mut counts = [0usize; CLASSES.len()];
    let mut sample: Vec<f64> = Vec::with_capacity(pts.len().min(200_000));
    let every = (pts.len() / 200_000).max(1);
    for (i, p) in pts.iter().enumerate() {
        let s = spacing(p);
        counts[class_of(s) as usize] += 1;
        if i % every == 0 {
            sample.push(s);
        }
    }
    sample.sort_by(|a, b| a.total_cmp(b));
    let pct = |q: f64| sample.get(((sample.len() as f64 - 1.0) * q).round() as usize).map(|v| (v * 10.0).round() / 10.0);
    let n = pts.len().max(1) as f64;

    // Plan view: best class per cell, and the farthest return per direction.
    let near: Vec<(f64, f64, u8)> = pts
        .iter()
        .filter_map(|p| {
            let (x, y) = (p.point.x as f64, p.point.y as f64);
            (x.hypot(y) <= PLAN_RADIUS).then(|| (x, y, class_of(spacing(p))))
        })
        .collect();
    let clipped = 1.0 - near.len() as f64 / n;
    let (mut x0, mut y0, mut x1, mut y1) = (0f64, 0f64, 0f64, 0f64);
    for &(x, y, _) in &near {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let w = (((x1 - x0) / PLAN_CELL).ceil() as usize + 1).max(1);
    let h = (((y1 - y0) / PLAN_CELL).ceil() as usize + 1).max(1);
    let cell = |x: f64, y: f64| (((y - y0) / PLAN_CELL) as usize).min(h - 1) * w + (((x - x0) / PLAN_CELL) as usize).min(w - 1);
    let az = |x: f64, y: f64| ((y.atan2(x) + std::f64::consts::PI) / std::f64::consts::TAU * AZ_BINS as f64) as usize % AZ_BINS;
    let mut best = vec![u8::MAX; w * h];
    let mut reach = vec![0f64; AZ_BINS];
    for &(x, y, c) in &near {
        let i = cell(x, y);
        best[i] = best[i].min(c);
        let b = az(x, y);
        reach[b] = reach[b].max(x.hypot(y));
    }
    // A direction's reach is the farthest return in it or either neighbour,
    // so a sparse scan's gaps between columns aren't read as nothing there.
    let reach: Vec<f64> = (0..AZ_BINS).map(|b| reach[(b + AZ_BINS - 1) % AZ_BINS].max(reach[b]).max(reach[(b + 1) % AZ_BINS])).collect();
    let all_round = reach.iter().filter(|&&r| r > 0.0).count() as f64 >= ALL_ROUND * AZ_BINS as f64;
    let mut plan = vec![NONE; w * h];
    let (mut seen, mut shadow) = (0usize, 0usize);
    for r in 0..h {
        for c in 0..w {
            let i = r * w + c;
            if best[i] != u8::MAX {
                plan[i] = SEEN_BASE + (CLASSES.len() as u8 - 1 - best[i]);
                seen += 1;
                continue;
            }
            let (x, y) = (x0 + (c as f64 + 0.5) * PLAN_CELL, y0 + (r as f64 + 0.5) * PLAN_CELL);
            let d = x.hypot(y);
            if all_round && d > TRIPOD_BLIND && d < SHADOW_REACH * reach[az(x, y)] {
                plan[i] = SHADOW;
                shadow += 1;
            }
        }
    }
    // Keep shadows at least three cells wide (an opening: erode, then dilate,
    // 3 × 3): thin rays through gaps to far returns are not areas to fill.
    let near8 = |grid: &[bool], r: usize, c: usize, all: bool| {
        let mut hit = all;
        for dr in -1i64..=1 {
            for dc in -1i64..=1 {
                let (rr, cc) = (r as i64 + dr, c as i64 + dc);
                let v = rr >= 0 && cc >= 0 && (rr as usize) < h && (cc as usize) < w && grid[rr as usize * w + cc as usize];
                hit = if all { hit && v } else { hit || v };
            }
        }
        hit
    };
    let mask: Vec<bool> = plan.iter().map(|&v| v == SHADOW).collect();
    let core: Vec<bool> = (0..w * h).map(|i| mask[i] && near8(&mask, i / w, i % w, true)).collect();
    for i in 0..w * h {
        if mask[i] && !near8(&core, i / w, i % w, false) {
            plan[i] = NONE;
            shadow -= 1;
        }
    }
    let area = |cells: usize| (cells as f64 * PLAN_CELL * PLAN_CELL * 10.0).round() / 10.0;
    let image = plan_png(&plan, w, h, x0, y0);

    let coverage = json!({
        "point_count": pts.len(),
        "angle_step_rad": step,
        "spacing_mm": {"p10": pct(0.1), "p50": pct(0.5), "p90": pct(0.9)},
        "classes": CLASSES.iter().zip(counts).map(|((name, max), c)| json!({
            "name": name,
            "max_mm": if max.is_finite() { json!(max) } else { Value::Null },
            "share": (c as f64 / n * 1000.0).round() / 1000.0,
        })).collect::<Vec<_>>(),
        "plan": {
            "cell_m": PLAN_CELL,
            "radius_m": PLAN_RADIUS,
            "origin_m": [x0, y0],
            "width": w,
            "height": h,
            "seen_m2": area(seen),
            "shadows_assessed": all_round,
            "shadow_m2": if all_round { json!(area(shadow)) } else { Value::Null },
            "shadow_share": if !all_round { Value::Null } else if seen + shadow > 0 { json!((shadow as f64 / (seen + shadow) as f64 * 1000.0).round() / 1000.0) } else { json!(0.0) },
            "beyond_radius_share": (clipped * 1000.0).round() / 1000.0,
        },
        "view": {"points": keep.len(), "stride": k},
    });
    Built { view, coverage, image }
}

/// The plan colour of a spacing class (0 fine … 3 sparse).
pub fn class_colour(class: u8) -> [u8; 3] {
    COLOURS[(SEEN_BASE + (CLASSES.len() as u8 - 1 - class.min(3))) as usize]
}

/// Plan colours by cell code: nothing, shadow, sparse, coarse, good, fine.
const COLOURS: [[u8; 3]; 6] = [[17, 24, 39], [215, 48, 39], [252, 141, 89], [254, 224, 139], [145, 207, 96], [26, 152, 80]];

/// The plan as a PNG: x to the right, y up, one pixel per cell, the scanner
/// as a white cross. It shows only coverage classes, never intensity.
fn plan_png(plan: &[u8], w: usize, h: usize, x0: f64, y0: f64) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    for r in 0..h {
        for c in 0..w {
            let px = COLOURS[plan[r * w + c] as usize];
            let o = ((h - 1 - r) * w + c) * 3;
            rgb[o..o + 3].copy_from_slice(&px);
        }
    }
    let (sc, sr) = ((-x0 / PLAN_CELL) as i64, (-y0 / PLAN_CELL) as i64);
    for d in -3i64..=3 {
        for (c, r) in [(sc + d, sr), (sc, sr + d)] {
            if c >= 0 && r >= 0 && (c as usize) < w && (r as usize) < h {
                let o = ((h - 1 - r as usize) * w + c as usize) * 3;
                rgb[o..o + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
    }
    crate::measure::png_rgb(w, h, &rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trimble_tzf::Point;

    fn meta(step: f64) -> ScanMeta {
        ScanMeta {
            grid_rows: 0,
            grid_cols: 0,
            tile_size: 0,
            n_points: 0,
            angle_step_rad: [step, step],
            start_ha_rad: 0.0,
            start_va_rad: 0.0,
            tilt: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            levelling: trimble_tzf::Levelling::Applied,
        }
    }

    /// A 4 m × 4 m room around the scanner: floor and four walls, with a
    /// 1 m box on the floor east of the scanner that blocks the floor behind it.
    fn room() -> Vec<GridPoint> {
        let mut v = Vec::new();
        let mut push = |x: f64, y: f64, z: f64| {
            let i = v.len() as u32;
            v.push(GridPoint { point: Point { x: x as f32, y: y as f32, z: z as f32, intensity: 0.0 }, row: i / 1000, col: i % 1000 });
        };
        let mut t: f64 = -2.0;
        while t <= 2.0 {
            let mut z = -1.5;
            while z <= 1.0 {
                push(t, -2.0, z);
                push(t, 2.0, z);
                push(-2.0, t, z);
                push(2.0, t, z);
                z += 0.1;
            }
            t += 0.01;
        }
        let mut t: f64 = -2.0;
        while t <= 2.0 {
            let mut s: f64 = -2.0;
            while s <= 2.0 {
                // Floor, except where the box hides it.
                let behind_box = t > 1.0 && s.abs() < 0.3 * t;
                if !behind_box {
                    push(t, s, -1.5);
                }
                s += 0.05;
            }
            t += 0.05;
        }
        v
    }

    #[test]
    fn classes_follow_the_thresholds() {
        assert_eq!(class_of(1.0), 0);
        assert_eq!(class_of(6.0), 0);
        assert_eq!(class_of(6.1), 1);
        assert_eq!(class_of(20.0), 2);
        assert_eq!(class_of(500.0), 3);
    }

    #[test]
    fn stride_keeps_about_the_asked_number() {
        assert_eq!(stride_for(1000, 2000), 1);
        assert_eq!(stride_for(8_000_000, 2_000_000), 2);
        assert_eq!(stride_for(40_000_000, 2_000_000), 5);
    }

    #[test]
    fn a_blocked_floor_is_a_shadow_and_the_view_round_trips() {
        let pts = room();
        let b = build(&pts, &meta(0.0005), &|_| 7, 1 << 30);
        let n = u32::from_le_bytes(b.view[4..8].try_into().unwrap()) as usize;
        assert_eq!(&b.view[..4], MAGIC);
        assert_eq!(n, pts.len());
        assert_eq!(b.view.len(), 16 + n * 14);
        assert_eq!(b.view[16 + n * 12], 7);
        let plan = &b.coverage["plan"];
        let shadow = plan["shadow_m2"].as_f64().unwrap();
        // The hidden wedge is ~0.3·(2²−1²) ≈ 0.9 m² of floor; the shadow test is
        // a little conservative (the 0.9 reach and the cells' edges).
        assert!(shadow > 0.4 && shadow < 1.2, "shadow {shadow} m²");
        assert!(plan["seen_m2"].as_f64().unwrap() > 14.0);
        assert_eq!(plan["shadows_assessed"], true);
        // Ranges of 1.5–3 m at 0.5 mrad: 0.75–1.5 mm, all fine.
        assert_eq!(b.coverage["classes"][0]["share"], 1.0);
        assert_eq!(&b.image[1..4], b"PNG");
    }

    #[test]
    fn a_partial_sweep_has_no_shadows() {
        // The same room seen only to the east (an area scan's window).
        let pts: Vec<GridPoint> = room().into_iter().filter(|p| p.point.x > 0.5 && p.point.y.abs() < p.point.x).collect();
        let b = build(&pts, &meta(0.0005), &|_| 0, 1 << 30);
        let plan = &b.coverage["plan"];
        assert_eq!(plan["shadows_assessed"], false);
        assert!(plan["shadow_m2"].is_null() && plan["seen_m2"].as_f64().unwrap() > 1.0, "{plan}");
    }

    #[test]
    fn thinning_takes_every_kth_row_and_column() {
        let pts = room();
        let b = build(&pts, &meta(0.0005), &|_| 0, pts.len() / 4);
        assert_eq!(b.coverage["view"]["stride"], 2);
        let n = b.coverage["view"]["points"].as_u64().unwrap() as usize;
        assert!(n <= pts.len() / 4 + 1000 && n > pts.len() / 6, "{n}");
    }
}
