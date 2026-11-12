//! A mission from a list (`specs/list-mission.md`, parts A and B): the list a
//! job must document, read from JSON (`specs/list-mission/list.schema.json`),
//! from an inspection and test plan's spreadsheet, or from a door and window
//! schedule (`specs/construction-counts.md`, part A), and the record that
//! answers it line by line.
//!
//! Loading a list makes the project's mission: one target per line, its
//! `target_id` the line's id. A new revision of the same list keeps the
//! answer of every line whose requirement, question and tolerance did not
//! change; the record of the revision it replaces is kept as it stood.
//!
//! Built here: the list, the record, sorting a line (the agent proposes its
//! grade, question and tolerance; a person confirms, `sort`), and binding a
//! line to a wall face (a person does it). Measured lines answer as their questions do: `plumb` and
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
    /// With `question`: the line is sorted. A line from a spreadsheet has
    /// neither until a person confirms them (part B).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    /// On an unsorted line, the list's words alone; sorting adds the arithmetic.
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
    fn words(&self) -> (&str, &Option<Evidence>, &Option<Question>, &Option<Tolerance>) {
        (self.requirement.trim(), &self.evidence, &self.question, &self.tolerance)
    }

    /// The tolerance's words as the list gives them.
    fn tolerance_words(&self) -> Option<&str> {
        self.tolerance.as_ref().map(|t| t.words.trim())
    }

    /// The question's kind, once the line is sorted.
    pub fn kind(&self) -> Option<Kind> {
        self.question.as_ref().map(|q| q.kind)
    }

    pub fn sorted(&self) -> bool {
        self.evidence.is_some() && self.question.is_some()
    }

    /// The highest grade the line's method allows: dimensional can be
    /// measured, visual seen, a test or a review only attested.
    pub fn method_grade(&self) -> Evidence {
        match self.method {
            Method::Dimensional => Evidence::Measured,
            Method::Visual => Evidence::Seen,
            Method::Test | Method::Review => Evidence::Attested,
        }
    }
}

impl Evidence {
    pub fn word(self) -> &'static str {
        match self {
            Evidence::Measured => "measured",
            Evidence::Seen => "seen",
            Evidence::Attested => "attested",
        }
    }
    fn rank(self) -> u8 {
        match self {
            Evidence::Attested => 0,
            Evidence::Seen => 1,
            Evidence::Measured => 2,
        }
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
            self.check_line(l).map_err(|e| format!("line {id}: {e}"))?;
        }
        Ok(())
    }

    /// One line's checks, past its id: what the schema says and what it can't.
    pub fn check_line(&self, l: &Line) -> Result<(), String> {
        text("activity", &l.activity, 200)?;
        text("requirement", &l.requirement, 600)?;
        if let Some(p) = l.points.keys().find(|p| !self.parties.iter().any(|q| &q.id == *p)) {
            return Err(format!("party {p} has a point on the line but is not in parties"));
        }
        let (evidence, q) = match (l.evidence, &l.question) {
            (None, None) => {
                // Unsorted: a tolerance is the list's words until sorting adds the arithmetic.
                return match &l.tolerance {
                    Some(t) if t.steps.is_empty() && t.mm_per_m.is_none() && t.max_mm.is_none() => text("tolerance: words", &t.words, 200),
                    Some(_) => Err("an unsorted line's tolerance is its words; the arithmetic comes with its sorting".into()),
                    None => Ok(()),
                };
            }
            (Some(e), Some(q)) => (e, q),
            _ => return Err("a sorted line has both evidence and question; an unsorted one neither".into()),
        };
        let k = q.kind;
        match evidence {
            Evidence::Measured if !k.measured() => return Err(format!("measured evidence answers a measured question, not {}", k.word())),
            Evidence::Seen if k != Kind::Photo => return Err("seen evidence answers a photo question".into()),
            Evidence::Attested if k != Kind::Statement => return Err("attested evidence answers a statement question".into()),
            Evidence::Attested if l.record.as_deref().is_none_or(|r| r.trim().is_empty()) => return Err("an attested line names the record kept as proof".into()),
            _ => {}
        }
        match (&q.opening, k) {
            (None, Kind::Opening) => return Err("an opening question gives the opening it expects: type, width_mm and height_mm".into()),
            (Some(_), k) if k != Kind::Opening => return Err("only an opening question gives an opening".into()),
            (Some(e), _) => {
                for (what, v) in [("width_mm", e.width_mm), ("height_mm", e.height_mm)] {
                    if !(v.is_finite() && v > 0.0 && v <= 20_000.0) {
                        return Err(format!("{what}: a size in mm, up to 20 m"));
                    }
                }
            }
            _ => {}
        }
        if let Some(t) = &l.tolerance {
            if evidence != Evidence::Measured {
                return Err("a tolerance goes with measured evidence".into());
            }
            if k == Kind::Opening { check_opening_tolerance(t) } else { t.check() }?;
        }
        Ok(())
    }

    pub fn line(&self, id: &str) -> Option<&Line> {
        self.lines.iter().find(|l| l.line_id == id)
    }
}

/// A target for each line, as a fresh mission holds them: open, unbound.
pub fn target_for(l: &Line) -> Target {
    let name: String = match l.question.as_ref().and_then(|q| q.opening.as_ref()) {
        Some(e) => format!("{} {}", capital(e.kind.word()), l.line_id),
        None => format!("{} {}", l.line_id, l.activity.trim()),
    };
    let kind = l.kind().map(Kind::word).unwrap_or("unsorted");
    Target { target_id: l.line_id.clone(), name: name.chars().take(80).collect(), kind: kind.into(), updated_at: crate::util::now_rfc3339(), ..Default::default() }
}

/// Each line's tolerance in the list's own words, as it was loaded, `None`
/// for a line with none: sorting may add a tolerance or leave one out, and a
/// revision is read against these.
fn words_as_loaded(list: &List) -> BTreeMap<String, Option<String>> {
    list.lines.iter().map(|l| (l.line_id.clone(), l.tolerance_words().map(String::from))).collect()
}

/// Whether a line of a new revision says what the earlier one said, as loaded:
/// its requirement, method and tolerance words. A mission saved before the
/// words were kept (an empty map) is read from the line itself.
fn same_row(new: &Line, old: &Line, old_words: &BTreeMap<String, Option<String>>) -> bool {
    let was = if old_words.is_empty() { old.tolerance_words() } else { old_words.get(&old.line_id).and_then(|w| w.as_deref()) };
    new.requirement.trim() == old.requirement.trim() && new.method == old.method && new.tolerance_words() == was
}

fn sorting_of(l: &Line) -> Option<Sorting> {
    Some(Sorting { line_id: l.line_id.clone(), evidence: l.evidence?, question: l.question.clone()?, tolerance: l.tolerance.clone() })
}

/// The line as the list gives it, before any sorting: its tolerance the words alone.
fn unsort(l: &mut Line) {
    (l.evidence, l.question) = (None, None);
    if let Some(t) = &mut l.tolerance {
        (t.steps, t.mm_per_m, t.max_mm) = (vec![], None, None);
    }
}

/// A new revision's unsorted lines take the earlier revision's sorting where
/// the row says the same: the person sorted those words already. A sorting
/// that no longer checks on the new line is left off, with a note.
fn carry_sorting(list: &mut List, old: &Mission, notes: &mut BTreeMap<String, String>) {
    let Some(before) = &old.list else { return };
    let probe = List { lines: vec![], ..list.clone() };
    for l in list.lines.iter_mut().filter(|l| !l.sorted()) {
        let Some(b) = before.line(&l.line_id).filter(|b| b.sorted() && same_row(l, b, &old.loaded_words)) else { continue };
        let mut carried = l.clone();
        (carried.evidence, carried.question, carried.tolerance) = (b.evidence, b.question.clone(), b.tolerance.clone());
        match probe.check_line(&carried) {
            Ok(()) => *l = carried,
            Err(e) => {
                notes.insert(l.line_id.clone(), format!("revision {}: the earlier sorting no longer fits this line ({e}); sort it again", list.revision));
            }
        }
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A mission from a list. `prev`: the project's mission now, kept when it is
/// an earlier revision of the same list: lines whose words did not change keep
/// their target (place, answer), and the earlier record is kept as it stood.
/// `person`: whether a person loads it. From anyone else (the agent), a
/// sorted line whose sorting is new or differs from the earlier revision's is
/// loaded unsorted, its sorting kept as a proposal for a person.
pub fn mission(mut list: List, project_id: &str, by: &str, prev: Option<&Mission>, person: bool) -> Result<Mission, String> {
    list.check()?;
    let same = prev.filter(|m| m.list.as_ref().is_some_and(|l| l.list_id == list.list_id));
    let mut demoted: Vec<Sorting> = Vec::new();
    if !person {
        let before = same.and_then(|m| m.list.as_ref());
        let old_words = same.map(|m| &m.loaded_words);
        for l in list.lines.iter_mut() {
            let b = before.and_then(|b| b.line(&l.line_id));
            if l.sorted() {
                if b.is_some_and(|b| sorting_of(b) == sorting_of(l) && same_row(l, b, old_words.expect("a list"))) {
                    continue;
                }
                demoted.extend(sorting_of(l));
                unsort(l);
            }
            // In JSON a tolerance is the list's words and the sorting's
            // arithmetic at once. Where the row is the earlier one's and only
            // the sorting differs (sent with another sorting or none, the
            // tolerance left out, or the words the earlier list or its sorting
            // had), the list's words stay as they were loaded, so the
            // person's sorting and answer carry over.
            if let (Some(b), Some(words)) = (b, old_words) {
                let was = if words.is_empty() { b.tolerance_words().map(String::from) } else { words.get(&b.line_id).cloned().flatten() };
                let now = l.tolerance_words().map(String::from);
                let only_sorting = now.is_none() || now == was || now.as_deref() == b.tolerance_words();
                if l.requirement.trim() == b.requirement.trim() && l.method == b.method && only_sorting {
                    l.tolerance = was.map(|w| Tolerance { words: w, steps: vec![], mm_per_m: None, max_mm: None });
                }
            }
        }
    }
    let loaded = words_as_loaded(&list);
    let mut notes = BTreeMap::new();
    if let Some(old) = same {
        carry_sorting(&mut list, old, &mut notes);
    }
    let at = crate::util::now_rfc3339();
    let propose_into = |m: &mut Mission| {
        for s in &demoted {
            m.proposal_seq += 1;
            m.proposed.insert(s.line_id.clone(), Proposed { sorting: s.clone(), by: by.into(), at: at.clone(), seq: m.proposal_seq });
        }
    };
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
            let mut m = m.clone();
            propose_into(&mut m);
            return Ok(m);
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
    // A proposal waiting on a line the new revision didn't change still waits;
    // one on a line whose words changed is dropped, and the line says so.
    if let (Some(old), Some(before)) = (same, old_list) {
        m.proposal_seq = old.proposal_seq;
        for (id, p) in &old.proposed {
            if list.line(id).zip(before.line(id)).is_some_and(|(a, b)| same_row(a, b, &old.loaded_words)) {
                m.proposed.insert(id.clone(), p.clone());
            } else if list.line(id).is_some() {
                notes.insert(id.clone(), format!("revision {}: the sorting proposed for this line was dropped, since its words changed; propose it again", list.revision));
            }
        }
    }
    propose_into(&mut m);
    m.line_notes = notes;
    m.loaded_words = loaded;
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
        let Some(k) = l.kind() else { return Err(format!("line {id} isn't sorted yet: a person confirms its grade and question first")) };
        if !k.on_a_face() {
            return Err(format!("line {id} asks {}, which isn't asked of a wall face", k.word()));
        }
        let t = m.targets.iter_mut().find(|t| t.target_id == l.line_id).ok_or_else(|| format!("line {id} has no target"))?;
        let moved = t.face != Some(face);
        t.face = Some(face);
        t.geometry = cov.clone();
        t.bound = Some(crate::mission::Bound { by: by.into(), via: via.into(), at: crate::util::now_rfc3339(), label: label.into() });
        if moved {
            ask_on_face(t, l, k);
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

/// Set a bound line's question up on its face, from the start: a plumb or
/// flatness line is asked there, an opening line counted there.
fn ask_on_face(t: &mut Target, l: &Line, k: Kind) {
    (t.question, t.tolerance, t.opening) = (None, None, None);
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

/// The lines whose place, in the list's words, is `place` (case aside).
pub fn lines_at(list: &List, place: &str) -> Vec<String> {
    let want = place.trim().to_lowercase();
    list.lines.iter().filter(|l| l.place_hint.as_deref().is_some_and(|h| h.trim().to_lowercase() == want)).map(|l| l.line_id.clone()).collect()
}

// --------------------------------------------------------------- sorting ---

/// A line's sorting (part B): the grade of evidence it asks for, the question
/// the bridge is asked, and the tolerance with its arithmetic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sorting {
    pub line_id: String,
    pub evidence: Evidence,
    pub question: Question,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<Tolerance>,
}

/// A sorting the agent proposed, waiting for a person. `seq` orders the
/// proposals: a person's yes names the last one they saw (`as_of`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proposed {
    pub sorting: Sorting,
    pub by: String,
    pub at: String,
    #[serde(default)]
    pub seq: u64,
}

/// The line as `s` sorts it, checked, with what the person should hear about it.
fn sorted_line(list: &List, s: &Sorting) -> Result<(Line, Vec<String>), String> {
    let id = s.line_id.trim();
    let before = list.line(id).ok_or_else(|| format!("there is no line {id} in the list"))?;
    let mut l = before.clone();
    (l.evidence, l.question, l.tolerance) = (Some(s.evidence), Some(s.question.clone()), s.tolerance.clone());
    list.check_line(&l).map_err(|e| format!("line {id}: {e}"))?;
    let mut notes = Vec::new();
    let k = s.question.kind;
    let top = l.method_grade();
    if s.evidence.rank() > top.rank() {
        notes.push(format!("line {id}: {} is more than a {} line carries ({}); a person may still sort it so", s.evidence.word(), serde_json::to_value(l.method).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(), top.word()));
    }
    let words = before.tolerance.as_ref().map(|t| t.words.trim().to_string());
    match (&s.tolerance, &words) {
        (None, Some(w)) if s.evidence == Evidence::Measured => notes.push(format!("line {id}: the list's tolerance \"{w}\" isn't used: the answer is a figure with no call")),
        (None, None) if k == Kind::Plumb => notes.push(format!("line {id}: no tolerance: the answer is a figure with no call (not_applicable). Plumb to what?")),
        (Some(t), Some(w)) if t.words.trim() != w => notes.push(format!("line {id}: the tolerance reads \"{}\"; the list says \"{w}\"", t.words.trim())),
        _ => {}
    }
    if s.evidence == Evidence::Measured && !k.on_a_face() {
        notes.push(format!("line {id}: the bridge can't ask {} yet; the line stays open until a person answers or declines it", k.word()));
    }
    Ok((l, notes))
}

/// The agent (or a person) proposes sortings. Nothing changes on the lines;
/// each waits for a person (`sort`). Returns, per line, what to put to them.
pub fn propose(m: &mut Mission, sortings: &[Sorting], by: &str) -> Result<Vec<Value>, String> {
    if m.state == crate::mission::State::SignedOff {
        return Err("the mission is signed off; start a new one to change it".into());
    }
    let list = m.list.clone().ok_or("the mission has no list: load one first")?;
    if sortings.is_empty() {
        return Err("sortings: at least one {line_id, evidence, question, tolerance}".into());
    }
    let mut ids = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in sortings {
        let id = s.line_id.trim().to_string();
        if !ids.insert(id.clone()) {
            return Err(format!("line {id} is proposed twice"));
        }
        let (_, notes) = sorted_line(&list, s)?;
        out.push(json!({"line_id": id, "notes": notes}));
    }
    let at = crate::util::now_rfc3339();
    for s in sortings {
        let mut s = s.clone();
        s.line_id = s.line_id.trim().into();
        m.proposal_seq += 1;
        m.proposed.insert(s.line_id.clone(), Proposed { sorting: s, by: by.into(), at: at.clone(), seq: m.proposal_seq });
    }
    Ok(out)
}

/// What a person's sorting did.
#[derive(Debug, Default, Serialize)]
pub struct SortDone {
    /// Lines whose sorting changed: they start again, open.
    pub sorted: Vec<String>,
    /// Lines sorted as they already were: their answers stand.
    pub unchanged: Vec<String>,
    /// Of the lines sorted, how many as the agent proposed, and how many the person changed.
    pub as_proposed: usize,
    pub changed_by_person: usize,
    pub notes: Vec<String>,
}

/// Why a person's sort was refused: the request is wrong, or a proposal
/// changed after the one the person saw (`as_of`).
#[derive(Debug, PartialEq)]
pub enum SortError {
    Invalid(String),
    Newer(String),
}

impl std::fmt::Display for SortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SortError::Invalid(e) | SortError::Newer(e) => f.write_str(e),
        }
    }
}

impl From<String> for SortError {
    fn from(e: String) -> Self {
        SortError::Invalid(e)
    }
}

/// A person sorts lines (part B): the proposals named by `ids`, and their own
/// `own` sortings, which stand over a proposal for the same line. `as_of`:
/// the last proposal the person saw (its `seq`); with it and no `ids` or
/// `own`, every proposal up to it, so one yes covers the whole sorting. A
/// proposal newer than `as_of` is refused: the person hasn't seen it. A line
/// whose sorting changes starts again, open: its answer was to other words.
pub fn sort(m: &mut Mission, ids: &[String], own: &[Sorting], as_of: Option<u64>) -> Result<SortDone, SortError> {
    if m.state == crate::mission::State::SignedOff {
        return Err(SortError::Invalid("the mission is signed off; start a new one to change it".into()));
    }
    let mut list = m.list.clone().ok_or("the mission has no list: load one first".to_string())?;
    let mut plan: Vec<Sorting> = Vec::new();
    let ids: Vec<String> = match (ids.is_empty() && own.is_empty(), as_of) {
        (true, None) => return Err(SortError::Invalid("name what the person confirmed: their own sortings, or the proposals' lines with as_of (the last proposal they saw)".into())),
        (true, Some(_)) => m.proposed.keys().cloned().collect(),
        _ if !ids.is_empty() && as_of.is_none() => {
            return Err(SortError::Invalid("with the proposals' lines, give as_of: the last proposal the person saw (GET …/list gives it)".into()))
        }
        _ => ids.iter().map(|i| i.trim().to_string()).collect(),
    };
    if ids.is_empty() && own.is_empty() {
        return Err(SortError::Invalid("nothing is proposed: give the sortings".into()));
    }
    for id in &ids {
        let p = m.proposed.get(id).ok_or_else(|| format!("line {id} has no proposal: give its sorting"))?;
        if as_of.is_some_and(|a| p.seq > a) {
            return Err(SortError::Newer(format!("the proposal for line {id} changed after the one the person saw (as_of {}); show them this one", as_of.unwrap_or(0))));
        }
        plan.push(p.sorting.clone());
    }
    for s in own {
        let mut s = s.clone();
        s.line_id = s.line_id.trim().into();
        match plan.iter_mut().find(|p| p.line_id == s.line_id) {
            Some(p) => *p = s,
            None => plan.push(s),
        }
    }
    let mut done = SortDone::default();
    let mut lines = Vec::new();
    for s in &plan {
        let (l, notes) = sorted_line(&list, s)?;
        done.notes.extend(notes);
        lines.push(l);
    }
    for (s, l) in plan.iter().zip(lines) {
        match m.proposed.remove(&s.line_id) {
            Some(p) if p.sorting == *s => done.as_proposed += 1,
            Some(_) => done.changed_by_person += 1,
            None => {}
        }
        let i = list.lines.iter().position(|x| x.line_id == l.line_id).expect("checked");
        if list.lines[i].words() == l.words() {
            done.unchanged.push(l.line_id.clone());
            continue;
        }
        let t = m.targets.iter_mut().find(|t| t.target_id == l.line_id).ok_or_else(|| format!("line {} has no target", l.line_id))?;
        let k = l.kind().expect("sorted");
        t.kind = k.word().into();
        // Its answer, or its decline, was to other words: it starts again, open.
        if t.status == Status::Declined {
            (t.status, t.decided_by, t.decided_via) = (Status::Open, None, None);
            t.note.clear();
        }
        if t.face.is_some() && k.on_a_face() {
            ask_on_face(t, &l, k);
        } else {
            (t.face, t.geometry, t.bound, t.question, t.tolerance, t.opening) = (None, None, None, None, None, None);
            t.status = Status::Open;
            t.stations.clear();
        }
        t.updated_at = crate::util::now_rfc3339();
        m.line_notes.remove(&l.line_id);
        done.sorted.push(l.line_id.clone());
        list.lines[i] = l;
    }
    for id in &done.unchanged {
        m.line_notes.remove(id);
    }
    list.check()?;
    m.list = Some(list);
    if !done.sorted.is_empty() && m.state == crate::mission::State::Ready {
        m.state = crate::mission::State::Active;
        m.ready_summary = None;
    }
    Ok(done)
}

/// The lines not sorted yet, and those with a proposal waiting.
pub fn unsorted(m: &Mission) -> Vec<String> {
    m.list.as_ref().map(|l| l.lines.iter().filter(|l| !l.sorted()).map(|l| l.line_id.clone()).collect()).unwrap_or_default()
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
    let (Some(k), Some(evidence)) = (l.kind(), l.evidence) else {
        return st("open", "not sorted yet: the agent proposes its grade, question and tolerance, and a person confirms them");
    };
    if !k.on_a_face() {
        let why = match evidence {
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
    if l.kind() == Some(Kind::Opening) {
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
        let note = m.line_notes.get(&l.line_id);
        if !s.reason.is_empty() && (s.status != "open" || !l.sorted()) {
            v["reason"] = json!(match note {
                Some(n) => format!("{}; {n}", s.reason),
                None => s.reason.clone(),
            });
        } else if let Some(n) = note {
            v["reason"] = json!(n);
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
            let mut v = json!({"line_id": l.line_id, "activity": l.activity, "requirement": l.requirement, "method": l.method, "evidence": l.evidence,
                               "question": l.kind().map(Kind::word), "sorted": l.sorted(), "status": s.status, "reason": s.reason});
            if let Some(p) = m.proposed.get(&l.line_id) {
                v["proposed"] = json!(p);
            }
            if let Some(n) = m.line_notes.get(&l.line_id) {
                v["note"] = json!(n);
            }
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

/// Rows of a CSV: fields split on commas, with "quoted, fields" and "" for a
/// quote. Rows end in LF, CRLF or a lone CR (older Mac spreadsheets).
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
            ('\r', false) if chars.peek() == Some(&'\n') => {}
            ('\r' | '\n', false) => {
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
            evidence: Some(Evidence::Measured),
            question: Some(Question { kind: Kind::Opening, thresholds: None, opening: Some(Expected { kind: t, width_mm: r1(w), height_mm: r1(h) }) }),
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

// ----------------------------------------------------------- the ITP CSV ---

/// An inspection and test plan's own columns. Every other column is a party's.
const ITP: [&str; 8] = ["line", "group", "activity", "requirement", "reference", "method", "tolerance", "record"];

/// Whether a spreadsheet was saved with semicolons between its fields (as
/// some locales' "CSV" is): its header is one field holding semicolons.
pub fn semicolons(csv: &str) -> bool {
    csv_rows(csv).first().is_some_and(|h| h.len() == 1 && h[0].contains(';'))
}

/// Whether a CSV's header is an inspection and test plan's.
pub fn is_itp(csv: &str) -> bool {
    let names: Vec<String> = csv_rows(csv).first().map(|h| h.iter().map(|c| c.trim().to_lowercase()).collect()).unwrap_or_default();
    names.iter().any(|n| n == "line") && names.iter().any(|n| n == "requirement")
}

fn party_role(s: &str) -> Option<PartyRole> {
    match s.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
        "contractor" => Some(PartyRole::Contractor),
        "subcontractor" | "sub" => Some(PartyRole::Subcontractor),
        "client" | "owner" => Some(PartyRole::Client),
        "inspector" => Some(PartyRole::Inspector),
        "third_party" => Some(PartyRole::ThirdParty),
        _ => None,
    }
}

/// A party from its column's header: `checker (client)`, or a role's own name
/// (`contractor`, `client`, `inspector`, `subcontractor`, `third party`). A
/// header that names no role is read as a third party, and a note says so.
fn party_of(header: &str) -> Result<(Party, Option<String>), String> {
    let h = header.trim();
    let (name, role) = match h.split_once('(') {
        Some((n, r)) if r.trim_end().ends_with(')') => {
            let r = r.trim_end().trim_end_matches(')');
            (n.trim(), Some(party_role(r).ok_or_else(|| format!("column {h:?}: the role in brackets is contractor, subcontractor, client, inspector or third party"))?))
        }
        _ => (h, None),
    };
    let id: String = name.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-");
    if !is_id(&id) {
        return Err(format!("column {h:?}: a party's column is named with letters and digits (trade, checker (client))"));
    }
    let (role, note) = match role.or_else(|| party_role(name)) {
        Some(r) => (r, None),
        None => (PartyRole::ThirdParty, Some(format!("party {id}: its column names no role, so it is read as a third party; name it as \"{name} (client)\" or \"{name} (contractor)\" to say"))),
    };
    Ok((Party { id, role, name: None }, note))
}

fn point(s: &str) -> Option<Option<Point>> {
    match s.trim().to_lowercase().as_str() {
        "" => Some(None),
        "h" | "hold" => Some(Some(Point::H)),
        "w" | "witness" => Some(Some(Point::W)),
        "r" | "review" => Some(Some(Point::R)),
        "s" | "surveillance" => Some(Some(Point::S)),
        _ => None,
    }
}

fn method(s: &str) -> Option<Method> {
    match s.trim().to_lowercase().as_str() {
        "dimensional" => Some(Method::Dimensional),
        "visual" => Some(Method::Visual),
        "test" => Some(Method::Test),
        "review" => Some(Method::Review),
        _ => None,
    }
}

/// A list from an inspection and test plan's spreadsheet (part A): one
/// unsorted line per row, the line's id its number. The columns are `line`,
/// `group`, `activity`, `requirement`, `reference`, `method`, `tolerance`,
/// `record`, and one per party, whose cells are H, W, R, S or blank. Returns
/// the list and notes for the person (a party read without a role).
pub fn from_itp(csv: &str, list_id: &str, revision: &str, title: Option<&str>) -> Result<(List, Vec<String>), String> {
    let rows = csv_rows(csv);
    let Some((header, body)) = rows.split_first() else { return Err("the spreadsheet is empty".into()) };
    let names: Vec<String> = header.iter().map(|h| h.trim().to_lowercase()).collect();
    let mut seen = std::collections::HashSet::new();
    if let Some(n) = names.iter().find(|n| !n.is_empty() && !seen.insert(n.as_str())) {
        return Err(format!("the column {n:?} is in the header twice"));
    }
    let col = |c: &str| names.iter().position(|n| n == c);
    let missing: Vec<&str> = ["line", "activity", "requirement", "method"].into_iter().filter(|c| col(c).is_none()).collect();
    if !missing.is_empty() {
        return Err(format!("an inspection and test plan has the columns {}, and one per party; this one has no {}", ITP.join(", "), missing.join(", ")));
    }
    let mut parties: Vec<(usize, Party)> = Vec::new();
    let mut notes = Vec::new();
    for (i, h) in header.iter().enumerate() {
        if ITP.contains(&names[i].as_str()) {
            continue;
        }
        if h.trim().is_empty() {
            if body.iter().any(|r| r.get(i).is_some_and(|c| !c.trim().is_empty())) {
                return Err(format!("column {} has no name, and something in it: name it (a party's column), or empty it", i + 1));
            }
            continue;
        }
        let (p, note) = party_of(h)?;
        if parties.iter().any(|(_, q)| q.id == p.id) {
            return Err(format!("two columns name the party {}", p.id));
        }
        notes.extend(note);
        parties.push((i, p));
    }
    if parties.is_empty() {
        return Err("an inspection and test plan has a column per party (trade, checker (client)), with H, W, R or S in its cells; this one has none".into());
    }
    let mut lines = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for (n, r) in body.iter().enumerate() {
        let cell = |c: &str| col(c).and_then(|i| r.get(i)).map(|s| s.trim()).unwrap_or("");
        let opt = |c: &str| Some(cell(c).to_string()).filter(|s| !s.is_empty());
        let id = cell("line");
        let at = |e: String| format!("row {} ({}): {e}", n + 2, if id.is_empty() { "no line number" } else { id });
        if !is_line_id(id) {
            return Err(at("the line number is letters and digits joined by '.' or '-' (1.2, W1)".into()));
        }
        if !ids.insert(id.to_string()) {
            return Err(at("this line number is in the spreadsheet twice".into()));
        }
        let m = method(cell("method")).ok_or_else(|| at(format!("method {:?}: dimensional, visual, test or review", cell("method"))))?;
        let mut points = BTreeMap::new();
        for (i, p) in &parties {
            let v = r.get(*i).map(|s| s.trim()).unwrap_or("");
            match point(v) {
                Some(Some(pt)) => {
                    points.insert(p.id.clone(), pt);
                }
                Some(None) => {}
                None => {
                    return Err(at(format!(
                        "column {:?} holds {v:?}. A column other than {} is a party's, and its cells are H (hold), W (witness), R (review), S (surveillance) or blank",
                        header[*i].trim(),
                        ITP.join(", ")
                    )))
                }
            }
        }
        let line = Line {
            line_id: id.to_string(),
            group: opt("group"),
            activity: cell("activity").to_string(),
            requirement: cell("requirement").to_string(),
            reference: opt("reference"),
            method: m,
            evidence: None,
            question: None,
            tolerance: opt("tolerance").map(|w| Tolerance { words: w, steps: vec![], mm_per_m: None, max_mm: None }),
            points,
            record: opt("record"),
            required: None,
            place_hint: None,
            note: None,
        };
        let parties: Vec<Party> = parties.iter().map(|(_, p)| p.clone()).collect();
        let probe = List { format: FORMAT.into(), version: 0, list_id: "x".into(), title: "x".into(), revision: "x".into(), issued: None, issued_by: None, source: None, parties, lines: vec![] };
        probe.check_line(&line).map_err(at)?;
        lines.push(line);
    }
    if lines.is_empty() {
        return Err("the spreadsheet has a header and no rows".into());
    }
    let list = List {
        format: FORMAT.into(),
        version: 0,
        list_id: list_id.trim().into(),
        title: title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Inspection and test plan").chars().take(200).collect(),
        revision: revision.trim().into(),
        issued: None,
        issued_by: None,
        source: Some(Source { kind: SourceKind::Itp, name: None, reference: None }),
        parties: parties.into_iter().map(|(_, p)| p).collect(),
        lines,
    };
    list.check()?;
    Ok((list, notes))
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
        let e = w1.question.as_ref().unwrap().opening.as_ref().unwrap();
        assert_eq!((e.kind, e.width_mm, e.height_mm), (OpeningType::Window, 914.4, 1219.2));
        assert_eq!(w1.tolerance.as_ref().unwrap().max_mm, Some(6.4));
        assert_eq!(l.lines[1].question.as_ref().unwrap().opening.as_ref().unwrap().width_mm, 914.0);
        let d1 = &l.lines[2];
        assert_eq!((d1.question.as_ref().unwrap().opening.as_ref().unwrap().kind, d1.tolerance.is_none()), (OpeningType::Door, true));
        assert_eq!(d1.requirement, "Rough opening 914.4 × 2032 mm");
        // Every line is measured, and the list is a valid list.
        assert!(l.lines.iter().all(|l| l.evidence == Some(Evidence::Measured) && l.kind() == Some(Kind::Opening)));
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
        let e = l.lines[0].question.as_ref().unwrap().opening.as_ref().unwrap();
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
        assert!(fails(&|l| l.lines[0].question.as_mut().unwrap().kind = Kind::Photo).contains("measured evidence answers a measured question"));
        assert!(fails(&|l| l.lines[5].question.as_mut().unwrap().kind = Kind::Plumb).contains("seen evidence answers a photo question"));
        assert!(fails(&|l| l.lines[7].record = None).contains("names the record"));
        assert!(fails(&|l| l.lines[5].tolerance = l.lines[0].tolerance.clone()).contains("a tolerance goes with measured evidence"));
        assert!(fails(&|l| { l.lines[0].points.insert("nobody".into(), Point::H); }).contains("party nobody"));
        assert!(fails(&|l| l.lines[0].question.as_mut().unwrap().kind = Kind::Opening).contains("gives the opening it expects"));
        assert!(fails(&|l| l.lines[0].question.as_mut().unwrap().opening = Some(Expected { kind: OpeningType::Door, width_mm: 900.0, height_mm: 2000.0 })).contains("only an opening question"));
        assert!(fails(&|l| {
            l.lines[0].question = Some(Question { kind: Kind::Opening, thresholds: None, opening: Some(Expected { kind: OpeningType::Door, width_mm: -1.0, height_mm: 2000.0 }) });
            l.lines[0].tolerance = None;
        })
        .contains("width_mm"));
        assert!(fails(&|l| {
            l.lines[0].question = Some(Question { kind: Kind::Opening, thresholds: None, opening: Some(Expected { kind: OpeningType::Door, width_mm: 900.0, height_mm: 2000.0 }) });
        })
        .contains("one figure either way"));
        assert!(fails(&|l| l.lines[0].tolerance.as_mut().unwrap().steps.clear()).contains("give one of steps"));
    }

    #[test]
    fn a_revision_keeps_the_answers_of_unchanged_lines() {
        let a = from_schedule(SCHEDULE_CSV, "A", None).unwrap();
        let mut m = mission(a.clone(), "p1", "agent", None, true).unwrap();
        assert_eq!(m.targets.iter().map(|t| t.target_id.as_str()).collect::<Vec<_>>(), ["W1", "W2", "D1"]);
        assert!(m.targets.iter().all(|t| t.status == Status::Open && t.face.is_none()));
        m.targets[0].note = "kept".into();
        m.targets[1].note = "kept".into();
        // Revision B changes W2's size and adds D2.
        let b = from_schedule(&SCHEDULE_CSV.replace("W2,window,914 mm", "W2,window,1000 mm").replace("\nD1", "\nD2,door,3',7',,East\nD1"), "B", None).unwrap();
        let n = mission(b.clone(), "p1", "agent", Some(&m), true).unwrap();
        assert_eq!(n.mission_id, m.mission_id);
        let note = |id: &str| n.targets.iter().find(|t| t.target_id == id).unwrap().note.clone();
        assert_eq!((note("W1"), note("W2"), note("D2")), ("kept".into(), String::new(), String::new()));
        assert_eq!(n.earlier.len(), 1);
        assert_eq!((n.earlier[0]["list_revision"].as_str(), n.list.as_ref().unwrap().revision.as_str()), (Some("A"), "B"));
        // The same revision with other lines is refused; the same list again is fine.
        assert!(mission(b.clone(), "p1", "agent", Some(&n), true).is_ok());
        let mut b2 = b.clone();
        b2.lines.pop();
        assert!(mission(b2, "p1", "agent", Some(&n), true).unwrap_err().contains("already loaded with other lines"));
        // Another list on the same project is refused while the mission runs.
        let mut other = b;
        other.list_id = "other".into();
        assert!(mission(other, "p1", "agent", Some(&n), true).unwrap_err().contains("one list per project"));
    }

    #[test]
    fn the_record_says_what_each_line_came_to() {
        let mut m = mission(sample(), "p1", "agent", None, true).unwrap();
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

    // ------------------------------------------------- the ITP and sorting ---

    const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../specs/list-mission/samples");

    fn itp_sample() -> String {
        std::fs::read_to_string(format!("{SAMPLES}/demo-closeout.csv")).unwrap()
    }

    /// The sample's sorting of each line, as a person would confirm it.
    fn sortings_of(l: &List) -> Vec<Sorting> {
        l.lines.iter().map(|l| Sorting { line_id: l.line_id.clone(), evidence: l.evidence.unwrap(), question: l.question.clone().unwrap(), tolerance: l.tolerance.clone() }).collect()
    }

    #[test]
    fn the_itp_sample_reads_into_its_list_less_the_sorting() {
        let (got, notes) = from_itp(&itp_sample(), "demo-closeout", "A", Some("Closeout list, demonstration building")).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let want = sample();
        assert_eq!(got.parties, want.parties);
        assert_eq!(got.source.as_ref().unwrap().kind, SourceKind::Itp);
        assert!(got.lines.iter().all(|l| !l.sorted() && l.evidence.is_none()));
        for (g, w) in got.lines.iter().zip(&want.lines) {
            let mut w = w.clone();
            (w.evidence, w.question, w.place_hint, w.note) = (None, None, None, None);
            if let Some(t) = &mut w.tolerance {
                (t.steps, t.mm_per_m, t.max_mm) = (vec![], None, None);
            }
            assert_eq!(g, &w);
        }
        got.check().unwrap();
        // Sorted as the sample is, it is the sample's lines.
        let mut m = mission(got, "p1", "agent", None, true).unwrap();
        assert_eq!(unsorted(&m).len(), 8);
        propose(&mut m, &sortings_of(&want), "agent").unwrap();
        let done = sort(&mut m, &[], &[], Some(u64::MAX)).unwrap();
        assert_eq!((done.sorted.len(), done.as_proposed, done.changed_by_person), (8, 8, 0));
        for (g, w) in m.list.as_ref().unwrap().lines.iter().zip(&want.lines) {
            assert_eq!((&g.evidence, &g.question, &g.tolerance), (&w.evidence, &w.question, &w.tolerance), "{}", g.line_id);
        }
        assert!(unsorted(&m).is_empty() && m.proposed.is_empty());
    }

    #[test]
    fn a_bad_itp_is_refused_with_its_reason() {
        let bad = |csv: &str| from_itp(csv, "itp", "A", None).unwrap_err();
        let h = "line,activity,requirement,method,trade\n";
        for (csv, why) in [
            ("", "empty".to_string()),
            ("line,activity,requirement,trade\n1,a,b,H\n", "this one has no method".to_string()),
            (h, "no rows".to_string()),
            ("line,activity,requirement,method\n1,a,b,visual\n", "a column per party".to_string()),
            ("line,activity,requirement,method,trade,Trade\n1,a,b,visual,H,W\n", "\"trade\" is in the header twice".to_string()),
            ("line,activity,requirement,method,trade,trade (client)\n1,a,b,visual,H,W\n", "two columns name the party trade".to_string()),
            ("line,activity,requirement,method,trade,\n1,a,b,visual,H,x\n", "column 6 has no name".to_string()),
            ("line,activity,requirement,method,checker (boss)\n1,a,b,visual,H\n", "the role in brackets".to_string()),
            ("line,activity,requirement,method,!!\n1,a,b,visual,H\n", "letters and digits (trade".to_string()),
            (&format!("{h}1 1,a,b,visual,H\n"), "row 2 (1 1): the line number".to_string()),
            (&format!("{h},a,b,visual,H\n"), "row 2 (no line number)".to_string()),
            (&format!("{h}1,a,b,visual,H\n1,c,d,visual,W\n"), "row 3 (1): this line number is in the spreadsheet twice".to_string()),
            (&format!("{h}1,a,b,looking,H\n"), "method \"looking\": dimensional, visual, test or review".to_string()),
            (&format!("{h}1,a,b,visual,maybe\n"), "column \"trade\" holds \"maybe\"".to_string()),
            ("line,activity,requirement,method,notes\n1,a,b,visual,see the drawing\n", "A column other than line, group".to_string()),
            (&format!("{h}1,a,,visual,H\n"), "row 2 (1): requirement".to_string()),
        ] {
            let e = bad(csv);
            assert!(e.contains(&why), "{why}: {e}");
        }
    }

    #[test]
    fn an_itp_names_its_parties_and_points() {
        let csv = "Line,Group,Activity,Requirement,Method,Tolerance,Contractor,Third party,Engineer,Record,\n\
                   1.1,Walls,\"Facade, south\",Plumb,Dimensional,\"1/4\"\" in 10'\",hold,w,R,,\n\
                   2,Doors,Entry,\"Hung, latching\",visual,,S,,,Picture,\n";
        let (l, notes) = from_itp(csv, "job-7", "B", Some("Level 2")).unwrap();
        assert_eq!((l.list_id.as_str(), l.revision.as_str(), l.title.as_str()), ("job-7", "B", "Level 2"));
        let parties: Vec<(&str, PartyRole)> = l.parties.iter().map(|p| (p.id.as_str(), p.role)).collect();
        assert_eq!(parties, [("contractor", PartyRole::Contractor), ("third-party", PartyRole::ThirdParty), ("engineer", PartyRole::ThirdParty)]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("party engineer: its column names no role"), "{notes:?}");
        let a = &l.lines[0];
        assert_eq!((a.activity.as_str(), a.tolerance.as_ref().unwrap().words.as_str(), a.method), ("Facade, south", "1/4\" in 10'", Method::Dimensional));
        assert_eq!(a.points, BTreeMap::from([("contractor".to_string(), Point::H), ("third-party".to_string(), Point::W), ("engineer".to_string(), Point::R)]));
        assert_eq!((l.lines[1].points.len(), l.lines[1].record.as_deref()), (1, Some("Picture")));
        assert!(is_itp(csv) && !is_schedule(csv) && !is_itp("mark,type,width\n"));
        // An unsorted line's tolerance is words only; a sorted line needs both halves.
        let mut x = l.clone();
        x.lines[0].tolerance.as_mut().unwrap().max_mm = Some(6.0);
        assert!(x.check().unwrap_err().contains("the arithmetic comes with its sorting"));
        x.lines[0].tolerance = None;
        x.lines[0].evidence = Some(Evidence::Measured);
        assert!(x.check().unwrap_err().contains("both evidence and question"));
    }

    fn itp_mission() -> Mission {
        let (l, _) = from_itp(&itp_sample(), "demo-closeout", "A", None).unwrap();
        mission(l, "p1", "agent", None, true).unwrap()
    }

    fn plumb(id: &str, tol: Option<Tolerance>) -> Sorting {
        Sorting { line_id: id.into(), evidence: Evidence::Measured, question: Question { kind: Kind::Plumb, thresholds: None, opening: None }, tolerance: tol }
    }

    fn quarter_inch() -> Tolerance {
        Tolerance { words: "a quarter inch in ten feet".into(), steps: vec![crate::ask::Step { up_to_m: Some(3.05), mm: 6.4 }], mm_per_m: None, max_mm: None }
    }

    #[test]
    fn a_proposal_waits_for_a_person() {
        let mut m = itp_mission();
        let r = record(&m);
        assert!(r["lines"].as_array().unwrap().iter().all(|l| l["status"] == "open"));
        let said = lines_said(&m);
        assert!(said[0]["reason"].as_str().unwrap().starts_with("not sorted yet"), "{}", said[0]);
        assert_eq!((said[0]["sorted"].as_bool(), said[0]["question"].clone()), (Some(false), Value::Null));
        assert_eq!(m.targets[0].kind, "unsorted");
        // What a proposal may not be.
        for (s, why) in [
            (vec![plumb("9.9", None)], "there is no line 9.9"),
            (vec![plumb("1.1", None), plumb("1.1", None)], "line 1.1 is proposed twice"),
            (vec![Sorting { evidence: Evidence::Seen, ..plumb("1.1", None) }], "line 1.1: seen evidence answers a photo question"),
            (vec![Sorting { evidence: Evidence::Seen, question: Question { kind: Kind::Photo, thresholds: None, opening: None }, ..plumb("5.1", Some(quarter_inch())) }], "a tolerance goes with measured"),
            (vec![plumb("1.1", Some(Tolerance { words: "1/4 in 10 ft".into(), steps: vec![], mm_per_m: None, max_mm: None }))], "give one of steps"),
            (vec![], "at least one"),
        ] {
            let e = propose(&mut m, &s, "agent").unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
        }
        assert!(m.proposed.is_empty());
        // The notes the person is told: no tolerance, the list's unused, a grade above the method's, a kind not asked yet.
        let notes = propose(&mut m, &[plumb("1.1", None), plumb("1.2", Some(Tolerance { words: "6 mm in 3 m".into(), ..quarter_inch() })), plumb("5.1", None),
                                     Sorting { question: Question { kind: Kind::Height, thresholds: None, opening: None }, ..plumb("2.1", None) }], "agent").unwrap();
        let n = |i: usize| notes[i]["notes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect::<Vec<_>>().join(" | ");
        assert!(n(0).contains("the list's tolerance \"a quarter inch in ten feet\" isn't used"), "{}", n(0));
        assert!(n(1).contains("the tolerance reads \"6 mm in 3 m\"; the list says"), "{}", n(1));
        assert!(n(2).contains("measured is more than a visual line carries (seen)") && n(2).contains("Plumb to what?"), "{}", n(2));
        assert!(n(3).contains("can't ask height yet"), "{}", n(3));
        // Nothing is sorted until a person sorts it.
        assert_eq!(unsorted(&m).len(), 8);
        assert_eq!(lines_said(&m)[0]["proposed"]["by"], "agent");
        assert!(bind(&mut m, &["1.1".into()], crate::register::Face { from: [0.0, 0.0], to: [5.0, 0.0] }, "w1", "crew", "web UI", &crate::register::Geo::none())
            .unwrap_err()
            .contains("isn't sorted yet"));
    }

    #[test]
    fn a_person_sorts_part_of_the_list_and_sorts_again() {
        let mut m = itp_mission();
        assert!(sort(&mut m, &[], &[], Some(u64::MAX)).unwrap_err().to_string().contains("nothing is proposed"));
        assert!(sort(&mut m, &[], &[], None).unwrap_err().to_string().contains("name what the person confirmed"));
        propose(&mut m, &[plumb("1.1", Some(quarter_inch())), plumb("1.2", Some(quarter_inch()))], "agent").unwrap();
        assert!(sort(&mut m, &["1.1".into()], &[], None).unwrap_err().to_string().contains("give as_of"));
        let seq = m.proposal_seq;
        assert!(sort(&mut m, &["2.1".into()], &[], Some(seq)).unwrap_err().to_string().contains("line 2.1 has no proposal"));
        // A proposal newer than the one the person saw is refused.
        let seen = m.proposal_seq;
        propose(&mut m, &[plumb("1.1", None)], "agent").unwrap();
        assert!(matches!(sort(&mut m, &["1.1".into()], &[], Some(seen)), Err(SortError::Newer(e)) if e.contains("changed after the one the person saw")));
        assert!(matches!(sort(&mut m, &[], &[], Some(seen)), Err(SortError::Newer(_))));
        propose(&mut m, &[plumb("1.1", Some(quarter_inch()))], "agent").unwrap();
        // The person takes 1.1 as proposed and sorts 6.1 themselves.
        let statement = Sorting { line_id: "6.1".into(), evidence: Evidence::Attested, question: Question { kind: Kind::Statement, thresholds: None, opening: None }, tolerance: None };
        let seq = m.proposal_seq;
        let done = sort(&mut m, &["1.1".into()], std::slice::from_ref(&statement), Some(seq)).unwrap();
        assert_eq!((done.sorted.clone(), done.as_proposed, done.changed_by_person), (vec!["1.1".to_string(), "6.1".to_string()], 1, 0));
        assert_eq!((m.proposed.keys().collect::<Vec<_>>(), unsorted(&m).len()), (vec![&"1.2".to_string()], 6));
        assert_eq!(m.targets[0].kind, "plumb");
        assert!(lines_said(&m)[7]["reason"].as_str().unwrap().contains("a person's statement answers it"));
        // 1.2's proposal changed by the person: no tolerance after all.
        let done = sort(&mut m, &[], &[plumb("1.2", None)], None).unwrap();
        assert_eq!((done.as_proposed, done.changed_by_person), (0, 1));
        assert!(done.notes.iter().any(|n| n.contains("isn't used")), "{:?}", done.notes);
        // Bound and answered, 1.1 sorted again the same way keeps its answer; sorted otherwise, it starts again on its face.
        let face = crate::register::Face { from: [0.0, 0.0], to: [14.6, 0.0] };
        bind(&mut m, &["1.1".into()], face, "w1", "crew", "web UI", &crate::register::Geo::none()).unwrap();
        m.targets[0].question.as_mut().unwrap().verdict = Some(Verdict::NotYet);
        m.targets[0].stations = vec!["s1".into()];
        let done = sort(&mut m, &[], &[plumb("1.1", Some(quarter_inch()))], None).unwrap();
        assert_eq!(done.unchanged, ["1.1"]);
        assert_eq!(state_of(&m.list.as_ref().unwrap().lines[0], &m.targets[0]).status, "not_yet");
        let flat = Sorting { question: Question { kind: Kind::Flatness, thresholds: None, opening: None }, ..plumb("1.1", None) };
        sort(&mut m, &[], &[flat], None).unwrap();
        let t = &m.targets[0];
        assert_eq!((t.face, t.kind.as_str(), t.question.as_ref().unwrap().verdict, t.tolerance.is_none(), t.stations.len()), (Some(face), "flatness", None, true, 0));
        // Sorted to a question not asked of a face, it leaves the face.
        sort(&mut m, &[], &[Sorting { question: Question { kind: Kind::Height, thresholds: None, opening: None }, ..plumb("1.1", None) }], None).unwrap();
        assert!(m.targets[0].face.is_none() && m.targets[0].bound.is_none() && m.targets[0].question.is_none());
        // A declined line sorted differently starts again, open: the decline was to other words.
        (m.targets[1].status, m.targets[1].note, m.targets[1].decided_by) = (Status::Declined, "behind scaffold".into(), Some("crew".into()));
        sort(&mut m, &[], &[plumb("1.2", Some(quarter_inch()))], None).unwrap();
        let t = &m.targets[1];
        assert_eq!((t.status, t.note.as_str(), t.decided_by.clone()), (Status::Open, "", None));
        // A ready mission goes back to work; a signed-off one refuses.
        m.state = crate::mission::State::Ready;
        sort(&mut m, &[], &[plumb("1.2", None)], None).unwrap();
        assert_eq!(m.state, crate::mission::State::Active);
        m.state = crate::mission::State::SignedOff;
        assert!(sort(&mut m, &[], &[plumb("1.2", None)], None).unwrap_err().to_string().contains("signed off"));
        assert!(propose(&mut m, &[plumb("1.2", None)], "agent").unwrap_err().contains("signed off"));
        let mut none = Mission::new(&crate::mission::MissionRequest { goal: "g".into(), kind: crate::mission::MissionKind::Custom, done_when: "d".into(), targets: vec![] }, "p", "a");
        assert!(sort(&mut none, &[], &[plumb("1.2", None)], None).unwrap_err().to_string().contains("has no list"));
        assert!(propose(&mut none, &[plumb("1.2", None)], "a").unwrap_err().contains("has no list"));
        assert!(unsorted(&none).is_empty());
    }

    #[test]
    fn a_new_revision_keeps_the_sorting_of_rows_that_say_the_same() {
        let mut a = itp_mission();
        let statement = Sorting { line_id: "6.1".into(), evidence: Evidence::Attested, question: Question { kind: Kind::Statement, thresholds: None, opening: None }, tolerance: None };
        propose(&mut a, &[plumb("1.1", Some(quarter_inch())), plumb("1.2", Some(quarter_inch())), plumb("2.1", None), plumb("3.1", None), plumb("4.1", None)], "agent").unwrap();
        let seq = a.proposal_seq;
        sort(&mut a, &["1.1".into(), "1.2".into(), "4.1".into()], std::slice::from_ref(&statement), Some(seq)).unwrap();
        // A re-sort of 1.1 waits on, and so does one of 1.2, whose words change.
        propose(&mut a, &[Sorting { question: Question { kind: Kind::Flatness, thresholds: None, opening: None }, ..plumb("1.1", None) }, plumb("1.2", None)], "agent").unwrap();
        a.targets[0].note = "answered".into();
        a.targets[1].note = "answered".into();
        // Revision B: 1.2's tolerance words change; 3.1's requirement changes.
        let csv = itp_sample().replace("1.2,Masonry,Fourth wall face,Plumb within a quarter inch in ten feet,,dimensional,a quarter inch in ten feet",
                                       "1.2,Masonry,Fourth wall face,Plumb within a quarter inch in ten feet,,dimensional,three eighths in ten feet")
                              .replace("Every place water would stand", "Every place water stands")
                              .replace(",\"Torque log, signed by the installer\"", ",");
        let (b, _) = from_itp(&csv, "demo-closeout", "B", None).unwrap();
        let m = mission(b, "p1", "agent", Some(&a), true).unwrap();
        let l = m.list.as_ref().unwrap();
        assert!(l.line("1.1").unwrap().sorted() && !l.line("1.2").unwrap().sorted());
        assert_eq!((m.targets[0].note.as_str(), m.targets[1].note.as_str(), m.targets[1].kind.as_str()), ("answered", "", "unsorted"));
        // 4.1, which had no tolerance words, keeps its sorting too.
        assert!(l.line("4.1").unwrap().sorted());
        // 6.1's statement no longer checks (no record named now): unsorted, and it says why.
        assert!(!l.line("6.1").unwrap().sorted());
        assert!(m.line_notes["6.1"].contains("the earlier sorting no longer fits this line (an attested line names the record"), "{:?}", m.line_notes);
        assert!(record(&m)["lines"][7]["reason"].as_str().unwrap().contains("no longer fits"), "{}", record(&m)["lines"][7]);
        // Proposals on 1.1 and 2.1 wait on; 1.2's and 3.1's went with their old words, and the lines say so.
        assert_eq!(m.proposed.keys().collect::<Vec<_>>(), ["1.1", "2.1"]);
        assert!(m.line_notes["1.2"].contains("the sorting proposed for this line was dropped"), "{:?}", m.line_notes);
        assert!(lines_said(&m)[3]["note"].as_str().unwrap().contains("dropped"));
        assert_eq!(m.earlier.len(), 1);
        // The same sheet again, sorted since, is the same list: nothing changes.
        let mut m2 = m.clone();
        sort(&mut m2, &[], &[plumb("1.2", None)], None).unwrap();
        assert!(!m2.line_notes.contains_key("1.2"), "sorting the line clears its note");
        let (again, _) = from_itp(&csv, "demo-closeout", "B", None).unwrap();
        let m3 = mission(again, "p1", "agent", Some(&m2), true).unwrap();
        assert_eq!((m3.earlier.len(), m3.list, m2.list.clone()), (1, m2.list.clone(), m2.list));
    }

    #[test]
    fn an_agents_sorted_list_is_proposed_not_sorted() {
        // The agent loads the sample JSON: every line arrives unsorted, its sorting proposed.
        let m = mission(sample(), "p1", "agent", None, false).unwrap();
        assert_eq!((unsorted(&m).len(), m.proposed.len(), m.proposal_seq), (8, 8, 8));
        assert_eq!(m.proposed["1.1"].sorting.tolerance, sample().lines[0].tolerance);
        assert!(m.list.as_ref().unwrap().lines[0].tolerance.as_ref().unwrap().steps.is_empty());
        // A person loads it: sorted as given.
        let mut p = mission(sample(), "p1", "crew", None, true).unwrap();
        assert!(unsorted(&p).is_empty() && p.proposed.is_empty());
        p.targets[0].note = "answered".into();
        // The agent sends the same revision with one sorting changed: the lines stay as the person sorted them, and the change is proposed.
        let mut changed = sample();
        changed.lines[0].tolerance.as_mut().unwrap().steps[0].mm = 6.0;
        let q = mission(changed.clone(), "p1", "agent", Some(&p), false).unwrap();
        assert_eq!((q.list, q.targets[0].note.as_str(), q.proposed.keys().collect::<Vec<_>>()), (p.list.clone(), "answered", vec![&"1.1".to_string()]));
        // The same, unchanged, from the agent: nothing proposed.
        assert!(mission(sample(), "p1", "agent", Some(&p), false).unwrap().proposed.is_empty());
        // Revision B from the agent with the change: 1.1 keeps the earlier sorting (its row is the same) and the change waits.
        changed.revision = "B".into();
        let r = mission(changed, "p1", "agent", Some(&p), false).unwrap();
        assert_eq!((r.list.as_ref().unwrap().lines[0].tolerance.as_ref().unwrap().steps[0].mm, r.targets[0].note.as_str()), (6.4, "answered"));
        assert_eq!(r.proposed["1.1"].sorting.tolerance.as_ref().unwrap().steps[0].mm, 6.0);
        // The agent re-sorts 1.1 without its tolerance, in the same revision and in a new one:
        // the person's sorting and answer stay, and the agent's sorting is a proposal.
        for rev in ["A", "C"] {
            let mut dropped = sample();
            dropped.revision = rev.into();
            dropped.lines[0].tolerance = None;
            let r = mission(dropped, "p1", "agent", Some(&p), false).unwrap();
            let l = &r.list.as_ref().unwrap().lines[0];
            assert_eq!((l.tolerance.as_ref().map(|t| t.steps.len()), r.targets[0].note.as_str()), (Some(1), "answered"), "revision {rev}");
            assert_eq!((r.proposed["1.1"].sorting.tolerance.clone(), r.loaded_words["1.1"].as_deref()), (None, Some("a quarter inch in ten feet")), "revision {rev}");
        }
        // The agent sends 1.1 with no sorting and no tolerance: the same; the person's sorting and answer stay.
        for rev in ["A", "E"] {
            let mut bare = sample();
            bare.revision = rev.into();
            (bare.lines[0].evidence, bare.lines[0].question, bare.lines[0].tolerance) = (None, None, None);
            let r = mission(bare, "p1", "agent", Some(&p), false).unwrap();
            assert!(r.list.as_ref().unwrap().lines[0].sorted(), "revision {rev}");
            assert_eq!((r.targets[0].note.as_str(), r.list.as_ref().unwrap().lines[0].tolerance.as_ref().unwrap().steps[0].mm), ("answered", 6.4), "revision {rev}");
            assert!(r.proposed.is_empty(), "revision {rev}");
        }
        // New tolerance words alone (neither the list's nor its sorting's) are a change to the list: the line starts again.
        let mut retoleranced = sample();
        retoleranced.revision = "F".into();
        retoleranced.lines[0].tolerance.as_mut().unwrap().words = "three eighths in ten feet".into();
        let r = mission(retoleranced, "p1", "agent", Some(&p), false).unwrap();
        let l = &r.list.as_ref().unwrap().lines[0];
        assert!(!l.sorted() && r.targets[0].note.is_empty(), "{l:?}");
        assert_eq!((l.tolerance.as_ref().unwrap().words.as_str(), r.loaded_words["1.1"].as_deref()), ("three eighths in ten feet", Some("three eighths in ten feet")));
        assert_eq!(r.proposed["1.1"].sorting.tolerance.as_ref().unwrap().words, "three eighths in ten feet");
        // A changed requirement is a new line: no sorting carried, its answer starts again.
        let mut reworded = sample();
        reworded.revision = "D".into();
        reworded.lines[0].requirement = "Plumb within 6 mm".into();
        reworded.lines[0].tolerance = None;
        let r = mission(reworded, "p1", "agent", Some(&p), false).unwrap();
        assert!(!r.list.as_ref().unwrap().lines[0].sorted() && r.targets[0].note.is_empty());
    }

    #[test]
    fn spreadsheets_from_other_programs_read_or_say_why_not() {
        let crlf = "line,activity,requirement,method,trade\r\n1,a,b,visual,H\r\n2,c,d,test,W\r\n";
        for csv in [crlf.to_string(), crlf.replace("\r\n", "\r"), crlf.replace("\r\n", "\n")] {
            let (l, _) = from_itp(&csv, "itp", "A", None).unwrap();
            assert_eq!(l.lines.len(), 2, "{csv:?}");
        }
        assert!(semicolons("line;activity;requirement;method\n1;a;b;visual\n") && !semicolons(crlf) && !semicolons(""));
    }

    #[tokio::test]
    async fn an_itp_is_loaded_proposed_and_sorted_through_the_routes() {
        use crate::testkit::{Bridge, AGENT, OPERATOR, VIEWER};
        use axum::http::StatusCode;
        const P: &str = "39187455-8278-428c-89ad-8a4835f95be6";
        let b = Bridge::new().await;
        let list = format!("/v1/projects/{P}/list");
        // A header that is neither, or both, is refused.
        let (code, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "a,b\n1,2\n").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("this header is neither"), "{e}");
        let (_, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "line,requirement,mark,width\n").await;
        assert!(e["detail"].as_str().unwrap().contains("send one or the other"), "{e}");
        let (_, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "line;activity;requirement;method;trade\n1;a;b;visual;H\n").await;
        assert!(e["detail"].as_str().unwrap().contains("save it as comma-separated"), "{e}");
        let (code, e) = b.send("PUT", &list, Some(AGENT), "text/csv", "line,activity,requirement,method,trade\n1,a,b,seen,H\n").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("row 2 (1): method \"seen\""), "{e}");
        // The agent loads the plan: every line open and unsorted.
        let (code, m) = b.send("PUT", &format!("{list}?list_id=demo-closeout&title=Closeout"), Some(AGENT), "text/csv", &itp_sample()).await;
        assert_eq!(code, StatusCode::OK, "{m}");
        assert_eq!((m["unsorted"].as_array().unwrap().len(), m["lines"][0]["sorted"].as_bool()), (8, Some(false)));
        assert!(m.get("notes").is_none(), "{m}");
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text == "List loaded: Closeout, revision A: 8 lines; 8 to sort"));
        let (_, m) = b.send("PUT", &format!("{list}?list_id=other-plan"), Some(AGENT), "text/csv", "line,activity,requirement,method,engineer\n1,a,b,visual,H\n").await;
        assert_eq!(m["status"], 409, "another list on the project is refused: {m}");
        // The agent proposes; a viewer can't; nothing is sorted yet.
        let proposal = format!("{list}/proposal");
        let body = json!({"sortings": [{"line_id": "1.1", "evidence": "measured", "question": {"kind": "plumb"}, "tolerance": {"words": "a quarter inch in ten feet", "steps": [{"up_to_m": 3.05, "mm": 6.4}]}},
                                       {"line_id": "5.1", "evidence": "seen", "question": {"kind": "photo"}},
                                       {"line_id": "2.1", "evidence": "measured", "question": {"kind": "plumb"}}]});
        assert_eq!(b.call("POST", &proposal, Some(VIEWER), Some(body.clone())).await.0, StatusCode::FORBIDDEN);
        let (code, e) = b.call("POST", &proposal, Some(AGENT), Some(json!({"sortings": [{"line_id": "1.1", "evidence": "measured", "question": {"kind": "photo"}}]}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("measured evidence answers a measured question"), "{e}");
        let (code, p) = b.call("POST", &proposal, Some(AGENT), Some(body)).await;
        assert_eq!(code, StatusCode::OK, "{p}");
        assert!(p["proposed"][2]["notes"][0].as_str().unwrap().contains("Plumb to what?"), "{p}");
        assert_eq!(p["not_proposed"].as_array().unwrap().len(), 5);
        let (_, l) = b.call("GET", &list, Some(VIEWER), None).await;
        assert_eq!((l["unsorted"].as_array().unwrap().len(), l["proposed"]["1.1"]["by"].as_str()), (8, Some("agent")), "{l}");
        // The agent can't sort; the person sorts with one yes, changing 2.1.
        let sort = format!("{list}/sort");
        assert_eq!(b.call("POST", &sort, Some(AGENT), None).await.0, StatusCode::FORBIDDEN);
        let (code, e) = b.call("POST", &sort, Some(OPERATOR), None).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "an empty yes sorts nothing: {e}");
        assert_eq!(p["as_of"], 3);
        let (code, e) = b.call("POST", &sort, Some(OPERATOR), Some(json!({"as_of": 2}))).await;
        assert_eq!(code, StatusCode::CONFLICT, "the person saw only the first two proposals: {e}");
        let (code, e) = b.call("POST", &sort, Some(OPERATOR), Some(json!({"lines": ["1.1"]}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("give as_of"), "{e}");
        let (code, e) = b.call("POST", &sort, Some(OPERATOR), Some(json!({"lines": ["3.1"], "as_of": 3}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(e["detail"].as_str().unwrap().contains("line 3.1 has no proposal"), "{e}");
        let height = json!({"line_id": "2.1", "evidence": "measured", "question": {"kind": "height"}});
        let (code, s) = b.call("POST", &sort, Some(OPERATOR), Some(json!({"lines": ["1.1", "5.1", "2.1"], "as_of": 3, "sortings": [height], "confirmed_via": "asked in the chat"}))).await;
        assert_eq!(code, StatusCode::OK, "{s}");
        assert_eq!((s["as_proposed"].as_u64(), s["changed_by_person"].as_u64(), s["unsorted"].as_array().unwrap().len()), (Some(2), Some(1), 5), "{s}");
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text == "Sorted 3 lines (2 as proposed, 1 changed by the person) (asked in the chat)"));
        let (_, r) = b.call("GET", &format!("/v1/projects/{P}/record"), Some(VIEWER), None).await;
        let line = |id: &str| r["lines"].as_array().unwrap().iter().find(|l| l["line_id"] == id).unwrap().clone();
        assert_eq!(line("1.1")["status"], "open");
        let (_, l) = b.call("GET", &list, Some(VIEWER), None).await;
        let said = |id: &str| l["lines"].as_array().unwrap().iter().find(|x| x["line_id"] == id).unwrap().clone();
        assert!(said("1.1")["reason"].as_str().unwrap().contains("not bound to a wall face"), "{l}");
        assert!(said("2.1")["reason"].as_str().unwrap().contains("can't ask this kind of question yet"), "{l}");
        assert!(said("3.1")["reason"].as_str().unwrap().starts_with("not sorted yet"), "{l}");
        // Sorting the same line the same way again changes nothing.
        let (_, s) = b.call("POST", &sort, Some(OPERATOR), Some(json!({"sortings": [{"line_id": "5.1", "evidence": "seen", "question": {"kind": "photo"}}]}))).await;
        assert_eq!(s["unchanged"], json!(["5.1"]));
        assert!(b.st.inner.lock().unwrap().log.iter().any(|e| e.text == "Sorted 1 line (0 as proposed); 5.1 as before (web UI)"));
        // A signed-off mission isn't sorted again.
        b.st.inner.lock().unwrap().missions.get_mut(P).unwrap().state = crate::mission::State::SignedOff;
        assert_eq!(b.call("POST", &sort, Some(OPERATOR), Some(json!({"sortings": [height]}))).await.0, StatusCode::CONFLICT);
        assert_eq!(b.call("POST", &proposal, Some(AGENT), Some(json!({"sortings": [height]}))).await.0, StatusCode::CONFLICT);
    }
}
