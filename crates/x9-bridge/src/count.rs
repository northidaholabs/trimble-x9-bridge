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
//! Counted on the Pi in this version; the laptop doesn't take counts yet.

use crate::ask::Verdict;
use crate::list::Kind;
use crate::mission::{Mission, Status};
use crate::openings::{judge_line, propose, Counted, FaceCount, LineCheck, Match, Raster};
use crate::problem::ApiError;
use crate::register::{Face, Pose, FACE_COVERED};
use crate::state::AppState;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// A face to count: id, label, face, its lines, its coverage, the stations that saw it.
type Look = (String, String, Face, Vec<String>, Value, Vec<u16>);
/// What a count reads off the points: the count, and each face's picture.
type Read = (Counted, Vec<(String, Vec<u8>)>);

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
        .filter(|l| l.question.kind == Kind::Opening)
        .filter_map(|l| {
            let t = m.targets.iter().find(|t| t.target_id == l.line_id)?;
            Some(Bound {
                line_id: l.line_id.clone(),
                expected: l.question.opening.clone()?,
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
    for b in &bound {
        let Some(fc) = counted.faces.iter().find(|f| f.face == b.face) else { continue };
        let Some(t) = m.targets.iter_mut().find(|t| t.target_id == b.line_id) else { continue };
        let mut c = t.opening.clone().unwrap_or_default();
        let before = (c.verdict, c.size.clone(), c.conformance.clone());
        judge_line(&mut c, &b.expected, b.max_mm, fc, &b.line_id);
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
    // Which opening each matched line is on now (it may have a new id after a new count).
    let mut on_now: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for t in &m.targets {
        let Some(c) = &t.opening else { continue };
        let (Some(mt), Some(fc)) = (&c.matched, counted.faces.iter().find(|f| Some(f.face) == t.face)) else { continue };
        if let Some(o) = fc.found.openings.iter().find(|o| (o.centre[0] - mt.centre[0]).hypot(o.centre[1] - mt.centre[1]) <= crate::openings::REMATCH_M) {
            on_now.insert(o.opening_id.clone(), t.target_id.clone());
        }
    }
    let mut proposed = Vec::new();
    let mut not_on = Vec::new();
    let mut faces = Vec::new();
    let mut parts = Vec::new();
    let (mut found_all, mut measured_all) = (0, 0);
    for fc in &counted.faces {
        let unmatched_lines: Vec<(String, crate::list::Expected)> = bound
            .iter()
            .filter(|b| b.face == fc.face && !on_now.values().any(|l| l == &b.line_id))
            .filter(|b| m.targets.iter().find(|t| t.target_id == b.line_id).is_some_and(|t| t.status != Status::Declined))
            .map(|b| (b.line_id.clone(), b.expected.clone()))
            .collect();
        let free: Vec<&crate::openings::Opening> = fc.found.openings.iter().filter(|o| !on_now.contains_key(&o.opening_id)).collect();
        if fc.seen_share >= FACE_COVERED {
            for (l, o, why) in propose(&unmatched_lines, &free) {
                proposed.push(json!({"line_id": l, "opening_id": o, "why": why}));
            }
            if unmatched_lines.is_empty() {
                not_on.extend(free.iter().map(|o| o.opening_id.clone()));
            }
        }
        let n = fc.found.openings.len();
        let measured = fc.found.openings.iter().filter(|o| o.size.is_some() && o.misfit.is_none()).count();
        found_all += n;
        measured_all += measured;
        let least = if fc.seen_share < FACE_COVERED { "at least " } else { "" };
        parts.push(format!("{}: {least}{n} opening{} ({:.0}% seen)", fc.label, if n == 1 { "" } else { "s" }, fc.seen_share * 100.0));
        let openings: Vec<Value> = fc
            .found
            .openings
            .iter()
            .map(|o| {
                let mut v = json!(o);
                if let Some(s) = &o.size {
                    v["said"] = json!(crate::openings::size_said(s));
                }
                v["line_id"] = json!(on_now.get(&o.opening_id));
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
                l["opening_id"] = json!(on_now.iter().find(|(_, v)| **v == id).map(|(k, _)| k));
            }
            l
        })
        .collect();
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
    let recesses: usize = counted.faces.iter().map(|f| f.found.not_counted.len()).sum();
    if recesses > 0 {
        say.push_str(&format!(" {recesses} gap{} not counted (see not_counted).", if recesses == 1 { "" } else { "s" }));
    }
    json!({"project_id": project_id, "counted_at": counted.counted_at, "say": say, "faces": faces, "lines": lines,
           "proposed": proposed, "not_on_schedule": not_on, "where": "on the bridge"})
}

/// Count the openings on every face a schedule line is bound to, and judge
/// the lines. `by`: who asked ("bridge" after a registration).
pub async fn count(st: &Arc<AppState>, project_id: &str, by: &str) -> Result<Value, ApiError> {
    let t0 = std::time::Instant::now();
    let lock = {
        let mut inner = st.inner.lock().unwrap();
        inner.asking.entry((project_id.to_string(), "#count".into())).or_default().clone()
    };
    let _one = lock.lock().await;
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
    let _reading = crate::files::reading(st, wanted.iter().filter_map(|n| scans.get(n).cloned()));
    let mut files: Vec<(u16, Pose, PathBuf)> = Vec::new();
    if let Some(reg) = &reg {
        for s in reg.stations.iter().filter(|s| wanted.contains(&s.station)) {
            files.push((s.station, poses[&s.station], crate::files::scan_file(st, &s.scan_id, crate::files::FileKind::Tzf).await?));
        }
    }
    let specs = looks;
    let (counted, pictures) = tokio::task::spawn_blocking(move || -> anyhow::Result<Read> {
        let mut rasters: Vec<Option<Raster>> = specs
            .iter()
            .map(|(_, _, face, _, cov, stations)| {
                let (lo, hi) = (cov["height_m"][0].as_f64()?, cov["height_m"][1].as_f64()?);
                let xy: Vec<[f64; 2]> = files.iter().filter(|(n, _, _)| stations.contains(n)).map(|(_, p, _)| [p.xyz_m[0], p.xyz_m[1]]).collect();
                Some(Raster::new(face, [lo - 0.5, hi + 0.5], &xy))
            })
            .collect();
        for (n, pose, path) in &files {
            let mine: Vec<usize> = (0..specs.len()).filter(|&k| rasters[k].is_some() && specs[k].5.contains(n)).collect();
            if mine.is_empty() {
                continue;
            }
            let s = pose.xyz_m;
            let ds: Vec<f64> = mine.iter().map(|&k| rasters[k].as_ref().unwrap().standoff(s)).collect();
            crate::analyze::station_points(path, pose, |_, w, range| {
                if range <= crate::openings::RANGE_M {
                    for (j, &k) in mine.iter().enumerate() {
                        if ds[j] > 0.3 {
                            rasters[k].as_mut().unwrap().add(s, ds[j], w);
                        }
                    }
                }
            })?;
        }
        let mut faces = Vec::new();
        let mut pictures = Vec::new();
        for ((id, label, face, lines, cov, stations), r) in specs.into_iter().zip(rasters) {
            let mut found = r.as_ref().map(Raster::find).unwrap_or_default();
            for (k, o) in found.openings.iter_mut().enumerate() {
                o.opening_id = format!("{id}-{}", k + 1);
            }
            let share = cov["seen_share"].as_f64().unwrap_or(0.0);
            if let Some(r) = &r {
                let n = found.openings.len();
                pictures.push((id.clone(), r.png(&format!("{label}: {n} opening{}, {:.0}% seen", if n == 1 { "" } else { "s" }, share * 100.0), &found)));
            }
            faces.push(FaceCount { face_id: id, label, face, length_m: cov["length_m"].as_f64().unwrap_or(0.0), seen_share: share, stations, unseen: cov["unseen"].clone(), found, lines });
        }
        Ok((Counted { counted_at: crate::util::now_rfc3339(), faces }, pictures))
    })
    .await
    .map_err(|e| ApiError::cache(format!("counting: {e}")))?
    .map_err(|e| ApiError::cache(format!("counting: {e:#}")))?;
    // Judge the lines on the mission as it is now (it may have changed meanwhile).
    let out = {
        let mut inner = st.inner.lock().unwrap();
        let mut m = inner.missions.get(project_id).cloned().ok_or_else(|| ApiError::not_found("mission"))?;
        apply(&mut m, &counted, &scans);
        let v = said(project_id, &counted, &m);
        inner.missions.insert(project_id.to_string(), m.clone());
        inner.counts.insert(project_id.to_string(), counted.clone());
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
        st.log("count", format!("Counted openings ({:.1} s): {}", took, out["say"].as_str().unwrap_or("")), by, Some(project_id), &[]);
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
        let mut lines = Vec::new();
        for it in &req.matches {
            let id = it.line_id.trim();
            let line = m.list.as_ref().and_then(|l| l.line(id)).ok_or_else(|| ApiError::invalid(format!("there is no line {id} in the list")))?;
            if line.question.kind != Kind::Opening {
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
        // One line per opening.
        let mut taken: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for t in &m.targets {
            let Some(mt) = t.opening.as_ref().and_then(|c| c.matched.as_ref()) else { continue };
            if let Some(other) = taken.insert(mt.opening_id.clone(), &t.target_id) {
                return Err(ApiError::invalid(format!("opening {} would be both {other} and {}: undo one match (opening_id null) in the same request", mt.opening_id, t.target_id)));
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
mod tests {
    use super::*;
    use crate::register::{Merged, Registration, StationPose};
    use crate::testkit::{Bridge, AGENT, OPERATOR, VIEWER};
    use axum::http::StatusCode;

    const SCAN: &str = "5b5314cf-76b8-4f59-a445-ceee1e6df4c3";
    const P: &str = "39187455-8278-428c-89ad-8a4835f95be6";

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

    fn line<'a>(v: &'a Value, id: &str) -> &'a Value {
        v["lines"].as_array().unwrap().iter().find(|l| l["line_id"] == id).unwrap_or_else(|| panic!("no line {id} in {v}"))
    }

    #[tokio::test]
    async fn a_schedule_is_bound_counted_matched_and_recorded() {
        let (b, fx) = facade_bridge().await;
        let list = format!("/v1/projects/{P}/list");
        // Before a list: nothing to count, no record.
        assert_eq!(b.call("POST", &format!("/v1/projects/{P}/count"), Some(AGENT), None).await.0, StatusCode::NOT_FOUND);
        // An ITP spreadsheet isn't read yet; a viewer can't load lists.
        let (code, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "line,group,activity\n1.1,Masonry,Facade\n").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("needs its lines sorted first"), "{e}");
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
        let (code, m) = b.send("PUT", &format!("/v1/projects/{P}/list"), Some(AGENT), "application/json", &sample).await;
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
