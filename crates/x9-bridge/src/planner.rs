//! Where to set up next, from what this station saw: a greedy next-best-view
//! over the plan of the levelled points.
//!
//! The plan is 0.5 m cells around the scanner. A cell is *ground* (points at
//! its floor and nothing at body height above it), *standing* (points at body
//! height above a floor near the tripod's: walls, trunks, posts, cars) or
//! *overhead* (only points well above the tripod's floor: canopy, eaves). An
//! empty cell is *hidden* if it is nearer than the farthest return in its
//! direction or a standing cell lies on the sight line to it: something
//! stood in the way. Some hidden cells no setup can see (inside a building);
//! the rays below only ever count the ones a candidate would.
//!
//! Candidates are ground cells clear of anything standing that a person can
//! walk to from the tripod over ground the scan saw. From each, rays every
//! degree (stopped by standing cells) count the hidden cells it would see,
//! and the standing cells this station also saw: the shared surfaces the two
//! stations are registered by. The first pick sees the most hidden ground
//! with enough shared surfaces; the next is the best on what is still hidden,
//! a step away from the picks before it (greedy cover).
//!
//! The shortest step scales with the place: a quarter of the median reach,
//! at least 2 m, so a room's stations sit metres apart and a campus's tens.
//!
//! Limits: the plan is flat. A cell is judged against its own floor and the
//! tripod's, so ground more than 1.5 m above the tripod's reads as overhead
//! and is never a candidate, and overhead cells never block a ray. It knows
//! only this station; what earlier stations saw is for the agent to weigh
//! (`scanner_registration`).

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use trimble_tzf::GridPoint;

/// Plan cells per side. The cell (m) scales with how far the scanner saw, so
/// a room gets a fine plan and a campus a coarse one; the radius is half the
/// side.
const SIDE: usize = 160;
const CELL_RANGE: (f64, f64) = (0.1, 0.5);
const CELLS_PER_REACH: f64 = 50.0;
/// Under and around the tripod, where nothing is called hidden (m), and the
/// clearance a tripod needs from anything standing (m).
const BLIND: f64 = 1.5;
const TRIPOD_CLEAR: f64 = 0.3;
/// Body height above a cell's floor (m): points here make it standing,
/// if there are enough of them over enough height to be a surface, not
/// stray returns (a passer-by, the edge of a wall).
const BODY: (f64, f64) = (0.3, 2.0);
const BODY_POINTS: u32 = 16;
const BODY_SPAN: f32 = 0.3;
/// A cell whose lowest point is this far above the tripod's floor (m) has
/// no floor the scan saw: canopy, eaves.
const OVERHEAD: f64 = 1.5;
/// Directions for the reach, the share with returns for a full dome, and
/// how much nearer than the reach an empty cell must be to be hidden.
const AZ_BINS: usize = 720;
const ALL_ROUND: f64 = 0.9;
const HIDDEN_REACH: f64 = 0.9;
/// Rays from each candidate, and candidates every `STRIDE` cells.
const RAYS: usize = 360;
const STRIDE: usize = 2;
/// Surface both stations see from the same side (m, in plan) for the next
/// to be registered to this one, and the least newly seen ground (m²)
/// worth a setup.
const MIN_SHARED_M: f64 = 5.0;
const MIN_GAIN_M2: f64 = 1.0;
const PICKS: usize = 3;
/// The shortest step (m), and its share of the median reach.
const MIN_STEP: f64 = 2.0;
const STEP_OF_REACH: f64 = 0.25;
/// How far from a pick its landmark may stand (m), the most a landmark may
/// cover to count as a post rather than a wall (m²), and how tall (m) it
/// must stand to be pointed at in the preview.
const LANDMARK_REACH: f64 = 6.0;
const POST_M2: f64 = 1.0;
const LANDMARK_TALL: f64 = 1.5;
/// Either side of a named subject (rad): its flanks are hidden ground too.
const SUBJECT_MARGIN: f64 = 15.0 * std::f64::consts::PI / 180.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Empty,
    Ground,
    Standing,
    Overhead,
}

/// The plan's cell size; it spans `SIDE` cells centred on the scanner.
#[derive(Clone, Copy, Debug)]
struct Grid {
    cell: f64,
}

impl Grid {
    fn radius(&self) -> f64 {
        self.cell * SIDE as f64 / 2.0
    }

    fn cell_of(&self, x: f64, y: f64) -> Option<usize> {
        let r = self.radius();
        let (c, w) = (((x + r) / self.cell).floor(), ((y + r) / self.cell).floor());
        (c >= 0.0 && w >= 0.0 && (c as usize) < SIDE && (w as usize) < SIDE).then(|| w as usize * SIDE + c as usize)
    }

    fn centre(&self, i: usize) -> (f64, f64) {
        let r = self.radius();
        ((i % SIDE) as f64 * self.cell + self.cell / 2.0 - r, (i / SIDE) as f64 * self.cell + self.cell / 2.0 - r)
    }

    /// The cells a ray from `(x, y)` along `(dx, dy)` (unit) crosses within
    /// `len` metres, nearest first, not counting the one it starts in, until
    /// `visit` returns false.
    fn trace(&self, x: f64, y: f64, dx: f64, dy: f64, len: f64, mut visit: impl FnMut(usize) -> bool) {
        let r = self.radius();
        let (gx, gy) = ((x + r) / self.cell, (y + r) / self.cell);
        let (mut ix, mut iy) = (gx.floor() as i64, gy.floor() as i64);
        let step = |d: f64| if d > 0.0 { 1 } else { -1 };
        let first = |g: f64, i: i64, d: f64| match d {
            d if d > 0.0 => (i as f64 + 1.0 - g) / d,
            d if d < 0.0 => (g - i as f64) / -d,
            _ => f64::INFINITY,
        };
        let (mut tx, mut ty) = (first(gx, ix, dx), first(gy, iy, dy));
        let (ddx, ddy) = (if dx != 0.0 { 1.0 / dx.abs() } else { f64::INFINITY }, if dy != 0.0 { 1.0 / dy.abs() } else { f64::INFINITY });
        let end = len / self.cell;
        loop {
            let t = if tx < ty {
                ix += step(dx);
                tx += ddx;
                tx - ddx
            } else {
                iy += step(dy);
                ty += ddy;
                ty - ddy
            };
            if t > end || ix < 0 || iy < 0 || ix as usize >= SIDE || iy as usize >= SIDE || !visit(iy as usize * SIDE + ix as usize) {
                return;
            }
        }
    }

    /// Cells in plan to m² (to 0.1).
    fn area(&self, cells: usize) -> f64 {
        r1(cells as f64 * self.cell * self.cell)
    }
}

fn neighbours(i: usize) -> impl Iterator<Item = (usize, bool)> {
    let (r, c) = ((i / SIDE) as i64, (i % SIDE) as i64);
    (-1i64..=1).flat_map(move |dr| (-1i64..=1).map(move |dc| (dr, dc))).filter_map(move |(dr, dc)| {
        let (rr, cc) = (r + dr, c + dc);
        ((dr, dc) != (0, 0) && rr >= 0 && cc >= 0 && (rr as usize) < SIDE && (cc as usize) < SIDE)
            .then(|| (rr as usize * SIDE + cc as usize, dr != 0 && dc != 0))
    })
}

fn bin(x: f64, y: f64) -> usize {
    ((y.atan2(x).rem_euclid(TAU)) / TAU * AZ_BINS as f64) as usize % AZ_BINS
}

/// Walking cost (tenths of a cell) from the tripod to every ground cell over
/// ground cells, never cutting a corner past something standing. The walk
/// starts on the ground round the tripod (the blind circle under it has no
/// points) that nothing standing separates from it.
fn walk(g: Grid, kind: &[Kind]) -> Vec<u32> {
    let mut cost = vec![u32::MAX; kind.len()];
    let mut heap = std::collections::BinaryHeap::new();
    let open_to_tripod = |x: f64, y: f64, d: f64| {
        let mut clear = true;
        g.trace(0.0, 0.0, x / d, y / d, d - g.cell, |j| {
            clear = kind[j] != Kind::Standing;
            clear
        });
        clear
    };
    for (i, k) in kind.iter().enumerate() {
        let (x, y) = g.centre(i);
        let d = x.hypot(y);
        if *k == Kind::Ground && d <= BLIND + 1.0 && open_to_tripod(x, y, d) {
            cost[i] = (x.hypot(y) / g.cell * 10.0) as u32;
            heap.push(std::cmp::Reverse((cost[i], i)));
        }
    }
    while let Some(std::cmp::Reverse((c, i))) = heap.pop() {
        if c > cost[i] {
            continue;
        }
        for (j, diag) in neighbours(i) {
            if kind[j] != Kind::Ground {
                continue;
            }
            if diag {
                let (r, cc) = (j / SIDE, i % SIDE);
                let (a, b) = (r * SIDE + cc, (i / SIDE) * SIDE + j % SIDE);
                if kind[a] != Kind::Ground || kind[b] != Kind::Ground {
                    continue;
                }
            }
            let n = c + if diag { 14 } else { 10 };
            if n < cost[j] {
                cost[j] = n;
                heap.push(std::cmp::Reverse((n, j)));
            }
        }
    }
    cost
}

/// Keep only hidden areas at least three cells wide (an opening: erode, then
/// dilate): slivers behind posts and between sparse columns are not worth a
/// setup.
fn open(mask: &[bool]) -> Vec<bool> {
    let all = |m: &[bool], i: usize| m[i] && neighbours(i).count() == 8 && neighbours(i).all(|(j, _)| m[j]);
    let core: Vec<bool> = (0..mask.len()).map(|i| all(mask, i)).collect();
    (0..mask.len()).map(|i| mask[i] && (core[i] || neighbours(i).any(|(j, _)| core[j]))).collect()
}

/// Standing cells grouped into things (8-connected): a label per cell and
/// each label's size in cells.
fn things(kind: &[Kind]) -> (Vec<usize>, Vec<usize>) {
    let mut label = vec![usize::MAX; kind.len()];
    let mut size = Vec::new();
    for s in 0..kind.len() {
        if kind[s] != Kind::Standing || label[s] != usize::MAX {
            continue;
        }
        let l = size.len();
        let mut n = 0;
        let mut q = VecDeque::from([s]);
        label[s] = l;
        while let Some(i) = q.pop_front() {
            n += 1;
            for (j, _) in neighbours(i) {
                if kind[j] == Kind::Standing && label[j] == usize::MAX {
                    label[j] = l;
                    q.push_back(j);
                }
            }
        }
        size.push(n);
    }
    (label, size)
}

/// The landmark for a pick at cell `at`: the nearest standing thing within
/// reach, a post tall enough to see first, then anything that tall, then
/// whatever is nearest.
fn landmark(g: Grid, kind: &[Kind], label: &[usize], size: &[usize], height: &[f64], at: usize) -> Option<usize> {
    let (x, y) = g.centre(at);
    (0..kind.len())
        .filter(|&j| kind[j] == Kind::Standing)
        .map(|j| (j, { let (lx, ly) = g.centre(j); (lx - x).hypot(ly - y) }))
        .filter(|&(_, d)| d <= LANDMARK_REACH)
        .min_by(|a, b| {
            let rank = |j: usize| match (height[j] >= LANDMARK_TALL, g.area(size[label[j]]) <= POST_M2) {
                (true, true) => 0,
                (true, false) => 1,
                _ => 2,
            };
            rank(a.0).cmp(&rank(b.0)).then(a.1.total_cmp(&b.1))
        })
        .map(|(j, _)| j)
}

/// Bearings (rad, from the scanner) of the subject framed by preview columns
/// `x0..x1` (`x1 < x0` wraps across the seam), widened by the margin: a test
/// for whether a bearing falls in it.
fn sector(pts: &[GridPoint], grid: (u32, u32), preview: (usize, usize), x0: usize, x1: usize) -> Option<impl Fn(f64) -> bool> {
    if preview.0 == 0 || grid.0 == 0 {
        return None;
    }
    let mut az = vec![f64::NAN; grid.0 as usize];
    for g in pts {
        let c = g.col as usize;
        if c < az.len() && az[c].is_nan() && (g.point.x != 0.0 || g.point.y != 0.0) {
            az[c] = (g.point.y as f64).atan2(g.point.x as f64).rem_euclid(TAU);
        }
    }
    let at = |px: usize| {
        let c = (px as f64 * grid.0 as f64 / preview.0 as f64) as usize;
        (0..az.len()).flat_map(|k| [c + k, c + az.len() - k]).map(|i| az[i % az.len()]).find(|a| !a.is_nan())
    };
    let w = preview.0;
    let width = if x1 >= x0 { x1 - x0 } else { x1 + w - x0 };
    let (a0, a1, mid) = (at(x0)?, at(x1 % w)?, at((x0 + width / 2) % w)?);
    let ccw = |from: f64, to: f64| (to - from).rem_euclid(TAU);
    let (start, span) = if ccw(a0, mid) <= ccw(a0, a1) { (a0, ccw(a0, a1)) } else { (a1, ccw(a1, a0)) };
    let (start, span) = (start - SUBJECT_MARGIN, (span + 2.0 * SUBJECT_MARGIN).min(TAU));
    Some(move |b: f64| ccw(start, b) <= span)
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The `q` quantile of the directions' reach (those with returns).
fn reach_at(reach: &[f64], q: f64) -> f64 {
    let mut r: Vec<f64> = reach.iter().copied().filter(|&r| r > 0.0).collect();
    r.sort_by(|a, b| a.total_cmp(b));
    r.get(((r.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0.0)
}

/// Next-station candidates from one full-dome scan's levelled points (scanner
/// at the origin). `grid` is the scan's (columns, rows) and `preview` its
/// preview's (width, height), for the pixel of each pick. `subject`, preview
/// columns framing what the job is about, counts only the hidden ground in
/// its direction (and either side of it): behind a building, past an object.
pub fn plan(pts: &[GridPoint], grid: (u32, u32), preview: (usize, usize), subject: Option<(usize, usize)>) -> Value {
    let mut reach = vec![0f64; AZ_BINS];
    for p in pts {
        let (x, y) = (p.point.x as f64, p.point.y as f64);
        let b = bin(x, y);
        reach[b] = reach[b].max(x.hypot(y));
    }
    // A direction's reach is the farthest return in it or either neighbour.
    let reach: Vec<f64> = (0..AZ_BINS).map(|b| reach[(b + AZ_BINS - 1) % AZ_BINS].max(reach[b]).max(reach[(b + 1) % AZ_BINS])).collect();
    let g = Grid { cell: (reach_at(&reach, 0.9) / CELLS_PER_REACH).clamp(CELL_RANGE.0, CELL_RANGE.1) };
    let radius = g.radius();
    let empty = |reason: &str| json!({"cell_m": g.cell, "radius_m": radius, "candidates": [], "reason": reason});
    if (reach.iter().filter(|&&r| r > 0.0).count() as f64) < ALL_ROUND * AZ_BINS as f64 {
        return empty("not a full dome: planning needs the whole horizon");
    }

    let n = SIDE * SIDE;
    let mut zmin = vec![f32::INFINITY; n];
    let mut low = vec![(0u32, 0u32); n];
    for p in pts {
        if let Some(i) = g.cell_of(p.point.x as f64, p.point.y as f64) {
            if p.point.z < zmin[i] {
                zmin[i] = p.point.z;
                low[i] = (p.col, p.row);
            }
        }
    }
    let mut near: Vec<f32> = (0..n)
        .filter(|&i| zmin[i].is_finite() && (BLIND..=6.0).contains(&{ let (x, y) = g.centre(i); x.hypot(y) }))
        .map(|i| zmin[i])
        .collect();
    if near.is_empty() {
        return empty("no floor seen round the tripod");
    }
    near.sort_by(|a, b| a.total_cmp(b));
    let floor = near[near.len() / 2] as f64;

    // Body height, and the point that shows a standing cell best.
    let mut body = vec![(0u32, f32::INFINITY, f32::NEG_INFINITY); n];
    let mut top = vec![f32::NEG_INFINITY; n];
    let mut show = vec![(0u32, 0u32, 0f32, 0f32, f32::INFINITY); n];
    for p in pts {
        let Some(i) = g.cell_of(p.point.x as f64, p.point.y as f64) else { continue };
        let h = (p.point.z - zmin[i]) as f64;
        top[i] = top[i].max(p.point.z);
        if (BODY.0..=BODY.1).contains(&h) {
            let b = &mut body[i];
            *b = (b.0 + 1, b.1.min(p.point.z), b.2.max(p.point.z));
            let off = (h - 1.2).abs() as f32;
            if off < show[i].4 {
                show[i] = (p.col, p.row, p.point.x, p.point.y, off);
            }
        }
    }
    let kind: Vec<Kind> = (0..n)
        .map(|i| match () {
            _ if !zmin[i].is_finite() => Kind::Empty,
            _ if zmin[i] as f64 > floor + OVERHEAD => Kind::Overhead,
            _ if body[i].0 >= BODY_POINTS && body[i].2 - body[i].1 >= BODY_SPAN => Kind::Standing,
            _ => Kind::Ground,
        })
        .collect();
    // Hidden: empty, and either nearer than the reach or behind something
    // standing (a building hides everything behind it, the reach included).
    let behind = |x: f64, y: f64, d: f64| {
        let mut hit = false;
        g.trace(0.0, 0.0, x / d, y / d, d - g.cell, |j| {
            hit = kind[j] == Kind::Standing;
            !hit
        });
        hit
    };
    let hidden = open(
        &(0..n)
            .map(|i| {
                let (x, y) = g.centre(i);
                let d = x.hypot(y);
                kind[i] == Kind::Empty && d > BLIND && d <= radius && (d < HIDDEN_REACH * reach[bin(x, y)] || behind(x, y, d))
            })
            .collect::<Vec<_>>(),
    );
    let all_hidden = hidden.iter().filter(|&&h| h).count();
    let toward = match subject {
        Some((x0, x1)) if x0 < preview.0 && x1 <= preview.0 => match sector(pts, grid, preview, x0, x1) {
            Some(s) => Some(s),
            None => return empty("the subject's columns could not be placed on this scan"),
        },
        Some(_) => return empty("the subject's columns lie outside the preview"),
        None => None,
    };
    let hidden: Vec<bool> = (0..n)
        .map(|i| hidden[i] && toward.as_ref().is_none_or(|s| { let (x, y) = g.centre(i); s(y.atan2(x).rem_euclid(TAU)) }))
        .collect();
    let hidden_cells = hidden.iter().filter(|&&h| h).count();

    let min_step = (STEP_OF_REACH * reach_at(&reach, 0.5)).max(MIN_STEP);
    let min_gain = (MIN_GAIN_M2 / (g.cell * g.cell)).ceil() as usize;
    let min_shared = (MIN_SHARED_M / g.cell).ceil() as usize;
    let mut base = json!({"cell_m": g.cell, "radius_m": radius, "floor_z_m": (floor * 100.0).round() / 100.0,
                          "min_step_m": r1(min_step), "hidden_m2": g.area(all_hidden)});
    if let Some((x0, x1)) = subject {
        base["subject_px"] = json!([x0, x1]);
        base["subject_hidden_m2"] = json!(g.area(hidden_cells));
    }
    let with = |mut v: Value, cands: Vec<Value>, reason: Option<&str>, still: usize| {
        v["candidates"] = json!(cands);
        v["still_hidden_m2"] = json!(g.area(still));
        if let Some(r) = reason {
            v["reason"] = json!(r);
        }
        v
    };
    let none = "nothing hidden that another setup would see";
    if hidden_cells < min_gain {
        return with(base, vec![], Some(none), hidden_cells);
    }

    // What each candidate would see.
    let cost = walk(g, &kind);
    let k = (TRIPOD_CLEAR / g.cell).ceil() as i64;
    let clear = |i: usize| {
        let (r, c) = ((i / SIDE) as i64, (i % SIDE) as i64);
        kind[i] == Kind::Ground
            && cost[i] != u32::MAX
            && (-k..=k).all(|dr| {
                (-k..=k).all(|dc| {
                    let (rr, cc) = (r + dr, c + dc);
                    rr < 0 || cc < 0 || rr as usize >= SIDE || cc as usize >= SIDE || kind[rr as usize * SIDE + cc as usize] != Kind::Standing
                })
            })
    };
    let mut stamp = vec![usize::MAX; n];
    let mut cands: Vec<(usize, Vec<u32>, usize)> = Vec::new();
    let (mut most, mut tried) = (0usize, 0usize);
    for i in 0..n {
        let (x, y) = g.centre(i);
        let d = x.hypot(y);
        if !(i / SIDE).is_multiple_of(STRIDE) || !(i % SIDE).is_multiple_of(STRIDE) || d < min_step || d > radius - 4.0 * g.cell || !clear(i) {
            continue;
        }
        tried += 1;
        let (mut sees, mut shared) = (Vec::new(), 0usize);
        for k in 0..RAYS {
            let a = k as f64 * TAU / RAYS as f64;
            g.trace(x, y, a.cos(), a.sin(), radius, |j| {
                if stamp[j] == i {
                    return kind[j] != Kind::Standing;
                }
                stamp[j] = i;
                if kind[j] == Kind::Standing {
                    // Shared only if seen from the same side as this station saw it.
                    let (sx, sy) = g.centre(j);
                    if (sx - x) * sx + (sy - y) * sy > 0.0 {
                        shared += 1;
                    }
                    return false;
                }
                if hidden[j] {
                    sees.push(j as u32);
                }
                true
            });
        }
        most = most.max(sees.len());
        if shared >= min_shared {
            cands.push((i, sees, shared));
        }
    }
    base["spots_tried"] = json!(tried);
    if tried == 0 {
        return with(base, vec![], Some("no open ground reachable from the tripod a step away"), hidden_cells);
    }
    if most < min_gain {
        return with(base, vec![], Some(none), hidden_cells);
    }

    // Greedy cover.
    let (label, size) = things(&kind);
    let height: Vec<f64> = (0..n).map(|i| if zmin[i].is_finite() { (top[i] - zmin[i]) as f64 } else { 0.0 }).collect();
    let px = |col: u32, row: u32| {
        if preview.0 == 0 || preview.1 == 0 || grid.0 == 0 || grid.1 == 0 {
            return Value::Null;
        }
        let x = (col as f64 * preview.0 as f64 / grid.0 as f64).round() as i64 % preview.0 as i64;
        let y = (row as f64 * preview.1 as f64 / grid.1 as f64).round().min(preview.1 as f64 - 1.0);
        json!([x, y as i64])
    };
    let mut still = hidden.clone();
    let mut still_cells = hidden_cells;
    let mut picks: Vec<(f64, f64)> = Vec::new();
    let mut out = Vec::new();
    for rank in 1..=PICKS {
        let gain = |s: &[u32]| s.iter().filter(|&&j| still[j as usize]).count();
        let best = cands
            .iter()
            .filter(|(i, _, _)| {
                let (x, y) = g.centre(*i);
                picks.iter().all(|&(px, py)| (px - x).hypot(py - y) >= min_step)
            })
            .map(|c| (gain(&c.1), c))
            .max_by(|(ga, a), (gb, b)| ga.cmp(gb).then(a.2.cmp(&b.2)).then(cost[b.0].cmp(&cost[a.0])));
        let Some((gained, &(i, ref sees, shared))) = best else { break };
        if gained < min_gain {
            break;
        }
        for &j in sees {
            if still[j as usize] {
                still[j as usize] = false;
                still_cells -= 1;
            }
        }
        let (x, y) = g.centre(i);
        picks.push((x, y));
        let landmark = landmark(g, &kind, &label, &size, &height, i).map_or(Value::Null, |j| {
            let (col, row, lx, ly, _) = show[j];
            json!({
                "xy_m": [r1(lx as f64), r1(ly as f64)],
                "distance_m": r1((lx as f64 - x).hypot(ly as f64 - y)),
                "footprint_m2": g.area(size[label[j]]),
                "shape": if g.area(size[label[j]]) <= POST_M2 { "post" } else { "mass" },
                "height_m": r1(height[j]),
                "preview_px": px(col, row),
            })
        });
        let (col, row) = low[i];
        out.push(json!({
            "rank": rank,
            "xy_m": [r1(x), r1(y)],
            "distance_m": r1(x.hypot(y)),
            "walk_m": r1(cost[i] as f64 / 10.0 * g.cell),
            "sees_hidden_m2": g.area(gained),
            "shared_surface_m": r1(shared as f64 * g.cell),
            "preview_px": px(col, row),
            "landmark": landmark,
        }));
    }
    let reason = out.is_empty().then_some("no reachable spot sees the hidden ground and enough of what this station saw");
    with(base, out, reason, still_cells)
}

/// `x9-bridge plan STATION.tzf [--subject x0,x1]`: the plan for a recorded
/// station, as JSON on stdout. The preview is the `.jpg` beside the file.
pub fn cli(args: &[String]) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    let (mut file, mut subject) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--subject" => {
                let v = it.next().context("--subject needs x0,x1")?;
                let (a, b) = v.split_once(',').context("--subject needs x0,x1")?;
                subject = Some((a.trim().parse()?, b.trim().parse()?));
            }
            _ if file.is_none() => file = Some(std::path::PathBuf::from(a)),
            _ => bail!("usage: x9-bridge plan STATION.tzf [--subject x0,x1]"),
        }
    }
    let file = file.context("usage: x9-bridge plan STATION.tzf [--subject x0,x1]")?;
    let t = trimble_tzf::Tzf::open(&file).with_context(|| format!("opening {}", file.display()))?;
    let meta = t.scan_meta()?;
    let pts = t.grid_points()?;
    let preview = std::fs::read(file.with_extension("jpg")).ok().and_then(|j| crate::aim::jpeg_size(&j).ok()).unwrap_or((0, 0));
    let mut v = plan(&pts, (meta.grid_cols, meta.grid_rows), preview, subject);
    v["preview_size"] = json!([preview.0, preview.1]);
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use trimble_tzf::Point;

    const GROUND: f64 = -1.6;
    const GRID: (u32, u32) = (4000, 2000);
    const PREVIEW: (usize, usize) = (2000, 1000);
    const STEP: f64 = TAU / 4000.0;

    /// A vertical surface in plan, `top` metres above the ground. `through`
    /// is a mesh fence: the scanner sees past it, a person cannot.
    #[derive(Clone, Copy)]
    struct Wall {
        a: (f64, f64),
        b: (f64, f64),
        top: f64,
        through: bool,
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64, top: f64, through: bool) -> Vec<Wall> {
        let c = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        (0..4).map(|i| Wall { a: c[i], b: c[(i + 1) % 4], top, through }).collect()
    }

    /// Where the sight line from the scanner to `p` crosses `w`, as a share
    /// of the way (0..1), if it does.
    fn crossing(w: &Wall, p: (f64, f64)) -> Option<f64> {
        let (dx, dy) = (w.b.0 - w.a.0, w.b.1 - w.a.1);
        let den = p.0 * dy - p.1 * dx;
        if den.abs() < 1e-12 {
            return None;
        }
        let t = (w.a.0 * dy - w.a.1 * dx) / den;
        let s = (w.a.0 * p.1 - w.a.1 * p.0) / den;
        (t > 1e-6 && t < 1.0 - 1e-3 && (0.0..=1.0).contains(&s)).then_some(t)
    }

    /// What a full dome at the origin (instrument 1.6 m above flat ground)
    /// returns: ground out to 36 m except under `solid` footprints, and the
    /// walls, minus anything an opaque wall hides.
    fn scene(walls: &[Wall], solid: &[(f64, f64, f64, f64)]) -> Vec<GridPoint> {
        scene_at(walls, solid, 36.0, 0.25, 0.1)
    }

    /// The same, ground out to `extent` sampled every `ground` m, walls every `wall` m.
    fn scene_at(walls: &[Wall], solid: &[(f64, f64, f64, f64)], extent: f64, ground: f64, wall: f64) -> Vec<GridPoint> {
        let mut v = Vec::new();
        let hidden = |p: (f64, f64, f64), own: Option<usize>| {
            walls.iter().enumerate().any(|(i, w)| {
                Some(i) != own && !w.through && crossing(w, (p.0, p.1)).is_some_and(|t| t * p.2 < GROUND + w.top)
            })
        };
        let mut push = |x: f64, y: f64, z: f64| {
            let col = ((y.atan2(x).rem_euclid(TAU)) / STEP) as u32;
            let row = ((x.hypot(y).atan2(z)) / STEP) as u32;
            v.push(GridPoint { point: Point { x: x as f32, y: y as f32, z: z as f32, intensity: 0.0 }, row, col });
        };
        let steps = (2.0 * extent / ground).round() as usize;
        for ix in 0..=steps {
            for iy in 0..=steps {
                let (x, y) = (ix as f64 * ground - extent, iy as f64 * ground - extent);
                let inside = solid.iter().any(|&(x0, y0, x1, y1)| x > x0 && x < x1 && y > y0 && y < y1);
                if x.hypot(y) > 1.0 && !inside && !hidden((x, y, GROUND), None) {
                    push(x, y, GROUND);
                }
            }
        }
        for (i, w) in walls.iter().enumerate() {
            let len = (w.b.0 - w.a.0).hypot(w.b.1 - w.a.1);
            let n = (len / wall).ceil().max(1.0) as usize;
            for k in 0..=n {
                let f = k as f64 / n as f64;
                let (px, py) = (w.a.0 + f * (w.b.0 - w.a.0), w.a.1 + f * (w.b.1 - w.a.1));
                let mut z = GROUND;
                while z <= GROUND + w.top {
                    if !hidden((px, py, z), Some(i)) {
                        push(px, py, z);
                    }
                    z += 0.2;
                }
            }
        }
        v
    }

    fn fence() -> Vec<Wall> {
        rect(-36.5, -36.5, 36.5, 36.5, 2.5, false)
    }

    /// A building 10 m × 10 m east of the scanner, 6 m tall.
    fn block(x0: f64) -> (Vec<Wall>, (f64, f64, f64, f64)) {
        (rect(x0, -5.0, x0 + 10.0, 5.0, 6.0, false), (x0, -5.0, x0 + 10.0, 5.0))
    }

    /// Lamp posts, 0.3 m square and 3 m tall, every 7 m on a grid, none
    /// inside `solid`.
    fn posts(solid: (f64, f64, f64, f64)) -> Vec<Wall> {
        let mut v = Vec::new();
        for i in 0..10 {
            for j in 0..10 {
                let (x, y) = (i as f64 * 7.0 - 31.5, j as f64 * 7.0 - 31.5);
                if !(x > solid.0 - 1.0 && x < solid.2 + 1.0 && y > solid.1 - 1.0 && y < solid.3 + 1.0) {
                    v.extend(rect(x, y, x + 0.3, y + 0.3, 3.0, false));
                }
            }
        }
        v
    }

    fn picks(p: &Value) -> Vec<&Value> {
        p["candidates"].as_array().expect("candidates").iter().collect()
    }

    fn xy(c: &Value) -> (f64, f64) {
        (c["xy_m"][0].as_f64().unwrap(), c["xy_m"][1].as_f64().unwrap())
    }

    #[test]
    fn steps_out_to_see_behind_a_building() {
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        let p = plan(&scene(&walls, &[solid]), GRID, PREVIEW, None);
        let c = picks(&p);
        assert!(!c.is_empty(), "{p}");
        let first = c[0];
        let (x, y) = xy(first);
        let d = x.hypot(y);
        let min_step = p["min_step_m"].as_f64().unwrap();
        assert!(min_step >= 2.0 && d >= min_step, "{d} m < {min_step} m: {first}");
        assert!((first["distance_m"].as_f64().unwrap() - d).abs() < 0.1);
        // Clear of the building, on its far side or flanks, where its hidden
        // ground is.
        assert!(!(x > 9.0 && x < 21.0 && y.abs() < 6.0), "on the building: {first}");
        assert!(x > 5.0, "still on the scanner's side: {first}");
        let seen = first["sees_hidden_m2"].as_f64().unwrap();
        let hidden = p["hidden_m2"].as_f64().unwrap();
        assert!(hidden > 100.0 && seen >= 0.3 * hidden, "{seen} of {hidden} m²");
        assert!(first["shared_surface_m"].as_f64().unwrap() >= MIN_SHARED_M, "{first}");
        assert_eq!(p["cell_m"], 0.5, "a site this size plans on the coarsest cell");
        assert!(first["walk_m"].as_f64().unwrap() >= d - 0.5, "{first}");
    }

    #[test]
    fn the_figures_match_the_geometry() {
        // The block scene by hand: ground at -1.6 m; a 10 m building 10 m out
        // inside a fence 36.5 m out; the plan 40 m round on 0.5 m cells.
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        let p = plan(&scene(&walls, &[solid]), GRID, PREVIEW, None);
        assert_eq!((p["cell_m"].as_f64(), p["radius_m"].as_f64(), p["floor_z_m"].as_f64()), (Some(0.5), Some(40.0), Some(GROUND)));
        // A quarter of the median reach: the fence square's is ~39.5 m, the
        // building's short sector pulls it a little nearer.
        let min_step = p["min_step_m"].as_f64().unwrap();
        assert!((9.3..=10.3).contains(&min_step), "{min_step}");
        // The building's shadow to the fence, (36.5² - 10²) / 2 = 616 m², and
        // the plan's four caps past the fence, 4 × 75.5 = 302 m², less the
        // building's walls.
        let hidden = p["hidden_m2"].as_f64().unwrap();
        assert!((830.0..=930.0).contains(&hidden), "{hidden}");
        // Spots 1 m apart over the ground a step out and inside the fence,
        // less the building and its shadow: ~3,300.
        let tried = p["spots_tried"].as_u64().unwrap();
        assert!((3_100..=3_700).contains(&tried), "{tried}");
        let c = picks(&p);
        let sees: Vec<f64> = c.iter().map(|c| c["sees_hidden_m2"].as_f64().unwrap()).collect();
        assert!(sees[0] >= 0.6 * hidden && sees.iter().all(|&s| s <= sees[0]), "{sees:?} of {hidden}");
        let still = p["still_hidden_m2"].as_f64().unwrap();
        assert!(still <= hidden - sees[0] + 0.15 && still >= hidden - sees.iter().sum::<f64>() - 0.15, "{still}");
        let (x, y) = xy(c[0]);
        let walk = c[0]["walk_m"].as_f64().unwrap();
        assert!(walk >= x.hypot(y) - 0.5 && walk <= 1.3 * x.hypot(y), "{walk} m to {x}, {y}");
    }

    #[test]
    fn never_picks_ground_a_person_cannot_walk_to() {
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        // A mesh fence round all the ground east of the building.
        walls.extend(rect(21.0, -36.0, 36.0, 36.0, 2.0, true));
        let p = plan(&scene(&walls, &[solid]), GRID, PREVIEW, None);
        let c = picks(&p);
        assert!(!c.is_empty(), "{p}");
        for c in c {
            let (x, _) = xy(c);
            assert!(x < 21.0, "inside the fence: {c}");
        }
    }

    #[test]
    fn later_picks_cover_what_the_first_left() {
        let (mut walls, east) = block(10.0);
        let (west_walls, west) = block(-20.0);
        walls.extend(west_walls);
        walls.extend(fence());
        let p = plan(&scene(&walls, &[east, west]), GRID, PREVIEW, None);
        let c = picks(&p);
        assert!(c.len() >= 2, "{p}");
        for c in &c {
            assert!(c["distance_m"].as_f64().unwrap() <= p["radius_m"].as_f64().unwrap() - 1.9, "beyond the plan's radius: {c}");
        }
        let (a, b) = (xy(c[0]), xy(c[1]));
        assert!(a.0.signum() != b.0.signum(), "both on one side: {a:?} {b:?}");
        let min_step = p["min_step_m"].as_f64().unwrap();
        assert!((a.0 - b.0).hypot(a.1 - b.1) >= min_step);
        assert!(c[1]["sees_hidden_m2"].as_f64().unwrap() <= c[0]["sees_hidden_m2"].as_f64().unwrap());
    }

    /// Preview columns that frame the block at `x0` (in this scene's grid:
    /// column = azimuth / STEP, halved for the preview).
    fn subject_cols(x0: f64) -> (usize, usize) {
        let col = |x: f64, y: f64| ((y.atan2(x).rem_euclid(TAU)) / STEP / 2.0) as usize;
        // Left edge to right edge, in increasing azimuth.
        if x0 > 0.0 { (col(x0, -5.0), col(x0, 5.0)) } else { (col(x0 + 10.0, 5.0), col(x0 + 10.0, -5.0)) }
    }

    #[test]
    fn a_named_subject_decides_which_side_comes_first() {
        let (mut walls, east) = block(10.0);
        let (west_walls, west) = block(-20.0);
        walls.extend(west_walls);
        walls.extend(fence());
        let pts = scene(&walls, &[east, west]);
        for (x0, east_side) in [(10.0, true), (-20.0, false)] {
            let (a, b) = subject_cols(x0);
            let p = plan(&pts, GRID, PREVIEW, Some((a, b)));
            let c = picks(&p);
            assert!(!c.is_empty(), "{p}");
            let (x, _) = xy(c[0]);
            assert_eq!(x > 0.0, east_side, "subject at x0 {x0}, first pick {}", c[0]);
            assert_eq!(p["subject_px"][0].as_u64().unwrap() as usize, a);
        }
    }

    #[test]
    fn a_subject_across_the_seam_wraps() {
        // The east block straddles azimuth 0, the panorama's seam.
        let (mut walls, east) = block(10.0);
        walls.extend(fence());
        let (a, b) = subject_cols(10.0);
        assert!(a > b, "not across the seam: {a} {b}");
        let pts = scene(&walls, &[east]);
        let first = |s| plan(&pts, GRID, PREVIEW, s)["candidates"].as_array().unwrap().first().map(|c| c["sees_hidden_m2"].as_f64().unwrap()).unwrap_or(0.0);
        // The block's shadow is all there is to see: the box across the seam
        // keeps all of it, the box of everything else (b..a) loses its core.
        let (free, right, wrong) = (first(None), first(Some((a, b))), first(Some((b, a))));
        assert!(right >= 0.95 * free, "{right} m² in the box vs {free} m² unboxed");
        assert!(wrong < 0.8 * right, "{right} m² toward the block vs {wrong} m² away from it");
    }

    #[test]
    fn a_cell_holds_its_centre_and_nothing_beyond_the_radius() {
        for cell in [0.1, 0.37, 0.5] {
            let g = Grid { cell };
            for i in (0..SIDE * SIDE).step_by(97) {
                let (x, y) = g.centre(i);
                assert_eq!(g.cell_of(x, y), Some(i), "cell {cell}, index {i}");
            }
            let r = g.radius();
            for (x, y) in [(r, 0.0), (-r - 0.01, 0.0), (0.0, r), (0.0, -r - 0.01), (r + 5.0, r + 5.0)] {
                assert_eq!(g.cell_of(x, y), None, "({x}, {y}) at cell {cell}");
            }
            assert!(g.cell_of(r - 0.001, r - 0.001).is_some() && g.cell_of(-r, -r) == Some(0));
        }
    }

    #[test]
    fn neighbours_stop_at_the_edge_of_the_plan() {
        let around = |r: usize, c: usize| neighbours(r * SIDE + c).collect::<Vec<_>>();
        assert_eq!(around(0, 0).len(), 3);
        assert_eq!(around(0, 7).len(), 5);
        assert_eq!(around(SIDE - 1, SIDE - 1).len(), 3);
        assert_eq!(around(7, SIDE - 1).len(), 5);
        for (r, c) in [(0, 0), (7, SIDE - 1), (SIDE - 1, 3), (40, 40)] {
            for (j, diag) in around(r, c) {
                let (dr, dc) = ((j / SIDE).abs_diff(r), (j % SIDE).abs_diff(c));
                assert!(dr <= 1 && dc <= 1 && (dr, dc) != (0, 0), "({r}, {c}) → {j}");
                assert_eq!(diag, dr == 1 && dc == 1);
            }
        }
    }

    #[test]
    fn hidden_slivers_are_dropped_and_blocks_kept() {
        let mut mask = vec![false; SIDE * SIDE];
        let set = |m: &mut Vec<bool>, r0: usize, r1: usize, c0: usize, c1: usize| {
            for r in r0..r1 {
                for c in c0..c1 {
                    m[r * SIDE + c] = true;
                }
            }
        };
        set(&mut mask, 10, 13, 10, 13); // 3 × 3: kept whole
        set(&mut mask, 30, 32, 10, 40); // two cells wide: dropped
        set(&mut mask, 0, 2, 60, 90); // two wide along the plan's edge: dropped
        let kept = open(&mask);
        assert_eq!(kept.iter().filter(|k| **k).count(), 9);
        assert!((10..13).all(|r| (10..13).all(|c| kept[r * SIDE + c])));
    }

    #[test]
    fn the_walk_goes_round_and_never_squeezes_a_corner() {
        let g = Grid { cell: 0.1 };
        let mid = SIDE / 2;
        let at = |r: usize, c: usize| r * SIDE + c;
        let open_cost = walk(g, &vec![Kind::Ground; SIDE * SIDE]);
        // Over open ground a step costs 10 straight and 14 on the diagonal.
        assert_eq!(open_cost[at(mid, mid + 61)] - open_cost[at(mid, mid + 60)], 10);
        assert_eq!(open_cost[at(mid + 61, mid + 61)] - open_cost[at(mid + 60, mid + 60)], 14);
        // From the tripod, ~6 m east costs about 6 m of walking.
        let (x, _) = g.centre(at(mid, mid + 60));
        assert!((open_cost[at(mid, mid + 60)] as f64 - x / g.cell * 10.0).abs() <= 10.0);
        // Two walls 4 m north, each with one gap, the gaps a diagonal apart:
        // the only way through squeezes between two standing corners.
        let mut kind = vec![Kind::Ground; SIDE * SIDE];
        for c in 0..SIDE {
            if c != mid {
                kind[at(mid + 40, c)] = Kind::Standing;
            }
            if c != mid + 1 {
                kind[at(mid + 41, c)] = Kind::Standing;
            }
        }
        let cost = walk(g, &kind);
        assert!(cost[at(mid + 39, mid)] < u32::MAX);
        assert_eq!(cost[at(mid + 42, mid + 1)], u32::MAX, "squeezed through the corner");
        // The same with the gaps in line is a door.
        kind[at(mid + 41, mid)] = Kind::Ground;
        assert!(walk(g, &kind)[at(mid + 42, mid + 1)] < u32::MAX);
        // A corridor east from the tripod, then one step that is a diagonal
        // past a single post on either corner: the walk goes round it.
        // Open ground all round, so a corner looked up in the wrong place is
        // ground and lets the cut through.
        for post_on_row in [true, false] {
            let mut kind = vec![Kind::Ground; SIDE * SIDE];
            for c in mid + 5..=mid + 42 {
                kind[at(mid - 1, c)] = Kind::Standing;
                kind[at(mid + 1, c)] = Kind::Standing;
            }
            for r in mid - 1..=mid + 2 {
                kind[at(r, mid + 42)] = Kind::Standing;
            }
            for c in mid + 39..=mid + 42 {
                kind[at(mid + 2, c)] = Kind::Standing;
            }
            let (end, to) = (at(mid, mid + 40), at(mid + 1, mid + 41));
            kind[to] = Kind::Ground;
            let (open, post) = if post_on_row { (at(mid + 1, mid + 40), at(mid, mid + 41)) } else { (at(mid, mid + 41), at(mid + 1, mid + 40)) };
            kind[open] = Kind::Ground;
            kind[post] = Kind::Standing;
            let cost = walk(g, &kind);
            assert!(cost[end] < u32::MAX);
            assert_eq!(cost[to], cost[end] + 20, "cut the corner past the post (post on row: {post_on_row})");
        }
        // A ring of posts 2 m out: ground just beyond it is not a start.
        let mut ring = vec![Kind::Ground; SIDE * SIDE];
        for (i, k) in ring.iter_mut().enumerate() {
            let (x, y) = g.centre(i);
            if (x.hypot(y) - 2.0).abs() < 0.08 {
                *k = Kind::Standing;
            }
        }
        let cost = walk(g, &ring);
        assert!(cost[at(mid, mid + 12)] < u32::MAX);
        assert_eq!(cost[at(mid, mid + 23)], u32::MAX);
        assert_eq!(cost[at(mid + 16, mid + 16)], u32::MAX);
    }

    #[test]
    fn area_is_cells_times_the_cell_squared() {
        assert_eq!(Grid { cell: 0.5 }.area(100), 25.0);
        assert_eq!(Grid { cell: 0.1 }.area(250), 2.5);
        assert_eq!(Grid { cell: 0.2 }.area(3), 0.1);
    }

    #[test]
    fn a_ray_visits_the_cells_it_crosses_in_order() {
        let g = Grid { cell: 0.5 };
        // Directions and starts chosen off the cell corners, so there are no ties.
        for k in 0..37 {
            let a = k as f64 * 0.1737 + 0.031;
            let (dx, dy) = (a.cos(), a.sin());
            let (x, y) = (-3.123 + k as f64 * 0.311, 2.071 - k as f64 * 0.173);
            let mut got = Vec::new();
            g.trace(x, y, dx, dy, 15.0, |j| {
                got.push(j);
                true
            });
            let start = g.cell_of(x, y).unwrap();
            let mut want: Vec<usize> = Vec::new();
            let mut s = 0.0;
            while s <= 15.0 {
                if let Some(j) = g.cell_of(x + s * dx, y + s * dy) {
                    if j != start && want.last() != Some(&j) {
                        want.push(j);
                    }
                }
                s += 0.0005;
            }
            assert_eq!(got, want, "ray {k}");
        }
        // Along the axes: a direction with no y (or no x) never steps that way.
        for (dx, dy, di) in [(1.0, 0.0, 1i64), (-1.0, 0.0, -1), (0.0, 1.0, SIDE as i64), (0.0, -1.0, -(SIDE as i64))] {
            let start = g.cell_of(0.13, 0.27).unwrap() as i64;
            let mut got = Vec::new();
            g.trace(0.13, 0.27, dx, dy, 2.0, |j| {
                got.push(j as i64);
                true
            });
            assert_eq!(got, (1..=4).map(|k| start + k * di).collect::<Vec<_>>(), "along ({dx}, {dy})");
        }
        // A visitor that says stop is never asked again.
        let mut n = 0;
        g.trace(0.1, 0.1, 1.0, 0.0, 30.0, |_| {
            n += 1;
            n < 3
        });
        assert_eq!(n, 3);
    }

    #[test]
    fn a_subject_reaches_fifteen_degrees_past_its_edges() {
        // One point per column all round, at azimuth = column × STEP.
        let pts: Vec<GridPoint> = (0..GRID.0)
            .map(|c| {
                let a = c as f64 * STEP;
                GridPoint { point: Point { x: (10.0 * a.cos()) as f32, y: (10.0 * a.sin()) as f32, z: 0.0, intensity: 0.0 }, row: 0, col: c }
            })
            .collect();
        let deg = |d: f64| d.to_radians();
        let px = |d: f64| (deg(d) / STEP / 2.0) as usize;
        let inside = sector(&pts, GRID, PREVIEW, px(90.0), px(120.0)).unwrap();
        for (b, want) in [(105.0, true), (78.0, true), (132.0, true), (70.0, false), (140.0, false), (285.0, false)] {
            assert_eq!(inside(deg(b)), want, "bearing {b}°");
        }
        // Across the seam: 350° to 10°.
        let wrap = sector(&pts, GRID, PREVIEW, px(350.0), px(10.0)).unwrap();
        for (b, want) in [(0.0, true), (355.0, true), (20.0, true), (340.0, true), (30.0, false), (180.0, false)] {
            assert_eq!(wrap(deg(b)), want, "bearing {b}°");
        }
        assert!(sector(&pts, GRID, (0, 0), 1, 2).is_none());
    }

    #[test]
    fn a_landmark_is_a_post_tall_enough_to_point_at() {
        // A pick at the origin cell, a stump 1 m away, a post 3 m away, a wall 2 m away.
        let g = Grid { cell: 0.5 };
        let mut kind = vec![Kind::Ground; SIDE * SIDE];
        let at = g.cell_of(0.1, 0.1).unwrap();
        let (stump, post) = (g.cell_of(1.1, 0.1).unwrap(), g.cell_of(0.1, 3.1).unwrap());
        let wall: Vec<usize> = (0..10).map(|k| g.cell_of(-2.1, k as f64 * 0.5 - 2.0).unwrap()).collect();
        for &i in [stump, post].iter().chain(&wall) {
            kind[i] = Kind::Standing;
        }
        let (label, size) = things(&kind);
        let mut height = vec![0.0f64; SIDE * SIDE];
        height[stump] = 0.6;
        height[post] = 4.0;
        for &i in &wall {
            height[i] = 3.0;
        }
        let pick = |kind: &[Kind], height: &[f64]| landmark(g, kind, &label, &size, height, at);
        assert_eq!(pick(&kind, &height), Some(post));
        height[post] = 0.6;
        // No post tall enough: the wall, since the stump is too short to see.
        assert_eq!(pick(&kind, &height), Some(*wall.iter().min_by(|a, b| {
            let d = |i: usize| { let (x, y) = g.centre(i); (x - 0.25).hypot(y - 0.25) };
            d(**a).total_cmp(&d(**b))
        }).unwrap()));
        let far = vec![Kind::Ground; SIDE * SIDE];
        assert_eq!(landmark(g, &far, &label, &size, &height, at), None);
        // Beyond reach, even a good post is not this pick's landmark.
        let mut lone = vec![Kind::Ground; SIDE * SIDE];
        let away = g.cell_of(0.1, LANDMARK_REACH + 1.0).unwrap();
        lone[away] = Kind::Standing;
        let (l2, s2) = things(&lone);
        height[away] = 4.0;
        assert_eq!(landmark(g, &lone, &l2, &s2, &height, at), None);
    }

    #[test]
    fn stray_returns_do_not_wall_the_tripod_in() {
        // Station 7 of the admin survey: single returns at body height (a
        // passer-by, edge noise) scattered over the walkway round the tripod.
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        let mut pts = scene(&walls, &[solid]);
        let mut a = 0.0f64;
        while a < TAU {
            for r in [2.0, 2.5, 3.0] {
                let (x, y) = (r * a.cos(), r * a.sin());
                for k in 0..3 {
                    pts.push(GridPoint { point: Point { x: x as f32, y: y as f32, z: (GROUND + 0.8) as f32 + k as f32 * 0.01, intensity: 0.0 }, row: 0, col: 0 });
                }
            }
            a += 0.05;
        }
        let p = plan(&pts, GRID, PREVIEW, None);
        assert!(!picks(&p).is_empty(), "{p}");
    }

    #[test]
    fn a_room_is_planned_on_a_finer_plan_through_its_door() {
        // A 4 m × 5 m room, 2.4 m high, the scanner in it; a desk, and a
        // 0.9 m door in the east wall to a corridor 1.2 m wide.
        let mut walls = vec![
            Wall { a: (-2.0, -2.5), b: (2.0, -2.5), top: 2.4, through: false },
            Wall { a: (-2.0, 2.5), b: (2.0, 2.5), top: 2.4, through: false },
            Wall { a: (-2.0, -2.5), b: (-2.0, 2.5), top: 2.4, through: false },
            Wall { a: (2.0, -2.5), b: (2.0, -0.45), top: 2.4, through: false },
            Wall { a: (2.0, 0.45), b: (2.0, 2.5), top: 2.4, through: false },
            // The corridor: its far wall, and its walls running north and south.
            Wall { a: (3.2, -12.0), b: (3.2, 12.0), top: 2.4, through: false },
            Wall { a: (2.0, 2.5), b: (2.0, 12.0), top: 2.4, through: false },
            Wall { a: (2.0, -2.5), b: (2.0, -12.0), top: 2.4, through: false },
            Wall { a: (2.0, 12.0), b: (3.2, 12.0), top: 2.4, through: false },
            Wall { a: (2.0, -12.0), b: (3.2, -12.0), top: 2.4, through: false },
        ];
        walls.extend(rect(-1.9, 1.2, -0.7, 1.9, 0.75, false));
        let solid = [(-12.0, -12.0, 2.0, -2.5), (-12.0, 2.5, 2.0, 12.0), (-12.0, -12.0, -2.0, 12.0), (3.2, -12.0, 12.0, 12.0), (-1.9, 1.2, -0.7, 1.9)];
        let p = plan(&scene_at(&walls, &solid, 12.0, 0.05, 0.03), GRID, PREVIEW, None);
        assert!(p["cell_m"].as_f64().unwrap() <= 0.15, "{p}");
        let c = picks(&p);
        assert!(!c.is_empty(), "{p}");
        let (x, _) = xy(c[0]);
        // In the doorway or the corridor past it, which the room's walls hid.
        assert!(x > 1.9 && x < 3.2, "{}", c[0]);
        assert!(c[0]["walk_m"].as_f64().unwrap() >= c[0]["distance_m"].as_f64().unwrap() - 0.2);
    }

    #[test]
    fn a_fence_close_round_the_tripod_is_not_walked_through() {
        // A mesh fence 2 m out, nearer than the tripod's own start area.
        let mut walls = fence();
        walls.extend(rect(-2.0, -2.0, 2.0, 2.0, 2.0, true));
        let (more, solid) = block(10.0);
        walls.extend(more);
        let p = plan(&scene(&walls, &[solid]), GRID, PREVIEW, None);
        assert!(picks(&p).is_empty(), "walked through the fence: {p}");
    }

    #[test]
    fn a_preview_with_no_height_or_width_gives_no_pixels() {
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        let pts = scene(&walls, &[solid]);
        for preview in [(2000, 0), (0, 0)] {
            let p = plan(&pts, GRID, preview, None);
            let c = picks(&p);
            assert!(!c.is_empty(), "{p}");
            assert!(c.iter().all(|c| c["preview_px"].is_null()), "{preview:?}: {p}");
        }
    }

    #[test]
    fn a_tripod_walled_in_says_so() {
        let mut walls = fence();
        walls.extend(rect(-3.0, -3.0, 3.0, 3.0, 2.5, true));
        let (more, solid) = block(10.0);
        walls.extend(more);
        let p = plan(&scene(&walls, &[solid]), GRID, PREVIEW, None);
        assert!(picks(&p).is_empty(), "{p}");
        assert_eq!(p["spots_tried"], 0);
        assert!(p["reason"].as_str().unwrap().contains("no open ground"), "{p}");
    }

    #[test]
    fn an_open_field_needs_no_next_station() {
        let p = plan(&scene(&fence(), &[]), GRID, PREVIEW, None);
        assert!(picks(&p).is_empty(), "{p}");
        assert!(p["reason"].as_str().unwrap().contains("nothing hidden"), "{p}");
    }

    #[test]
    fn a_partial_sweep_is_not_planned_from() {
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        let pts: Vec<GridPoint> = scene(&walls, &[solid]).into_iter().filter(|g| g.point.x > 0.0).collect();
        let p = plan(&pts, GRID, PREVIEW, None);
        assert!(picks(&p).is_empty());
        assert!(p["reason"].as_str().unwrap().contains("full dome"), "{p}");
    }

    #[test]
    fn the_pixel_is_the_ground_there_and_the_landmark_a_post() {
        let (mut walls, solid) = block(10.0);
        walls.extend(fence());
        walls.extend(posts(solid));
        let pts = scene(&walls, &[solid]);
        let p = plan(&pts, GRID, PREVIEW, None);
        let c = picks(&p);
        assert!(!c.is_empty(), "{p}");
        let (sx, sy) = (GRID.0 as f64 / PREVIEW.0 as f64, GRID.1 as f64 / PREVIEW.1 as f64);
        let near_px = |px: &Value, want: (f64, f64), z_max: f64| {
            let (col, row) = (px[0].as_f64().unwrap() * sx, px[1].as_f64().unwrap() * sy);
            pts.iter().any(|g| {
                (g.col as f64 - col).abs() <= sx + 1.0
                    && (g.row as f64 - row).abs() <= sy + 1.0
                    && (g.point.x as f64 - want.0).hypot(g.point.y as f64 - want.1) < 0.8
                    && (g.point.z as f64) <= z_max
            })
        };
        // Posts the scan saw (one behind another is hidden).
        let seen_posts: Vec<(f64, f64)> = posts(solid)
            .chunks(4)
            .map(|r| (r[0].a.0 + 0.15, r[0].a.1 + 0.15))
            .filter(|&(x, y)| pts.iter().any(|g| (g.point.x as f64 - x).hypot(g.point.y as f64 - y) < 0.3 && g.point.z as f64 > GROUND + 0.5))
            .collect();
        let mut posts_named = 0;
        for c in c {
            let at = xy(c);
            assert!(near_px(&c["preview_px"], at, GROUND + 0.05), "pixel is not the ground at {at:?}: {c}");
            let l = &c["landmark"];
            assert!(l.is_object(), "no landmark: {c}");
            let lxy = (l["xy_m"][0].as_f64().unwrap(), l["xy_m"][1].as_f64().unwrap());
            if seen_posts.iter().any(|p| (p.0 - at.0).hypot(p.1 - at.1) < LANDMARK_REACH - 0.5) {
                assert!(l["footprint_m2"].as_f64().unwrap() <= 1.0, "a post is in reach, but not named: {c}");
                let h = l["height_m"].as_f64().unwrap();
                assert!((2.8..=3.05).contains(&h), "the posts stand 3 m: {l}");
                posts_named += 1;
            }
            assert_eq!(l["shape"], if l["footprint_m2"].as_f64().unwrap() <= POST_M2 { "post" } else { "mass" }, "{l}");
            assert!((lxy.0 - at.0).hypot(lxy.1 - at.1) <= LANDMARK_REACH + 0.5, "{l}");
            assert!(near_px(&l["preview_px"], lxy, GROUND + 6.1), "landmark pixel is not the landmark: {l}");
        }
        assert!(posts_named > 0, "no pick had a post in reach: {p}");
    }
}
