//! A mission from a list (`specs/list-mission.md`, parts A and B): the list a
//! job must document, read from JSON (`specs/list-mission/list.schema.json`)
//! or from a door and window schedule (`specs/construction-counts.md`, part
//! A), and the record that answers it line by line.
//!
//! Loading a list makes the project's mission: one target per line, its
//! `target_id` the line's id. A new revision of the same list keeps the
//! answer of every line whose requirement, question and tolerance did not
//! change; the record of the revision it replaces is kept as it stood.
//!
//! Built here: the list, the record, and binding a line to a wall face (a
//! person does it). Measured lines answer as their questions do: `plumb` and
//! `flatness` through `ask`, `opening` through `openings`. Seen and attested
//! answers, signatures, acceptance and the export are not built yet; those
//! lines stay open and say so.

use crate::ask::{Tolerance, Verdict};
use crate::mission::{Mission, Status, Target};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const FORMAT: &str = "nil-list";
pub const RECORD_FORMAT: &str = "nil-list-record";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Itp,
    PunchList,
    InspectionForm,
    CloseoutList,
    Schedule,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartyRole {
    Contractor,
    Subcontractor,
    Client,
    Inspector,
    ThirdParty,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Party {
    pub id: String,
    pub role: PartyRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Dimensional,
    Visual,
    Test,
    Review,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Measured,
    Seen,
    Attested,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Plumb,
    Flatness,
    Height,
    Opening,
    Clearance,
    LowSpots,
    Map,
    Photo,
    Statement,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Plumb => "plumb",
            Kind::Flatness => "flatness",
            Kind::Height => "height",
            Kind::Opening => "opening",
            Kind::Clearance => "clearance",
            Kind::LowSpots => "low_spots",
            Kind::Map => "map",
            Kind::Photo => "photo",
            Kind::Statement => "statement",
        }
    }
    fn measured(self) -> bool {
        !matches!(self, Kind::Photo | Kind::Statement)
    }
    /// Asked of a wall face, so a person binds the line to one.
    pub fn on_a_face(self) -> bool {
        matches!(self, Kind::Plumb | Kind::Flatness | Kind::Opening)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpeningType {
    Window,
    Door,
    Opening,
}

impl OpeningType {
    pub fn word(self) -> &'static str {
        match self {
            OpeningType::Window => "window",
            OpeningType::Door => "door",
            OpeningType::Opening => "opening",
        }
    }
}

/// What an `opening` line expects: the schedule's type and rough opening.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    #[serde(rename = "type")]
    pub kind: OpeningType,
    pub width_mm: f64,
    pub height_mm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<BTreeMap<String, f64>>,
    /// With `opening`: the type and size the line expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening: Option<Expected>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Point {
    H,
    W,
    R,
    S,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub line_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub activity: String,
    pub requirement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub method: Method,
    pub evidence: Evidence,
    pub question: Question,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<Tolerance>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub points: BTreeMap<String, Point>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Line {
    /// The words an answer was given to: a line whose words changed starts again.
    fn words(&self) -> (&str, &Question, &Option<Tolerance>) {
        (self.requirement.trim(), &self.question, &self.tolerance)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub format: String,
    pub version: u32,
    pub list_id: String,
    pub title: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<Person>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub parties: Vec<Party>,
    pub lines: Vec<Line>,
}

fn is_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 64 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A line's id: an ITP item number (`1.2`) or a schedule mark (`W1`, `D-3`):
/// letters and digits in parts joined by `.` or `-`, up to 32 characters.
pub fn is_line_id(s: &str) -> bool {
    s.len() <= 32 && !s.is_empty() && s.split(['.', '-']).all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn text(what: &str, s: &str, max: usize) -> Result<(), String> {
    let n = s.trim().chars().count();
    if n == 0 || n > max {
        return Err(format!("{what}: 1–{max} characters"));
    }
    Ok(())
}

/// An opening's tolerance is one figure either way: `max_mm`.
fn check_opening_tolerance(t: &Tolerance) -> Result<(), String> {
    text("tolerance: words", &t.words, 200)?;
    if !t.steps.is_empty() || t.mm_per_m.is_some() {
        return Err("an opening's tolerance is one figure either way: give max_mm (±), not steps or mm_per_m".into());
    }
    match t.max_mm {
        Some(m) if m.is_finite() && m > 0.0 => Ok(()),
        _ => Err("an opening's tolerance gives max_mm, the allowance either way, in mm".into()),
    }
}

impl List {
    /// The checks the list schema makes, and the ones it can't say.
    pub fn check(&self) -> Result<(), String> {
        if self.format != FORMAT || self.version != 0 {
            return Err(format!("not a list: format is \"{FORMAT}\", version 0"));
        }
        if !is_id(&self.list_id) {
            return Err(format!("list_id {:?}: lower-case letters, digits and '-', up to 64", self.list_id));
        }
        text("title", &self.title, 200)?;
        text("revision", &self.revision, 20)?;
        if self.parties.is_empty() {
            return Err("parties: at least one".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.parties {
            if !is_id(&p.id) {
                return Err(format!("party {:?}: an id of lower-case letters, digits and '-'", p.id));
            }
            if !seen.insert(p.id.as_str()) {
                return Err(format!("party {} is named twice", p.id));
            }
        }
        if self.lines.is_empty() {
            return Err("lines: at least one".into());
        }
        if self.lines.len() > crate::mission::MAX_TARGETS {
            return Err(format!("a list has at most {} lines", crate::mission::MAX_TARGETS));
        }
        let mut ids = std::collections::HashSet::new();
        for l in &self.lines {
            let id = l.line_id.as_str();
            if !is_line_id(id) {
                return Err(format!("line {id:?}: a number (1.2) or a mark (W1), letters and digits joined by '.' or '-'"));
            }
            if !ids.insert(id) {
                return Err(format!("line {id} is in the list twice"));
            }
            let at = |e: String| format!("line {id}: {e}");
            text("activity", &l.activity, 200).map_err(at)?;
            text("requirement", &l.requirement, 600).map_err(at)?;
            let k = l.question.kind;
            match l.evidence {
                Evidence::Measured if !k.measured() => return Err(at(format!("measured evidence answers a measured question, not {}", k.word()))),
                Evidence::Seen if k != Kind::Photo => return Err(at("seen evidence answers a photo question".into())),
                Evidence::Attested if k != Kind::Statement => return Err(at("attested evidence answers a statement question".into())),
                Evidence::Attested if l.record.as_deref().is_none_or(|r| r.trim().is_empty()) => {
                    return Err(at("an attested line names the record kept as proof".into()))
                }
                _ => {}
            }
            match (&l.question.opening, k) {
                (None, Kind::Opening) => return Err(at("an opening question gives the opening it expects: type, width_mm and height_mm".into())),
                (Some(_), k) if k != Kind::Opening => return Err(at("only an opening question gives an opening".into())),
                (Some(e), _) => {
                    for (what, v) in [("width_mm", e.width_mm), ("height_mm", e.height_mm)] {
                        if !(v.is_finite() && v > 0.0 && v <= 20_000.0) {
                            return Err(at(format!("{what}: a size in mm, up to 20 m")));
                        }
                    }
                }
                _ => {}
            }
            if let Some(t) = &l.tolerance {
                if l.evidence != Evidence::Measured {
                    return Err(at("a tolerance goes with measured evidence".into()));
                }
                if k == Kind::Opening { check_opening_tolerance(t) } else { t.check() }.map_err(at)?;
            }
            if let Some(p) = l.points.keys().find(|p| !self.parties.iter().any(|q| &q.id == *p)) {
                return Err(at(format!("party {p} has a point on the line but is not in parties")));
            }
        }
        Ok(())
    }

    pub fn line(&self, id: &str) -> Option<&Line> {
        self.lines.iter().find(|l| l.line_id == id)
    }
}

/// A target for each line, as a fresh mission holds them: open, unbound.
pub fn target_for(l: &Line) -> Target {
    let name: String = match &l.question.opening {
        Some(e) => format!("{} {}", capital(e.kind.word()), l.line_id),
        None => format!("{} {}", l.line_id, l.activity.trim()),
    };
    Target { target_id: l.line_id.clone(), name: name.chars().take(80).collect(), kind: l.question.kind.word().into(), updated_at: crate::util::now_rfc3339(), ..Default::default() }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A mission from a list. `prev`: the project's mission now, kept when it is
/// an earlier revision of the same list: lines whose words did not change keep
/// their target (place, answer), and the earlier record is kept as it stood.
pub fn mission(list: List, project_id: &str, by: &str, prev: Option<&Mission>) -> Result<Mission, String> {
    list.check()?;
    let same = prev.filter(|m| m.list.as_ref().is_some_and(|l| l.list_id == list.list_id));
    if let (Some(m), Some(l)) = (prev, prev.and_then(|m| m.list.as_ref())) {
        if l.list_id != list.list_id && m.state != crate::mission::State::SignedOff {
            return Err(format!("the project's mission is the list {:?}; one list per project: send a revision of it, or start a new project", l.list_id));
        }
    }
    if let Some(m) = same {
        let old = m.list.as_ref().expect("same list");
        if old.revision == list.revision && *old != list {
            return Err(format!("revision {} of {} is already loaded with other lines: give the new revision a new letter or number", list.revision, list.list_id));
        }
        if m.state == crate::mission::State::SignedOff {
            return Err("the mission is signed off; start a new project to answer the list again".into());
        }
        if *old == list {
            return Ok(m.clone());
        }
    }
    let req = crate::mission::MissionRequest { goal: list.title.clone(), kind: crate::mission::MissionKind::Custom, done_when: "every line answered or declined".into(), targets: vec![] };
    let mut m = Mission::new(&req, project_id, by);
    if let Some(old) = same {
        m.mission_id = old.mission_id.clone();
        m.created_at = old.created_at.clone();
        m.earlier = old.earlier.clone();
        m.earlier.push(record(old));
    }
    let old_list = same.and_then(|o| o.list.as_ref());
    for l in &list.lines {
        let kept = same.and_then(|o| {
            let before = old_list?.line(&l.line_id)?;
            (before.words() == l.words()).then(|| o.targets.iter().find(|t| t.target_id == l.line_id).cloned())?
        });
        m.targets.push(kept.unwrap_or_else(|| target_for(l)));
    }
    m.list = Some(list);
    Ok(m)
}

/// Bind lines to a wall face: a person's act (`specs/list-mission.md`, part
/// B). A plumb or flatness line gets its question on the face; an opening line
/// is counted there. A line bound to another face than before starts again;
/// one bound to the same face keeps its answer. Returns the lines' names.
pub fn bind(m: &mut Mission, line_ids: &[String], face: crate::register::Face, label: &str, by: &str, via: &str, geo: &crate::register::Geo) -> Result<Vec<String>, String> {
    if m.state == crate::mission::State::SignedOff {
        return Err("the mission is signed off; start a new one to change it".into());
    }
    let list = m.list.clone().ok_or("the mission has no list: load one first")?;
    if line_ids.is_empty() {
        return Err("name the lines to bind (lines), or the place their list gives (place)".into());
    }
    let cov = geo.judge(&face);
    if let Some(e) = cov.as_ref().and_then(|v| v["error"].as_str()) {
        return Err(format!("{label}: {e}"));
    }
    let mut names = Vec::new();
    for id in line_ids {
        let l = list.line(id.trim()).ok_or_else(|| format!("there is no line {id} in the list"))?;
        let k = l.question.kind;
        if !k.on_a_face() {
            return Err(format!("line {id} asks {}, which isn't asked of a wall face", k.word()));
        }
        let t = m.targets.iter_mut().find(|t| t.target_id == l.line_id).ok_or_else(|| format!("line {id} has no target"))?;
        let moved = t.face != Some(face);
        t.face = Some(face);
        t.geometry = cov.clone();
        t.bound = Some(crate::mission::Bound { by: by.into(), via: via.into(), at: crate::util::now_rfc3339(), label: label.into() });
        if moved {
            match k {
                Kind::Opening => t.opening = Some(crate::openings::LineCheck::default()),
                _ => {
                    t.question = Some(crate::ask::Question::new("plumb"));
                    t.tolerance = if k == Kind::Plumb { l.tolerance.clone() } else { None };
                }
            }
            if t.status != Status::Declined {
                t.status = Status::Open;
                t.stations.clear();
            }
        }
        t.updated_at = crate::util::now_rfc3339();
        names.push(t.name.clone());
    }
    if m.state == crate::mission::State::Ready {
        m.state = crate::mission::State::Active;
        m.ready_summary = None;
    }
    Ok(names)
}

/// The lines whose place, in the list's words, is `place` (case aside).
pub fn lines_at(list: &List, place: &str) -> Vec<String> {
    let want = place.trim().to_lowercase();
    list.lines.iter().filter(|l| l.place_hint.as_deref().is_some_and(|h| h.trim().to_lowercase() == want)).map(|l| l.line_id.clone()).collect()
}

/// A line's state in the record: status, conformance, reason.
pub struct LineState {
    pub status: &'static str,
    pub conformance: Option<&'static str>,
    pub reason: String,
}

/// What a line has come to, from its target.
pub fn state_of(l: &Line, t: &Target) -> LineState {
    let st = |status, reason: &str| LineState { status, conformance: None, reason: reason.to_string() };
    if t.status == Status::Declined {
        return st("declined", &t.note);
    }
    let k = l.question.kind;
    if !k.on_a_face() {
        let why = match l.evidence {
            Evidence::Seen => "a picture marked by a person answers it; this bridge doesn't take one yet",
            Evidence::Attested => "a person's statement answers it; this bridge doesn't take one yet",
            Evidence::Measured => "the bridge can't ask this kind of question yet",
        };
        return st("open", &format!("{} ({why})", k.word()));
    }
    if t.face.is_none() {
        return st("open", "not bound to a wall face yet: a person binds it");
    }
    if k == Kind::Opening {
        return match &t.opening {
            Some(c) => c.state(),
            None => st("open", "not counted yet: count the openings on its face"),
        };
    }
    let Some(q) = &t.question else { return st("open", "no question set on its face") };
    match q.verdict {
        None => st("open", "not asked yet"),
        Some(Verdict::NotYet) => st("not_yet", &q.reason),
        Some(Verdict::DoesNotFit) => st("does_not_fit", &q.reason),
        Some(Verdict::Answer) => {
            let said = q.said.clone().unwrap_or(Value::Null);
            let (conformance, reason) = if k == Kind::Flatness {
                ("not_applicable", "the flatness figure is recorded; the bridge judges tolerances on plumb only".to_string())
            } else {
                match (t.tolerance.is_some(), said["within"].as_bool()) {
                    (false, _) => ("not_applicable", String::new()),
                    (true, Some(true)) => ("conforming", String::new()),
                    (true, Some(false)) => ("non_conforming", String::new()),
                    (true, None) => ("neutral", said["tolerance_note"].as_str().unwrap_or("the tolerance makes no call for this case").to_string()),
                }
            };
            LineState { status: "answered", conformance: Some(conformance), reason }
        }
    }
}

fn measured_evidence(l: &Line, t: &Target, project_id: &str) -> Option<Value> {
    if l.question.kind == Kind::Opening {
        return t.opening.as_ref().and_then(|c| c.evidence(project_id));
    }
    let q = t.question.as_ref()?;
    let a = q.answer.as_ref()?;
    let mut reading = json!({"out_of_plumb_mm": a.out_of_plumb_mm, "lean_mm_per_m": a.lean_mm_per_m, "flat_rms_mm": a.flat_rms_mm});
    if let Some(h) = a.over_m {
        reading["over_m"] = json!(h);
    }
    if let Some(v) = q.said.as_ref().and_then(|s| s["allowed_mm"].as_f64()) {
        reading["allowed_mm"] = json!(v);
    }
    let over = a.over_m.map(|h| format!(" over {h:.1} m")).unwrap_or_default();
    let mut e = json!({"grade": "measured", "reading": reading, "stations": a.stations,
                       "say": format!("{:.1} mm out of plumb{over}, flat to {:.1} mm RMS", a.out_of_plumb_mm.abs(), a.flat_rms_mm)});
    if let Some(s) = q.seen_share {
        e["seen_share"] = json!((s * 1000.0).round() / 1000.0);
    }
    Some(e)
}

/// The record of the mission's list, as `specs/list-mission/record.schema.json` has it.
pub fn record(m: &Mission) -> Value {
    let Some(list) = &m.list else { return Value::Null };
    let mut lines = Vec::new();
    for l in &list.lines {
        let Some(t) = m.targets.iter().find(|t| t.target_id == l.line_id) else { continue };
        let s = state_of(l, t);
        let mut v = json!({"line_id": l.line_id, "status": s.status});
        if let Some(c) = s.conformance {
            v["conformance"] = json!(c);
            if let Some(e) = measured_evidence(l, t, &m.project_id) {
                v["evidence"] = e;
            }
        }
        if !s.reason.is_empty() && s.status != "open" {
            v["reason"] = json!(s.reason);
        }
        if s.status == "declined" {
            v["decided"] = json!({"by": {"name": t.decided_by.clone().unwrap_or_else(|| "not recorded".into())}, "via": t.decided_via.clone().unwrap_or_else(|| "not recorded".into())});
        }
        if let (Some(f), Some(b)) = (t.face, &t.bound) {
            v["place"] = json!({"face": {"from": f.from, "to": f.to}, "bound_by": {"name": b.by}});
        }
        if !t.updated_at.is_empty() {
            v["updated_at"] = json!(t.updated_at);
        }
        lines.push(v);
    }
    let sign_off = match (&m.signed_off_by, &m.signed_off_at) {
        (Some(by), Some(at)) => json!({"by": {"name": by}, "at": at, "via": m.confirmed_via.clone().unwrap_or_else(|| "web UI".into())}),
        _ => Value::Null,
    };
    json!({"format": RECORD_FORMAT, "version": 0, "list_id": list.list_id, "list_revision": list.revision,
           "project_id": m.project_id, "mission_id": m.mission_id, "lines": lines, "capture_sign_off": sign_off})
}

/// Each line of the list with its state, for the agent and the web UI.
pub fn lines_said(m: &Mission) -> Vec<Value> {
    let Some(list) = &m.list else { return vec![] };
    list.lines
        .iter()
        .filter_map(|l| {
            let t = m.targets.iter().find(|t| t.target_id == l.line_id)?;
            let s = state_of(l, t);
            let mut v = json!({"line_id": l.line_id, "activity": l.activity, "requirement": l.requirement, "evidence": l.evidence,
                               "question": l.question.kind.word(), "status": s.status, "reason": s.reason});
            if let Some(c) = s.conformance {
                v["conformance"] = json!(c);
            }
            if let Some(h) = &l.place_hint {
                v["place_hint"] = json!(h);
            }
            if let Some(b) = &t.bound {
                v["bound"] = json!({"face": b.label, "by": b.by});
            }
            Some(v)
        })
        .collect()
}

// ------------------------------------------------------ the schedule CSV ---

/// Rows of a CSV: fields split on commas, with "quoted, fields" and "" for a quote.
pub fn csv_rows(s: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let (mut row, mut field, mut quoted, mut chars) = (Vec::new(), String::new(), false, s.trim_start_matches('\u{feff}').chars().peekable());
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('"', false) if field.trim().is_empty() => {
                field.clear();
                quoted = true;
            }
            (',', false) => row.push(std::mem::take(&mut field)),
            ('\r', false) => {}
            ('\n', false) => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (c, _) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.into_iter().filter(|r| r.iter().any(|f| !f.trim().is_empty())).collect()
}

/// A length in mm, from "914", "914 mm", "91.4 cm", "0.914 m", "36 in",
/// `36"`, `3'-0"`, `3' 1/2"`, "3 ft", "1/4 in". A bare number takes `unit`
/// (from the column's name); without one it is refused: inches and millimetres
/// are both common in schedules.
pub fn length_mm(s: &str, unit: Option<&str>) -> Result<f64, String> {
    let s = s.trim().to_lowercase().replace(['″', '”'], "\"").replace(['′', '’'], "'");
    let num = |t: &str| -> Option<f64> {
        let t = t.trim();
        if t.is_empty() {
            return Some(0.0);
        }
        let mut total = 0.0;
        for part in t.split_whitespace() {
            total += match part.split_once('/') {
                Some((a, b)) => a.parse::<f64>().ok()? / b.parse::<f64>().ok().filter(|b| *b != 0.0)?,
                None => part.parse::<f64>().ok()?,
            };
        }
        Some(total)
    };
    let bad = || format!("{s:?}: a length such as 914 mm, 36 in or 3'-0\"");
    if let Some((ft, rest)) = s.split_once('\'') {
        let ft = num(ft).ok_or_else(bad)?;
        let inches = num(rest.trim().trim_start_matches('-').trim_end_matches('"').trim_end_matches("in")).ok_or_else(bad)?;
        return Ok((ft * 12.0 + inches) * 25.4);
    }
    for (suffix, mm) in [("mm", 1.0), ("cm", 10.0), ("inches", 25.4), ("inch", 25.4), ("in", 25.4), ("\"", 25.4), ("ft", 304.8), ("feet", 304.8), ("m", 1000.0)] {
        if let Some(n) = s.strip_suffix(suffix) {
            return num(n).filter(|v| *v > 0.0).map(|v| v * mm).ok_or_else(bad);
        }
    }
    let v = num(&s).filter(|v| *v > 0.0).ok_or_else(bad)?;
    match unit {
        Some("mm") => Ok(v),
        Some("in") => Ok(v * 25.4),
        _ => Err(format!("{s:?} has no unit: write 914 mm or 36 in, or name the column with its unit (width_mm, width_in)")),
    }
}

fn opening_type(s: &str) -> Option<OpeningType> {
    match s.trim().to_lowercase().trim_end_matches('s') {
        "window" | "w" => Some(OpeningType::Window),
        "door" | "d" => Some(OpeningType::Door),
        "opening" | "o" | "cased opening" => Some(OpeningType::Opening),
        _ => None,
    }
}

/// The schedule's columns: mark, type, width, height, tolerance, wall; width,
/// height and tolerance may carry their unit (`width_mm`, `tolerance_in`).
const SCHEDULE: [&str; 6] = ["mark", "type", "width", "height", "tolerance", "wall"];

/// Whether a CSV's header is a door and window schedule's.
pub fn is_schedule(csv: &str) -> bool {
    let names: Vec<String> = csv_rows(csv).first().map(|h| h.iter().map(|c| c.trim().to_lowercase()).collect()).unwrap_or_default();
    names.iter().any(|n| n == "mark") && names.iter().any(|n| n.starts_with("width"))
}

/// A list from a door and window schedule: one measured `opening` line per
/// mark, the line's id the mark.
pub fn from_schedule(csv: &str, revision: &str, title: Option<&str>) -> Result<List, String> {
    let rows = csv_rows(csv);
    let Some((header, body)) = rows.split_first() else { return Err("the schedule is empty".into()) };
    let names: Vec<String> = header.iter().map(|h| h.trim().to_lowercase()).collect();
    let col = |base: &str| -> Option<(usize, Option<&'static str>)> {
        names.iter().enumerate().find_map(|(i, n)| match n.strip_prefix(base) {
            Some("") => Some((i, None)),
            Some("_mm") | Some(" (mm)") => Some((i, Some("mm"))),
            Some("_in") | Some(" (in)") => Some((i, Some("in"))),
            _ => None,
        })
    };
    let missing: Vec<&str> = SCHEDULE.iter().copied().filter(|c| matches!(*c, "mark" | "type" | "width" | "height") && col(c).is_none()).collect();
    if !missing.is_empty() {
        return Err(format!("a schedule has the columns {}; this one has no {}", SCHEDULE.join(", "), missing.join(", ")));
    }
    let [mark, kind, width, height] = ["mark", "type", "width", "height"].map(|c| col(c).expect("checked"));
    let (tolerance, wall) = (col("tolerance"), col("wall"));
    let mut lines = Vec::new();
    let mut marks = std::collections::HashSet::new();
    for (n, r) in body.iter().enumerate() {
        let cell = |(i, _): (usize, Option<&str>)| r.get(i).map(|s| s.trim()).unwrap_or("");
        let m = cell(mark);
        let at = |e: String| format!("row {} ({}): {e}", n + 2, if m.is_empty() { "no mark" } else { m });
        if !is_line_id(m) {
            return Err(at("the mark is letters and digits, joined by '.' or '-' (W1, D-3)".into()));
        }
        if !marks.insert(m.to_string()) {
            return Err(at("this mark is in the schedule twice".into()));
        }
        let t = opening_type(cell(kind)).ok_or_else(|| at(format!("type {:?}: window, door or opening", cell(kind))))?;
        let size = |c: (usize, Option<&'static str>), what: &str| -> Result<f64, String> {
            let v = cell(c);
            if v.is_empty() {
                return Err(at(format!("no {what}")));
            }
            length_mm(v, c.1).map_err(|e| at(format!("{what} {e}")))
        };
        let (w, h) = (size(width, "width")?, size(height, "height")?);
        let tol = match tolerance.map(|c| (cell(c), c.1)) {
            Some((v, unit)) if !v.is_empty() => {
                let v2 = v.trim_start_matches('±').trim_start_matches("+/-").trim_start_matches("+-").trim();
                if v2.contains(['+', '-']) && !v2.contains('\'') {
                    return Err(at(format!("tolerance {v:?}: give one figure either way (±6 mm, 1/4 in)")));
                }
                let mm = length_mm(v2, unit).map_err(|e| at(format!("tolerance {e}")))?;
                Some(Tolerance { words: v.to_string(), steps: vec![], mm_per_m: None, max_mm: Some((mm * 10.0).round() / 10.0) })
            }
            _ => None,
        };
        let r1 = |v: f64| (v * 10.0).round() / 10.0;
        let tol_words = tol.as_ref().map(|t| format!(", {}", t.words)).unwrap_or_default();
        lines.push(Line {
            line_id: m.to_string(),
            group: Some("Openings".into()),
            activity: format!("{} {m}", capital(t.word())),
            requirement: format!("Rough opening {} × {} mm{tol_words}", r1(w), r1(h)),
            reference: None,
            method: Method::Dimensional,
            evidence: Evidence::Measured,
            question: Question { kind: Kind::Opening, thresholds: None, opening: Some(Expected { kind: t, width_mm: r1(w), height_mm: r1(h) }) },
            tolerance: tol,
            points: BTreeMap::new(),
            record: Some("The opening's size, with the face's picture".into()),
            required: None,
            place_hint: wall.map(|c| cell(c).to_string()).filter(|s| !s.is_empty()),
            note: None,
        });
    }
    if lines.is_empty() {
        return Err("the schedule has a header and no rows".into());
    }
    let list = List {
        format: FORMAT.into(),
        version: 0,
        list_id: "schedule".into(),
        title: title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Door and window schedule").chars().take(200).collect(),
        revision: revision.trim().into(),
        issued: None,
        issued_by: None,
        source: Some(Source { kind: SourceKind::Schedule, name: None, reference: None }),
        parties: vec![Party { id: "contractor".into(), role: PartyRole::Contractor, name: None }],
        lines,
    };
    list.check()?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEDULE_CSV: &str = "mark,type,width,height,tolerance,wall\n\
        W1,Window,3'-0\",4'-0\",±1/4\",North wall\n\
        W2,window,914 mm,1219 mm,6 mm,North wall\n\
        D1,Door,\"36 in\",80 in,,East wall\n";

    #[test]
    fn a_schedule_reads_into_opening_lines() {
        let l = from_schedule(SCHEDULE_CSV, "A", None).unwrap();
        assert_eq!(l.lines.len(), 3);
        let w1 = &l.lines[0];
        assert_eq!((w1.line_id.as_str(), w1.activity.as_str(), w1.place_hint.as_deref()), ("W1", "Window W1", Some("North wall")));
        let e = w1.question.opening.as_ref().unwrap();
        assert_eq!((e.kind, e.width_mm, e.height_mm), (OpeningType::Window, 914.4, 1219.2));
        assert_eq!(w1.tolerance.as_ref().unwrap().max_mm, Some(6.4));
        assert_eq!(l.lines[1].question.opening.as_ref().unwrap().width_mm, 914.0);
        let d1 = &l.lines[2];
        assert_eq!((d1.question.opening.as_ref().unwrap().kind, d1.tolerance.is_none()), (OpeningType::Door, true));
        assert_eq!(d1.requirement, "Rough opening 914.4 × 2032 mm");
        // Every line is measured, and the list is a valid list.
        assert!(l.lines.iter().all(|l| l.evidence == Evidence::Measured && l.question.kind == Kind::Opening));
        l.check().unwrap();
        let back: List = serde_json::from_value(serde_json::to_value(&l).unwrap()).unwrap();
        assert_eq!(back, l);
    }

    /// The schedule sample reads into the list beside it (`specs/list-mission/samples`).
    #[test]
    fn the_schedule_sample_reads_into_its_list() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../specs/list-mission/samples");
        let csv = std::fs::read_to_string(format!("{dir}/demo-schedule.csv")).unwrap();
        let mut got = from_schedule(&csv, "A", Some("Door and window schedule, sample")).unwrap();
        got.source = Some(Source { kind: SourceKind::Schedule, name: Some("Sample written for the model; no building has these openings.".into()), reference: None });
        let want: List = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/demo-schedule.list.json")).unwrap()).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn a_bad_schedule_is_refused_with_its_reason() {
        let bad = |csv: &str| from_schedule(csv, "A", None).unwrap_err();
        assert!(bad("mark,type,width,height\nW1,window,,4'\n").contains("row 2 (W1): no width"), "{}", bad("mark,type,width,height\nW1,window,,4'\n"));
        assert!(bad("mark,type,width,height\nW1,skylight,3',4'\n").contains("type \"skylight\": window, door or opening"));
        assert!(bad("mark,type,width,height\nW1,window,3',4'\nW1,window,3',4'\n").contains("row 3 (W1): this mark is in the schedule twice"));
        assert!(bad("mark,type,width,height\nW1,window,36,48\n").contains("has no unit"));
        assert!(bad("mark,type,width,height,tolerance\nW1,window,3',4',+1/2 -0\n").contains("one figure either way"));
        assert!(bad("mark,type,height\nW1,window,4'\n").contains("this one has no width"));
        assert!(bad("mark,type,width,height\n").contains("no rows"));
        assert!(bad("").contains("empty"));
        assert!(bad("mark,type,width,height\nW 1,window,3',4'\n").contains("the mark is letters and digits"));
        // A bare number takes the column's unit.
        let l = from_schedule("mark,type,width_in,height_mm,tolerance_mm\nW1,window,36,1219,6\n", "A", Some("Level 1")).unwrap();
        let e = l.lines[0].question.opening.as_ref().unwrap();
        assert_eq!((e.width_mm, e.height_mm, l.lines[0].tolerance.as_ref().unwrap().max_mm, l.title.as_str()), (914.4, 1219.0, Some(6.0), "Level 1"));
    }

    #[test]
    fn lengths_read_in_the_units_schedules_use() {
        for (s, mm) in [("914", 914.0), ("914 mm", 914.0), ("91.4 cm", 914.0), ("0.914 m", 914.0), ("36 in", 914.4), ("36\"", 914.4),
                        ("3'-0\"", 914.4), ("3' 0\"", 914.4), ("3'", 914.4), ("2' 11 1/2\"", 901.7), ("3 ft", 914.4), ("1/4 in", 6.35), ("1/4\"", 6.35)] {
            let got = length_mm(s, Some("mm")).unwrap();
            assert!((got - mm).abs() < 0.01, "{s}: {got}");
        }
        assert!(length_mm("36", None).is_err());
        assert!(length_mm("wide", Some("mm")).is_err());
        assert!(length_mm("1/0 in", None).is_err());
    }

    #[test]
    fn csv_fields_may_be_quoted() {
        let rows = csv_rows("a,\"b, c\",\"say \"\"hi\"\"\"\r\n\n1,2,3");
        assert_eq!(rows, vec![vec!["a", "b, c", "say \"hi\""], vec!["1", "2", "3"]]);
    }

    fn sample() -> List {
        serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../specs/list-mission/samples/demo-closeout.list.json")).unwrap()).unwrap()
    }

    #[test]
    fn the_sample_list_reads_and_checks() {
        let l = sample();
        l.check().unwrap();
        assert_eq!(l.lines.len(), 8);
    }

    #[test]
    fn a_list_says_what_is_wrong_with_it() {
        let base = sample();
        let fails = |f: &dyn Fn(&mut List)| {
            let mut l = base.clone();
            f(&mut l);
            l.check().unwrap_err()
        };
        assert!(fails(&|l| l.format = "x".into()).contains("not a list"));
        assert!(fails(&|l| l.list_id = "Not Ok".into()).contains("list_id"));
        assert!(fails(&|l| l.parties.clear()).contains("parties"));
        assert!(fails(&|l| l.parties.push(l.parties[0].clone())).contains("named twice"));
        assert!(fails(&|l| l.lines[1].line_id = "1.1".into()).contains("line 1.1 is in the list twice"));
        assert!(fails(&|l| l.lines[0].line_id = "1..1".into()).contains("a number (1.2) or a mark"));
        assert!(fails(&|l| l.lines[0].question.kind = Kind::Photo).contains("measured evidence answers a measured question"));
        assert!(fails(&|l| l.lines[5].question.kind = Kind::Plumb).contains("seen evidence answers a photo question"));
        assert!(fails(&|l| l.lines[7].record = None).contains("names the record"));
        assert!(fails(&|l| l.lines[5].tolerance = l.lines[0].tolerance.clone()).contains("a tolerance goes with measured evidence"));
        assert!(fails(&|l| { l.lines[0].points.insert("nobody".into(), Point::H); }).contains("party nobody"));
        assert!(fails(&|l| l.lines[0].question.kind = Kind::Opening).contains("gives the opening it expects"));
        assert!(fails(&|l| l.lines[0].question.opening = Some(Expected { kind: OpeningType::Door, width_mm: 900.0, height_mm: 2000.0 })).contains("only an opening question"));
        assert!(fails(&|l| {
            l.lines[0].question = Question { kind: Kind::Opening, thresholds: None, opening: Some(Expected { kind: OpeningType::Door, width_mm: -1.0, height_mm: 2000.0 }) };
            l.lines[0].tolerance = None;
        })
        .contains("width_mm"));
        assert!(fails(&|l| {
            l.lines[0].question = Question { kind: Kind::Opening, thresholds: None, opening: Some(Expected { kind: OpeningType::Door, width_mm: 900.0, height_mm: 2000.0 }) };
        })
        .contains("one figure either way"));
        assert!(fails(&|l| l.lines[0].tolerance.as_mut().unwrap().steps.clear()).contains("give one of steps"));
    }

    #[test]
    fn a_revision_keeps_the_answers_of_unchanged_lines() {
        let a = from_schedule(SCHEDULE_CSV, "A", None).unwrap();
        let mut m = mission(a.clone(), "p1", "agent", None).unwrap();
        assert_eq!(m.targets.iter().map(|t| t.target_id.as_str()).collect::<Vec<_>>(), ["W1", "W2", "D1"]);
        assert!(m.targets.iter().all(|t| t.status == Status::Open && t.face.is_none()));
        m.targets[0].note = "kept".into();
        m.targets[1].note = "kept".into();
        // Revision B changes W2's size and adds D2.
        let b = from_schedule(&SCHEDULE_CSV.replace("W2,window,914 mm", "W2,window,1000 mm").replace("\nD1", "\nD2,door,3',7',,East\nD1"), "B", None).unwrap();
        let n = mission(b.clone(), "p1", "agent", Some(&m)).unwrap();
        assert_eq!(n.mission_id, m.mission_id);
        let note = |id: &str| n.targets.iter().find(|t| t.target_id == id).unwrap().note.clone();
        assert_eq!((note("W1"), note("W2"), note("D2")), ("kept".into(), String::new(), String::new()));
        assert_eq!(n.earlier.len(), 1);
        assert_eq!((n.earlier[0]["list_revision"].as_str(), n.list.as_ref().unwrap().revision.as_str()), (Some("A"), "B"));
        // The same revision with other lines is refused; the same list again is fine.
        assert!(mission(b.clone(), "p1", "agent", Some(&n)).is_ok());
        let mut b2 = b.clone();
        b2.lines.pop();
        assert!(mission(b2, "p1", "agent", Some(&n)).unwrap_err().contains("already loaded with other lines"));
        // Another list on the same project is refused while the mission runs.
        let mut other = b;
        other.list_id = "other".into();
        assert!(mission(other, "p1", "agent", Some(&n)).unwrap_err().contains("one list per project"));
    }

    #[test]
    fn the_record_says_what_each_line_came_to() {
        let mut m = mission(sample(), "p1", "agent", None).unwrap();
        let r = record(&m);
        assert_eq!((r["format"].as_str(), r["list_revision"].as_str(), r["lines"].as_array().unwrap().len()), (Some(RECORD_FORMAT), Some("A"), 8));
        assert!(r["lines"].as_array().unwrap().iter().all(|l| l["status"] == "open"), "{r}");
        let said = lines_said(&m);
        assert!(said[0]["reason"].as_str().unwrap().contains("not bound to a wall face yet"), "{}", said[0]);
        assert!(said[2]["reason"].as_str().unwrap().contains("can't ask this kind of question yet"), "{}", said[2]);
        assert!(said[5]["reason"].as_str().unwrap().contains("a picture marked by a person"), "{}", said[5]);
        assert!(said[7]["reason"].as_str().unwrap().contains("a person's statement"), "{}", said[7]);
        // An answered plumb line, against the quarter inch in ten feet: no
        // allowance for a 14.6 m face, so neutral with the bridge's reason.
        let t = &mut m.targets[0];
        t.face = Some(crate::register::Face { from: [0.0, 0.0], to: [14.6, 0.0] });
        t.bound = Some(crate::mission::Bound { by: "crew".into(), via: "web UI".into(), at: "t".into(), label: "w1".into() });
        t.tolerance = m.list.as_ref().unwrap().lines[0].tolerance.clone();
        let reading = crate::ask::Reading { out_of_plumb_mm: 4.1, lean_mm_per_m: 0.3, flat_rms_mm: 3.0, flat_p95_mm: None, straightedge_ok_pct: None,
                                            worst_strip_mm_per_m: None, over_m: Some(14.6), stations: vec![1, 2] };
        let said = crate::ask::reading_said(&reading, t.tolerance.as_ref());
        t.question = Some(crate::ask::Question { verdict: Some(Verdict::Answer), answer: Some(reading), said: Some(said), seen_share: Some(0.9), ..crate::ask::Question::new("plumb") });
        m.targets[1].status = Status::Declined;
        m.targets[1].note = "behind scaffold".into();
        m.targets[1].decided_by = Some("crew".into());
        let r = record(&m);
        let l0 = &r["lines"][0];
        assert_eq!((l0["status"].as_str(), l0["conformance"].as_str(), l0["evidence"]["grade"].as_str()), (Some("answered"), Some("neutral"), Some("measured")), "{l0}");
        assert!(l0["reason"].as_str().unwrap().contains("no allowance for 14.6 m"), "{l0}");
        assert_eq!((l0["place"]["bound_by"]["name"].as_str(), l0["evidence"]["reading"]["over_m"].as_f64()), (Some("crew"), Some(14.6)));
        let l1 = &r["lines"][1];
        assert_eq!((l1["status"].as_str(), l1["reason"].as_str(), l1["decided"]["by"]["name"].as_str()), (Some("declined"), Some("behind scaffold"), Some("crew")));
        // Within the tolerance at a height it covers: conforming; outside it: non-conforming.
        let at = |mm: f64, over: f64| {
            let mut m = m.clone();
            let t = &mut m.targets[0];
            let q = t.question.as_mut().unwrap();
            let a = q.answer.as_mut().unwrap();
            (a.out_of_plumb_mm, a.over_m) = (mm, Some(over));
            q.said = Some(crate::ask::reading_said(a, t.tolerance.as_ref()));
            record(&m)["lines"][0]["conformance"].as_str().unwrap().to_string()
        };
        assert_eq!((at(4.0, 3.0).as_str(), at(9.0, 3.0).as_str()), ("conforming", "non_conforming"));
    }
}
