//! A scan's world in its own picture. For a point in the scan's frame (or the
//! project's), where it falls on the scan's preview, how far it is, and
//! whether the scan saw something nearer in that direction, so it is hidden.
//! The web UI draws real-size things with it: tripods on the proposed spots,
//! the walk there, earlier stations, the faces asked about, distance rings.
//!
//! Nothing about the scanner is assumed: the preview's mapping from azimuth
//! and elevation to pixels is fitted from the scan's own points, as
//! `station::ground_at` does for one marker.

use std::collections::HashMap;
use trimble_tzf::GridPoint;

/// Preview pixels per cell of the depth image.
const CELL: usize = 2;
/// Ground plan cells (m).
const GROUND_CELL: f64 = 0.5;
/// A point is hidden when the scan saw something this much nearer in its
/// direction: at least 0.3 m or 6% of its range, plus what a cell's sliver of
/// elevation spans on ground seen at a grazing angle (see `project`).
const HIDDEN_M: f64 = 0.3;
const HIDDEN_SHARE: f64 = 0.06;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Seen {
    /// The scan saw nothing nearer that way.
    Visible,
    /// Something the scan saw stands in front of it.
    Hidden,
    /// The scan saw nothing at all that way (sky, or out of range).
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projected {
    pub px: (f64, f64),
    pub range_m: f64,
    pub seen: Seen,
}

pub struct DepthView {
    pub preview: (usize, usize),
    /// preview x = (x0 + kx·azimuth) mod width; preview y = y0 + ky·elevation.
    x0: f64,
    kx: f64,
    y0: f64,
    ky: f64,
    /// The nearest return per cell (0: none).
    near: Vec<f32>,
    nw: usize,
    nh: usize,
    /// The lowest return per ground cell below the scanner, with how many.
    ground: HashMap<(i32, i32), (f32, u32)>,
    /// The ground under the tripod (z, negative): for places it didn't see.
    pub floor: f64,
}

impl DepthView {
    /// From a scan's points (`grid`: its columns and rows) and its preview's
    /// size. `None` when the points don't give a mapping.
    pub fn build(pts: &[GridPoint], grid: (u32, u32), preview: (usize, usize)) -> Option<DepthView> {
        let (pw, ph) = (preview.0 as f64, preview.1 as f64);
        if pw < 8.0 || ph < 8.0 || grid.0 == 0 || grid.1 == 0 || pts.len() < 1000 {
            return None;
        }
        let (fx, fy) = (pw / grid.0 as f64, ph / grid.1 as f64);
        let tau = std::f64::consts::TAU;
        let step = (pts.len() / 200_000).max(1);
        let sample: Vec<(f64, f64, f64, f64)> = pts
            .iter()
            .step_by(step)
            .filter_map(|g| {
                let (x, y, z) = (g.point.x as f64, g.point.y as f64, g.point.z as f64);
                let out = x.hypot(y);
                (out > 0.5 && x.is_finite() && y.is_finite() && z.is_finite()).then(|| (y.atan2(x), z.atan2(out), g.col as f64 * fx, g.row as f64 * fy))
            })
            .collect();
        if sample.len() < 500 {
            return None;
        }
        // Across: a full turn spans the preview's width, one way or the other.
        // The offset is the circular mean of what's left; the sign is the one
        // that leaves it tight.
        let kmag = pw / tau;
        let fit_x = |k: f64| {
            let (s, c) = sample.iter().fold((0.0, 0.0), |(s, c), &(az, _, px, _)| {
                let a = (px - k * az) / pw * tau;
                (s + a.sin(), c + a.cos())
            });
            let n = sample.len() as f64;
            ((s.atan2(c) / tau * pw).rem_euclid(pw), (s * s + c * c).sqrt() / n)
        };
        let ((xa, ra), (xb, rb)) = (fit_x(kmag), fit_x(-kmag));
        let (x0, kx, tight) = if ra >= rb { (xa, kmag, ra) } else { (xb, -kmag, rb) };
        if tight < 0.9 {
            return None;
        }
        // Down: least squares of row on elevation.
        let n = sample.len() as f64;
        let (me, mp) = (sample.iter().map(|s| s.1).sum::<f64>() / n, sample.iter().map(|s| s.3).sum::<f64>() / n);
        let (sep, see) = sample.iter().fold((0.0, 0.0), |(a, b), s| (a + (s.1 - me) * (s.3 - mp), b + (s.1 - me) * (s.1 - me)));
        if see < 1e-9 {
            return None;
        }
        let ky = sep / see;
        let y0 = mp - ky * me;
        if ky.abs() < 1.0 {
            return None;
        }

        let (nw, nh) = (preview.0.div_ceil(CELL), preview.1.div_ceil(CELL));
        let mut near = vec![0f32; nw * nh];
        let mut ground: HashMap<(i32, i32), (f32, u32)> = HashMap::new();
        for g in pts {
            let (x, y, z) = (g.point.x, g.point.y, g.point.z);
            if !(x.is_finite() && y.is_finite() && z.is_finite()) {
                continue;
            }
            let r = (x * x + y * y + z * z).sqrt();
            if r < 0.3 {
                continue;
            }
            let (cx, cy) = ((g.col as f64 * fx) as usize / CELL, (g.row as f64 * fy) as usize / CELL);
            if cx < nw && cy < nh {
                let c = &mut near[cy * nw + cx];
                if *c == 0.0 || r < *c {
                    *c = r;
                }
            }
            if z < -0.4 && x.hypot(y) < 80.0 {
                let k = ((x as f64 / GROUND_CELL).floor() as i32, (y as f64 / GROUND_CELL).floor() as i32);
                let e = ground.entry(k).or_insert((f32::MAX, 0));
                e.0 = e.0.min(z);
                e.1 += 1;
            }
        }
        // The ground round the tripod, past its legs.
        let mut close: Vec<f32> = ground
            .iter()
            .filter(|(k, v)| v.1 >= 3 && {
                let (x, y) = ((k.0 as f64 + 0.5) * GROUND_CELL, (k.1 as f64 + 0.5) * GROUND_CELL);
                (0.8..3.5).contains(&x.hypot(y))
            })
            .map(|(_, v)| v.0)
            .collect();
        close.sort_by(|a, b| a.total_cmp(b));
        let floor = close.get(close.len() / 2).map_or(-1.6, |z| *z as f64);
        Some(DepthView { preview, x0, kx, y0, ky, near, nw, nh, ground, floor })
    }

    /// The ground's height (scan frame) under (x, y): the lowest return there
    /// or nearby, else the ground under the tripod.
    pub fn ground_z(&self, x: f64, y: f64) -> f64 {
        let (cx, cy) = ((x / GROUND_CELL).floor() as i32, (y / GROUND_CELL).floor() as i32);
        for reach in 0..=3 {
            let mut lowest: Option<f32> = None;
            for dx in -reach..=reach {
                for dy in -reach..=reach {
                    if let Some(&(z, n)) = self.ground.get(&(cx + dx, cy + dy)) {
                        if n >= 3 {
                            lowest = Some(lowest.map_or(z, |l| l.min(z)));
                        }
                    }
                }
            }
            if let Some(z) = lowest {
                return z as f64;
            }
        }
        self.floor
    }

    /// Where a point (scan frame) falls on the preview, or `None` when it is
    /// at the scanner or outside the picture's rows.
    pub fn project(&self, p: [f64; 3]) -> Option<Projected> {
        let (x, y, z) = (p[0], p[1], p[2]);
        let out = x.hypot(y);
        let range_m = (out * out + z * z).sqrt();
        if !range_m.is_finite() || range_m < 0.2 {
            return None;
        }
        let (pw, ph) = (self.preview.0 as f64, self.preview.1 as f64);
        let u = (self.x0 + self.kx * y.atan2(x)).rem_euclid(pw);
        let v = self.y0 + self.ky * z.atan2(out);
        if !(0.0..ph).contains(&v) {
            return None;
        }
        let (cx, cy) = (u as usize / CELL, (v as usize / CELL).min(self.nh - 1));
        let mut nearest: Option<f64> = None;
        for dx in -1i64..=1 {
            let xx = (cx as i64 + dx).rem_euclid(self.nw as i64) as usize;
            let r = self.near[cy * self.nw + xx];
            if r > 0.0 {
                nearest = Some(nearest.map_or(r as f64, |n: f64| n.min(r as f64)));
            }
        }
        // A cell spans a sliver of elevation; on ground seen at a grazing
        // angle that sliver is metres of range, so its nearest return falls
        // short of a point in its middle by about range x del / tan(el).
        let del = CELL as f64 / self.ky.abs();
        let grazing = (range_m * del / z.atan2(out).abs().tan().max(1e-3)).min(0.5 * range_m);
        let seen = match nearest {
            None => Seen::Open,
            Some(n) if range_m <= n + HIDDEN_M.max(HIDDEN_SHARE * range_m) + grazing => Seen::Visible,
            Some(_) => Seen::Hidden,
        };
        Some(Projected { px: (u, v), range_m, seen })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f64 = 1.6;

    /// A dome over flat ground 1.6 m down with a wall 10 m out along +x
    /// (y −3..3, up to 2 m above the ground). `turn`: +1 when columns run with
    /// azimuth, −1 against it.
    fn dome(turn: f64) -> (Vec<GridPoint>, (u32, u32), (usize, usize)) {
        let (gc, gr) = (3600u32, 900u32);
        let mut v = Vec::new();
        for row in (0..gr).step_by(2) {
            let el = (70.0 - 140.0 * row as f64 / gr as f64).to_radians();
            for col in (0..gc).step_by(2) {
                let az = turn * col as f64 / gc as f64 * std::f64::consts::TAU;
                let d = (el.cos() * az.cos(), el.cos() * az.sin(), el.sin());
                let mut hit: Option<f64> = None;
                if d.2 < -1e-6 {
                    hit = Some(-H / d.2);
                }
                if d.0 > 1e-6 {
                    let t = 10.0 / d.0;
                    let (y, z) = (t * d.1, t * d.2);
                    if y.abs() <= 3.0 && z >= -H && z <= 2.0 - H && hit.is_none_or(|h| t < h) {
                        hit = Some(t);
                    }
                }
                let Some(t) = hit.filter(|t| *t < 120.0) else { continue };
                let point = trimble_tzf::Point { x: (t * d.0) as f32, y: (t * d.1) as f32, z: (t * d.2) as f32, intensity: 0.0 };
                v.push(GridPoint { point, row, col });
            }
        }
        (v, (gc, gr), (1800, 450))
    }

    /// The pixel the dome puts a direction at.
    fn px_of(turn: f64, az: f64, el_deg: f64) -> (f64, f64) {
        let col = (turn * az / std::f64::consts::TAU).rem_euclid(1.0) * 1800.0;
        (col, (70.0 - el_deg) / 140.0 * 450.0)
    }

    #[test]
    fn a_ground_point_lands_where_the_scan_saw_it_and_is_visible() {
        for turn in [1.0, -1.0] {
            let (pts, grid, preview) = dome(turn);
            let v = DepthView::build(&pts, grid, preview).expect("mapping");
            assert!((v.floor + H).abs() < 0.02, "floor {}", v.floor);
            for (out, az) in [(6.0f64, 2.0f64), (20.0, -1.0), (35.0, 3.0)] {
                let p = [out * az.cos(), out * az.sin(), -H];
                let got = v.project(p).expect("on the picture");
                let want = px_of(turn, az, (-H).atan2(out).to_degrees());
                let du = (got.px.0 - want.0 + 900.0).rem_euclid(1800.0) - 900.0;
                assert!(du.abs() < 2.0 && (got.px.1 - want.1).abs() < 2.0, "turn {turn}: {got:?} vs {want:?}");
                assert_eq!(got.seen, Seen::Visible, "turn {turn} out {out}");
                assert!((v.ground_z(p[0], p[1]) + H).abs() < 0.05);
            }
        }
    }

    #[test]
    fn behind_the_wall_is_hidden_and_the_sky_is_open() {
        let (pts, grid, preview) = dome(1.0);
        let v = DepthView::build(&pts, grid, preview).unwrap();
        // The ground behind the wall, and the wall's face itself.
        assert_eq!(v.project([16.0, 0.0, -H]).unwrap().seen, Seen::Hidden);
        assert_eq!(v.project([10.0, 0.0, -0.5]).unwrap().seen, Seen::Visible);
        // A tripod top over open ground beside the wall: nothing behind it but sky.
        assert_eq!(v.project([0.0, 12.0, 0.05]).unwrap().seen, Seen::Open);
        // Straight up is off the picture's rows (it spans ±70°).
        assert!(v.project([0.0, 0.0, 5.0]).is_none());
    }
}
