//! Openings on a wall face, counted against a schedule
//! (`specs/construction-counts.md`, part A).
//!
//! Every station that saw the face is read at full resolution. Each point is
//! placed by where its ray met the face's plane:
//!
//! - **on the plane** (within 3 cm): wall;
//! - **behind it**: the ray went through. Less than 35 cm behind is a surface
//!   in the wall's depth (a recess, a frame); further is the room beyond;
//! - **in front of it**: something stood between the station and the face,
//!   so the face there wasn't seen from that station.
//!
//! On the face's raster (2 cm cells or more), small holes in the wall are
//! closed, and every gap at least 40 × 40 cm with wall around it is looked
//! at. A gap is an opening when its outline closes on wall (a door's on the
//! floor) and the stations saw through it. It doesn't fit when something
//! stood in front of it, or its outline runs into a stretch no station saw.
//! A gap backed by a surface less than 35 cm behind across most of it, with
//! nothing seen beyond, is a recess and isn't counted.
//!
//! An opening's width is the median, over the middle 60% of its rows, of the
//! distance between the wall's last points either side; its height likewise
//! over the middle 60% of its columns, from the wall's foot beside it for a
//! door. Half the point spacing at each edge is taken off (the edge lies
//! between the last point on the wall and the first through the gap), and the
//! uncertainty comes from that spacing and the spread between rows.
//!
//! The agent proposes which opening is which schedule line; a person confirms
//! the matches. Each line then gets the verdicts `on-site-answers.md` gives:
//! `not_yet` while its face is under 80% seen, `does_not_fit` with the
//! reason, or the answer, with a conformance against the line's tolerance.

use crate::ask::Verdict;
use crate::list::{Expected, LineState, OpeningType};
use crate::register::{Face, FACE_COVERED};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Cells this size or more, at most `MAX_CELLS` on a face.
pub const CELL_MIN_M: f64 = 0.02;
const MAX_CELLS: f64 = 1.0e6;
/// A point this close to the face's plane is the wall.
pub const PLANE_TOL_M: f64 = 0.03;
/// Behind the face by less than this: a surface in the wall's depth.
pub const RECESS_M: f64 = 0.35;
/// A gap is looked at from this size, across and high.
pub const MIN_SIZE_M: f64 = 0.4;
/// Points beyond this from their station aren't used, as for walls.
pub const RANGE_M: f64 = 35.0;
/// Holes in the wall up to about twice this many cells across are closed.
const CLOSE: usize = 2;
/// More of the gap than this hidden behind something in front: it doesn't fit.
pub const BLOCKED_MAX: f64 = 0.05;
/// A gap fills at least this share of its box to be an opening's outline.
pub const RECT_MIN: f64 = 0.8;
/// A recess: a surface close behind across this share, and this little beyond.
pub const RECESS_SHARE: f64 = 0.7;
pub const FAR_SHARE_MAX: f64 = 0.05;
/// Below this from the wall's foot, an opening's sill is the floor: a door.
pub const DOOR_SILL_M: f64 = 0.05;
/// A matched opening is found again after a new count within this (m).
pub const REMATCH_M: f64 = 0.25;

fn dot(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

/// One face, rastered along it (u, from `from`) and up it (z).
pub struct Raster {
    from: [f64; 2],
    dir: [f64; 2],
    /// Towards the stations that saw it.
    nrm: [f64; 2],
    len: f64,
    z0: f64,
    pub cell: f64,
    pub nu: usize,
    pub nz: usize,
    wall: Vec<u32>,
    umin: Vec<f32>,
    umax: Vec<f32>,
    zmin: Vec<f32>,
    zmax: Vec<f32>,
    back: Vec<u32>,
    depth: Vec<f32>,
    far: Vec<u32>,
    blocked: Vec<u32>,
}

impl Raster {
    /// A raster of the face between heights `z` (m, project frame), facing
    /// the stations at `stations` (plan positions).
    pub fn new(f: &Face, z: [f64; 2], stations: &[[f64; 2]]) -> Raster {
        let (dx, dy) = (f.to[0] - f.from[0], f.to[1] - f.from[1]);
        let len = dx.hypot(dy).max(1e-6);
        let dir = [dx / len, dy / len];
        let mut nrm = [-dir[1], dir[0]];
        let side: f64 = stations.iter().map(|s| dot([s[0] - f.from[0], s[1] - f.from[1]], nrm).signum()).sum();
        if side < 0.0 {
            nrm = [-nrm[0], -nrm[1]];
        }
        let Grid { cell, nu, nz, .. } = Grid::of(f, z);
        let n = nu * nz;
        Raster {
            from: f.from, dir, nrm, len, z0: z[0], cell, nu, nz,
            wall: vec![0; n], umin: vec![f32::MAX; n], umax: vec![f32::MIN; n], zmin: vec![f32::MAX; n], zmax: vec![f32::MIN; n],
            back: vec![0; n], depth: vec![0.0; n], far: vec![0; n], blocked: vec![0; n],
        }
    }

    fn at(&self, u: f64, z: f64) -> Option<usize> {
        if !(0.0..self.len).contains(&u) || z < self.z0 {
            return None;
        }
        let (iu, iz) = ((u / self.cell) as usize, ((z - self.z0) / self.cell) as usize);
        (iu < self.nu && iz < self.nz).then_some(iz * self.nu + iu)
    }

    /// How far a station stands in front of the face (m); negative behind it.
    pub fn standoff(&self, s: [f64; 3]) -> f64 {
        dot([s[0] - self.from[0], s[1] - self.from[1]], self.nrm)
    }

    /// A point `w` seen from a station at `s` (project frame), where the
    /// station stands `ds` in front of the face.
    pub fn add(&mut self, s: [f64; 3], ds: f64, w: [f64; 3]) {
        let d = dot([w[0] - self.from[0], w[1] - self.from[1]], self.nrm);
        if d.abs() <= PLANE_TOL_M {
            let u = dot([w[0] - self.from[0], w[1] - self.from[1]], self.dir);
            if let Some(k) = self.at(u, w[2]) {
                let (uu, zz) = (u as f32, w[2] as f32);
                self.wall[k] += 1;
                self.umin[k] = self.umin[k].min(uu);
                self.umax[k] = self.umax[k].max(uu);
                self.zmin[k] = self.zmin[k].min(zz);
                self.zmax[k] = self.zmax[k].max(zz);
            }
            return;
        }
        // Where the ray from the station through the point meets the plane.
        if d >= ds - 0.1 {
            return;
        }
        let t = ds / (ds - d);
        let c = [s[0] + (w[0] - s[0]) * t, s[1] + (w[1] - s[1]) * t, s[2] + (w[2] - s[2]) * t];
        let u = dot([c[0] - self.from[0], c[1] - self.from[1]], self.dir);
        let Some(k) = self.at(u, c[2]) else { return };
        if d > 0.0 {
            self.blocked[k] += 1;
        } else if -d <= RECESS_M {
            self.back[k] += 1;
            self.depth[k] += -d as f32;
        } else {
            self.far[k] += 1;
        }
    }

    /// The wall after closing small holes in it (dilate, then erode; outside
    /// the raster counts as wall, so the edges don't wear away).
    fn closed(&self) -> Vec<bool> {
        let (nu, nz) = (self.nu, self.nz);
        let raw: Vec<bool> = self.wall.iter().map(|c| *c > 0).collect();
        let pass = |src: &[bool], keep: bool| -> Vec<bool> {
            // `keep` true: dilate (any wall near); false: erode (all wall near).
            let mut rows = vec![false; nu * nz];
            for iz in 0..nz {
                for iu in 0..nu {
                    let (lo, hi) = (iu.saturating_sub(CLOSE), (iu + CLOSE).min(nu - 1));
                    let near = &src[iz * nu + lo..=iz * nu + hi];
                    rows[iz * nu + iu] = if keep { near.iter().any(|c| *c) } else { near.iter().all(|c| *c) };
                }
            }
            let mut out = vec![false; nu * nz];
            for iz in 0..nz {
                let lo = iz.saturating_sub(CLOSE);
                let hi = (iz + CLOSE).min(nz - 1);
                for iu in 0..nu {
                    out[iz * nu + iu] = if keep { (lo..=hi).any(|k| rows[k * nu + iu]) } else { (lo..=hi).all(|k| rows[k * nu + iu]) };
                }
            }
            out
        };
        pass(&pass(&raw, true), false)
    }

    /// The rows the wall is seen in: from the lowest to the highest with
    /// wall in a tenth of its cells or more. Below and above is ground, sky,
    /// or what the face's height left over.
    fn rows(&self) -> Option<(usize, usize)> {
        let busy = |iz: usize| (0..self.nu).filter(|&iu| self.wall[iz * self.nu + iu] > 0).count() * 10 >= self.nu;
        let lo = (0..self.nz).find(|&iz| busy(iz))?;
        let hi = (0..self.nz).rev().find(|&iz| busy(iz))?;
        Some((lo, hi))
    }

    /// The openings on the face, and the gaps not counted.
    pub fn find(&self) -> Found {
        let (nu, nz) = (self.nu, self.nz);
        let mut found = Found::default();
        let Some((rlo, rhi)) = self.rows() else { return found };
        let wall = self.closed();
        // Rows outside the wall's own are left out, as if visited.
        let mut seen: Vec<bool> = (0..nu * nz).map(|k| k / nu < rlo || k / nu > rhi).collect();
        for start in 0..nu * nz {
            if wall[start] || seen[start] {
                continue;
            }
            let mut cells = vec![start];
            seen[start] = true;
            let mut i = 0;
            while i < cells.len() {
                let k = cells[i];
                i += 1;
                let (iu, iz) = (k % nu, k / nu);
                let mut push = |n: usize| {
                    if !wall[n] && !seen[n] {
                        seen[n] = true;
                        cells.push(n);
                    }
                };
                if iu > 0 {
                    push(k - 1);
                }
                if iu + 1 < nu {
                    push(k + 1);
                }
                if iz > 0 {
                    push(k - nu);
                }
                if iz + 1 < nz {
                    push(k + nu);
                }
            }
            let (mut u0, mut u1, mut z0, mut z1) = (nu, 0, nz, 0);
            for &k in &cells {
                (u0, u1, z0, z1) = (u0.min(k % nu), u1.max(k % nu), z0.min(k / nu), z1.max(k / nu));
            }
            let (w, h) = ((u1 - u0 + 1) as f64 * self.cell, (z1 - z0 + 1) as f64 * self.cell);
            if w < MIN_SIZE_M || h < MIN_SIZE_M {
                continue;
            }
            self.judge(&cells, [u0, u1, z0, z1], (rlo, rhi), &mut found);
        }
        found.openings.sort_by(|a, b| a.along_m.total_cmp(&b.along_m));
        found
    }

    fn judge(&self, cells: &[usize], b: [usize; 4], (rlo, rhi): (usize, usize), found: &mut Found) {
        let [u0, u1, z0, z1] = b;
        let n = cells.len() as f64;
        let share = |f: &dyn Fn(usize) -> bool| cells.iter().filter(|&&k| f(k)).count() as f64 / n;
        let far = share(&|k| self.far[k] > 0);
        let back = share(&|k| self.far[k] == 0 && self.back[k] > 0);
        let blocked = share(&|k| self.far[k] == 0 && self.back[k] == 0 && self.blocked[k] > 0);
        let none = (1.0 - far - back - blocked).max(0.0);
        let filled = n / ((u1 - u0 + 1) * (z1 - z0 + 1)) as f64;
        let ends = u0 == 0 || u1 + 1 == self.nu;
        let top = z1 == rhi;
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        let gap = |why: String| Gap {
            along_m: r2(u0 as f64 * self.cell),
            bottom_m: r2(self.z0 + z0 as f64 * self.cell),
            width_m: r2((u1 - u0 + 1) as f64 * self.cell),
            height_m: r2((z1 - z0 + 1) as f64 * self.cell),
            why,
        };
        let pct = |v: f64| format!("{:.0}%", v * 100.0);
        if (ends || top) && none > 0.5 {
            // A stretch no station saw, not an opening: the face's coverage names it.
            return;
        }
        let misfit = if ends || top {
            Some(format!("it runs off the face's {}: its outline can't be closed", if top { "top" } else { "end" }))
        } else if blocked > BLOCKED_MAX {
            Some(format!("something stands in front of it: {} of it is hidden from every station that saw it", pct(blocked)))
        } else if filled < RECT_MIN {
            Some(if none > 0.2 {
                format!("its outline isn't closed: part of it ({} unseen) joins a stretch no station saw", pct(none))
            } else {
                format!("its outline isn't one rectangle (it fills {} of its box)", pct(filled))
            })
        } else {
            None
        };
        if misfit.is_none() && back >= RECESS_SHARE && far <= FAR_SHARE_MAX {
            let (d, c) = cells.iter().filter(|&&k| self.back[k] > 0).fold((0.0f64, 0u32), |(d, c), &k| (d + self.depth[k] as f64, c + self.back[k]));
            let depth = d / c.max(1) as f64;
            found.not_counted.push(gap(format!("a recess, not an opening: a surface {:.0} mm behind the face across {} of it, and nothing seen beyond", depth * 1000.0, pct(back))));
            return;
        }
        let centre = [(u0 + u1 + 1) as f64 * self.cell / 2.0, self.z0 + (z0 + z1 + 1) as f64 * self.cell / 2.0];
        let mut o = Opening {
            opening_id: String::new(),
            along_m: r2(u0 as f64 * self.cell),
            centre: [r2(centre[0]), r2(centre[1])],
            box_m: [r2(u0 as f64 * self.cell), r2(self.z0 + z0 as f64 * self.cell), r2((u1 + 1) as f64 * self.cell), r2(self.z0 + (z1 + 1) as f64 * self.cell)],
            looks_like: OpeningType::Opening,
            size: None,
            misfit,
            seen_through: r2(far + back),
        };
        if o.misfit.is_none() {
            match self.measure(cells, b, rlo) {
                Ok(s) => {
                    o.looks_like = if s.sill_mm.is_some_and(|v| v < DOOR_SILL_M * 1000.0) { OpeningType::Door } else { OpeningType::Window };
                    o.along_m = s.along_m;
                    o.size = Some(s);
                }
                Err(why) => o.misfit = Some(why),
            }
        }
        found.openings.push(o);
    }

    /// The wall's points nearest the gap's edges, per row and column.
    fn measure(&self, cells: &[usize], [u0, u1, z0, z1]: [usize; 4], rlo: usize) -> Result<Size, String> {
        let nu = self.nu;
        let inside: std::collections::HashSet<usize> = cells.iter().copied().collect();
        let reach = CLOSE + 3;
        let middle = |a: usize, b: usize| -> (usize, usize) {
            let m = ((b - a + 1) as f64 * 0.2).round() as usize;
            (a + m, b.saturating_sub(m).max(a + m))
        };
        // Width, row by row.
        let (ra, rb) = middle(z0, z1);
        let uc = (u0 + u1) / 2;
        let mut widths = Vec::new();
        for iz in ra..=rb {
            if !inside.contains(&(iz * nu + uc)) {
                continue;
            }
            let left = (u0.saturating_sub(reach)..uc).rev().find(|&k| self.wall[iz * nu + k] > 0).map(|k| self.umax[iz * nu + k] as f64);
            let right = (uc + 1..(u1 + reach + 1).min(nu)).find(|&k| self.wall[iz * nu + k] > 0).map(|k| self.umin[iz * nu + k] as f64);
            if let (Some(l), Some(r)) = (left, right) {
                widths.push((l, r));
            }
        }
        // Height, column by column; a gap down to the face's foot is a door.
        let (ca, cb) = middle(u0, u1);
        let zc = (z0 + z1) / 2;
        let mut heights = Vec::new();
        let mut to_floor = 0;
        for iu in ca..=cb {
            if !inside.contains(&(zc * nu + iu)) {
                continue;
            }
            let below = (z0.saturating_sub(reach)..zc).rev().find(|&k| self.wall[k * nu + iu] > 0).map(|k| self.zmax[k * nu + iu] as f64);
            let above = (zc + 1..(z1 + reach + 1).min(self.nz)).find(|&k| self.wall[k * nu + iu] > 0).map(|k| self.zmin[k * nu + iu] as f64);
            if below.is_none() && z0 <= rlo + reach {
                to_floor += 1;
            }
            heights.push((below, above));
        }
        if widths.len() < 3 {
            return Err("too few rows have wall seen on both sides of it to measure its width".into());
        }
        // The wall's point spacing beside the gap, from the points per cell.
        let ring: Vec<u32> = (z0..=z1)
            .flat_map(|iz| [u0.saturating_sub(reach)..u0, (u1 + 1)..(u1 + 1 + reach).min(nu)].into_iter().flatten().map(move |iu| iz * nu + iu))
            .map(|k| self.wall[k])
            .filter(|c| *c > 0)
            .collect();
        let per_cell = ring.iter().map(|c| *c as f64).sum::<f64>() / ring.len().max(1) as f64;
        let spacing = self.cell / per_cell.max(1.0).sqrt();
        let door = to_floor * 2 > heights.len();
        let foot = if door {
            let near = (0.3 / self.cell).ceil() as usize;
            let cols = (u0.saturating_sub(near)..u0).chain((u1 + 1)..(u1 + 1 + near).min(nu));
            let lowest = cols.flat_map(|iu| (0..self.nz).map(move |iz| iz * nu + iu)).filter(|&k| self.wall[k] > 0).map(|k| self.zmin[k] as f64).fold(f64::MAX, f64::min);
            if lowest == f64::MAX {
                return Err("the wall's foot beside it isn't seen, so its height can't be measured".into());
            }
            Some(lowest)
        } else {
            None
        };
        let hs: Vec<f64> = heights
            .iter()
            .filter_map(|(lo, hi)| {
                let top = (*hi)? - spacing / 2.0;
                match (lo, foot) {
                    (_, Some(f)) => Some(top - f),
                    (Some(lo), None) => Some(top - (lo + spacing / 2.0)),
                    (None, None) => None,
                }
            })
            .collect();
        if hs.len() < 3 {
            return Err("too few columns have wall seen above and below it to measure its height".into());
        }
        let ws: Vec<f64> = widths.iter().map(|(l, r)| r - l - spacing).collect();
        let (w, wu) = robust(&ws, spacing);
        let (h, hu) = robust(&hs, if door { spacing / 2.0 } else { spacing });
        let left = median(&mut widths.iter().map(|(l, _)| l + spacing / 2.0).collect::<Vec<_>>());
        let bottom = match foot {
            Some(_) => None,
            None => {
                let mut b: Vec<f64> = heights.iter().filter_map(|(lo, _)| lo.map(|v| v + spacing / 2.0)).collect();
                Some(median(&mut b))
            }
        };
        let foot_here = foot.unwrap_or_else(|| {
            let near = (0.3 / self.cell).ceil() as usize;
            (u0.saturating_sub(near)..(u1 + 1 + near).min(nu))
                .flat_map(|iu| (0..self.nz).map(move |iz| iz * nu + iu))
                .filter(|&k| self.wall[k] > 0)
                .map(|k| self.zmin[k] as f64)
                .fold(f64::MAX, f64::min)
        });
        let r1 = |v: f64| (v * 10.0).round() / 10.0;
        Ok(Size {
            width_mm: r1(w * 1000.0),
            height_mm: r1(h * 1000.0),
            width_uncertainty_mm: r1((wu * 1000.0).max(1.0)),
            height_uncertainty_mm: r1((hu * 1000.0).max(1.0)),
            sill_mm: Some(match bottom {
                Some(b) if foot_here < f64::MAX => r1(((b - foot_here) * 1000.0).max(0.0)),
                _ => 0.0,
            }),
            along_m: (left * 100.0).round() / 100.0,
            from_foot: door,
            rows: widths.len(),
            columns: hs.len(),
        })
    }

    /// The face as the count saw it, with the openings outlined and numbered
    /// by their ids' last part. PNG, `Grid::picture` pixels.
    pub fn png(&self, title: &str, found: &Found) -> Vec<u8> {
        let wall = self.closed();
        let scale = picture_scale(self.nu);
        let pad = PICTURE_PAD;
        let (w, h) = picture_size(self.nu, self.nz);
        let mut cv = crate::station::Canvas::new(w, h, [250, 250, 250]);
        for iz in 0..self.nz {
            for iu in 0..self.nu {
                let k = iz * self.nu + iu;
                let c = if self.wall[k] > 0 {
                    [160, 160, 160]
                } else if wall[k] {
                    [190, 190, 190]
                } else if self.far[k] > 0 {
                    [40, 62, 96]
                } else if self.back[k] > 0 {
                    [74, 128, 134]
                } else if self.blocked[k] > 0 {
                    [214, 132, 120]
                } else {
                    [246, 246, 246]
                };
                let (x0, y0) = ((iu as f64 * scale) as i64, (pad + (self.nz - 1 - iz) as f64 * scale) as i64);
                for y in y0..y0 + scale.ceil() as i64 {
                    for x in x0..x0 + scale.ceil() as i64 {
                        cv.put(x, y, c, 1.0);
                    }
                }
            }
        }
        let to_px = |u: f64, z: f64| ((u / self.cell * scale) as f32, (pad + (self.nz as f64 - (z - self.z0) / self.cell) * scale) as f32);
        let font = crate::station::font();
        for g in &found.not_counted {
            let (a, b) = (to_px(g.along_m, g.bottom_m + g.height_m), to_px(g.along_m + g.width_m, g.bottom_m));
            cv.rect(a.0, a.1, b.0, b.1, [110, 110, 110], 2.0);
        }
        for o in &found.openings {
            let col = if o.misfit.is_some() { [220, 38, 38] } else { [245, 158, 11] };
            let (a, b) = (to_px(o.box_m[0], o.box_m[3]), to_px(o.box_m[2], o.box_m[1]));
            cv.rect(a.0, a.1, b.0, b.1, col, 3.0);
            let label = o.opening_id.rsplit('-').next().unwrap_or("").to_string();
            cv.text(&font, 14.0, ((a.0 + b.0) / 2.0, a.1 + 4.0), 0.5, &label, col);
        }
        cv.text(&font, 15.0, (6.0, 6.0), 0.0, title, [240, 240, 240]);
        cv.png()
    }
}

const PICTURE_PAD: f64 = 34.0;

fn picture_scale(nu: usize) -> f64 {
    (1200.0 / nu as f64).clamp(1.0, 6.0)
}

fn picture_size(nu: usize, nz: usize) -> (usize, usize) {
    let scale = picture_scale(nu);
    ((nu as f64 * scale).ceil() as usize, (nz as f64 * scale).ceil() as usize + PICTURE_PAD as usize)
}

/// The cells a face is counted on: their size, how many along and up, and
/// where they reach (u from the face's `from` end, z), from the face and the
/// heights it is read between alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pub cell: f64,
    pub nu: usize,
    pub nz: usize,
    pub u_max: f64,
    pub z: [f64; 2],
}

impl Grid {
    pub fn of(f: &Face, z: [f64; 2]) -> Grid {
        let len = (f.to[0] - f.from[0]).hypot(f.to[1] - f.from[1]).max(1e-6);
        let h = (z[1] - z[0]).max(0.1);
        let cell = (len * h / MAX_CELLS).sqrt().max(CELL_MIN_M);
        let (nu, nz) = ((len / cell).ceil() as usize, (h / cell).ceil() as usize);
        Grid { cell, nu, nz, u_max: nu as f64 * cell, z: [z[0], z[0] + nz as f64 * cell] }
    }

    /// The width and height of the face's picture, in pixels.
    pub fn picture(&self) -> (usize, usize) {
        picture_size(self.nu, self.nz)
    }
}

/// The median and its uncertainty: point spacing at the edges (`edge`, two
/// edges each within half of it) and the spread between rows.
fn robust(v: &[f64], edge: f64) -> (f64, f64) {
    let mut s = v.to_vec();
    let m = median(&mut s);
    let mut dev: Vec<f64> = v.iter().map(|x| (x - m).abs()).collect();
    let sd = 1.4826 * median(&mut dev);
    let spread = 1.2533 * sd / (v.len() as f64).sqrt();
    (m, (edge * edge / 6.0 + spread * spread).sqrt())
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// An opening's size, as measured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Size {
    pub width_mm: f64,
    pub height_mm: f64,
    pub width_uncertainty_mm: f64,
    pub height_uncertainty_mm: f64,
    /// Above the wall's foot beside it; 0 for a door.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sill_mm: Option<f64>,
    /// Its left edge, from the face's `from` end (m).
    pub along_m: f64,
    /// A door's height is measured from the wall's foot beside it.
    #[serde(default)]
    pub from_foot: bool,
    #[serde(default)]
    pub rows: usize,
    #[serde(default)]
    pub columns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Opening {
    pub opening_id: String,
    pub along_m: f64,
    /// The middle of its box (u along the face, z), m.
    pub centre: [f64; 2],
    /// u0, z0, u1, z1 (m).
    pub box_m: [f64; 4],
    pub looks_like: OpeningType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    /// Why it can't be measured: something in front, or its outline open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misfit: Option<String>,
    /// Share of it the stations saw through.
    pub seen_through: f64,
}

/// A gap not counted as an opening, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Gap {
    pub along_m: f64,
    pub bottom_m: f64,
    pub width_m: f64,
    pub height_m: f64,
    pub why: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Found {
    pub openings: Vec<Opening>,
    pub not_counted: Vec<Gap>,
}

/// One face of a count: where it is, how much of it was seen, what was found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaceCount {
    pub face_id: String,
    pub label: String,
    pub face: Face,
    pub length_m: f64,
    pub seen_share: f64,
    pub stations: Vec<u16>,
    #[serde(default)]
    pub unseen: Value,
    pub found: Found,
    pub lines: Vec<String>,
}

/// A project's latest count, kept for confirming matches against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Counted {
    pub counted_at: String,
    /// The worker that counted it and the token it claimed the count with;
    /// none when the bridge did.
    #[serde(default)]
    pub counted_on: Option<String>,
    #[serde(default)]
    pub counted_by: Option<String>,
    pub faces: Vec<FaceCount>,
}

impl Counted {
    pub fn opening(&self, id: &str) -> Option<(&FaceCount, &Opening)> {
        self.faces.iter().find_map(|f| f.found.openings.iter().find(|o| o.opening_id == id).map(|o| (f, o)))
    }
}

/// A person's match of a line to an opening found on its face.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Match {
    pub opening_id: String,
    pub centre: [f64; 2],
    pub by: String,
    pub via: String,
    pub at: String,
}

/// An opening line's state, as the mission keeps it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LineCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<Match>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conformance: Option<String>,
    #[serde(default)]
    pub face_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_share: Option<f64>,
    #[serde(default)]
    pub stations: Vec<u16>,
    #[serde(default)]
    pub checked_at: String,
    /// Where the count it rests on was made: a worker's name, or "bridge";
    /// and the token the worker claimed it with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_by: Option<String>,
}

impl LineCheck {
    pub fn state(&self) -> LineState {
        let reason = self.reason.clone();
        match self.verdict {
            None => LineState { status: "open", conformance: None, reason },
            Some(Verdict::NotYet) => LineState { status: "not_yet", conformance: None, reason },
            Some(Verdict::DoesNotFit) => LineState { status: "does_not_fit", conformance: None, reason },
            Some(Verdict::Answer) => LineState {
                status: "answered",
                conformance: Some(match self.conformance.as_deref() {
                    Some("conforming") => "conforming",
                    Some("non_conforming") => "non_conforming",
                    Some("neutral") => "neutral",
                    _ => "not_applicable",
                }),
                reason,
            },
        }
    }

    /// The record's evidence for an answered line.
    pub fn evidence(&self, project_id: &str) -> Option<Value> {
        let s = self.size.as_ref().filter(|_| self.verdict == Some(Verdict::Answer))?;
        let mut reading = json!({"width_mm": s.width_mm, "height_mm": s.height_mm, "width_uncertainty_mm": s.width_uncertainty_mm,
                                 "height_uncertainty_mm": s.height_uncertainty_mm, "along_m": s.along_m});
        if let Some(v) = s.sill_mm {
            reading["sill_mm"] = json!(v);
        }
        let mut e = json!({"grade": "measured", "reading": reading, "stations": self.stations,
                           "say": size_said(s), "pictures": [format!("/v1/projects/{project_id}/count/{}/image", self.face_id)]});
        if let Some(v) = self.seen_share {
            e["seen_share"] = json!(v);
        }
        e["counted_on"] = json!(self.counted_on.as_deref().unwrap_or("bridge"));
        if let Some(by) = &self.counted_by {
            e["counted_by"] = json!(by);
        }
        Some(e)
    }
}

/// "914 × 1219 mm (36 × 48 in), ±3 mm"
pub fn size_said(s: &Size) -> String {
    let foot = if s.from_foot { ", its height from the wall's foot beside it" } else { "" };
    format!(
        "{:.0} × {:.0} mm ({} × {}), to ±{:.0} mm across and ±{:.0} mm high{foot}",
        s.width_mm, s.height_mm, crate::ask::inches(s.width_mm), crate::ask::inches(s.height_mm), s.width_uncertainty_mm.max(1.0), s.height_uncertainty_mm.max(1.0)
    )
}

/// A measured size against the schedule's: conforming only when both sides are
/// within the tolerance by more than their uncertainty, non-conforming when
/// either is outside it by more than that, neutral in between (the tolerance
/// can't decide on this measurement), not applicable with no tolerance.
pub fn conformance(e: &Expected, max_mm: Option<f64>, s: &Size) -> (&'static str, String) {
    let Some(tol) = max_mm else { return ("not_applicable", "the schedule gives no tolerance: the size is recorded".into()) };
    let mut calls = Vec::new();
    let mut words = Vec::new();
    for (what, got, want, unc) in [("width", s.width_mm, e.width_mm, s.width_uncertainty_mm), ("height", s.height_mm, e.height_mm, s.height_uncertainty_mm)] {
        let off = got - want;
        let call = if off.abs() + unc <= tol {
            "conforming"
        } else if off.abs() - unc > tol {
            "non_conforming"
        } else {
            "neutral"
        };
        if call != "conforming" {
            words.push(format!("{what} {got:.0} mm against {want:.0} ± {tol:.0} mm, {:.0} mm {}, measured to ±{unc:.0} mm", off.abs(), if off >= 0.0 { "over" } else { "under" }));
        }
        calls.push(call);
    }
    let call = if calls.contains(&"non_conforming") {
        "non_conforming"
    } else if calls.contains(&"neutral") {
        "neutral"
    } else {
        "conforming"
    };
    let why = match call {
        "neutral" => format!("too close to the limit to call: {}", words.join("; ")),
        "non_conforming" => format!("outside the tolerance: {}", words.join("; ")),
        _ => String::new(),
    };
    (call, why)
}

/// Pairs the bridge proposes (line, opening), for the agent to put to a
/// person: on each face, the closest in size, of the expected type, within
/// 10% or 100 mm on both sides, each opening and line once.
pub fn propose(lines: &[(String, Expected)], openings: &[&Opening]) -> Vec<(String, String, String)> {
    let mut cands = Vec::new();
    for (line, e) in lines {
        for o in openings {
            let Some(s) = &o.size else { continue };
            let near = |got: f64, want: f64| (got - want).abs() <= (want * 0.1).max(100.0);
            if !near(s.width_mm, e.width_mm) || !near(s.height_mm, e.height_mm) {
                continue;
            }
            let type_ok = e.kind == OpeningType::Opening || e.kind == o.looks_like;
            let cost = (s.width_mm - e.width_mm).abs() + (s.height_mm - e.height_mm).abs() + if type_ok { 0.0 } else { 1000.0 };
            let why = format!("{} {}, measured {:.0} × {:.0} mm against {:.0} × {:.0}{}", o.looks_like.word(), o.opening_id, s.width_mm, s.height_mm, e.width_mm, e.height_mm,
                              if type_ok { String::new() } else { format!("; it looks like a {}, the schedule says {}", o.looks_like.word(), e.kind.word()) });
            cands.push((cost, line.clone(), o.opening_id.clone(), why));
        }
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut used_l, mut used_o) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let mut out = Vec::new();
    for (_, l, o, why) in cands {
        if used_l.contains(&l) || used_o.contains(&o) {
            continue;
        }
        used_l.insert(l.clone());
        used_o.insert(o.clone());
        out.push((l, o, why));
    }
    out.sort_by_key(|(l, _, _)| lines.iter().position(|(x, _)| x == l));
    out
}

/// A line's verdict from the latest count of its face.
pub fn judge_line(c: &mut LineCheck, e: &Expected, max_mm: Option<f64>, face: &FaceCount, mark: &str) {
    c.face_id = face.face_id.clone();
    c.seen_share = Some(face.seen_share);
    c.checked_at = crate::util::now_rfc3339();
    let found = face.found.openings.len();
    if face.seen_share < FACE_COVERED {
        c.verdict = Some(Verdict::NotYet);
        c.size = None;
        c.conformance = None;
        c.stations = face.stations.clone();
        c.reason = format!("{} is {:.0}% seen; the count needs {:.0}% ({found} opening{} found so far)", face.label, face.seen_share * 100.0, FACE_COVERED * 100.0, if found == 1 { "" } else { "s" });
        return;
    }
    let Some(m) = &c.matched else {
        c.verdict = None;
        c.size = None;
        c.conformance = None;
        c.reason = format!("{found} opening{} found on {}; a person confirms which one is {mark}", if found == 1 { "" } else { "s" }, face.label);
        return;
    };
    let near = face.found.openings.iter().filter(|o| ((o.centre[0] - m.centre[0]).powi(2) + (o.centre[1] - m.centre[1]).powi(2)).sqrt() <= REMATCH_M).min_by(|a, b| {
        let d = |o: &Opening| (o.centre[0] - m.centre[0]).hypot(o.centre[1] - m.centre[1]);
        d(a).total_cmp(&d(b))
    });
    let Some(o) = near else {
        c.verdict = None;
        c.size = None;
        c.conformance = None;
        c.reason = format!("nothing was found where {mark} was matched this time; confirm its match again");
        return;
    };
    c.stations = face.stations.clone();
    match (&o.size, &o.misfit) {
        (Some(s), None) => {
            let (call, why) = conformance(e, max_mm, s);
            c.verdict = Some(Verdict::Answer);
            c.size = Some(s.clone());
            c.conformance = Some(call.into());
            c.reason = why;
        }
        (_, why) => {
            c.verdict = Some(Verdict::DoesNotFit);
            c.size = None;
            c.conformance = None;
            c.reason = why.clone().unwrap_or_else(|| "it couldn't be measured".into());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::register::Pose;

    /// A rectangle on the wall plane x = 5: y from `y`, z from `z` (m).
    #[derive(Clone, Copy)]
    pub(crate) struct Hole {
        pub y: [f64; 2],
        pub z: [f64; 2],
    }

    /// A building's front wall at x = 5, from y = -5 to 5 and z = -1.5 (the
    /// ground, 1.5 m below the scanner) to 1.5; a room behind it to x = 9 with
    /// a ceiling at 1.5. `holes` go through the wall; `recesses` stop at a
    /// surface 20 cm behind it; `boxes` stand in front of it (lo, hi).
    pub(crate) struct Facade {
        pub holes: Vec<Hole>,
        pub recesses: Vec<Hole>,
        pub boxes: Vec<([f64; 3], [f64; 3])>,
    }

    impl Facade {
        /// Range from a scanner at `at` along (va from the zenith, ha), in the
        /// scanner's own frame turned by `turn` (rad).
        pub(crate) fn hit(&self, at: [f64; 3], turn: f64, va: f64, ha: f64) -> Option<f64> {
            let (s, c) = turn.sin_cos();
            let l = [va.sin() * ha.cos(), va.sin() * ha.sin(), va.cos()];
            let d = [c * l[0] - s * l[1], s * l[0] + c * l[1], l[2]];
            let mut best = f64::MAX;
            let mut take = |t: f64, ok: bool| {
                if ok && t > 1e-6 && t < best {
                    best = t;
                }
            };
            let p = |t: f64| [at[0] + d[0] * t, at[1] + d[1] * t, at[2] + d[2] * t];
            let inside = |h: &Hole, q: [f64; 3]| (h.y[0]..=h.y[1]).contains(&q[1]) && (h.z[0]..=h.z[1]).contains(&q[2]);
            // Plane x = k.
            let at_x = |k: f64| if d[0].abs() > 1e-12 { (k - at[0]) / d[0] } else { -1.0 };
            let at_z = |k: f64| if d[2].abs() > 1e-12 { (k - at[2]) / d[2] } else { -1.0 };
            let at_y = |k: f64| if d[1].abs() > 1e-12 { (k - at[1]) / d[1] } else { -1.0 };
            let t = at_x(5.0);
            let q = p(t);
            let on_wall = (-5.0..=5.0).contains(&q[1]) && (-1.5..=1.5).contains(&q[2]);
            take(t, on_wall && !self.holes.iter().chain(&self.recesses).any(|h| inside(h, q)));
            let t = at_x(5.2);
            take(t, self.recesses.iter().any(|h| inside(h, p(t))));
            // The recesses' sides.
            for h in &self.recesses {
                for (k, along) in [(h.y[0], true), (h.y[1], true), (h.z[0], false), (h.z[1], false)] {
                    let t = if along { at_y(k) } else { at_z(k) };
                    let q = p(t);
                    let within = (5.0..=5.2).contains(&q[0]) && if along { (h.z[0]..=h.z[1]).contains(&q[2]) } else { (h.y[0]..=h.y[1]).contains(&q[1]) };
                    take(t, within);
                }
            }
            // Ground outside, and the room: floor, ceiling, back and side walls.
            let t = at_z(-1.5);
            take(t, p(t)[0] < 9.0);
            let t = at_z(1.5);
            take(t, (5.0..=9.0).contains(&p(t)[0]) && (-5.0..=5.0).contains(&p(t)[1]));
            let t = at_x(9.0);
            take(t, (-5.0..=5.0).contains(&p(t)[1]) && (-1.5..=1.5).contains(&p(t)[2]));
            for k in [-5.0, 5.0] {
                let t = at_y(k);
                take(t, (5.0..=9.0).contains(&p(t)[0]) && (-1.5..=1.5).contains(&p(t)[2]));
            }
            // Boxes in front: slab intersection.
            for (lo, hi) in &self.boxes {
                let (mut t0, mut t1) = (f64::MIN, f64::MAX);
                for k in 0..3 {
                    if d[k].abs() < 1e-12 {
                        if at[k] < lo[k] || at[k] > hi[k] {
                            t0 = f64::MAX;
                        }
                        continue;
                    }
                    let (a, b) = ((lo[k] - at[k]) / d[k], (hi[k] - at[k]) / d[k]);
                    t0 = t0.max(a.min(b));
                    t1 = t1.min(a.max(b));
                }
                take(t0, t0 <= t1);
            }
            (best < 40.0).then_some(best)
        }
    }

    /// Ray-cast scan files of the facade from stations at `at` (turned by
    /// `turn`), 2 mrad apart over the part of the dome facing the wall.
    pub(crate) fn facade_scans(f: &Facade, dir: &std::path::Path, at: &[([f64; 3], f64)]) -> Vec<(u16, Pose, std::path::PathBuf)> {
        let step = 0.002;
        at.iter()
            .enumerate()
            .map(|(i, &(p, turn))| {
                // Columns from the station's left of the wall to its right.
                let ha0 = (-5.6 - p[1]).atan2(5.0 - p[0]) - turn;
                let ha1 = (5.6 - p[1]).atan2(5.0 - p[0]) - turn;
                let cols = ((ha1 - ha0) / step).ceil() as usize;
                let (va0, rows) = (0.95, (1.35 / step) as usize);
                let bytes = crate::export::tests::tzf_bytes(rows, cols, va0, ha0, step, |va, ha| f.hit(p, turn, va, ha));
                let path = dir.join(format!("station_{:02}.tzf", i + 1));
                std::fs::write(&path, bytes).unwrap();
                ((i + 1) as u16, Pose { turn_deg: turn.to_degrees(), xyz_m: p }, path)
            })
            .collect()
    }

    pub(crate) fn raster_of(files: &[(u16, Pose, std::path::PathBuf)]) -> Raster {
        let face = Face { from: [5.0, -5.0], to: [5.0, 5.0] };
        let xy: Vec<[f64; 2]> = files.iter().map(|(_, p, _)| [p.xyz_m[0], p.xyz_m[1]]).collect();
        let mut r = Raster::new(&face, [-1.5, 1.5], &xy);
        for (_, pose, path) in files {
            let s = pose.xyz_m;
            let ds = r.standoff(s);
            crate::analyze::station_points(path, pose, |_, w, range| {
                if range <= RANGE_M {
                    r.add(s, ds, w);
                }
            })
            .unwrap();
        }
        r
    }

    /// Two windows, one with a bin standing in front of it, a door, and a recess.
    pub(crate) fn scene() -> Facade {
        Facade {
            holes: vec![
                Hole { y: [-3.5, -2.5], z: [-0.5, 0.7] },
                Hole { y: [-1.4, -0.5], z: [-0.3, 0.9] },
                Hole { y: [1.0, 1.9], z: [-1.5, 0.6] },
            ],
            recesses: vec![Hole { y: [3.0, 3.8], z: [-0.4, 0.4] }],
            boxes: vec![([4.2, -1.3, -1.5], [4.6, -0.6, 0.6])],
        }
    }

    #[test]
    fn openings_are_found_and_measured_on_a_ray_cast_facade() {
        let dir = crate::export::tests::scratch("x9-openings-facade");
        let files = facade_scans(&scene(), &dir, &[([0.0, 0.0, 0.0], 0.0), ([0.3, 1.2, 0.05], 0.4)]);
        let r = raster_of(&files);
        let mut f = r.find();
        for (k, o) in f.openings.iter_mut().enumerate() {
            o.opening_id = format!("f1-{}", k + 1);
        }
        assert_eq!(f.openings.len(), 3, "{:#?}", f);
        // Window 1: 1.0 × 1.2 m, its left edge 1.5 m along, sill 1.0 m.
        let w1 = f.openings[0].size.as_ref().unwrap_or_else(|| panic!("{:?}", f.openings[0]));
        assert!((w1.width_mm - 1000.0).abs() <= 10.0 && (w1.height_mm - 1200.0).abs() <= 10.0, "{w1:?}");
        assert!((w1.along_m - 1.5).abs() <= 0.02 && (w1.sill_mm.unwrap() - 1000.0).abs() <= 15.0, "{w1:?}");
        assert!(w1.width_uncertainty_mm < 10.0 && w1.height_uncertainty_mm < 10.0, "{w1:?}");
        assert_eq!(f.openings[0].looks_like, OpeningType::Window);
        // Window 2: the bin hides part of it from both stations.
        let w2 = &f.openings[1];
        assert!(w2.size.is_none() && w2.misfit.as_deref().is_some_and(|m| m.contains("something stands in front of it")), "{w2:?}");
        // The door: 0.9 × 2.1 m, from the floor seen at its threshold.
        let d = f.openings[2].size.as_ref().unwrap_or_else(|| panic!("{:?}", f.openings[2]));
        assert!((d.width_mm - 900.0).abs() <= 10.0 && (d.height_mm - 2100.0).abs() <= 10.0, "{d:?}");
        assert!(d.sill_mm.unwrap() < DOOR_SILL_M * 1000.0 && f.openings[2].looks_like == OpeningType::Door, "{d:?}");
        // The recess isn't an opening.
        assert_eq!(f.not_counted.len(), 1, "{:?}", f.not_counted);
        assert!(f.not_counted[0].why.contains("a recess") && (f.not_counted[0].along_m - 8.0).abs() < 0.05, "{:?}", f.not_counted);
        // The picture draws.
        let png = r.png("Front wall", &f);
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn a_face_seen_by_no_station_finds_nothing() {
        let r = Raster::new(&Face { from: [0.0, 0.0], to: [6.0, 0.0] }, [0.0, 3.0], &[[0.0, -4.0]]);
        let f = r.find();
        assert!(f.openings.is_empty() && f.not_counted.is_empty(), "{f:?}");
    }

    fn size(w: f64, h: f64, unc: f64) -> Size {
        Size { width_mm: w, height_mm: h, width_uncertainty_mm: unc, height_uncertainty_mm: unc, sill_mm: Some(900.0), along_m: 1.0, from_foot: false, rows: 20, columns: 20 }
    }

    #[test]
    fn conformance_takes_the_uncertainty_into_account() {
        let e = Expected { kind: OpeningType::Window, width_mm: 914.0, height_mm: 1219.0 };
        assert_eq!(conformance(&e, Some(6.0), &size(916.0, 1218.0, 2.0)).0, "conforming");
        let (c, why) = conformance(&e, Some(6.0), &size(919.0, 1219.0, 2.0));
        assert_eq!(c, "neutral");
        assert!(why.contains("width 919 mm against 914 ± 6 mm, 5 mm over"), "{why}");
        let (c, why) = conformance(&e, Some(6.0), &size(914.0, 1200.0, 2.0));
        assert_eq!(c, "non_conforming");
        assert!(why.contains("height 1200 mm against 1219 ± 6 mm, 19 mm under"), "{why}");
        assert_eq!(conformance(&e, None, &size(914.0, 1219.0, 2.0)).0, "not_applicable");
    }

    fn opening(id: &str, w: f64, h: f64, kind: OpeningType, centre: [f64; 2]) -> Opening {
        Opening { opening_id: id.into(), along_m: centre[0], centre, box_m: [0.0; 4], looks_like: kind, size: Some(size(w, h, 2.0)), misfit: None, seen_through: 0.9 }
    }

    #[test]
    fn matches_are_proposed_by_size_and_type() {
        let lines = vec![
            ("W1".to_string(), Expected { kind: OpeningType::Window, width_mm: 914.0, height_mm: 1219.0 }),
            ("W2".to_string(), Expected { kind: OpeningType::Window, width_mm: 610.0, height_mm: 914.0 }),
            ("D1".to_string(), Expected { kind: OpeningType::Door, width_mm: 914.0, height_mm: 2032.0 }),
            ("W9".to_string(), Expected { kind: OpeningType::Window, width_mm: 2400.0, height_mm: 600.0 }),
        ];
        let os = [opening("f1-1", 612.0, 910.0, OpeningType::Window, [1.0, 1.0]), opening("f1-2", 918.0, 1215.0, OpeningType::Window, [3.0, 1.0]),
                  opening("f1-3", 910.0, 2030.0, OpeningType::Door, [5.0, 0.0])];
        let p = propose(&lines, &os.iter().collect::<Vec<_>>());
        let pairs: Vec<(&str, &str)> = p.iter().map(|(l, o, _)| (l.as_str(), o.as_str())).collect();
        assert_eq!(pairs, [("W1", "f1-2"), ("W2", "f1-1"), ("D1", "f1-3")]);
        assert!(p[1].2.contains("measured 612 × 910 mm against 610 × 914"), "{}", p[0].2);
    }

    fn face(seen: f64, os: Vec<Opening>) -> FaceCount {
        FaceCount { face_id: "f1".into(), label: "North wall".into(), face: Face { from: [0.0, 0.0], to: [8.0, 0.0] }, length_m: 8.0, seen_share: seen,
                    stations: vec![1, 2], unseen: Value::Null, found: Found { openings: os, not_counted: vec![] }, lines: vec!["W1".into()] }
    }

    #[test]
    fn a_line_is_judged_from_its_match() {
        let e = Expected { kind: OpeningType::Window, width_mm: 914.0, height_mm: 1219.0 };
        let mut c = LineCheck::default();
        judge_line(&mut c, &e, Some(6.0), &face(0.4, vec![]), "W1");
        assert_eq!((c.verdict, c.state().status), (Some(Verdict::NotYet), "not_yet"));
        assert!(c.reason.contains("North wall is 40% seen; the count needs 80%"), "{}", c.reason);
        let o = opening("f1-2", 916.0, 1220.0, OpeningType::Window, [3.0, 1.0]);
        judge_line(&mut c, &e, Some(6.0), &face(0.9, vec![o.clone()]), "W1");
        assert_eq!(c.verdict, None);
        assert!(c.reason.contains("1 opening found on North wall; a person confirms which one is W1"), "{}", c.reason);
        c.matched = Some(Match { opening_id: "f1-2".into(), centre: [3.1, 1.05], by: "crew".into(), via: "web UI".into(), at: "t".into() });
        judge_line(&mut c, &e, Some(6.0), &face(0.9, vec![o.clone()]), "W1");
        let s = c.state();
        assert_eq!((s.status, s.conformance), ("answered", Some("conforming")));
        let ev = c.evidence("p1").unwrap();
        assert_eq!((ev["grade"].as_str(), ev["reading"]["width_mm"].as_f64(), ev["pictures"][0].as_str()), (Some("measured"), Some(916.0), Some("/v1/projects/p1/count/f1/image")));
        assert!(ev["say"].as_str().unwrap().starts_with("916 × 1220 mm (36 1/16 in × 48 1/16 in)"), "{}", ev["say"]);
        // Found again a little way off after a new registration; gone elsewhere.
        judge_line(&mut c, &e, Some(6.0), &face(0.9, vec![opening("f1-1", 916.0, 1220.0, OpeningType::Window, [3.25, 1.0])]), "W1");
        assert_eq!(c.verdict, Some(Verdict::Answer));
        judge_line(&mut c, &e, Some(6.0), &face(0.9, vec![opening("f1-1", 916.0, 1220.0, OpeningType::Window, [5.0, 1.0])]), "W1");
        assert!(c.verdict.is_none() && c.reason.contains("nothing was found where W1 was matched"), "{c:?}");
        // Something in front of it now: does not fit, with the reason.
        let mut blocked = o;
        (blocked.size, blocked.misfit) = (None, Some("something stands in front of it".into()));
        judge_line(&mut c, &e, Some(6.0), &face(0.9, vec![blocked]), "W1");
        assert_eq!((c.state().status, c.reason.as_str()), ("does_not_fit", "something stands in front of it"));
    }
}
