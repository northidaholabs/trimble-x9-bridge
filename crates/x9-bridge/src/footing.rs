//! Whether a tripod can stand at a spot, from one station's levelled points
//! (scanner at the origin): ground the scan saw across the tripod's
//! footprint, level enough to set up on, even, nothing standing in it, and
//! not on top of something raised.
//!
//! The points go into 0.1 m cells. Each keeps its ground: the second lowest
//! point once it has three, so a stray return below the ground (a
//! reflection, a mixed pixel) doesn't count; else the lowest. And how many
//! points stand 0.4 to 2 m above that. A spot is judged on the cells within
//! the legs' reach:
//!
//! - *clear*: no wall, furniture, plant or post rising in it;
//! - *seen*: enough cells with ground, some in each of the footprint's
//!   quarters (nearer and farther than the spot, and at least 0.12 m either
//!   side of the line to it), spread at least 0.3 m along that line, so one
//!   scan line across it is not taken for a floor, nor a footprint half in
//!   the shadow of something. Far out the scan lines on the ground are
//!   tens of centimetres apart along the line, so two of them will do;
//! - *level*: the plane fitted to the cells slopes 15° at most;
//! - *even*: no two cells more than 0.2 m off that plane (a step, a stair, a
//!   hole), and nine in ten within 0.1 m of it (not low plants or rubble);
//! - *on the ground*: the ground seen within 1 m doesn't drop more than
//!   0.4 m below it (not a table, a wall top, a vehicle, or the edge of a
//!   drop), and something was seen beyond it, farther along the line from
//!   the scanner: seen from above, the ground below an edge is in its
//!   shadow, so a spot at the edge has nothing seen past it.
//!
//! A cell whose ground is well above the rest holds only something overhead
//! (eaves, canopy): its ground was not seen, and it doesn't count against
//! the spot either.

use trimble_tzf::GridPoint;

/// How far the tripod's legs reach from the spot (m), how far round it the
/// ground is looked at for a drop (m), and how far past it, away from the
/// scanner, something must have been seen (m). `REACH` covers them all.
pub const FOOT: f64 = 0.5;
pub const AROUND: f64 = 1.0;
const BEYOND: f64 = 1.5;
pub const REACH: f64 = BEYOND;
/// The cell (m).
const FINE: f64 = 0.1;
/// Cells with ground under the footprint; how far either side of the line
/// to the spot (m) a cell must be to stand for its quarter; and how far the
/// cells must spread along that line (m).
const MIN_CELLS: usize = 8;
const ACROSS_OFF: f64 = 0.12;
const ALONG_SPREAD: f64 = 0.3;
/// Steepest ground to set up on (degrees).
pub const MAX_SLOPE_DEG: f64 = 15.0;
/// A cell this far off the fitted ground (m) is a step or an edge; this many
/// of them make the spot uneven.
const LUMP: f64 = 0.2;
const LUMP_CELLS: usize = 2;
/// How far (m) most cells may sit off a steep fit for it to be a slope
/// rather than a step.
const SPREAD: f64 = 0.05;
/// Rough: one cell in ten this far (m) off the fitted ground (low plants,
/// rubble). Lawn and paving stay within a few centimetres.
const ROUGH: f64 = 0.1;
/// Points this high above a cell's ground (m) stand in the footprint
/// (higher than grass, lower than the scanner's head); a cell with this
/// many is taken.
const OVER: (f32, f32) = (0.4, 2.0);
const OVER_POINTS: u16 = 3;
/// A cell whose ground is this far (m) above the footprint's lower quartile
/// holds only something overhead.
const OVERHEAD: f64 = 1.8;
/// Ground round the footprint this far (m) below its plane, in this many
/// cells of at least `DROP_POINTS` points each, means the spot is on
/// something raised or at a drop.
const DROP: f64 = 0.4;
const DROP_CELLS: usize = 3;
const DROP_POINTS: u16 = 2;
/// Beyond the footprint along the line from the scanner (from this far past
/// the spot, m, so a leg isn't at the edge), within this far either side of
/// the line (m), at least this many cells of ground near the spot's level
/// (no more than `DROP` below its plane nor `ABOVE` above it: not branches
/// or canopy over a drop) must have been seen.
const BEYOND_FROM: f64 = 0.6;
const BEYOND_ACROSS: f64 = 0.3;
const BEYOND_CELLS: usize = 3;
const ABOVE: f64 = 0.25;

/// A footprint cell: offset from the spot (m), ground (m), points over it.
type Cell = (f64, f64, f64, u16);

/// What the scan says about the ground at a spot.
#[derive(Debug, Clone, PartialEq)]
pub struct Footing {
    /// Why a tripod can't stand there, or `None` when it can.
    pub why: Option<String>,
    /// The slope of the ground fitted under the footprint (degrees).
    pub slope_deg: f64,
    /// The height of that ground at the spot (m, the scan's frame).
    pub ground_z: f64,
}

/// The 0.1 m cells over a square of the plan.
pub struct Cells {
    x0: f64,
    y0: f64,
    side: usize,
    /// The two lowest points and how many points (saturating).
    low: Vec<(f32, f32, u16)>,
    over: Vec<u16>,
}

const TOO_LITTLE: &str = "the scan saw too little of the ground there";

impl Cells {
    /// The cells over (at least) the square `half` metres either side of
    /// `(cx, cy)`. Every set of cells lies on the same 0.1 m lattice from the
    /// scanner, so a spot is judged the same whichever covers it.
    pub fn new(pts: &[GridPoint], cx: f64, cy: f64, half: f64) -> Cells {
        let (x0, y0) = (((cx - half) / FINE).floor() * FINE, ((cy - half) / FINE).floor() * FINE);
        let side = (((2.0 * half / FINE).ceil() as usize) + 1).max(1);
        let mut c = Cells { x0, y0, side, low: vec![(f32::INFINITY, f32::INFINITY, 0); side * side], over: vec![0; side * side] };
        for p in pts {
            if let Some(i) = c.at(p.point.x as f64, p.point.y as f64) {
                let (a, b, n) = &mut c.low[i];
                let z = p.point.z;
                if z < *a {
                    (*a, *b) = (z, *a);
                } else if z < *b {
                    *b = z;
                }
                *n = n.saturating_add(1);
            }
        }
        for p in pts {
            if let Some(i) = c.at(p.point.x as f64, p.point.y as f64) {
                let h = p.point.z - c.ground(i);
                if h >= OVER.0 && h <= OVER.1 {
                    c.over[i] = c.over[i].saturating_add(1);
                }
            }
        }
        c
    }

    fn at(&self, x: f64, y: f64) -> Option<usize> {
        let (cx, cy) = (((x - self.x0) / FINE).floor(), ((y - self.y0) / FINE).floor());
        (cx >= 0.0 && cy >= 0.0 && (cx as usize) < self.side && (cy as usize) < self.side).then(|| cy as usize * self.side + cx as usize)
    }

    /// A cell's ground: its second lowest point once it has three, else its
    /// lowest (infinite when it has none).
    fn ground(&self, i: usize) -> f32 {
        let (a, b, n) = self.low[i];
        if n >= 3 { b } else { a }
    }

    /// The lower quartile of the ground of the cells whose centres fall in
    /// the square from `(x, y)` `size` metres on, if any has ground: clear of
    /// one stray low return and of anything standing on the ground.
    pub fn ground_in(&self, x: f64, y: f64, size: f64) -> Option<f32> {
        let n = ((size / FINE).round() as i64).max(1);
        let (c0, r0) = (((x - self.x0) / FINE + 0.5).floor() as i64, ((y - self.y0) / FINE + 0.5).floor() as i64);
        let mut v: Vec<f32> = Vec::new();
        for r in r0.max(0)..(r0 + n).min(self.side as i64) {
            for c in c0.max(0)..(c0 + n).min(self.side as i64) {
                let z = self.ground(r as usize * self.side + c as usize);
                if z.is_finite() {
                    v.push(z);
                }
            }
        }
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        Some(v[v.len() / 4])
    }

    /// Whether a tripod can stand at `(x, y)`.
    pub fn judge(&self, x: f64, y: f64) -> Footing {
        let unseen = || Footing { why: Some(TOO_LITTLE.into()), slope_deg: 0.0, ground_z: f64::NAN };
        // Along the line from the scanner, and across it.
        let d = x.hypot(y);
        let (ux, uy) = if d > 1e-6 { (x / d, y / d) } else { (1.0, 0.0) };
        let reach = (REACH / FINE).ceil() as i64 + 1;
        let Some(mid) = self.at(x, y) else { return unseen() };
        let (mc, mr) = ((mid % self.side) as i64, (mid / self.side) as i64);
        // The cells within the footprint, round it, and past it (with how many points each has).
        let (mut cells, mut round, mut ahead): (Vec<Cell>, Vec<Cell>, Vec<Cell>) = (Vec::new(), Vec::new(), Vec::new());
        for r in mr - reach..=mr + reach {
            for c in mc - reach..=mc + reach {
                if r < 0 || c < 0 || r as usize >= self.side || c as usize >= self.side {
                    continue;
                }
                let i = r as usize * self.side + c as usize;
                let (cx, cy) = (self.x0 + (c as f64 + 0.5) * FINE - x, self.y0 + (r as f64 + 0.5) * FINE - y);
                let (z, e) = (self.ground(i), cx.hypot(cy));
                if !z.is_finite() {
                    continue;
                }
                let (along, across) = (cx * ux + cy * uy, cy * ux - cx * uy);
                if e <= FOOT {
                    cells.push((cx, cy, z as f64, self.over[i]));
                } else if e <= AROUND {
                    round.push((cx, cy, z as f64, self.low[i].2));
                }
                if along > BEYOND_FROM && along <= BEYOND && across.abs() <= BEYOND_ACROSS {
                    ahead.push((cx, cy, z as f64, self.low[i].2));
                }
            }
        }
        if cells.is_empty() {
            return unseen();
        }
        let mut zs: Vec<f64> = cells.iter().map(|c| c.2).collect();
        zs.sort_by(|a, b| a.total_cmp(b));
        let base = zs[zs.len() / 4];
        cells.retain(|c| c.2 <= base + OVERHEAD);
        // Something standing in it says more than how much ground was seen.
        if cells.iter().any(|c| c.3 >= OVER_POINTS) {
            return Footing { why: Some(format!("something stands within {FOOT} m of it (a wall, furniture, a plant, a post)")), slope_deg: 0.0, ground_z: base };
        }
        let (mut quarters, mut lo, mut hi) = ([false; 4], f64::INFINITY, f64::NEG_INFINITY);
        for &(cx, cy, _, _) in &cells {
            let (along, across) = (cx * ux + cy * uy, cy * ux - cx * uy);
            (lo, hi) = (lo.min(along), hi.max(along));
            if across.abs() >= ACROSS_OFF {
                quarters[(along > 0.0) as usize * 2 + (across > 0.0) as usize] = true;
            }
        }
        if cells.len() < MIN_CELLS || !quarters.iter().all(|&q| q) || hi - lo < ALONG_SPREAD {
            return unseen();
        }
        let Some((a, b, z0)) = plane(&cells) else { return unseen() };
        let slope_deg = a.hypot(b).atan().to_degrees();
        let at = |why: Option<String>| Footing { why, slope_deg, ground_z: z0 };
        let mut off: Vec<f64> = cells.iter().map(|&(cx, cy, z, _)| (z - (z0 + a * cx + b * cy)).abs()).collect();
        off.sort_by(|a, b| a.total_cmp(b));
        let uneven = || at(Some("the ground there is uneven: a step, low plants, rubble or an edge under the tripod".into()));
        if slope_deg > MAX_SLOPE_DEG {
            // A plane tilted across a step fits neither level: that is a step, not a slope.
            return if off[off.len() * 9 / 10] > SPREAD { uneven() } else { at(Some(format!("the ground there slopes {slope_deg:.0}°, more than {MAX_SLOPE_DEG:.0}°"))) };
        }
        if off.iter().filter(|&&o| o > LUMP).count() >= LUMP_CELLS || off[off.len() * 9 / 10] > ROUGH {
            return uneven();
        }
        if round.iter().filter(|&&(cx, cy, z, n)| n >= DROP_POINTS && z < z0 + a * cx + b * cy - DROP).count() >= DROP_CELLS {
            return at(Some(format!("it is on something raised (a table, a wall top, a vehicle), within {AROUND} m of a drop, or by water or glass that shows ground below it")));
        }
        let level = |&&(cx, cy, z, _): &&Cell| (-DROP..=ABOVE).contains(&(z - (z0 + a * cx + b * cy)));
        if ahead.iter().filter(level).count() < BEYOND_CELLS {
            return at(Some(format!("no ground was seen within {BEYOND} m beyond it, as at an edge seen from above")));
        }
        at(None)
    }
}

/// Least-squares plane `z = z0 + a·x + b·y` through the cells (offsets from
/// the spot), or `None` when they don't span an area.
fn plane(cells: &[Cell]) -> Option<(f64, f64, f64)> {
    let n = cells.len() as f64;
    let (mut sx, mut sy, mut sz, mut sxx, mut syy, mut sxy, mut sxz, mut syz) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for &(x, y, z, _) in cells {
        sx += x;
        sy += y;
        sz += z;
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
        sxz += x * z;
        syz += y * z;
    }
    let (mx, my, mz) = (sx / n, sy / n, sz / n);
    let (cxx, cyy, cxy, cxz, cyz) = (sxx / n - mx * mx, syy / n - my * my, sxy / n - mx * my, sxz / n - mx * mz, syz / n - my * mz);
    let det = cxx * cyy - cxy * cxy;
    // Cells spread at least ~0.1 m both ways.
    if det < 1e-4 {
        return None;
    }
    let a = (cxz * cyy - cyz * cxy) / det;
    let b = (cyz * cxx - cxz * cxy) / det;
    Some((a, b, mz - a * mx - b * my))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trimble_tzf::Point;

    fn p(x: f64, y: f64, z: f64) -> GridPoint {
        GridPoint { point: Point { x: x as f32, y: y as f32, z: z as f32, intensity: 0.5 }, row: 0, col: 0 }
    }

    /// Ground every `step` m over the square `half` either side of `(cx, cy)`, at `z(x, y)`.
    fn ground(cx: f64, cy: f64, half: f64, step: f64, z: impl Fn(f64, f64) -> f64) -> Vec<GridPoint> {
        let n = (2.0 * half / step).round() as i64;
        let mut v = Vec::new();
        for i in 0..=n {
            for j in 0..=n {
                let (x, y) = (cx - half + i as f64 * step, cy - half + j as f64 * step);
                v.push(p(x, y, z(x, y)));
            }
        }
        v
    }

    fn judge(pts: &[GridPoint], x: f64, y: f64) -> Footing {
        Cells::new(pts, x, y, REACH + 0.2).judge(x, y)
    }

    #[test]
    fn level_ground_seen_all_round_takes_a_tripod() {
        let f = judge(&ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6), 10.0, 0.0);
        assert_eq!(f.why, None, "{f:?}");
        assert!(f.slope_deg < 0.5 && (f.ground_z + 1.6).abs() < 1e-3, "{f:?}");
    }

    #[test]
    fn grass_and_a_gentle_slope_are_fine() {
        // 10° up the line, with blades of grass a few cm above the soil.
        let t = 10f64.to_radians().tan();
        let mut pts = ground(10.0, 0.0, 1.0, 0.03, |x, _| -1.6 + (x - 10.0) * t);
        pts.extend(ground(10.0, 0.0, 1.0, 0.05, |x, y| -1.6 + (x - 10.0) * t + 0.04 * ((x * 37.0).sin() * (y * 23.0).cos()).abs()));
        let f = judge(&pts, 10.0, 0.0);
        assert_eq!(f.why, None, "{f:?}");
        assert!((f.slope_deg - 10.0).abs() < 1.0, "{f:?}");
    }

    #[test]
    fn a_steep_bank_is_refused_with_its_slope() {
        let t = 25f64.to_radians().tan();
        let f = judge(&ground(10.0, 0.0, 1.0, 0.03, |_, y| -1.6 + y * t), 10.0, 0.0);
        assert!(f.why.as_deref().unwrap().contains("slopes 25°"), "{f:?}");
    }

    fn ring(r: f64, z: f64) -> impl Iterator<Item = GridPoint> {
        (0..400).map(move |k| {
            let a = (k as f64 - 200.0) * 0.0005;
            p(r * a.cos(), r * a.sin(), z)
        })
    }

    fn why(f: &Footing) -> &str {
        f.why.as_deref().unwrap_or("standable")
    }

    #[test]
    fn a_few_returns_or_one_scan_line_are_not_ground_seen() {
        // Three stray returns.
        let f = judge(&[p(10.0, 0.0, -1.6), p(10.2, 0.1, -1.6), p(9.9, -0.2, -1.6)], 10.0, 0.0);
        assert!(why(&f).contains("too little"), "{f:?}");
        // One scan line across the spot (a ring round the scanner), through
        // it or just off it, however the cells fall.
        for r in [10.0, 10.005, 10.05, 10.1] {
            let f = judge(&ring(r, -1.6).collect::<Vec<_>>(), 10.0, 0.0);
            assert!(why(&f).contains("too little"), "ring at {r}: {f:?}");
        }
        // The same on a diagonal bearing.
        let diag: Vec<GridPoint> = ring(10.0, -1.6).map(|g| {
            let (c, s) = (std::f32::consts::FRAC_PI_4.cos(), std::f32::consts::FRAC_PI_4.sin());
            p((g.point.x * c - g.point.y * s) as f64, (g.point.x * s + g.point.y * c) as f64, -1.6)
        }).collect();
        let at = 10.0 * std::f64::consts::FRAC_1_SQRT_2;
        assert!(why(&judge(&diag, at, at)).contains("too little"));
        // Far out, flat ground shows as scan lines 0.5 to 0.8 m apart: two
        // across the footprint and one beyond it are enough, wherever they cross.
        for k in 0..10 {
            // At 0.75 m two lines cross the footprint only when the first is near its near edge.
            let (first, gap) = if k % 2 == 0 { (9.6 + 0.03 * k as f64, 0.5) } else { (9.6 + 0.01 * k as f64, 0.75) };
            let lines: Vec<GridPoint> = (0..4).flat_map(|j| ring(first + gap * j as f64, -1.6)).collect();
            assert_eq!(judge(&lines, 10.0, 0.0).why, None, "lines from {first} m");
        }
        // Nothing at all.
        assert!(why(&judge(&[], 10.0, 0.0)).contains("too little"));
    }

    #[test]
    fn a_wall_beside_the_spot_or_furniture_over_it_blocks_it() {
        let floor = ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6);
        let wall = |x: f64| (0..60).flat_map(move |k| (0..90).map(move |h| p(x, -0.9 + k as f64 * 0.03, -1.6 + h as f64 * 0.033)));
        // A wall 0.3 m from the spot, 3 m tall.
        let walled: Vec<GridPoint> = floor.iter().copied().chain(wall(10.3)).collect();
        assert!(why(&judge(&walled, 10.0, 0.0)).contains("something stands"));
        // A table top 0.75 m over the floor, the floor seen under it.
        let mut table = floor.clone();
        table.extend(ground(10.0, 0.0, 0.4, 0.03, |_, _| -0.85));
        assert!(why(&judge(&table, 10.0, 0.0)).contains("something stands"));
        // A post 5 cm across, 2 m tall, in one cell.
        let mut post = floor.clone();
        post.extend((0..40).map(|h| p(10.21, 0.21, -1.6 + h as f64 * 0.05)));
        assert!(why(&judge(&post, 10.0, 0.0)).contains("something stands"));
        // The wall 0.7 m away is outside the footprint.
        let clear: Vec<GridPoint> = floor.iter().copied().chain(wall(10.7)).collect();
        assert_eq!(judge(&clear, 10.0, 0.0).why, None);
    }

    #[test]
    fn stray_low_returns_grass_and_a_kerb_are_still_ground() {
        // Two returns 0.35 m below the pavement, in two cells.
        let mut pts = ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6);
        pts.extend([p(10.12, 0.12, -1.95), p(9.88, -0.18, -1.95)]);
        assert_eq!(judge(&pts, 10.0, 0.0).why, None);
        // Grass 0.3 m tall over the soil.
        let mut grass = ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6);
        grass.extend(ground(10.0, 0.0, 1.0, 0.04, |x, y| -1.6 + 0.3 * ((x * 41.0).sin() * (y * 29.0).cos()).abs()));
        assert_eq!(judge(&grass, 10.0, 0.0).why, None);
        // A 0.15 m kerb through the spot: a tripod straddles it.
        let kerb = ground(10.0, 0.0, 1.0, 0.03, |_, y| if y > 0.0 { -1.45 } else { -1.6 });
        assert_eq!(judge(&kerb, 10.0, 0.0).why, None);
    }

    #[test]
    fn a_step_or_stairs_under_the_tripod_are_uneven() {
        // A 0.3 m step under the tripod: uneven, not a slope.
        let f = judge(&ground(10.0, 0.0, 1.0, 0.03, |x, _| if x > 10.1 { -1.3 } else { -1.6 }), 10.0, 0.0);
        assert!(why(&f).contains("uneven"), "{f:?}");
        // Stairs: 0.18 m risers every 0.28 m.
        let f = judge(&ground(10.0, 0.0, 1.0, 0.02, |x, _| -1.6 + 0.18 * ((x - 9.0) / 0.28).floor()), 10.0, 0.0);
        assert!(why(&f).contains("uneven"), "{f:?}");
        // The edge of a table top 0.75 m up, its floor not seen.
        let mut pts = ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6);
        pts.retain(|g| !((g.point.x as f64) < 10.2 && (g.point.x as f64) > 9.0));
        pts.extend(ground(9.6, 0.0, 0.6, 0.03, |_, _| -0.85));
        assert!(why(&judge(&pts, 10.0, 0.0)).contains("uneven"));
    }

    /// Floor at -1.6 seen round a flat top at `z` over the square `half`
    /// either side of (10, 0), the floor under it not seen.
    fn raised(half: f64, z: impl Fn(f64, f64) -> f64) -> Vec<GridPoint> {
        let mut pts = ground(10.0, 0.0, 2.0, 0.03, |_, _| -1.6);
        pts.retain(|g| ((g.point.x as f64) - 10.0).abs() > half || (g.point.y as f64).abs() > half);
        pts.extend(ground(10.0, 0.0, half, 0.03, z));
        pts
    }

    #[test]
    fn a_table_top_a_car_roof_or_a_hedge_is_not_the_ground() {
        // A 1.2 m table top 0.75 m up.
        let f = judge(&raised(0.6, |_, _| -0.85), 10.0, 0.0);
        assert!(why(&f).contains("raised"), "{f:?}");
        // A car roof 1.45 m up, slightly crowned.
        let f = judge(&raised(0.65, |x, y| -0.15 - 0.05 * ((x - 10.0).powi(2) + y * y)), 10.0, 0.0);
        assert!(why(&f).contains("raised"), "{f:?}");
        // A hedge top 1 m up, bumpy by a few centimetres.
        let f = judge(&raised(0.6, |x, y| -0.6 + 0.04 * ((x * 13.0).sin() + (y * 11.0).cos())), 10.0, 0.0);
        assert!(why(&f).contains("raised"), "{f:?}");
        // A loading dock 1.2 m up: 0.7 m back from its edge is too near the drop, 1.5 m back is fine.
        let dock = |back: f64| {
            let mut pts = ground(10.0, 0.0, 2.0, 0.03, |x, _| if x < 10.0 - back { -1.6 } else { -0.4 });
            pts.retain(|g| !((g.point.x as f64) < 10.0 - back && (g.point.x as f64) > 10.0 - back - 0.2));
            judge(&pts, 10.0, 0.0)
        };
        assert!(why(&dock(0.7)).contains("drop"));
        assert_eq!(dock(1.5).why, None);
    }

    #[test]
    fn the_edge_of_a_drop_seen_from_above_is_refused() {
        // Ground only out to the edge; the ground below it in its shadow.
        for edge in [0.05, 0.1, 0.2, 0.3, 0.45] {
            let mut pts = ground(10.0, 0.0, 2.0, 0.03, |_, _| -1.6);
            pts.retain(|g| (g.point.x as f64) < 10.0 + edge);
            let f = judge(&pts, 10.0, 0.0);
            assert!(why(&f).contains("beyond it"), "edge {edge} m out: {f:?}");
        }
        // The same on a 30° bearing.
        let (c, s) = (30f64.to_radians().cos(), 30f64.to_radians().sin());
        let (x, y) = (10.0 * c, 10.0 * s);
        let mut pts = ground(x, y, 2.0, 0.03, |_, _| -1.6);
        pts.retain(|g| (g.point.x as f64 - x) * c + (g.point.y as f64 - y) * s < 0.1);
        assert!(why(&judge(&pts, x, y)).contains("beyond it"));
        // Ground seen a metre past it is fine.
        let mut pts = ground(10.0, 0.0, 2.0, 0.03, |_, _| -1.6);
        pts.retain(|g| (g.point.x as f64) < 11.0);
        assert_eq!(judge(&pts, 10.0, 0.0).why, None);
        // A branch or canopy over the drop isn't ground beyond it.
        let mut pts = ground(10.0, 0.0, 2.0, 0.03, |_, _| -1.6);
        pts.retain(|g| (g.point.x as f64) < 10.1);
        let mut branch = pts.clone();
        branch.extend(ground(10.9, 0.0, 0.3, 0.03, |_, _| -1.3));
        assert!(why(&judge(&branch, 10.0, 0.0)).contains("beyond it"));
        pts.extend(ground(11.0, 0.0, 0.5, 0.03, |_, _| 0.9));
        assert!(why(&judge(&pts, 10.0, 0.0)).contains("beyond it"));
        // A wall 0.8 m past it, its foot seen, counts as seen.
        let mut pts = ground(10.0, 0.0, 2.0, 0.03, |_, _| -1.6);
        pts.retain(|g| (g.point.x as f64) < 10.8);
        pts.extend((0..40).flat_map(|k| (0..60).map(move |h| p(10.8, -0.6 + k as f64 * 0.03, -1.6 + h as f64 * 0.04))));
        assert_eq!(judge(&pts, 10.0, 0.0).why, None);
    }

    #[test]
    fn low_plants_or_rubble_are_rough() {
        // Lumps up to 0.25 m over a sparse bed, as a shrub border seen from 20 m.
        let bumpy = ground(10.0, 0.0, 1.0, 0.1, |x, y| -1.6 + 0.25 * ((x * 7.3).sin() * (y * 5.1).cos()).abs());
        let f = judge(&bumpy, 10.0, 0.0);
        assert!(why(&f).contains("uneven"), "{f:?}");
    }

    #[test]
    fn eaves_overhead_are_not_counted_against_the_ground() {
        // Ground all round, and a roof 2.7 m above part of it, its ground seen too.
        let mut pts = ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6);
        pts.extend(ground(10.0, 0.5, 0.5, 0.03, |_, _| 1.1));
        assert_eq!(judge(&pts, 10.0, 0.0).why, None);
        // The roof alone where the ground under it wasn't seen: half the footprint unseen.
        let mut half = pts.clone();
        half.retain(|g| !(g.point.y > 0.0 && g.point.z < 0.0));
        let f = judge(&half, 10.0, 0.0);
        assert!(why(&f).contains("too little"), "{f:?}");
        // Ground seen 0.15 m past the line to the spot is just enough.
        pts.retain(|g| !(g.point.y > 0.15 && g.point.z < 0.0));
        assert_eq!(judge(&pts, 10.0, 0.0).why, None);
    }

    #[test]
    fn a_cells_ground_is_the_lower_quartile_of_the_fine_cells_in_it() {
        let mut pts = ground(10.0, 0.0, 1.0, 0.03, |_, _| -1.6);
        pts.push(p(10.05, 0.05, -3.0));
        pts.extend((0..9).map(|k| p(10.02 + 0.05 * k as f64, 0.33, -0.8)));
        let c = Cells::new(&pts, 10.0, 0.0, 1.0);
        assert!((c.ground_in(10.0, 0.0, 0.5).unwrap() + 1.6).abs() < 1e-6);
        assert_eq!(c.ground_in(30.0, 0.0, 0.5), None);
    }

    #[test]
    fn the_plane_needs_cells_that_span_an_area() {
        assert!(plane(&[(0.0, 0.0, 1.0, 0), (0.1, 0.0, 1.0, 0), (0.2, 0.0, 1.0, 0)]).is_none());
        let (a, b, z0) = plane(&[(0.0, 0.0, 1.0, 0), (0.3, 0.0, 1.3, 0), (0.0, 0.3, 0.7, 0), (0.3, 0.3, 1.0, 0)]).unwrap();
        assert!((a - 1.0).abs() < 1e-9 && (b + 1.0).abs() < 1e-9 && (z0 - 1.0).abs() < 1e-9);
    }
}
