//! Counting openings on the bridge (`specs/construction-counts.md`, part A):
//! the schedule's lines bound to wall faces, the openings found on each face,
//! the matches a person confirmed, and each line's verdict.
//!
//! A count reads every station that saw a bound face, once each, and keeps
//! what it found (`Inner::counts`) for matching against: a person confirms
//! which opening is which line (`confirm`), and the lines are judged again
//! from that count without reading the points again. After every
//! registration the faces are counted again, so a station that sees more of
//! a face, or moves one, shows in the verdicts.
//!
//! Counting runs where the full cloud is: on the laptop worker when one is
//! on the cable (`work::offer_count`), else on the Pi. Both run the same code
//! (`measure`) on the same stations; only the measuring moves. The bridge
//! checks what a worker sends back (`check_read`), proposes the matches,
//! judges the lines and keeps the record either way, with where the count
//! was made.

use crate::ask::Verdict;
use crate::list::Kind;
use crate::mission::{Mission, Status};
use crate::openings::{judge_line, propose, rematch, Again, Counted, FaceCount, LineCheck, Match, Raster};
use crate::problem::ApiError;
use crate::register::{Face, Pose, FACE_COVERED};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// A face to count: id, label, face, its lines, its coverage, the stations that saw it.
type Look = (String, String, Face, Vec<String>, Value, Vec<u16>);
/// What a count reads off one face: the openings, and its picture when the
/// face could be read.
pub type FaceRead = (crate::openings::Found, Option<Vec<u8>>);

/// What is measured on one face, as the bridge or a worker gets it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaceSpec {
    pub face_id: String,
    pub label: String,
    pub face: Face,
    /// The heights the stations saw it between (m); none when no station did.
    pub height_m: Option<[f64; 2]>,
    pub seen_share: f64,
    pub stations: Vec<u16>,
}

impl FaceSpec {
    /// The heights the face is read between: those seen, half a metre wider.
    fn band(&self) -> Option<[f64; 2]> {
        self.height_m.map(|[lo, hi]| [lo - 0.5, hi + 0.5])
    }

    /// The cells it is counted on; none when no station saw it.
    pub fn grid(&self) -> Option<crate::openings::Grid> {
        self.band().map(|z| crate::openings::Grid::of(&self.face, z))
    }
}

/// At most this many openings, and gaps not counted, on one face.
const MOST_PER_FACE: usize = 50;

/// Whether what a worker found on a face can be what `measure` finds there:
/// the bridge's numbering, every opening and gap inside the face's cells
/// with sizes no larger than their boxes, and a picture (only when the face
/// was read) of the size `measure` draws.
pub fn check_read(spec: &FaceSpec, found: &crate::openings::Found, png: Option<&[u8]>) -> Result<(), String> {
    let grid = spec.grid();
    match (grid, png) {
        (None, Some(_)) => return Err("a picture of a face no station saw".into()),
        (Some(_), None) => return Err("no picture of a face that was read".into()),
        (Some(g), Some(p)) => {
            let want = g.picture();
            let got = crate::work::png_size(p).ok_or("its picture isn't a PNG")?;
            if (got.0 as usize, got.1 as usize) != want {
                return Err(format!("its picture is {}×{} pixels, not {}×{}", got.0, got.1, want.0, want.1));
            }
        }
        (None, None) => {}
    }
    if found.openings.len() > MOST_PER_FACE || found.not_counted.len() > MOST_PER_FACE {
        return Err(format!("more than {MOST_PER_FACE} openings or gaps on one face"));
    }
    if grid.is_none() && !(found.openings.is_empty() && found.not_counted.is_empty()) {
        return Err("openings on a face no station saw".into());
    }
    let Some(g) = grid else { return Ok(()) };
    let slack = 1e-6;
    let inside = |u: f64, z: f64| (-slack..=g.u_max + slack).contains(&u) && (g.z[0] - slack..=g.z[1] + slack).contains(&z);
    let a_box = |[u0, z0, u1, z1]: [f64; 4]| [u0, z0, u1, z1].iter().all(|v| v.is_finite()) && u0 < u1 && z0 < z1 && inside(u0, z0) && inside(u1, z1);
    if found.openings.iter().any(|o| o.same_as.is_some()) || found.not_counted.iter().any(|g| g.same_as.is_some()) {
        return Err("same_as is the bridge's to say".into());
    }
    for (k, o) in found.openings.iter().enumerate() {
        let id = format!("{}-{}", spec.face_id, k + 1);
        if o.opening_id != id {
            return Err(format!("opening {} should be {id}: the openings are numbered in order", o.opening_id));
        }
        let [u0, z0, u1, z1] = o.box_m;
        let ok = a_box(o.box_m)
            && (u0 - slack..=u1 + slack).contains(&o.centre[0])
            && (z0 - slack..=z1 + slack).contains(&o.centre[1])
            && inside(o.along_m, z0)
            && (0.0..=1.0).contains(&o.seen_through)
            && o.size.as_ref().is_none_or(|s| {
                let mm = [s.width_mm, s.height_mm, s.width_uncertainty_mm, s.height_uncertainty_mm];
                let tallest = if s.from_foot { z1 - g.z[0] } else { z1 - z0 };
                mm.iter().all(|v| v.is_finite() && *v >= 0.0)
                    && s.sill_mm.is_none_or(f64::is_finite)
                    && s.width_mm <= (u1 - u0 + g.cell) * 1000.0
                    && s.height_mm <= (tallest + g.cell) * 1000.0
                    && inside(s.along_m, z0)
            });
        if !ok {
            return Err(format!("opening {id} isn't inside the face, or is larger than its box"));
        }
    }
    for gap in &found.not_counted {
        if !(gap.width_m.is_finite() && gap.height_m.is_finite() && a_box([gap.along_m, gap.bottom_m, gap.along_m + gap.width_m, gap.bottom_m + gap.height_m])) {
            return Err("a gap not counted isn't inside the face".into());
        }
    }
    Ok(())
}

/// Read the stations' points onto each face and find its openings, with a
/// picture per face that could be read. `files` are the stations that saw
/// any of the faces (number, pose, scan file), in the registration's order.
/// The bridge and a worker both count with this, so they find the same.
pub fn measure(specs: &[FaceSpec], files: &[(u16, Pose, PathBuf)]) -> anyhow::Result<Vec<FaceRead>> {
    let mut rasters: Vec<Option<Raster>> = specs
        .iter()
        .map(|f| {
            let band = f.band()?;
            let xy: Vec<[f64; 2]> = files.iter().filter(|(n, _, _)| f.stations.contains(n)).map(|(_, p, _)| [p.xyz_m[0], p.xyz_m[1]]).collect();
            Some(Raster::new(&f.face, band, &xy))
        })
        .collect();
    // The stations that saw a face, decoded several at once on the laptop
    // and added in the files' order.
    let used: Vec<(Vec<usize>, &Pose, &std::path::Path)> = files
        .iter()
        .map(|(n, pose, path)| ((0..specs.len()).filter(|&k| rasters[k].is_some() && specs[k].stations.contains(n)).collect::<Vec<usize>>(), pose, path.as_path()))
        .filter(|(mine, _, _)| !mine.is_empty())
        .collect();
    let paths: Vec<&std::path::Path> = used.iter().map(|u| u.2).collect();
    crate::points::each_station(&paths, |i, p| {
        let (mine, pose, _) = &used[i];
        let s = pose.xyz_m;
        let ds: Vec<f64> = mine.iter().map(|&k| rasters[k].as_ref().unwrap().standoff(s)).collect();
        p.walk(pose, |_, w, range| {
            if range <= crate::openings::RANGE_M {
                for (j, &k) in mine.iter().enumerate() {
                    if ds[j] > 0.3 {
                        rasters[k].as_mut().unwrap().add(s, ds[j], w);
                    }
                }
            }
        });
        Ok(())
    })?;
    Ok(specs
        .iter()
        .zip(rasters)
        .map(|(f, r)| {
            let mut found = r.as_ref().map(Raster::find).unwrap_or_default();
            number(&mut found, &f.face_id);
            let png = r.as_ref().map(|r| {
                let n = found.openings.len();
                r.png(&format!("{}: {n} opening{}, {:.0}% seen", f.label, if n == 1 { "" } else { "s" }, f.seen_share * 100.0), &found)
            });
            (found, png)
        })
        .collect())
}

/// Name a face's openings in order: f1-1, f1-2, …
fn number(found: &mut crate::openings::Found, face_id: &str) {
    for (k, o) in found.openings.iter_mut().enumerate() {
        o.opening_id = format!("{face_id}-{}", k + 1);
    }
}

/// Two bound faces are one wall when they run parallel within this angle…
const SAME_WALL_DEG: f64 = 5.0;
/// …and, where they overlap, each lies within this of the other's line: the
/// 15 cm the registration takes a point to be on a face at.
const SAME_WALL_M: f64 = 0.15;

/// Where `b` overlaps `a`, measured along `a`: (how far, m; how far off
/// `b`'s line `a` is at the overlap's two ends, the larger, m).
fn overlap_on(a: &Face, b: &Face) -> Option<(f64, f64)> {
    let d = [a.to[0] - a.from[0], a.to[1] - a.from[1]];
    let len = d[0].hypot(d[1]);
    let e = [b.to[0] - b.from[0], b.to[1] - b.from[1]];
    let len_b = e[0].hypot(e[1]);
    if len < 1e-6 || len_b < 1e-6 {
        return None;
    }
    let u = [d[0] / len, d[1] / len];
    let w = [e[0] / len_b, e[1] / len_b];
    let along = |p: [f64; 2]| u[0] * (p[0] - a.from[0]) + u[1] * (p[1] - a.from[1]);
    let (x, y) = (along(b.from), along(b.to));
    let (lo, hi) = (x.min(y).max(0.0), x.max(y).min(len));
    if hi <= lo {
        return None;
    }
    let off_b = |t: f64| {
        let p = [a.from[0] + u[0] * t, a.from[1] + u[1] * t];
        (w[0] * (p[1] - b.from[1]) - w[1] * (p[0] - b.from[0])).abs()
    };
    Some((hi - lo, off_b(lo).max(off_b(hi))))
}

/// How far `a` and `b` run along each other when they are one wall (m);
/// none when they aren't, or don't overlap. The same either way round:
/// parallel or antiparallel within `SAME_WALL_DEG`, and within
/// `SAME_WALL_M` of each other over the overlap, measured from both.
pub fn same_wall(a: &Face, b: &Face) -> Option<f64> {
    let (ab, ba) = (overlap_on(a, b)?, overlap_on(b, a)?);
    let dir = |f: &Face| {
        let d = [f.to[0] - f.from[0], f.to[1] - f.from[1]];
        let l = d[0].hypot(d[1]);
        [d[0] / l, d[1] / l]
    };
    let (u, w) = (dir(a), dir(b));
    if (u[0] * w[1] - u[1] * w[0]).abs() > SAME_WALL_DEG.to_radians().sin() || ab.1.min(ba.1) > SAME_WALL_M {
        return None;
    }
    Some(ab.0.min(ba.0))
}

/// Two openings, or gaps, on one wall are the same when their boxes overlap
/// across at least this share of the smaller: an opening one face's end
/// cuts through is part of the same hole, though its middle has moved.
const SAME_BOX: f64 = 0.5;

/// A box on face `f` (u0, z0, u1, z1, m along it), as it lies along face
/// `a` of the same wall.
fn along(a: &Face, f: &Face, b: [f64; 4]) -> [f64; 4] {
    let unit = |g: &Face| {
        let d = [g.to[0] - g.from[0], g.to[1] - g.from[1]];
        let len = d[0].hypot(d[1]).max(1e-6);
        [d[0] / len, d[1] / len]
    };
    let (ua, uf) = (unit(a), unit(f));
    let u = |t: f64| {
        let p = [f.from[0] + uf[0] * t, f.from[1] + uf[1] * t];
        ua[0] * (p[0] - a.from[0]) + ua[1] * (p[1] - a.from[1])
    };
    let (x, y) = (u(b[0]), u(b[2]));
    [x.min(y), b[1], x.max(y), b[3]]
}

/// Whether box `p` on face `fa` and box `q` on face `fb`, both one wall,
/// are the same opening (`SAME_BOX`).
fn same_box(fa: &Face, p: [f64; 4], fb: &Face, q: [f64; 4]) -> bool {
    let q = along(fa, fb, q);
    let w = (p[2].min(q[2]) - p[0].max(q[0])).max(0.0);
    let h = (p[3].min(q[3]) - p[1].max(q[1])).max(0.0);
    let area = |b: [f64; 4]| (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0);
    let least = area(p).min(area(q));
    least > 0.0 && w * h >= SAME_BOX * least
}

fn gap_box(g: &crate::openings::Gap) -> [f64; 4] {
    [g.along_m, g.bottom_m, g.along_m + g.width_m, g.bottom_m + g.height_m]
}

/// Count each opening once. Bound faces of one wall overlap (a wall and a
/// stretch of it, or two walls the registration split), and an opening in
/// the overlap is found on each. The earlier face in the list's order keeps
/// it; on a later one it is `same_as` that one: their boxes overlap
/// (`SAME_BOX`). A gap not counted is listed once the same way.
fn count_once(faces: &mut [FaceCount]) {
    for j in 1..faces.len() {
        let earlier: Vec<usize> = (0..j).filter(|&i| same_wall(&faces[i].face, &faces[j].face).is_some()).collect();
        if earlier.is_empty() {
            continue;
        }
        let fjf = faces[j].face;
        // Its openings: the same as one an earlier face counted.
        {
            let (before, rest) = faces.split_at_mut(j);
            for o in rest[0].found.openings.iter_mut() {
                o.same_as = earlier.iter().find_map(|&i| before[i].found.openings.iter().find(|p| same_box(&before[i].face, p.box_m, &fjf, o.box_m)).map(|p| p.physical().to_string()));
            }
        }
        // An earlier face's gap that is one of them (an earlier face saw
        // less of the hole) is that opening, not a gap of its own.
        let holes: Vec<([f64; 4], String)> = faces[j].found.openings.iter().map(|o| (o.box_m, o.physical().to_string())).collect();
        for &i in &earlier {
            let fi = faces[i].face;
            for h in faces[i].found.not_counted.iter_mut() {
                if let Some((_, id)) = holes.iter().find(|(b, _)| same_box(&fi, gap_box(h), &fjf, *b)) {
                    h.same_as = Some(id.clone());
                }
            }
        }
        // Its gaps: an opening counted already, or a gap listed already
        // (followed to where it was first listed).
        let (before, rest) = faces.split_at_mut(j);
        for g in rest[0].found.not_counted.iter_mut() {
            let b = gap_box(g);
            g.same_as = earlier.iter().find_map(|&i| {
                let fi = &before[i];
                let opening = fi.found.openings.iter().find(|p| same_box(&fi.face, p.box_m, &fjf, b)).map(|p| p.physical().to_string());
                opening.or_else(|| fi.found.not_counted.iter().find(|h| same_box(&fi.face, gap_box(h), &fjf, b)).map(|h| h.same_as.clone().unwrap_or_else(|| fi.face_id.clone())))
            });
        }
    }
}

/// Each matched line's opening in `counted`, found again on its face, all
/// lines at once (`rematch`).
fn found_again<'a>(m: &Mission, counted: &'a Counted) -> std::collections::HashMap<String, Again<'a>> {
    let wants: Vec<(String, &'a [crate::openings::Opening], [f64; 2])> = m
        .targets
        .iter()
        .filter_map(|t| {
            let mt = t.opening.as_ref()?.matched.as_ref()?;
            let fc = counted.faces.iter().find(|f| Some(f.face) == t.face)?;
            Some((t.target_id.clone(), fc.found.openings.as_slice(), mt.centre))
        })
        .collect();
    rematch(&wants)
}

/// The bound faces of opening lines that `face` runs along, other than
/// itself: (label, how far, m).
pub fn overlaps(m: &Mission, face: &Face) -> Vec<(String, f64)> {
    let mut out: Vec<(Face, String, f64)> = Vec::new();
    for b in bound_lines(m) {
        if b.face == *face || out.iter().any(|o| o.0 == b.face) {
            continue;
        }
        if let Some(by) = same_wall(&b.face, face) {
            out.push((b.face, b.label, (by * 10.0).round() / 10.0));
        }
    }
    out.into_iter().map(|(_, l, by)| (l, by)).collect()
}

/// A bound opening line: its id, what it expects, its tolerance, its face.
struct Bound {
    line_id: String,
    expected: crate::list::Expected,
    max_mm: Option<f64>,
    face: Face,
    label: String,
}

fn bound_lines(m: &Mission) -> Vec<Bound> {
    let Some(list) = &m.list else { return vec![] };
    list.lines
        .iter()
        .filter(|l| l.kind() == Some(Kind::Opening))
        .filter_map(|l| {
            let t = m.targets.iter().find(|t| t.target_id == l.line_id)?;
            Some(Bound {
                line_id: l.line_id.clone(),
                expected: l.question.as_ref()?.opening.clone()?,
                max_mm: l.tolerance.as_ref().and_then(|t| t.max_mm),
                face: t.face?,
                label: t.bound.as_ref().map(|b| b.label.clone()).unwrap_or_else(|| "the face".into()),
            })
        })
        .collect()
}

/// The faces of the bound lines, in the list's order: id, label, face, lines.
fn faces_of(bound: &[Bound]) -> Vec<(String, String, Face, Vec<String>)> {
    let mut faces: Vec<(String, String, Face, Vec<String>)> = Vec::new();
    for b in bound {
        match faces.iter_mut().find(|f| f.2 == b.face) {
            Some(f) => f.3.push(b.line_id.clone()),
            None => {
                let id = format!("f{}", faces.len() + 1);
                faces.push((id, b.label.clone(), b.face, vec![b.line_id.clone()]));
            }
        }
    }
    faces
}

/// Judge every bound opening line from the count, and set its target's
/// status: covered by an answer (naming the scans of the stations its face
/// was counted from), open otherwise. Declined lines stay declined.
fn apply(m: &mut Mission, counted: &Counted, scans: &std::collections::HashMap<u16, String>) {
    let bound = bound_lines(m);
    let again = found_again(m, counted);
    for b in &bound {
        let Some(fc) = counted.faces.iter().find(|f| f.face == b.face) else { continue };
        let Some(t) = m.targets.iter_mut().find(|t| t.target_id == b.line_id) else { continue };
        let mut c = t.opening.clone().unwrap_or_default();
        let before = (c.verdict, c.size.clone(), c.conformance.clone());
        judge_line(&mut c, &b.expected, b.max_mm, fc, &b.line_id, again.get(&b.line_id));
        // Openings are numbered afresh by every count: a match found again
        // takes the id it has now. Where it was matched stays as the person
        // matched it, so a match can't drift along the wall count by count.
        if let (Some(Again::Found(o)), Some(mt)) = (again.get(&b.line_id), c.matched.as_mut()) {
            mt.opening_id = o.opening_id.clone();
        }
        c.counted_on = Some(counted.counted_on.clone().unwrap_or_else(|| "bridge".into()));
        c.counted_by = counted.counted_by.clone();
        if t.status != Status::Declined {
            let answered = c.verdict == Some(Verdict::Answer);
            t.status = if answered { Status::Covered } else { Status::Open };
            t.stations = if answered { c.stations.iter().filter_map(|n| scans.get(n).cloned()).collect() } else { Vec::new() };
        }
        if (c.verdict, c.size.clone(), c.conformance.clone()) != before {
            t.updated_at = crate::util::now_rfc3339();
        }
        t.opening = Some(c);
    }
}

/// What a count says: per face, what was found and how much was seen; per
/// line, its state; the matches the bridge proposes for a person to confirm.
fn said(project_id: &str, counted: &Counted, m: &Mission) -> Value {
    let bound = bound_lines(m);
    // Which line each opening is now, by the opening it is (`physical`):
    // the matches found again in this count, all lines at once.
    let again = found_again(m, counted);
    let mut on_now: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (line, a) in &again {
        if let Again::Found(o) = a {
            on_now.insert(o.physical().to_string(), line.clone());
        }
    }
    let mut proposed = Vec::new();
    // Openings proposed, and those on no face with lines left to match, by the opening they are.
    let mut put: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut not_on: Vec<String> = Vec::new();
    // Openings a face with lines still to match could take.
    let mut wanted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut faces = Vec::new();
    let mut parts = Vec::new();
    let mut measured_once: std::collections::HashSet<String> = std::collections::HashSet::new();
    for fc in &counted.faces {
        let unmatched_lines: Vec<(String, crate::list::Expected)> = bound
            .iter()
            .filter(|b| b.face == fc.face && !on_now.values().any(|l| l == &b.line_id))
            .filter(|b| m.targets.iter().find(|t| t.target_id == b.line_id).is_some_and(|t| t.status != Status::Declined))
            .map(|b| (b.line_id.clone(), b.expected.clone()))
            .collect();
        let free: Vec<&crate::openings::Opening> = fc.found.openings.iter().filter(|o| !on_now.contains_key(o.physical()) && !put.contains(o.physical())).collect();
        if !unmatched_lines.is_empty() {
            wanted.extend(free.iter().map(|o| o.physical().to_string()));
        }
        if fc.seen_share >= FACE_COVERED {
            for (l, o, why) in propose(&unmatched_lines, &free) {
                if let Some(op) = fc.found.openings.iter().find(|x| x.opening_id == o) {
                    put.insert(op.physical().to_string());
                }
                proposed.push(json!({"line_id": l, "opening_id": o, "why": why}));
            }
            if unmatched_lines.is_empty() {
                not_on.extend(free.iter().map(|o| o.physical().to_string()));
            }
        }
        for o in fc.found.openings.iter().filter(|o| o.size.is_some() && o.misfit.is_none()) {
            measured_once.insert(o.physical().to_string());
        }
        let n = fc.found.openings.len();
        let least = if fc.seen_share < FACE_COVERED { "at least " } else { "" };
        let shared: Vec<&str> = fc.found.openings.iter().filter_map(|o| o.same_as.as_deref()).collect();
        let shared = match shared.len() {
            0 => String::new(),
            1 => format!(", 1 of them {} counted already", shared[0]),
            k => format!(", {k} of them {} counted already", shared.join(", ")),
        };
        parts.push(format!("{}: {least}{n} opening{}{shared} ({:.0}% seen)", fc.label, if n == 1 { "" } else { "s" }, fc.seen_share * 100.0));
        let openings: Vec<Value> = fc
            .found
            .openings
            .iter()
            .map(|o| {
                let mut v = json!(o);
                if let Some(s) = &o.size {
                    v["said"] = json!(crate::openings::size_said(s));
                }
                v["line_id"] = json!(on_now.get(o.physical()));
                v
            })
            .collect();
        faces.push(json!({
            "face_id": fc.face_id, "label": fc.label, "from": fc.face.from, "to": fc.face.to, "length_m": fc.length_m,
            "seen_share": fc.seen_share, "enough_seen": fc.seen_share >= FACE_COVERED, "stations": fc.stations, "unseen": fc.unseen,
            "openings": openings, "not_counted": fc.found.not_counted, "lines": fc.lines,
            "image": format!("/v1/projects/{project_id}/count/{}/image", fc.face_id),
        }));
    }
    let lines: Vec<Value> = crate::list::lines_said(m)
        .into_iter()
        .filter(|l| l["question"] == "opening")
        .map(|mut l| {
            let id = l["line_id"].as_str().unwrap_or("").to_string();
            if let Some(c) = m.targets.iter().find(|t| t.target_id == id).and_then(|t| t.opening.as_ref()) {
                if let Some(s) = &c.size {
                    l["size"] = json!(s);
                    l["said"] = json!(crate::openings::size_said(s));
                }
                l["opening_id"] = json!(match again.get(&id) {
                    Some(Again::Found(o)) => Some(o.opening_id.clone()),
                    _ => None,
                });
            }
            l
        })
        .collect();
    // Each opening once: those not the same as one on an earlier face.
    let found_all: usize = counted.faces.iter().map(|f| f.found.openings.iter().filter(|o| o.same_as.is_none()).count()).sum();
    let measured_all = measured_once.len();
    let mut seen_not_on = std::collections::HashSet::new();
    not_on.retain(|o| !put.contains(o) && !wanted.contains(o) && seen_not_on.insert(o.clone()));
    let answered = lines.iter().filter(|l| l["status"] == "answered").count();
    let unbound = lines.iter().filter(|l| l["bound"].is_null()).count();
    let mut say = if counted.faces.is_empty() {
        "No opening line is bound to a wall face yet: a person binds the schedule's lines to the walls they are on.".to_string()
    } else {
        format!("{found_all} opening{} found, {measured_all} measured. {}. {answered} of {} line{} answered.",
                if found_all == 1 { "" } else { "s" }, parts.join("; "), lines.len(), if lines.len() == 1 { "" } else { "s" })
    };
    if unbound > 0 && !counted.faces.is_empty() {
        say.push_str(&format!(" {unbound} line{} not bound to a face yet.", if unbound == 1 { " is" } else { "s are" }));
    }
    let recesses: usize = counted.faces.iter().map(|f| f.found.not_counted.iter().filter(|g| g.same_as.is_none()).count()).sum();
    if recesses > 0 {
        say.push_str(&format!(" {recesses} gap{} not counted (see not_counted).", if recesses == 1 { "" } else { "s" }));
    }
    json!({"project_id": project_id, "counted_at": counted.counted_at, "counted_on": counted.counted_on.as_deref().unwrap_or("bridge"), "counted_by": counted.counted_by,
           "say": say, "faces": faces, "lines": lines, "proposed": proposed, "not_on_schedule": not_on})
}

/// Count the openings on every face a schedule line is bound to, and judge
/// the lines. `by`: who asked ("bridge" after a registration).
///
/// Counts of one project run one at a time. A count that waited while
/// another, started after it was asked, finished gives that one's result:
/// it read the faces as they stood when this one was asked, or later.
pub async fn count(st: &Arc<AppState>, project_id: &str, by: &str) -> Result<Value, ApiError> {
    let t0 = std::time::Instant::now();
    let lock = {
        let mut inner = st.inner.lock().unwrap();
        inner.asking.entry((project_id.to_string(), "#count".into())).or_default().clone()
    };
    let _one = lock.lock().await;
    if st.inner.lock().unwrap().count_started.get(project_id).is_some_and(|s| *s > t0) {
        let mut out = latest(st, project_id)?;
        out["took_s"] = json!(((t0.elapsed().as_secs_f64()) * 10.0).round() / 10.0);
        out["newer_count"] = json!(true);
        return Ok(out);
    }
    let started = std::time::Instant::now();
    let (m, reg) = {
        let inner = st.inner.lock().unwrap();
        let m = inner.missions.get(project_id).cloned().ok_or_else(|| ApiError::not_found("mission"))?;
        (m, inner.registrations.get(project_id).cloned())
    };
    if m.list.is_none() {
        return Err(ApiError::invalid("the mission has no list: load the schedule first (PUT /v1/projects/{id}/list)"));
    }
    let bound = bound_lines(&m);
    let faces = faces_of(&bound);
    let geo = st.geo(project_id);
    let scans = reg.as_ref().map(|r| r.station_scans()).unwrap_or_default();
    let poses: std::collections::HashMap<u16, Pose> = reg.iter().flat_map(|r| r.stations.iter()).filter(|s| scans.contains_key(&s.station)).filter_map(|s| s.pose.map(|p| (s.station, p))).collect();
    // Each face's coverage, and the stations that saw it.
    let mut looks: Vec<Look> = Vec::new();
    for (id, label, face, lines) in &faces {
        let cov = geo.judge(face).unwrap_or_else(|| json!({"seen_share": 0.0, "stations": [], "unseen": []}));
        if let Some(e) = cov["error"].as_str() {
            return Err(ApiError::invalid(format!("{label}: {e}")));
        }
        let stations: Vec<u16> = cov["stations"].as_array().map(|a| a.iter().filter_map(|s| s["station"].as_u64().map(|n| n as u16)).collect()).unwrap_or_default();
        looks.push((id.clone(), label.clone(), *face, lines.clone(), cov, stations));
    }
    let wanted: BTreeSet<u16> = looks.iter().flat_map(|l| l.5.iter().copied()).filter(|n| poses.contains_key(n)).collect();
    let specs: Vec<FaceSpec> = looks
        .iter()
        .map(|(id, label, face, _, cov, stations)| FaceSpec {
            face_id: id.clone(),
            label: label.clone(),
            face: *face,
            height_m: cov["height_m"][0].as_f64().zip(cov["height_m"][1].as_f64()).map(|(lo, hi)| [lo, hi]),
            seen_share: cov["seen_share"].as_f64().unwrap_or(0.0),
            stations: stations.clone(),
        })
        .collect();
    let stations: Vec<(u16, String, Pose)> = reg.iter().flat_map(|r| r.stations.iter()).filter(|s| wanted.contains(&s.station)).map(|s| (s.station, s.scan_id.clone(), poses[&s.station])).collect();
    // On the laptop when one takes it in time, else here.
    let mut counted_on = None;
    let mut reads = None;
    if !stations.is_empty() {
        let body = json!({"faces": specs, "stations": stations.iter().map(|(n, id, pose)| json!({"station": n, "scan_id": id, "pose": pose})).collect::<Vec<_>>()});
        match crate::work::offer_count(st, project_id, body, specs.clone(), crate::work::COUNT_LIMIT).await {
            crate::work::CountOffered::Done(r, w, token) => {
                reads = Some(r);
                counted_on = Some((w, token));
            }
            crate::work::CountOffered::Failed(w, why) => {
                st.log("count", format!("The count on {w} didn't come back ({why}); the bridge counts it itself"), "bridge", Some(project_id), &[]);
            }
            crate::work::CountOffered::NoWorker => {}
        }
    }
    let reads = match reads {
        Some(r) => r,
        None => {
            let _reading = crate::files::reading(st, stations.iter().map(|(_, id, _)| id.clone()));
            let mut files: Vec<(u16, Pose, PathBuf)> = Vec::new();
            for (n, id, pose) in &stations {
                files.push((*n, *pose, crate::files::scan_file(st, id, crate::files::FileKind::Tzf).await?));
            }
            let these = specs.clone();
            tokio::task::spawn_blocking(move || measure(&these, &files))
                .await
                .map_err(|e| ApiError::cache(format!("counting: {e}")))?
                .map_err(|e| ApiError::cache(format!("counting: {e:#}")))?
        }
    };
    let mut faces = Vec::new();
    let mut pictures = Vec::new();
    for ((id, label, face, lines, cov, stations), (mut found, png)) in looks.into_iter().zip(reads) {
        number(&mut found, &id);
        if let Some(png) = png {
            pictures.push((id.clone(), png));
        }
        faces.push(FaceCount { face_id: id, label, face, length_m: cov["length_m"].as_f64().unwrap_or(0.0), seen_share: cov["seen_share"].as_f64().unwrap_or(0.0), stations, unseen: cov["unseen"].clone(), found, lines });
    }
    count_once(&mut faces);
    let (counted_on, counted_by) = counted_on.map_or((None, None), |(w, t)| (Some(w), Some(t)));
    let counted = Counted { counted_at: crate::util::now_rfc3339(), counted_on: counted_on.clone(), counted_by, faces };
    // Judge the lines on the mission as it is now (it may have changed meanwhile).
    let out = {
        let mut inner = st.inner.lock().unwrap();
        let mut m = inner.missions.get(project_id).cloned().ok_or_else(|| ApiError::not_found("mission"))?;
        apply(&mut m, &counted, &scans);
        let v = said(project_id, &counted, &m);
        inner.missions.insert(project_id.to_string(), m.clone());
        inner.counts.insert(project_id.to_string(), counted.clone());
        inner.count_started.insert(project_id.to_string(), started);
        inner.count_pictures.retain(|(p, _), _| p != project_id);
        for (id, png) in pictures {
            inner.count_pictures.insert((project_id.to_string(), id), png);
        }
        drop(inner);
        st.save_missions();
        st.emit("mission", json!(m));
        v
    };
    let took = t0.elapsed().as_secs_f64();
    if !counted.faces.is_empty() {
        let on = counted_on.map(|w| format!(", counted on {w}")).unwrap_or_default();
        st.log("count", format!("Counted openings ({took:.1} s{on}): {}", out["say"].as_str().unwrap_or("")), by, Some(project_id), &[]);
    }
    let mut out = out;
    out["took_s"] = json!((took * 10.0).round() / 10.0);
    Ok(out)
}

/// The latest count, as `count` said it, with the lines as they are now.
pub fn latest(st: &AppState, project_id: &str) -> Result<Value, ApiError> {
    let inner = st.inner.lock().unwrap();
    let m = inner.missions.get(project_id).ok_or_else(|| ApiError::not_found("mission"))?;
    let c = inner.counts.get(project_id).ok_or_else(|| ApiError::not_found("count (count the openings first: POST this route)"))?;
    Ok(said(project_id, c, m))
}

#[derive(Debug, Deserialize)]
pub struct MatchItem {
    pub line_id: String,
    /// The opening it is, or null to undo a match.
    pub opening_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MatchReq {
    pub matches: Vec<MatchItem>,
    #[serde(default)]
    pub confirmed_via: Option<String>,
}

/// A person says which opening is which line (or undoes a match). Checked
/// against the latest count; the lines are judged again from it.
pub fn confirm(st: &AppState, project_id: &str, req: &MatchReq, by: &str) -> Result<Value, ApiError> {
    let via = req.confirmed_via.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(|v| v.chars().take(300).collect::<String>()).unwrap_or_else(|| "web UI".into());
    let (out, lines) = {
        let mut inner = st.inner.lock().unwrap();
        let counted = inner.counts.get(project_id).cloned().ok_or_else(|| ApiError::conflict("nothing counted yet: count the openings first, then confirm the matches"))?;
        let mut m = inner.missions.get(project_id).cloned().ok_or_else(|| ApiError::not_found("mission"))?;
        if m.state == crate::mission::State::SignedOff {
            return Err(ApiError::conflict("the mission is signed off"));
        }
        if req.matches.is_empty() {
            return Err(ApiError::invalid("matches: at least one"));
        }
        // The opening each other matched line holds in this count, found
        // again as the count found it (its stored id may be an older count's).
        let held: std::collections::HashMap<String, String> = found_again(&m, &counted)
            .into_iter()
            .filter_map(|(l, a)| if let Again::Found(o) = a { Some((l, o.physical().to_string())) } else { None })
            .collect();
        let asked: std::collections::HashSet<&str> = req.matches.iter().map(|it| it.line_id.trim()).collect();
        let mut lines = Vec::new();
        for it in &req.matches {
            let id = it.line_id.trim();
            let line = m.list.as_ref().and_then(|l| l.line(id)).ok_or_else(|| ApiError::invalid(format!("there is no line {id} in the list")))?;
            if line.kind() != Some(Kind::Opening) {
                return Err(ApiError::invalid(format!("line {id} isn't an opening")));
            }
            let t = m.targets.iter_mut().find(|t| t.target_id == id).expect("a target per line");
            let face = t.face.ok_or_else(|| ApiError::invalid(format!("line {id} isn't bound to a face yet")))?;
            let c = t.opening.get_or_insert_with(LineCheck::default);
            match it.opening_id.as_deref().map(str::trim) {
                None | Some("") => {
                    c.matched = None;
                    lines.push(format!("{id}: no match"));
                }
                Some(oid) => {
                    let (fc, o) = counted.opening(oid).ok_or_else(|| ApiError::invalid(format!("there is no opening {oid} in the latest count")))?;
                    if fc.face != face {
                        return Err(ApiError::invalid(format!("opening {oid} is on {}, not on line {id}'s face", fc.label)));
                    }
                    c.matched = Some(Match { opening_id: oid.into(), centre: o.centre, by: by.into(), via: via.clone(), at: crate::util::now_rfc3339() });
                    let size = o.size.as_ref().map(|s| format!(", {}", crate::openings::size_said(s))).unwrap_or_default();
                    lines.push(format!("{id} is {oid} ({}{size})", o.looks_like.word()));
                }
            }
        }
        // One line per opening, whichever face it was matched on.
        let mut taken: std::collections::HashMap<String, (&str, &str)> = std::collections::HashMap::new();
        for t in &m.targets {
            let Some(mt) = t.opening.as_ref().and_then(|c| c.matched.as_ref()) else { continue };
            let physical = if asked.contains(t.target_id.as_str()) {
                counted.opening(&mt.opening_id).map_or(mt.opening_id.clone(), |(_, o)| o.physical().to_string())
            } else {
                // A match not found again in this count holds nothing.
                let Some(p) = held.get(&t.target_id) else { continue };
                p.clone()
            };
            if let Some((other, as_id)) = taken.insert(physical, (&t.target_id, &mt.opening_id)) {
                let what = if as_id == mt.opening_id { format!("opening {}", mt.opening_id) } else { format!("openings {as_id} and {} are the same, and", mt.opening_id) };
                return Err(ApiError::invalid(format!("{what} would be both {other} and {}: undo one match (opening_id null) in the same request", t.target_id)));
            }
        }
        let scans = inner.registrations.get(project_id).map(|r| r.station_scans()).unwrap_or_default();
        apply(&mut m, &counted, &scans);
        let v = said(project_id, &counted, &m);
        inner.missions.insert(project_id.to_string(), m.clone());
        drop(inner);
        st.save_missions();
        st.emit("mission", json!(m));
        (v, lines)
    };
    st.log("approval", format!("Matched openings: {} ({via})", lines.join("; ")), by, Some(project_id), &[]);
    Ok(out)
}

/// After a registration: count again when any opening line is bound.
pub async fn after_registration(st: &Arc<AppState>, project_id: &str) {
    let any = st.inner.lock().unwrap().missions.get(project_id).is_some_and(|m| m.state != crate::mission::State::SignedOff && !bound_lines(m).is_empty());
    if any {
        if let Err(e) = count(st, project_id, "bridge").await {
            st.log("count", format!("Counting the openings again after the registration failed: {}", e.detail.unwrap_or(e.title)), "bridge", Some(project_id), &[]);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::register::{Merged, Registration, StationPose};
    use crate::testkit::{Bridge, AGENT, OPERATOR, VIEWER};
    use axum::http::StatusCode;

    const SCAN: &str = "5b5314cf-76b8-4f59-a445-ceee1e6df4c3";
    pub(crate) const P: &str = "39187455-8278-428c-89ad-8a4835f95be6";

    const SCHEDULE: &str = "mark,type,width,height,tolerance,wall\n\
        W1,window,1000 mm,1200 mm,10 mm,Front\n\
        W2,window,900 mm,1200 mm,10 mm,Front\n\
        D1,door,900 mm,2100 mm,10 mm,Front\n\
        D9,door,800 mm,2000 mm,,Back\n";

    /// A bridge whose one scan is the ray-cast facade from the origin,
    /// registered as station 1, with the wall w1 along it.
    async fn facade_bridge() -> (Bridge, std::path::PathBuf) {
        let fx = crate::export::tests::scratch("x9-count-fixtures");
        let files = crate::openings::tests::facade_scans(&crate::openings::tests::scene(), &fx, &[([0.0, 0.0, 0.0], 0.0)]);
        std::fs::rename(&files[0].2, fx.join("dome.tzf")).unwrap();
        std::fs::write(fx.join("dome.jpg"), crate::export::tests::grey_jpeg()).unwrap();
        let b = Bridge::with_mock(x9_mock::Config { fixtures: Some(fx.to_string_lossy().into_owned()), ..crate::testkit::fast_mock() }).await;
        let face = Face { from: [5.0, -5.0], to: [5.0, 5.0] };
        let reg = Registration { project_id: P.into(), frame_scan_id: Some(SCAN.into()),
                                 stations: vec![StationPose { scan_id: SCAN.into(), station: 1, pose: Some(Pose { turn_deg: 0.0, xyz_m: [0.0; 3] }), audit: None }],
                                 pairs: vec![], walls: vec![crate::ask::face_wall("w1", &face, [-1.5, 1.5], vec![1])], summary: String::new(), updated_at: String::new() };
        let mut m = Merged::default();
        for iy in 0..=100 {
            for iz in 0..=30 {
                m.pts.push([5.0, -5.0 + iy as f32 * 0.1, -1.5 + iz as f32 * 0.1]);
                m.spacing_mm.push(10.0);
                m.station.push(1);
            }
        }
        {
            let mut inner = b.st.inner.lock().unwrap();
            inner.registrations.insert(P.into(), reg);
            inner.merged.insert(P.into(), Arc::new(m));
        }
        (b, fx)
    }

    /// The facade with the schedule loaded and its front lines bound to w1.
    pub(crate) async fn bound_facade() -> (Bridge, std::path::PathBuf) {
        let (b, fx) = facade_bridge().await;
        let (code, m) = b.send("PUT", &format!("/v1/projects/{P}/list"), Some(AGENT), "text/csv", SCHEDULE).await;
        assert_eq!(code, StatusCode::OK, "{m}");
        let (code, v) = b.call("POST", &format!("/v1/projects/{P}/list/bind"), Some(OPERATOR), Some(json!({"place": "front", "wall_id": "w1"}))).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        (b, fx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_count_the_laptop_fails_is_counted_on_the_bridge() {
        let (b, fx) = bound_facade().await;
        let (_, alone) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert_eq!(alone["counted_on"], "bridge", "{alone}");
        // A laptop is seen, takes the count, and fails it.
        b.st.work.lock().unwrap().claim("laptop", "agent");
        let st = b.st.clone();
        let counting = tokio::spawn(async move { count(&st, P, "agent").await });
        let job = loop {
            let (code, j) = b.call("POST", "/v1/work/claim", Some(AGENT), Some(json!({"worker": "laptop"}))).await;
            if code == StatusCode::OK {
                break j;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!((job["kind"].as_str(), job["faces"].as_array().unwrap().len()), (Some("count"), 1), "{job}");
        assert_eq!((job["faces"][0]["face_id"].as_str(), job["stations"][0]["scan_id"].as_str()), (Some("f1"), Some(SCAN)), "{job}");
        let id = job["job_id"].as_str().unwrap();
        assert_eq!(b.call("POST", &format!("/v1/work/{id}/fail"), Some(AGENT), Some(json!({"error": "out of memory"}))).await.0, StatusCode::NO_CONTENT);
        let c = counting.await.unwrap().unwrap();
        assert_eq!(c["counted_on"], "bridge", "{c}");
        assert_eq!(c["faces"], alone["faces"]);
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text == "The count on laptop didn't come back (failed: out of memory); the bridge counts it itself"));
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_count_queued_behind_a_newer_one_gives_that_ones_result() {
        let (b, fx) = bound_facade().await;
        let counted = |b: &Bridge| b.st.inner.lock().unwrap().log.iter().filter(|e| e.text.starts_with("Counted openings")).count();
        // Hold the project's counts, and ask twice meanwhile.
        let lock = b.st.inner.lock().unwrap().asking.entry((P.to_string(), "#count".into())).or_default().clone();
        let held = lock.lock().await;
        let (st1, st2) = (b.st.clone(), b.st.clone());
        let first = tokio::spawn(async move { count(&st1, P, "bridge").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = tokio::spawn(async move { count(&st2, P, "agent").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(held);
        // The first counts; it started after the second was asked, so the second gives its result.
        let (a, c) = (first.await.unwrap().unwrap(), second.await.unwrap().unwrap());
        assert!(a["newer_count"].is_null(), "{a}");
        assert_eq!(c["newer_count"], true, "{c}");
        assert_eq!((c["faces"].clone(), c["counted_at"].clone()), (a["faces"].clone(), a["counted_at"].clone()));
        assert_eq!(counted(&b), 1, "read once");
        // A count asked after that one finished counts again.
        let (_, d) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert!(d["newer_count"].is_null(), "{d}");
        assert_eq!(counted(&b), 2);
        std::fs::remove_dir_all(fx).unwrap();
    }

    fn line<'a>(v: &'a Value, id: &str) -> &'a Value {
        v["lines"].as_array().unwrap().iter().find(|l| l["line_id"] == id).unwrap_or_else(|| panic!("no line {id} in {v}"))
    }

    #[tokio::test]
    async fn a_schedule_is_bound_counted_matched_and_recorded() {
        let (b, fx) = facade_bridge().await;
        let list = format!("/v1/projects/{P}/list");
        // Before a list: nothing to count, no record.
        assert_eq!(b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await.0, StatusCode::NOT_FOUND);
        // A spreadsheet with an ITP's columns but not all of them is refused; a viewer can't load lists.
        let (code, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "line,group,activity,requirement\n1.1,Masonry,Facade,Plumb\n").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("this one has no method"), "{e}");
        assert_eq!(b.send("PUT", &list, Some(VIEWER), "text/csv", SCHEDULE).await.0, StatusCode::FORBIDDEN);
        let (code, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "mark,type,width,height\nW1,window,,1 m\n").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("row 2 (W1): no width"), "{e}");
        assert_eq!(b.send("PUT", "/v1/projects/nope/list", Some(AGENT), "text/csv", SCHEDULE).await.0, StatusCode::NOT_FOUND);
        // The agent loads the schedule: one open line per mark.
        let (code, m) = b.send("PUT", &list, Some(AGENT), "text/csv", SCHEDULE).await;
        assert_eq!(code, StatusCode::OK, "{m}");
        assert_eq!(m["targets"].as_array().unwrap().len(), 4);
        assert!(line(&m, "W1")["reason"].as_str().unwrap().contains("not bound to a wall face yet"), "{m}");
        assert_eq!(b.call("GET", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await.0, StatusCode::NOT_FOUND);
        // Counting before anything is bound says so.
        let (_, c) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert!(c["say"].as_str().unwrap().starts_with("No opening line is bound"), "{c}");
        // The agent can't bind lines; the person does, by the list's place.
        let bind = format!("/v1/projects/{P}/list/bind");
        let body = json!({"place": "front", "wall_id": "w1", "confirmed_via": "asked in the chat"});
        assert_eq!(b.call("POST", &bind, Some(AGENT), Some(body.clone())).await.0, StatusCode::FORBIDDEN);
        for (bad, why) in [(json!({"place": "nowhere", "wall_id": "w1"}), "no line of the list is at"), (json!({"lines": ["W1"], "wall_id": "w7"}), "there is no wall"),
                           (json!({"lines": ["W1"]}), "give the face"), (json!({"lines": ["Z9"], "wall_id": "w1"}), "there is no line Z9")] {
            let (code, e) = b.call("POST", &bind, Some(OPERATOR), Some(bad)).await;
            assert_eq!(code, StatusCode::BAD_REQUEST);
            assert!(e["detail"].as_str().unwrap().contains(why), "{e}");
        }
        let (code, bound) = b.call("POST", &bind, Some(OPERATOR), Some(body)).await;
        assert_eq!(code, StatusCode::OK, "{bound}");
        assert_eq!(bound["bound"], json!(["Door D1", "Window W1", "Window W2"]));
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text == "Bound to front (w1): Door D1, Window W1, Window W2 (asked in the chat)"));
        // The count: three openings and a recess on the front; matches proposed.
        let (code, c) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert_eq!(code, StatusCode::OK, "{c}");
        let f = &c["faces"][0];
        assert_eq!((f["label"].as_str(), f["openings"].as_array().unwrap().len(), f["not_counted"].as_array().unwrap().len()), (Some("front (w1)"), 3, 1), "{c}");
        assert!(c["say"].as_str().unwrap().starts_with("3 openings found, 2 measured. front (w1): 3 openings"), "{}", c["say"]);
        assert!(line(&c, "W1")["reason"].as_str().unwrap().contains("a person confirms which one is W1"), "{c}");
        let proposed: Vec<(&str, &str)> = c["proposed"].as_array().unwrap().iter().map(|p| (p["line_id"].as_str().unwrap(), p["opening_id"].as_str().unwrap())).collect();
        assert_eq!(proposed, [("W1", "f1-1"), ("D1", "f1-3")], "{c}");
        let (code, ct, png) = b.bytes("GET", &format!("/v1/projects/{P}/count/f1/image"), Some(VIEWER)).await;
        assert_eq!((code, ct.as_str(), &png[1..4]), (StatusCode::OK, "image/png", &b"PNG"[..]));
        assert_eq!(b.call("GET", &format!("/v1/projects/{P}/count/f9/image"), Some(VIEWER), None).await.0, StatusCode::NOT_FOUND);
        // The person confirms the matches; the agent can't.
        let matches = format!("/v1/projects/{P}/count/matches");
        let body = json!({"matches": [{"line_id": "W1", "opening_id": "f1-1"}, {"line_id": "D1", "opening_id": "f1-3"}, {"line_id": "W2", "opening_id": "f1-2"}], "confirmed_via": "asked in the chat"});
        assert_eq!(b.call("POST", &matches, Some(AGENT), Some(body.clone())).await.0, StatusCode::FORBIDDEN);
        let (code, c) = b.call("POST", &matches, Some(OPERATOR), Some(body)).await;
        assert_eq!(code, StatusCode::OK, "{c}");
        let w1 = line(&c, "W1");
        assert_eq!((w1["status"].as_str(), w1["opening_id"].as_str()), (Some("answered"), Some("f1-1")), "{w1}");
        assert!(matches!(w1["conformance"].as_str(), Some("conforming" | "neutral")), "{w1}");
        assert!((w1["size"]["width_mm"].as_f64().unwrap() - 1000.0).abs() <= 15.0, "{w1}");
        assert_eq!(line(&c, "D1")["status"], "answered", "{c}");
        let w2 = line(&c, "W2");
        assert_eq!(w2["status"], "does_not_fit");
        assert!(w2["reason"].as_str().unwrap().contains("something stands in front of it"), "{w2}");
        assert_eq!(c["proposed"], json!([]));
        // What a match may not be.
        for (bad, why) in [(json!({"matches": [{"line_id": "W2", "opening_id": "f1-1"}]}), "would be both W1 and W2"),
                           (json!({"matches": [{"line_id": "W1", "opening_id": "f7-1"}]}), "no opening f7-1"),
                           (json!({"matches": [{"line_id": "D9", "opening_id": "f1-1"}]}), "isn't bound to a face yet"),
                           (json!({"matches": [{"line_id": "Q1", "opening_id": "f1-1"}]}), "no line Q1"), (json!({"matches": []}), "at least one")] {
            let (code, e) = b.call("POST", &matches, Some(OPERATOR), Some(bad)).await;
            assert_eq!(code, StatusCode::BAD_REQUEST);
            assert!(e["detail"].as_str().unwrap().contains(why), "{why}: {e}");
        }
        // The record: answered with its reading and picture, does not fit, open.
        let (_, r) = b.call("GET", &format!("/v1/projects/{P}/record"), Some(VIEWER), None).await;
        let rw1 = line(&r, "W1");
        assert_eq!((rw1["evidence"]["grade"].as_str(), rw1["place"]["bound_by"]["name"].as_str()), (Some("measured"), Some("crew")), "{rw1}");
        assert_eq!(rw1["evidence"]["pictures"][0], format!("/v1/projects/{P}/count/f1/image"));
        assert_eq!((rw1["evidence"]["counted_on"].as_str(), rw1["evidence"]["counted_by"].as_str()), (Some("bridge"), None), "{rw1}");
        assert_eq!((line(&r, "W2")["status"].as_str(), line(&r, "D9")["status"].as_str()), (Some("does_not_fit"), Some("open")));
        // The mission's ledger: the answered lines are covered, the others open.
        let (_, ms) = b.call("GET", &format!("/v1/projects/{P}/mission"), Some(VIEWER), None).await;
        let target = |id: &str| ms["targets"].as_array().unwrap().iter().find(|t| t["target_id"] == id).unwrap().clone();
        assert_eq!((target("W1")["status"].clone(), target("W2")["status"].clone(), target("D9")["status"].clone()), (json!("covered"), json!("open"), json!("open")));
        assert_eq!(target("W1")["stations"], json!([SCAN]), "an answered line names the scans it rests on");
        // After a registration the faces are counted again; the matches hold.
        after_registration(&b.st, P).await;
        let (_, c) = b.call("GET", &format!("/v1/projects/{P}/count"), Some(VIEWER), None).await;
        assert_eq!(line(&c, "W1")["status"], "answered", "{c}");
        assert_eq!(b.st.inner.lock().unwrap().log.iter().filter(|e| e.text.starts_with("Counted openings")).count(), 2);
        // Undo a match.
        let (_, c) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "D1", "opening_id": null}]}))).await;
        assert_eq!(line(&c, "D1")["status"], "open", "{c}");
        // Revision B changes W2's size: W2 starts again, W1 keeps its answer.
        let (code, m) = b.send("PUT", &format!("{list}?revision=B"), Some(AGENT), "text/csv", &SCHEDULE.replace("W2,window,900 mm", "W2,window,950 mm")).await;
        assert_eq!(code, StatusCode::OK, "{m}");
        assert_eq!((line(&m, "W1")["status"].as_str(), line(&m, "W2")["status"].as_str()), (Some("answered"), Some("open")), "{m}");
        let (_, l) = b.call("GET", &list, Some(VIEWER), None).await;
        assert_eq!((l["list"]["revision"].as_str(), l["earlier_revisions"].clone()), (Some("B"), json!(["A"])));
        // Revision B again with other lines is refused.
        let (code, e) = b.send("PUT", &format!("{list}?revision=B"), Some(AGENT), "text/csv", SCHEDULE).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("already loaded with other lines"), "{e}");
        // A signed-off mission keeps its matches.
        b.st.inner.lock().unwrap().missions.get_mut(P).unwrap().state = crate::mission::State::SignedOff;
        assert_eq!(b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "W1", "opening_id": null}]}))).await.0, StatusCode::CONFLICT);
        assert_eq!(b.call("POST", &bind, Some(OPERATOR), Some(json!({"lines": ["W1"], "wall_id": "w1"}))).await.0, StatusCode::CONFLICT);
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test]
    async fn an_opening_on_two_faces_of_one_wall_is_counted_once() {
        // The front lines on w1; the back door's line on a face drawn along
        // the same wall from y = -1 to 5, over the front door and the recess.
        let (b, fx) = bound_facade().await;
        let (code, v) = b.call("POST", &format!("/v1/projects/{P}/list/bind"), Some(OPERATOR), Some(json!({"lines": ["D9"], "from": [5.02, -1.0], "to": [4.98, 5.0]}))).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["overlaps"], json!([{"label": "front (w1)", "overlap_m": 6.0}]), "{v}");
        assert!(v["say"].as_str().unwrap().contains("an opening on both is counted once"), "{v}");
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text.starts_with("Bound to a drawn face: Door D9") && e.text.ends_with("it runs along front (w1) (6.0 m), and an opening on both is counted once")));
        let (code, c) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert_eq!(code, StatusCode::OK, "{c}");
        let faces = c["faces"].as_array().unwrap();
        assert_eq!(faces.len(), 2, "{c}");
        // The front door is f1-3; on the drawn face it is the same opening.
        let door = faces[1]["openings"].as_array().unwrap().iter().find(|o| o["same_as"] == "f1-3").unwrap_or_else(|| panic!("{c}"));
        let door_id = door["opening_id"].as_str().unwrap().to_string();
        assert!(faces[1]["openings"].as_array().unwrap().iter().all(|o| !o["same_as"].is_null()), "every opening on the drawn face is one the front has: {c}");
        assert!(faces[1]["not_counted"].as_array().unwrap().iter().all(|g| !g["same_as"].is_null()), "the recess is listed once: {c}");
        // Counted once: three openings, two measured, one recess.
        let say = c["say"].as_str().unwrap();
        assert!(say.starts_with("3 openings found, 2 measured."), "{say}");
        assert!(say.contains("f1-3 counted already") && say.contains(" 1 gap not counted"), "{say}");
        // The door is proposed to one line, not to both D1 and D9.
        let proposed: Vec<(String, String)> = c["proposed"].as_array().unwrap().iter().map(|p| (p["line_id"].as_str().unwrap().into(), p["opening_id"].as_str().unwrap().into())).collect();
        assert!(proposed.contains(&("D1".into(), "f1-3".into())), "{proposed:?}");
        assert!(!proposed.iter().any(|(_, o)| *o == door_id), "{proposed:?}");
        assert!(c["not_on_schedule"].as_array().unwrap().iter().all(|o| o != "f1-3"), "{c}");
        // A person can't make the one door both D1 and D9.
        let matches = format!("/v1/projects/{P}/count/matches");
        let both = json!({"matches": [{"line_id": "D1", "opening_id": "f1-3"}, {"line_id": "D9", "opening_id": door_id}]});
        let (code, e) = b.call("POST", &matches, Some(OPERATOR), Some(both)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["detail"].as_str().unwrap().contains(&format!("openings f1-3 and {door_id} are the same, and would be both D1 and D9")), "{e}");
        // D9 may be that door, on its own face, and is judged from that face's reading.
        let (code, c) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "D9", "opening_id": door_id}]}))).await;
        assert_eq!(code, StatusCode::OK, "{c}");
        assert_eq!((line(&c, "D9")["status"].as_str(), line(&c, "D9")["opening_id"].as_str()), (Some("answered"), Some(door_id.as_str())), "{c}");
        // Then the door is D9's on the front too: shown so, and not offered to D1.
        let front_door = c["faces"][0]["openings"].as_array().unwrap().iter().find(|o| o["opening_id"] == "f1-3").unwrap().clone();
        assert_eq!(front_door["line_id"], "D9", "{c}");
        assert!(!c["proposed"].as_array().unwrap().iter().any(|p| p["opening_id"] == "f1-3"), "{c}");
        let (code, e) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "D1", "opening_id": "f1-3"}]}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["detail"].as_str().unwrap().contains("would be both"), "{e}");
        // The drawn face has no line left to match, but W2, on the front, could
        // still take the blocked window it shares: not "not on the schedule".
        assert!(c["not_on_schedule"].as_array().unwrap().iter().all(|o| o != "f1-2"), "{c}");
        // W1 is bound to the drawn face too: being first in the list, that face
        // is f1 now and keeps the shared openings; the front is f2.
        let (code, v) = b.call("POST", &format!("/v1/projects/{P}/list/bind"), Some(OPERATOR), Some(json!({"lines": ["W1"], "from": [5.02, -1.0], "to": [4.98, 5.0]}))).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let (_, c) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert_eq!((c["faces"][0]["label"].as_str(), c["faces"][1]["label"].as_str()), (Some("a drawn face"), Some("front (w1)")), "{c}");
        assert!(c["say"].as_str().unwrap().starts_with("3 openings found, 2 measured."), "{}", c["say"]);
        let d9 = line(&c, "D9");
        assert_eq!(d9["status"], "answered", "{c}");
        let d9_on = d9["opening_id"].as_str().unwrap().to_string();
        assert!(d9_on.starts_with("f1-"), "D9's match takes the door's new id: {d9_on}");
        let front_door = c["faces"][1]["openings"].as_array().unwrap().iter().find(|o| o["same_as"] == d9_on.as_str()).unwrap_or_else(|| panic!("{c}")).clone();
        assert_eq!(front_door["line_id"], "D9", "{c}");
        let (code, e) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "D1", "opening_id": front_door["opening_id"]}]}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["detail"].as_str().unwrap().contains("are the same, and would be both D1 and D9"), "{e}");
        std::fs::remove_dir_all(fx).unwrap();
    }

    #[tokio::test]
    async fn a_match_is_checked_by_the_opening_it_holds_now() {
        // W1 is matched to f1-1. Recounted, a new window to its left is f1-1
        // and W1's is f1-2: W2 may be the new f1-1.
        let (b, fx) = bound_facade().await;
        assert_eq!(b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await.0, StatusCode::OK);
        let matches = format!("/v1/projects/{P}/count/matches");
        let (code, c) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "W1", "opening_id": "f1-1"}]}))).await;
        assert_eq!(code, StatusCode::OK, "{c}");
        {
            let mut inner = b.st.inner.lock().unwrap();
            let f = &mut inner.counts.get_mut(P).unwrap().faces[0];
            let mut new = f.found.openings[0].clone();
            new.box_m = [0.2, -0.5, 1.1, 0.7];
            new.centre = [0.65, 0.1];
            new.along_m = 0.2;
            if let Some(s) = new.size.as_mut() {
                s.width_mm = 900.0;
                s.along_m = 0.2;
            }
            f.found.openings.insert(0, new);
            for (k, o) in f.found.openings.iter_mut().enumerate() {
                o.opening_id = format!("f1-{}", k + 1);
            }
        }
        let (code, c) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "W2", "opening_id": "f1-1"}]}))).await;
        assert_eq!(code, StatusCode::OK, "{c}");
        assert_eq!((line(&c, "W1")["opening_id"].as_str(), line(&c, "W2")["opening_id"].as_str()), (Some("f1-2"), Some("f1-1")), "{c}");
        // W1's stored match took its opening's new id.
        let w1 = b.st.inner.lock().unwrap().missions[P].targets.iter().find(|t| t.target_id == "W1").unwrap().opening.clone().unwrap();
        assert_eq!(w1.matched.unwrap().opening_id, "f1-2");
        // And a real double is still refused under the new ids.
        let (code, e) = b.call("POST", &matches, Some(OPERATOR), Some(json!({"matches": [{"line_id": "D1", "opening_id": "f1-2"}]}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{e}");
        assert!(e["detail"].as_str().unwrap().contains("would be both W1 and D1"), "{e}");
        std::fs::remove_dir_all(fx).unwrap();
    }

    fn face_count(id: &str, from: [f64; 2], to: [f64; 2], openings: Vec<[f64; 4]>, gaps: Vec<[f64; 4]>) -> FaceCount {
        let opening = |k: usize, b: [f64; 4]| crate::openings::Opening {
            opening_id: format!("{id}-{}", k + 1), along_m: b[0], centre: [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0], box_m: b,
            looks_like: crate::list::OpeningType::Window, size: None, misfit: None, seen_through: 0.9, same_as: None };
        let gap = |b: [f64; 4]| crate::openings::Gap { along_m: b[0], bottom_m: b[1], width_m: b[2] - b[0], height_m: b[3] - b[1], why: "a recess".into(), same_as: None };
        FaceCount { face_id: id.into(), label: id.into(), face: Face { from, to }, length_m: 10.0, seen_share: 1.0, stations: vec![1], unseen: Value::Null,
                    found: crate::openings::Found { openings: openings.into_iter().enumerate().map(|(k, b)| opening(k, b)).collect(), not_counted: gaps.into_iter().map(gap).collect() },
                    lines: vec![] }
    }

    #[test]
    fn gaps_are_listed_once_and_are_the_opening_another_face_counts() {
        // Three parallel faces 10 cm apart: f1–f2 and f2–f3 are one wall, f1–f3 aren't.
        let mut faces = vec![
            face_count("f1", [0.0, 0.0], [10.0, 0.0], vec![], vec![[5.0, 0.0, 6.0, 1.0]]),
            face_count("f2", [2.0, 0.1], [10.0, 0.1], vec![], vec![[3.0, 0.0, 4.0, 1.0]]),
            face_count("f3", [2.0, 0.2], [10.0, 0.2], vec![], vec![[3.0, 0.0, 4.0, 1.0]]),
        ];
        count_once(&mut faces);
        let same: Vec<Option<String>> = faces.iter().map(|f| f.found.not_counted[0].same_as.clone()).collect();
        assert_eq!(same, [None, Some("f1".into()), Some("f1".into())], "followed to where it was first listed");
        // A gap on f1 where f2 counts an opening (f2 saw through it): it is that opening.
        let mut faces = vec![
            face_count("f1", [0.0, 0.0], [10.0, 0.0], vec![], vec![[5.0, 0.0, 6.0, 1.0]]),
            face_count("f2", [10.0, 0.05], [2.0, 0.05], vec![[4.0, 0.0, 5.0, 1.0]], vec![]),
        ];
        count_once(&mut faces);
        assert_eq!(faces[0].found.not_counted[0].same_as.as_deref(), Some("f2-1"), "antiparallel faces too");
        // Another storey on the same plan line: one wall, but the boxes don't meet.
        let mut faces = vec![face_count("f1", [0.0, 0.0], [10.0, 0.0], vec![[5.0, 0.0, 6.0, 1.0]], vec![]), face_count("f2", [0.0, 0.0], [10.0, 0.0], vec![[5.0, 3.0, 6.0, 4.0]], vec![])];
        count_once(&mut faces);
        assert!(faces[1].found.openings[0].same_as.is_none());
    }

    #[test]
    fn faces_are_one_wall_when_parallel_close_and_overlapping() {
        let f = |from: [f64; 2], to: [f64; 2]| Face { from, to };
        let w = f([0.0, 0.0], [10.0, 0.0]);
        let both = |a: &Face, b: &Face| {
            let (x, y) = (same_wall(a, b), same_wall(b, a));
            assert_eq!(x.map(|v| (v * 1000.0).round()), y.map(|v| (v * 1000.0).round()), "the same either way round");
            x
        };
        assert!((both(&w, &f([4.0, 0.1], [12.0, -0.1])).unwrap() - 6.0).abs() < 0.01);
        assert!((both(&w, &f([12.0, 0.0], [4.0, 0.0])).unwrap() - 6.0).abs() < 0.01, "antiparallel");
        assert_eq!(both(&w, &f([4.0, 0.3], [8.0, 0.3])), None, "30 cm out: another wall");
        assert_eq!(both(&w, &f([4.0, 0.0], [8.0, 1.0])), None, "at an angle");
        assert_eq!(both(&w, &f([11.0, 0.0], [14.0, 0.0])), None, "beyond its end");
        assert_eq!(both(&w, &f([2.0, 0.0], [2.0, 0.0])), None, "no length");
        // A 4.2 m stretch at 1° to a 20 m wall, touching it at one end: one
        // wall, whichever is given first (its far end is 7 cm off; the long
        // wall's far end, 35 cm off the short one's line, isn't in the overlap).
        let long = f([0.0, 0.0], [20.0, 0.0]);
        let t = 1f64.to_radians();
        let short = f([8.0, 0.0], [8.0 + 4.2 * t.cos(), 4.2 * t.sin()]);
        assert!((both(&long, &short).unwrap() - 4.2).abs() < 0.01);
    }

    #[tokio::test]
    async fn a_mission_from_a_sentence_has_no_list_or_record() {
        let (b, fx) = facade_bridge().await;
        let (code, _) = b.call("PUT", &format!("/v1/projects/{P}/mission"), Some(AGENT), Some(json!({"goal": "Scan the front", "kind": "exterior", "done_when": "seen"}))).await;
        assert_eq!(code, StatusCode::OK);
        for path in ["list", "record"] {
            let (code, e) = b.call("GET", &format!("/v1/projects/{P}/{path}"), Some(VIEWER), None).await;
            assert_eq!(code, StatusCode::NOT_FOUND, "{path}: {e}");
        }
        let (code, e) = b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("the mission has no list"), "{e}");
        let (code, _) = b.call("POST", &format!("/v1/projects/{P}/count/matches"), Some(OPERATOR), Some(json!({"matches": [{"line_id": "W1", "opening_id": "f1-1"}]}))).await;
        assert_eq!(code, StatusCode::CONFLICT);
        // A JSON list loads too.
        let sample = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../specs/list-mission/samples/demo-closeout.list.json")).unwrap();
        let (code, m) = b.send("PUT", &format!("/v1/projects/{P}/list"), Some(OPERATOR), "application/json", &sample).await;
        assert_eq!(code, StatusCode::OK, "{m}");
        assert_eq!(m["lines"].as_array().unwrap().len(), 8);
        // Binding a plumb line asks plumb of its face, with its tolerance.
        let (code, bound) = b.call("POST", &format!("/v1/projects/{P}/list/bind"), Some(OPERATOR), Some(json!({"lines": ["1.1"], "wall_id": "w1"}))).await;
        assert_eq!(code, StatusCode::OK, "{bound}");
        let (_, ms) = b.call("GET", &format!("/v1/projects/{P}/mission"), Some(VIEWER), None).await;
        let t = &ms["targets"][0];
        assert_eq!((t["question"]["kind"].as_str(), t["tolerance"]["words"].as_str()), (Some("plumb"), Some("a quarter inch in ten feet")), "{t}");
        let (code, e) = b.call("POST", &format!("/v1/projects/{P}/list/bind"), Some(OPERATOR), Some(json!({"lines": ["2.1"], "wall_id": "w1"}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("asks height, which isn't asked of a wall face"), "{e}");
        // Another list on this project is refused while its mission runs.
        let (code, e) = b.send("PUT", &format!("/v1/projects/{P}/list"), Some(AGENT), "text/csv", SCHEDULE).await;
        assert_eq!(code, StatusCode::CONFLICT, "{e}");
        std::fs::remove_dir_all(fx).unwrap();
    }
}
