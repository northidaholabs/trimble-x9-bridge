//! Missions: what the job is for, and when it is done. The person's sentence
//! ("scan the exterior of the house", "measure all the windows") becomes a
//! mission on the project with criteria in prose and a ledger of targets the
//! agent keeps (each facade, each window): open, covered or measured (by named
//! stations), or declined (with a reason). "Ready to leave" is refused while
//! any target is open; the person then signs off once, and the mission is
//! done. Kept in `missions.json` so a restart keeps them.

use crate::ask::{Question, Tolerance, Verdict, KINDS};
use crate::register::{Face, Geo, FACE_COVERED};
use crate::util::now_rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissionKind {
    Exterior,
    Windows,
    Room,
    Custom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    Covered,
    Measured,
    Declined,
}

impl Status {
    pub fn is_final(self) -> bool {
        self != Status::Open
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Work in progress.
    Active,
    /// Every target settled; waiting for the person's sign-off.
    Ready,
    /// The person signed off: the job is done.
    SignedOff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub target_id: String,
    pub name: String,
    /// facade, window, corner, room, … (free text, short).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: Status,
    /// Scans that covered or measured it.
    #[serde(default)]
    pub stations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement_id: Option<String>,
    /// Why it was declined, or anything worth knowing.
    #[serde(default)]
    pub note: String,
    /// A wall face (project frame): the bridge judges it from the registered
    /// stations and keeps `geometry` (seen share, stations, unseen stretches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub face: Option<Face>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Value>,
    /// A question asked of the face (`ask`): covered only by its answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    /// The plumb tolerance the job uses: an answer says within or outside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<Tolerance>,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub mission_id: String,
    pub project_id: String,
    /// The person's sentence.
    pub goal: String,
    pub kind: MissionKind,
    /// When it is done, in prose (the agent's reading of the goal).
    pub done_when: String,
    pub state: State,
    pub created_by: String,
    pub created_at: String,
    pub targets: Vec<Target>,
    /// The agent's summary when it said ready to leave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_off_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_off_at: Option<String>,
    /// How the sign-off reached the bridge ("asked in the chat", "relayed by the agent: …", "web UI").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_via: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MissionRequest {
    pub goal: String,
    pub kind: MissionKind,
    pub done_when: String,
    #[serde(default)]
    pub targets: Vec<TargetUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetUpdate {
    pub target_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<Status>,
    #[serde(default)]
    pub stations: Option<Vec<String>>,
    #[serde(default)]
    pub measurement_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// A wall face, by its ends (project frame) …
    #[serde(default)]
    pub face: Option<Face>,
    /// … or by a wall of the registration (`w3`).
    #[serde(default)]
    pub wall_id: Option<String>,
    /// A question for its face (`plumb`), or "" to drop it.
    #[serde(default)]
    pub question: Option<String>,
    /// The question's tolerance, or null to drop it.
    #[serde(default, deserialize_with = "present")]
    pub tolerance: Option<Option<Tolerance>>,
}

/// A field that was sent, even as null (`Some(None)`); absent is `None`.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

pub const MAX_TARGETS: usize = 200;

fn short(what: &str, s: &str, max: usize, required: bool) -> Result<(), String> {
    let n = s.trim().chars().count();
    if (required && n == 0) || n > max {
        let range = if required { format!("1–{max}") } else { format!("up to {max}") };
        return Err(format!("{what}: {range} characters"));
    }
    Ok(())
}

impl MissionRequest {
    pub fn check(&self) -> Result<(), String> {
        short("goal", &self.goal, 300, true)?;
        short("done_when", &self.done_when, 600, true)?;
        Ok(())
    }
}

impl Mission {
    pub fn new(req: &MissionRequest, project_id: &str, by: &str) -> Mission {
        Mission {
            mission_id: crate::util::new_id("msn"),
            project_id: project_id.to_string(),
            goal: req.goal.trim().to_string(),
            kind: req.kind,
            done_when: req.done_when.trim().to_string(),
            state: State::Active,
            created_by: by.to_string(),
            created_at: now_rfc3339(),
            targets: Vec::new(),
            ready_summary: None,
            signed_off_by: None,
            signed_off_at: None,
            confirmed_via: None,
        }
    }

    /// Add or change targets (by id; unnamed fields keep their value). A
    /// covered or measured target names its stations, a declined one its
    /// reason. Any change sends a ready mission back to work; a signed-off
    /// mission doesn't change. `known_scans`: the project's scan ids.
    /// A target with a wall face is judged from the registration: covered
    /// once enough of it is seen (its stations filled in), and refused as
    /// covered while it isn't.
    pub fn update(&mut self, ups: &[TargetUpdate], known_scans: &[String], geo: &Geo) -> Result<(), String> {
        if self.state == State::SignedOff {
            return Err("the mission is signed off; start a new one to change it".into());
        }
        let mut next = self.targets.clone();
        for u in ups {
            let id = u.target_id.trim();
            if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err(format!("target_id {:?}: 1–64 letters, digits, '-' or '_'", u.target_id));
            }
            let i = match next.iter().position(|t| t.target_id == id) {
                Some(i) => i,
                None => {
                    if next.len() >= MAX_TARGETS {
                        return Err(format!("at most {MAX_TARGETS} targets"));
                    }
                    next.push(Target { target_id: id.into(), name: String::new(), kind: String::new(), status: Status::Open,
                                       stations: vec![], measurement_id: None, note: String::new(), face: None, geometry: None,
                                       question: None, tolerance: None, updated_at: String::new() });
                    next.len() - 1
                }
            };
            let t = &mut next[i];
            if let Some(v) = &u.name {
                t.name = v.trim().into();
            }
            if let Some(v) = &u.kind {
                t.kind = v.trim().into();
            }
            if let Some(v) = u.status {
                t.status = v;
            }
            if let Some(v) = &u.stations {
                t.stations = v.clone();
            }
            if let Some(v) = &u.measurement_id {
                t.measurement_id = Some(v.clone()).filter(|m| !m.is_empty());
            }
            if let Some(v) = &u.note {
                t.note = v.trim().into();
            }
            let was = t.face;
            if let Some(w) = &u.wall_id {
                t.face = Some(geo.wall(w.trim()).ok_or_else(|| format!("target {id}: there is no wall {w:?} in the registration (its walls list them)"))?);
            }
            if let Some(f) = u.face {
                t.face = Some(f);
            }
            // A new face is a new question: its answer was about the old one.
            if t.face != was {
                if let Some(q) = &mut t.question {
                    *q = Question::new(&q.kind);
                    if t.status == Status::Covered {
                        t.status = Status::Open;
                    }
                }
            }
            match u.question.as_deref().map(str::trim) {
                Some("") => t.question = None,
                Some(k) if !KINDS.contains(&k) => return Err(format!("target {id}: question {k:?}: v1 asks {}", KINDS.join(", "))),
                Some(_) if t.face.is_none() => return Err(format!("target {id}: a question is asked of a wall face; give face or wall_id")),
                Some(k) if t.question.as_ref().is_none_or(|q| q.kind != k) => {
                    t.question = Some(Question::new(k));
                    if t.status == Status::Covered {
                        t.status = Status::Open;
                    }
                }
                _ => {}
            }
            if let Some(tol) = &u.tolerance {
                if let Some(x) = tol {
                    x.check().map_err(|e| format!("target {id}: {e}"))?;
                }
                t.tolerance = tol.clone().map(|x| Tolerance { words: x.words.trim().into(), ..x });
            }
            if t.question.is_none() {
                if matches!(u.tolerance, Some(Some(_))) {
                    return Err(format!("target {id}: a tolerance goes with a question; set question to \"plumb\""));
                }
                t.tolerance = None;
            }
            if t.question.is_some() && u.status == Some(Status::Covered) {
                return Err(format!("target {id}: a question target is covered by its answer (scanner_ask), not by hand"));
            }
            judge(t, geo)?;
            t.updated_at = now_rfc3339();
            short(&format!("target {id}: name"), &t.name, 80, true)?;
            short(&format!("target {id}: kind"), &t.kind, 30, false)?;
            short(&format!("target {id}: note"), &t.note, 300, false)?;
            if let Some(s) = t.stations.iter().find(|s| !known_scans.contains(s)) {
                return Err(format!("target {id}: scan {s} is not in this project"));
            }
            match t.status {
                Status::Covered | Status::Measured if t.stations.is_empty() => {
                    return Err(format!("target {id}: a {} target names the stations that did it", if t.status == Status::Covered { "covered" } else { "measured" }));
                }
                Status::Measured if t.measurement_id.is_none() => {
                    return Err(format!("target {id}: a measured target names its measurement_id"));
                }
                Status::Declined if t.note.is_empty() => return Err(format!("target {id}: a declined target says why, in note")),
                _ => {}
            }
        }
        self.targets = next;
        if self.state == State::Ready {
            self.state = State::Active;
            self.ready_summary = None;
        }
        Ok(())
    }

    /// Re-judge wall faces after the registration changed: a face now seen
    /// enough is covered, a covered one no longer seen enough opens again.
    /// Returns what changed ("North facade: covered").
    pub fn refresh(&mut self, geo: &Geo) -> Vec<String> {
        if self.state == State::SignedOff {
            return vec![];
        }
        let mut changed = Vec::new();
        for t in &mut self.targets {
            let before = t.status;
            if judge(t, geo).is_err() {
                t.status = Status::Open;
            }
            if t.status != before {
                t.updated_at = now_rfc3339();
                changed.push(format!("{}: {}", t.name, if t.status == Status::Covered { "covered" } else { "open again" }));
            }
        }
        if !changed.is_empty() && self.state == State::Ready {
            self.state = State::Active;
            self.ready_summary = None;
        }
        changed
    }

    /// A question's new verdict (from `ask` or the re-check after a station):
    /// its answer covers the target, anything else leaves it open. `scans`
    /// maps station numbers to scan ids. Returns what changed, for the log.
    pub fn record_question(&mut self, target_id: &str, q: Question, scans: &std::collections::HashMap<u16, String>) -> Option<String> {
        if self.state == State::SignedOff {
            return None;
        }
        let t = self.targets.iter_mut().find(|t| t.target_id == target_id && t.question.as_ref().is_some_and(|tq| tq.kind == q.kind))?;
        let (status, verdict) = (t.status, t.question.as_ref().and_then(|q| q.verdict));
        match (&q.verdict, &q.answer) {
            (Some(Verdict::Answer), Some(a)) => {
                t.status = Status::Covered;
                t.stations = a.stations.iter().filter_map(|n| scans.get(n).cloned()).collect();
            }
            _ if t.status == Status::Covered => t.status = Status::Open,
            _ => {}
        }
        let said = match q.verdict {
            Some(Verdict::Answer) => "answered".to_string(),
            Some(Verdict::DoesNotFit) => format!("does not fit ({})", q.reason),
            Some(Verdict::NotYet) => format!("not yet ({})", q.reason),
            None => "not asked".into(),
        };
        let changed = t.status != status || q.verdict != verdict;
        t.question = Some(Question { checked_at: now_rfc3339(), ..q });
        if !changed {
            return None;
        }
        t.updated_at = now_rfc3339();
        let line = format!("{}: {said}", t.name);
        if self.state == State::Ready && !t.status.is_final() {
            self.state = State::Active;
            self.ready_summary = None;
        }
        Some(line)
    }

    /// Ready to leave: every target settled. Returns the open ones otherwise.
    pub fn ready(&mut self, summary: &str) -> Result<(), String> {
        if self.state == State::SignedOff {
            return Err("the mission is already signed off".into());
        }
        if self.targets.is_empty() {
            return Err("the ledger is empty: list what the mission needs (each facade, each window) first".into());
        }
        let open: Vec<&str> = self.targets.iter().filter(|t| !t.status.is_final()).map(|t| t.name.as_str()).collect();
        if !open.is_empty() {
            return Err(format!("{} target{} still open: {}", open.len(), if open.len() == 1 { " is" } else { "s are" }, open.join(", ")));
        }
        short("summary", summary, 1000, true)?;
        self.state = State::Ready;
        self.ready_summary = Some(summary.trim().into());
        Ok(())
    }

    pub fn sign_off(&mut self, by: &str, via: Option<String>) -> Result<(), String> {
        if self.state != State::Ready {
            return Err(format!("the mission is {}, not ready to leave", match self.state {
                State::Active => "still in progress",
                State::Ready => "ready",
                State::SignedOff => "already signed off",
            }));
        }
        self.state = State::SignedOff;
        self.signed_off_by = Some(by.into());
        self.signed_off_at = Some(now_rfc3339());
        self.confirmed_via = via.filter(|v| !v.trim().is_empty()).map(|v| v.chars().take(300).collect());
        Ok(())
    }

    /// "5 covered, 2 measured, 1 declined, 3 open"
    pub fn tally(&self) -> String {
        let n = |s: Status| self.targets.iter().filter(|t| t.status == s).count();
        let parts: Vec<String> = [("covered", Status::Covered), ("measured", Status::Measured), ("declined", Status::Declined), ("open", Status::Open)]
            .iter()
            .filter(|(_, s)| n(*s) > 0)
            .map(|(w, s)| format!("{} {w}", n(*s)))
            .collect();
        if parts.is_empty() { "no targets yet".into() } else { parts.join(", ") }
    }
}

/// Judge a target's wall face (when it has one and the project is registered).
fn judge(t: &mut Target, geo: &Geo) -> Result<(), String> {
    let Some(f) = t.face else { return Ok(()) };
    let Some(v) = geo.judge(&f) else { return Ok(()) };
    if let Some(e) = v["error"].as_str() {
        return Err(format!("target {}: {e}", t.target_id));
    }
    let covered = v["covered"] == true;
    let scans: Vec<String> = v["stations"].as_array().map(|a| a.iter().filter_map(|s| s["scan_id"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    let share = v["seen_share"].as_f64().unwrap_or(0.0);
    t.geometry = Some(v);
    if t.question.is_some() {
        return Ok(());
    }
    match t.status {
        Status::Open if covered => {
            t.status = Status::Covered;
            t.stations = scans;
        }
        Status::Covered if !covered => {
            return Err(format!(
                "target {}: only {:.0}% of its length is seen ({:.0}% needed); keep it open and cover the unseen stretches, or decline it with the reason",
                t.target_id, share * 100.0, FACE_COVERED * 100.0
            ));
        }
        Status::Covered if t.stations.is_empty() => t.stations = scans,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req() -> MissionRequest {
        MissionRequest { goal: "Scan the exterior of the house".into(), kind: MissionKind::Exterior,
                         done_when: "Every facade seen at ≤ 12 mm spacing, no shadow on the house's walls".into(), targets: vec![] }
    }
    fn up(id: &str, status: Option<Status>, stations: &[&str], note: &str) -> TargetUpdate {
        TargetUpdate { target_id: id.into(), name: Some(format!("{id} facade")), kind: Some("facade".into()), status,
                       stations: Some(stations.iter().map(|s| s.to_string()).collect()), measurement_id: None, note: Some(note.into()),
                       face: None, wall_id: None, question: None, tolerance: None }
    }

    #[test]
    fn the_ledger_must_be_settled_before_leaving_and_signed_once() {
        let scans = vec!["s1".to_string(), "s2".to_string()];
        let mut m = Mission::new(&req(), "p1", "claude");
        assert!(m.ready("all done").unwrap_err().contains("empty"));
        m.update(&[up("north", None, &[], ""), up("south", Some(Status::Covered), &["s1"], "")], &scans, &Geo::none()).unwrap();
        assert!(m.ready("all done").unwrap_err().contains("1 target is still open: north facade"));
        assert!(m.update(&[up("north", Some(Status::Covered), &[], "")], &scans, &Geo::none()).unwrap_err().contains("names the stations"));
        assert!(m.update(&[up("north", Some(Status::Covered), &["s9"], "")], &scans, &Geo::none()).unwrap_err().contains("not in this project"));
        assert!(m.update(&[up("north", Some(Status::Declined), &[], "")], &scans, &Geo::none()).unwrap_err().contains("says why"));
        m.update(&[up("north", Some(Status::Declined), &[], "behind a locked fence")], &scans, &Geo::none()).unwrap();
        assert_eq!(m.tally(), "1 covered, 1 declined");
        assert!(m.sign_off("crew", None).is_err());
        m.ready("South seen from s1; north behind a locked fence.").unwrap();
        assert_eq!(m.state, State::Ready);
        // A change sends it back to work.
        m.update(&[up("east", None, &[], "")], &scans, &Geo::none()).unwrap();
        assert_eq!(m.state, State::Active);
        m.update(&[up("east", Some(Status::Covered), &["s2"], "")], &scans, &Geo::none()).unwrap();
        m.ready("done").unwrap();
        m.sign_off("crew", Some("asked in the chat".into())).unwrap();
        assert_eq!(m.state, State::SignedOff);
        assert!(m.update(&[up("west", None, &[], "")], &scans, &Geo::none()).unwrap_err().contains("signed off"));
    }

    #[test]
    fn checks_the_request() {
        let mut r = req();
        r.goal = " ".into();
        assert!(r.check().unwrap_err().contains("goal"));
        let mut m = Mission::new(&req(), "p1", "claude");
        assert!(m.update(&[up("no spaces", None, &[], "")], &[], &Geo::none()).unwrap_err().contains("target_id"));
    }

    /// A wall seen over x 0..8 at y = 5 by station 1.
    fn geo() -> Geo {
        let mut m = crate::register::Merged::default();
        for i in 0..160 {
            for k in 0..30 {
                m.pts.push([i as f32 * 0.05, 5.0, -1.5 + k as f32 * 0.1]);
                m.spacing_mm.push(5.0);
                m.station.push(1);
            }
        }
        let walls = crate::register::walls(&m);
        Geo { walls, merged: Some(std::sync::Arc::new(m)), scans: [(1u16, "s1".to_string())].into() }
    }

    #[test]
    fn wall_faces_are_judged_from_the_registration() {
        let scans = vec!["s1".to_string()];
        let g = geo();
        let mut m = Mission::new(&req(), "p1", "claude");
        let face = |id: &str, to: f64, status: Option<Status>| TargetUpdate {
            target_id: id.into(), name: Some(id.into()), kind: Some("facade".into()), status, stations: None, measurement_id: None,
            note: None, face: Some(Face { from: [0.0, 5.0], to: [to, 5.0] }), wall_id: None, question: None, tolerance: None,
        };
        // Seen along its whole length: covered by the bridge, with its station.
        m.update(&[face("near", 8.0, None)], &scans, &g).unwrap();
        assert_eq!(m.targets[0].status, Status::Covered);
        assert_eq!(m.targets[0].stations, vec!["s1".to_string()]);
        // Seen over 40%: stays open, and can't be called covered.
        m.update(&[face("long", 20.0, None)], &scans, &g).unwrap();
        assert_eq!(m.targets[1].status, Status::Open);
        assert_eq!(m.targets[1].geometry.as_ref().unwrap()["seen_share"], 0.4);
        assert!(m.update(&[face("long", 20.0, Some(Status::Covered))], &scans, &g).unwrap_err().contains("only 40%"));
        // By wall id.
        let w = TargetUpdate { face: None, wall_id: Some("w1".into()), ..face("by-id", 0.0, None) };
        m.update(&[w], &scans, &g).unwrap();
        assert_eq!(m.targets[2].status, Status::Covered);
        let bad = TargetUpdate { face: None, wall_id: Some("w9".into()), ..face("nope", 0.0, None) };
        assert!(m.update(&[bad], &scans, &g).unwrap_err().contains("no wall"));
        // Without a registration a face is kept but not judged.
        m.update(&[face("later", 20.0, None)], &scans, &Geo::none()).unwrap();
        assert_eq!(m.targets[3].status, Status::Open);
        assert!(m.targets[3].geometry.is_none());
        let changed = m.refresh(&g);
        assert!(changed.is_empty(), "{changed:?}");
    }

    /// Criterion 3: a question target is done only by its answer, asked or not.
    #[test]
    fn a_question_target_is_covered_only_by_its_answer() {
        use crate::ask::{Reading, Verdict};
        let scans = vec!["s1".to_string()];
        let g = geo();
        let numbers: std::collections::HashMap<u16, String> = [(1u16, "s1".to_string())].into();
        let mut m = Mission::new(&req(), "p1", "claude");
        let q = |id: &str, to: f64| TargetUpdate {
            target_id: id.into(), name: Some(format!("{id} facade")), kind: Some("facade".into()), status: None, stations: None,
            measurement_id: None, note: None, face: Some(Face { from: [0.0, 5.0], to: [to, 5.0] }), wall_id: None, question: Some("plumb".into()), tolerance: None,
        };
        // Fully seen, never asked: open, not covered by its coverage.
        m.update(&[q("never", 8.0), q("rough", 8.0)], &scans, &g).unwrap();
        assert_eq!(m.targets[0].status, Status::Open);
        assert_eq!(m.targets[0].geometry.as_ref().unwrap()["seen_share"], 1.0);
        assert_eq!(m.targets[0].question.as_ref().unwrap().kind, "plumb");
        let err = m.ready("done").unwrap_err();
        assert!(err.contains("never facade") && err.contains("rough facade"), "{err}");
        // Calling it covered by hand is refused.
        let by_hand = TargetUpdate { status: Some(Status::Covered), stations: Some(scans.clone()), ..q("never", 8.0) };
        assert!(m.update(&[by_hand], &scans, &g).unwrap_err().contains("answer"));
        // A re-check after a station: not yet, then does not fit. Both block.
        let verdict = |v: Verdict, answer: Option<Reading>| Question { verdict: Some(v), reason: "x".into(), answer, ..Question::new("plumb") };
        m.record_question("never", verdict(Verdict::NotYet, None), &numbers);
        m.record_question("rough", verdict(Verdict::DoesNotFit, None), &numbers);
        assert!(m.ready("done").unwrap_err().contains("2 targets are still open"));
        // The answer arrives with the next station, without being asked again.
        let r_of = |p: f64| Reading { out_of_plumb_mm: p, lean_mm_per_m: 0.5, flat_rms_mm: 6.0, flat_p95_mm: None, straightedge_ok_pct: None,
                                       worst_strip_mm_per_m: None, over_m: None, stations: vec![1] };
        let r = r_of(4.0);
        let said = m.record_question("never", verdict(Verdict::Answer, Some(r)), &numbers).unwrap();
        assert!(said.contains("never facade"), "{said}");
        assert_eq!(m.targets[0].status, Status::Covered);
        assert_eq!(m.targets[0].stations, scans);
        assert!(m.ready("done").unwrap_err().contains("rough facade"));
        // Does not fit: the person declines it, with the reason.
        m.update(&[TargetUpdate { status: Some(Status::Declined), note: Some("stepped facade: piers proud of the wall".into()), ..q("rough", 8.0) }], &scans, &g).unwrap();
        m.ready("Long facade answered; the stepped one declined.").unwrap();
        // An answer that later drops opens the target again, and the mission with it.
        assert!(m.record_question("never", verdict(Verdict::NotYet, None), &numbers).is_some());
        assert_eq!(m.targets[0].status, Status::Open);
        assert_eq!(m.state, State::Active);
        // A question needs a face, and a kind the bridge asks.
        let no_face = TargetUpdate { face: None, ..q("nowhere", 8.0) };
        assert!(m.update(&[no_face], &scans, &g).unwrap_err().contains("face"));
        let odd = TargetUpdate { question: Some("colour".into()), ..q("odd", 8.0) };
        assert!(m.update(&[odd], &scans, &g).unwrap_err().contains("plumb"));
        // A new face is a new question: the old answer doesn't carry over.
        m.update(&[TargetUpdate { face: Some(Face { from: [0.0, 5.0], to: [6.0, 5.0] }), ..q("never", 8.0) }], &scans, &g).unwrap();
        m.record_question("never", verdict(Verdict::Answer, Some(Reading { stations: vec![1], ..r_of(4.0) })), &numbers);
        assert_eq!(m.targets[0].status, Status::Covered);
        m.update(&[TargetUpdate { face: Some(Face { from: [0.0, 5.0], to: [7.0, 5.0] }), question: None, ..q("never", 8.0) }], &scans, &g).unwrap();
        assert_eq!(m.targets[0].status, Status::Open);
        assert_eq!(m.targets[0].question.as_ref().unwrap().verdict, None);
        // The same face again changes nothing.
        m.record_question("never", verdict(Verdict::Answer, Some(Reading { stations: vec![1], ..r_of(4.0) })), &numbers);
        m.update(&[TargetUpdate { face: Some(Face { from: [0.0, 5.0], to: [7.0, 5.0] }), question: None, ..q("never", 8.0) }], &scans, &g).unwrap();
        assert_eq!(m.targets[0].status, Status::Covered);
        // A verdict for a question dropped meanwhile is not recorded.
        m.update(&[TargetUpdate { question: Some(String::new()), ..q("odd2", 8.0) }], &scans, &g).unwrap();
        assert!(m.record_question("odd2", verdict(Verdict::NotYet, None), &numbers).is_none());
        assert!(m.targets.iter().find(|t| t.target_id == "odd2").unwrap().question.is_none());
        // Asking the same question again keeps its verdict and its cover.
        m.update(&[q("never", 7.0)], &scans, &g).unwrap();
        assert_eq!(m.targets[0].status, Status::Covered);
        assert_eq!(m.targets[0].question.as_ref().unwrap().verdict, Some(Verdict::Answer));
        assert!(!m.targets[0].question.as_ref().unwrap().checked_at.is_empty());
        // The same verdict again is nothing new to log.
        assert!(m.record_question("never", verdict(Verdict::Answer, Some(Reading { stations: vec![1], ..r_of(4.5) })), &numbers).is_none());
        // A declined target stays declined whatever its re-check says, and a
        // ready mission stays ready.
        m.ready("Long facade answered; the stepped one declined.").unwrap();
        assert!(m.record_question("rough", verdict(Verdict::NotYet, None), &numbers).is_some());
        assert_eq!(m.targets[1].status, Status::Declined);
        assert_eq!(m.state, State::Ready);
        // A question added to a target its coverage had covered opens it.
        m.update(&[TargetUpdate { question: None, ..q("plain", 8.0) }], &scans, &g).unwrap();
        m.update(&[TargetUpdate { question: Some(String::new()), ..q("plain", 8.0) }], &scans, &g).unwrap();
        let plain = |m: &Mission| m.targets.iter().find(|t| t.target_id == "plain").unwrap().status;
        assert_eq!(plain(&m), Status::Covered);
        m.update(&[q("plain", 8.0)], &scans, &g).unwrap();
        assert_eq!(plain(&m), Status::Open);
        // A tolerance goes with a question, survives a new face, and null drops it.
        let tms = crate::ask::Tolerance { words: "¼ in in 10 ft".into(), steps: vec![crate::ask::Step { up_to_m: None, mm: 6.4 }], mm_per_m: None, max_mm: None };
        let tol = |id: &str, t: Option<crate::ask::Tolerance>| TargetUpdate { tolerance: Some(t), ..q(id, 8.0) };
        let bare = TargetUpdate { question: None, tolerance: Some(Some(tms.clone())), ..q("bare", 8.0) };
        assert!(m.update(&[bare], &scans, &g).unwrap_err().contains("question"));
        let wordless = crate::ask::Tolerance { words: String::new(), ..tms.clone() };
        assert!(m.update(&[tol("never", Some(wordless))], &scans, &g).unwrap_err().contains("words"));
        m.update(&[tol("never", Some(tms.clone()))], &scans, &g).unwrap();
        assert_eq!(m.targets[0].tolerance.as_ref(), Some(&tms));
        m.update(&[TargetUpdate { face: Some(Face { from: [0.0, 5.0], to: [7.5, 5.0] }), ..q("never", 8.0) }], &scans, &g).unwrap();
        assert_eq!(m.targets[0].tolerance.as_ref(), Some(&tms));
        let sent: TargetUpdate = serde_json::from_value(json!({"target_id": "never", "tolerance": {"words": "  ¼ in in 10 ft ", "steps": [{"mm": 6.4}]}})).unwrap();
        m.update(&[sent], &scans, &g).unwrap();
        assert_eq!(m.targets[0].tolerance.as_ref(), Some(&tms));
        let json: TargetUpdate = serde_json::from_value(json!({"target_id": "never", "tolerance": null})).unwrap();
        m.update(&[json], &scans, &g).unwrap();
        assert!(m.targets[0].tolerance.is_none());
        let absent: TargetUpdate = serde_json::from_value(json!({"target_id": "never"})).unwrap();
        assert!(absent.tolerance.is_none());
        // "" drops the question: the coverage rule applies again.
        m.update(&[TargetUpdate { question: Some(String::new()), ..q("never", 8.0) }], &scans, &g).unwrap();
        assert!(m.targets[0].question.is_none());
        assert_eq!(m.targets[0].status, Status::Covered);
    }
}
