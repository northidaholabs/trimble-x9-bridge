//! Questions asked of the point cloud while the operator is on site
//! (`specs/on-site-answers.md`). v1 asks one thing of a mission wall target:
//! is it plumb, and how flat is it.
//!
//! The answer is judged on the target's fixed face, never on a wall the
//! registration happened to detect this time: wall ids are renumbered and
//! walls split and merge between registrations. One gate, checked in order:
//!
//! 1. `not_yet`: under 80% of the face is seen (`face_coverage`'s share).
//! 2. `does_not_fit`: shorter than 4 m, or rougher than 15 mm RMS against one
//!    plane (a stepped facade, or one seen by too few stations to fit well;
//!    the re-check after each station can still turn it into an answer).
//! 3. `answer`, once the face has passed twice: once, and again after a later
//!    station that saw it, with plumb moving no more than 2 mm. Until then it
//!    is `not_yet`, "settling".
//!
//! Only `answer` carries plumb or flatness numbers. A reading waiting for its
//! second pass is never shown: the model could quote it.

use crate::analyze::{esc, measure_faces, withhold, FaceFit, Opts};
use crate::register::{face_coverage, Face, Merged, Pose, FACE_COVERED};
use crate::problem::ApiError;
use crate::state::AppState;
use anyhow::Context;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A face shorter than this can't answer plumb.
pub const MIN_LENGTH_M: f64 = 4.0;
/// Rougher than this against one plane: the question doesn't fit the surface.
pub const MAX_RMS_MM: f64 = 15.0;
/// The second pass may move plumb this much and still confirm the first.
pub const SETTLE_MM: f64 = 2.0;
/// The kinds of question v1 asks.
pub const KINDS: [&str; 1] = ["plumb"];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    NotYet,
    DoesNotFit,
    Answer,
}

/// What the gate says of one look at a face.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    NotYet(String),
    DoesNotFit(String),
    Pass,
}

/// Plumb and flatness of a face, from the stations that saw it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reading {
    pub out_of_plumb_mm: f64,
    pub lean_mm_per_m: f64,
    pub flat_rms_mm: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flat_p95_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub straightedge_ok_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worst_strip_mm_per_m: Option<f64>,
    /// The height plumb is measured over (m): the surface's 1st to 99th
    /// percentile of height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over_m: Option<f64>,
    /// Station numbers it rests on.
    pub stations: Vec<u16>,
}

/// A target's question and its latest verdict, as the mission keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub reason: String,
    /// Whether the model has asked it (it is re-judged either way).
    #[serde(default)]
    pub asked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_share: Option<f64>,
    /// Stations that saw the face at the last check.
    #[serde(default)]
    pub stations: Vec<u16>,
    /// Only with `answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Reading>,
    #[serde(default)]
    pub checked_at: String,
}

impl Question {
    pub fn new(kind: &str) -> Question {
        Question { kind: kind.into(), verdict: None, reason: String::new(), asked: false, seen_share: None, stations: vec![], answer: None, checked_at: String::new() }
    }
}

/// An allowance up to a height, or for any height without `up_to_m`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_to_m: Option<f64>,
    pub mm: f64,
}

/// The plumb tolerance the job uses, in the person's words and in one of the
/// forms the standards write it: allowances by height (TMS 602), or per metre
/// of height (ACI 117); either may have a cap. The bridge does the arithmetic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tolerance {
    pub words: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mm_per_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mm: Option<f64>,
}

impl Tolerance {
    pub fn check(&self) -> Result<(), String> {
        let n = self.words.trim().chars().count();
        if n == 0 || n > 200 {
            return Err("tolerance: words, as the person said them (1–200 characters)".into());
        }
        if self.steps.is_empty() == self.mm_per_m.is_none() {
            return Err("tolerance: give one of steps (allowances by height) or mm_per_m".into());
        }
        let positive = |v: f64| v.is_finite() && v > 0.0;
        let values = self.steps.iter().flat_map(|s| [Some(s.mm), s.up_to_m]).chain([self.mm_per_m, self.max_mm]).flatten();
        if !values.into_iter().all(positive) {
            return Err("tolerance: every value is positive, in mm or m".into());
        }
        if self.steps.iter().rev().skip(1).any(|s| s.up_to_m.is_none()) {
            return Err("tolerance: only the last step may leave out up_to_m".into());
        }
        if self.steps.windows(2).any(|w| matches!((w[0].up_to_m, w[1].up_to_m), (Some(a), Some(b)) if b <= a)) {
            return Err("tolerance: steps in order of up_to_m".into());
        }
        Ok(())
    }

    /// The allowance (mm) for a face whose plumb is measured over `over_m`;
    /// none when no step reaches that height and there is no cap.
    pub fn allowed_mm(&self, over_m: f64) -> Option<f64> {
        let base = match self.mm_per_m {
            Some(rate) => Some(rate * over_m),
            None => self.steps.iter().find(|s| s.up_to_m.is_none_or(|u| over_m <= u + 1e-9)).map(|s| s.mm),
        };
        let allowed = match (base, self.max_mm) {
            (Some(b), Some(m)) => b.min(m),
            (b, m) => b.or(m)?,
        };
        Some((allowed * 10.0).round() / 10.0)
    }
}

/// Millimetres as inches to the nearest 1/16 ("1 3/16 in").
pub fn inches(mm: f64) -> String {
    let sixteenths = (mm.abs() / 25.4 * 16.0).round() as u64;
    let (whole, mut num, mut den) = (sixteenths / 16, sixteenths % 16, 16);
    while num > 0 && num % 2 == 0 {
        (num, den) = (num / 2, den / 2);
    }
    match (whole, num) {
        (w, 0) => format!("{w} in"),
        (0, n) => format!("{n}/{den} in"),
        (w, n) => format!("{w} {n}/{den} in"),
    }
}

/// How far (m) walls detected along the face's line run on past its `from`
/// and `to` ends, when either is more than 1 m: the answer covers the face only.
pub fn runs_on(face: &Face, walls: &[crate::register::Wall]) -> Option<[f64; 2]> {
    let (dx, dy) = (face.to[0] - face.from[0], face.to[1] - face.from[1]);
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let along = |p: [f64; 2]| (p[0] - face.from[0]) * ux + (p[1] - face.from[1]) * uy;
    let off = |p: [f64; 2]| ((p[0] - face.from[0]) * uy - (p[1] - face.from[1]) * ux).abs();
    let (mut lo, mut hi) = (0.0f64, len);
    for w in walls {
        let (a, b) = (along(w.from), along(w.to));
        let (wx, wy) = (w.to[0] - w.from[0], w.to[1] - w.from[1]);
        let across = (wx * uy - wy * ux).abs() / wx.hypot(wy).max(1e-6);
        let on_line = off(w.from) <= 0.3 && off(w.to) <= 0.3 && across <= 5f64.to_radians().sin();
        if on_line && a.max(b) >= -1.0 && a.min(b) <= len + 1.0 {
            (lo, hi) = (lo.min(a.min(b)), hi.max(a.max(b)));
        }
    }
    let r1 = |v: f64| (v * 10.0).round() / 10.0;
    (-lo > 1.0 || hi - len > 1.0).then(|| [r1(-lo), r1(hi - len)])
}

/// One look at a face: its share seen, length, the stations that saw it, and
/// the fit (none when under 80% seen: not fitted, or too few points).
#[derive(Debug, Clone)]
pub struct Check {
    pub seen_share: f64,
    pub length_m: f64,
    pub stations: Vec<u16>,
    pub fit: Option<Reading>,
}

/// The gate for one look. `seen_share` 0–1; `rms_mm` none when there were
/// too few points to fit.
pub fn gate(seen_share: f64, length_m: f64, rms_mm: Option<f64>) -> Gate {
    if seen_share < FACE_COVERED {
        return Gate::NotYet(format!("{:.0}% of the face is seen; {:.0}% needed", (seen_share * 100.0).floor(), FACE_COVERED * 100.0));
    }
    if length_m < MIN_LENGTH_M {
        return Gate::DoesNotFit(format!("the face is {length_m:.1} m long; plumb needs {MIN_LENGTH_M:.0} m of it"));
    }
    match rms_mm {
        None => Gate::NotYet("too few points on the face to fit it".into()),
        Some(r) if r > MAX_RMS_MM => Gate::DoesNotFit(format!(
            "the face is rougher than {MAX_RMS_MM:.0} mm RMS against one plane: stepped, not one wall, or seen too thinly to tell"
        )),
        Some(_) => Gate::Pass,
    }
}

/// The question after a new look, from the last verdict and the reading
/// waiting for its second pass (kept by the caller, never published).
/// Returns the question and the reading to keep waiting, if any.
pub fn decide(prev: Option<&Question>, settling: Option<&Reading>, c: &Check) -> (Question, Option<Reading>) {
    let mut q = Question {
        kind: prev.map_or(KINDS[0].to_string(), |p| p.kind.clone()),
        asked: prev.is_some_and(|p| p.asked),
        seen_share: Some(c.seen_share),
        stations: c.stations.clone(),
        checked_at: prev.map(|p| p.checked_at.clone()).unwrap_or_default(),
        verdict: None,
        reason: String::new(),
        answer: None,
    };
    let fit = match (gate(c.seen_share, c.length_m, c.fit.as_ref().map(|f| f.flat_rms_mm)), &c.fit) {
        (Gate::Pass, Some(f)) => f,
        (g, _) => {
            (q.verdict, q.reason) = match g {
                Gate::DoesNotFit(r) => (Some(Verdict::DoesNotFit), r),
                Gate::NotYet(r) => (Some(Verdict::NotYet), r),
                Gate::Pass => (Some(Verdict::NotYet), "too few points on the face to fit it".into()),
            };
            return (q, None);
        }
    };
    let answered = prev.and_then(|p| if p.verdict == Some(Verdict::Answer) { p.answer.as_ref() } else { None });
    let base = answered.or(settling);
    let saw_more = base.is_some_and(|b| c.stations.iter().any(|s| !b.stations.contains(s)));
    let holds = base.is_some_and(|b| (fit.out_of_plumb_mm - b.out_of_plumb_mm).abs() <= SETTLE_MM + 1e-9);
    match (answered, saw_more) {
        // Nothing new saw the face: the last word stands.
        (Some(a), false) => {
            q.verdict = Some(Verdict::Answer);
            q.answer = Some(a.clone());
            (q, None)
        }
        (None, false) if base.is_some() => {
            q.verdict = Some(Verdict::NotYet);
            q.reason = "settling".into();
            (q, settling.cloned())
        }
        _ if saw_more && holds => {
            q.verdict = Some(Verdict::Answer);
            q.answer = Some(fit.clone());
            (q, None)
        }
        _ => {
            q.verdict = Some(Verdict::NotYet);
            q.reason = "settling".into();
            (q, Some(fit.clone()))
        }
    }
}

/// The reading waiting for its second pass, if it is this face's and the
/// question is still settling on it.
pub fn settling_for<'a>(prev: &Question, face: &Face, kept: Option<&'a (Face, Reading)>) -> Option<&'a Reading> {
    let still = prev.verdict == Some(Verdict::NotYet) && prev.reason == "settling";
    kept.filter(|k| still && k.0 == *face).map(|k| &k.1)
}

/// The question as one look judges it, with no settling: the report's
/// recompute on the final registration.
pub fn judged(g: &Gate, kind: &str) -> Question {
    let (verdict, reason) = match g {
        Gate::Pass => (Verdict::Answer, String::new()),
        Gate::NotYet(r) => (Verdict::NotYet, r.clone()),
        Gate::DoesNotFit(r) => (Verdict::DoesNotFit, r.clone()),
    };
    Question { verdict: Some(verdict), reason, ..Question::new(kind) }
}

/// How the report draws a question's evidence from the final fit: as an
/// answer only when it was answered on site and still is; a face that didn't
/// fit, with its reason; nothing for one that wasn't answered on site.
pub fn report_drawing(on_site: &Question, again: &Gate) -> Option<Question> {
    match on_site.verdict {
        Some(Verdict::Answer) if *again == Gate::Pass => Some(judged(again, &on_site.kind)),
        Some(Verdict::Answer) => Some(judged(again, &on_site.kind)).filter(|q| shows_evidence(q.verdict)),
        Some(Verdict::DoesNotFit) => Some(on_site.clone()),
        _ => None,
    }
}

/// Whether a verdict comes with the face's heat map.
pub fn shows_evidence(v: Option<Verdict>) -> bool {
    matches!(v, Some(Verdict::Answer | Verdict::DoesNotFit))
}

/// The answer as the model sees it: numbers only with `answer`, judged
/// against the job's tolerance when it has one.
pub fn public(target_id: &str, q: &Question, tolerance: Option<&Tolerance>) -> Value {
    let verdict = q.verdict.map_or(Value::String("not_asked".into()), |v| serde_json::to_value(v).unwrap_or(Value::Null));
    let mut v = json!({
        "target_id": target_id,
        "kind": q.kind,
        "verdict": verdict,
        "reason": q.reason,
        "seen_pct": q.seen_share.map(|s| (s * 100.0).floor()),
        "stations": q.stations,
    });
    if q.verdict == Some(Verdict::Answer) {
        if let Some(a) = &q.answer {
            v["answer"] = reading_said(a, tolerance);
        }
    }
    if let Some(t) = tolerance {
        v["tolerance"] = serde_json::to_value(t).unwrap_or(Value::Null);
    }
    v
}

/// The face asked about: its length and how far the detected wall runs on
/// past each end.
pub fn face_said(face: &Face, length_m: f64, walls: &[crate::register::Wall]) -> Value {
    let past = runs_on(face, walls);
    json!({"from": face.from, "to": face.to, "length_m": (length_m * 100.0).round() / 100.0,
           "runs_on": past.map(|[a, b]| json!({"past_from_m": a, "past_to_m": b}))})
}

/// A reading as the answer gives it: inches, which way it leans, and against
/// the tolerance when there is one.
pub fn reading_said(a: &Reading, tolerance: Option<&Tolerance>) -> Value {
    let mut ans = serde_json::to_value(a).unwrap_or(Value::Null);
    ans["out_of_plumb_in"] = json!(inches(a.out_of_plumb_mm));
    if a.out_of_plumb_mm != 0.0 {
        ans["leans"] = json!(if a.out_of_plumb_mm > 0.0 { "toward the side it was scanned from" } else { "away from the side it was scanned from" });
    }
    if let Some(t) = tolerance {
        match a.over_m.and_then(|h| t.allowed_mm(h)) {
            Some(allowed) => {
                ans["allowed_mm"] = json!(allowed);
                ans["allowed_in"] = json!(inches(allowed));
                ans["within"] = json!(within(a, allowed));
            }
            None => {
                ans["allowed_mm"] = Value::Null;
                ans["tolerance_note"] = json!(match a.over_m {
                    Some(h) => format!("the tolerance gives no allowance for {h:.1} m of height: add a step for it, or max_mm"),
                    None => "this answer has no measured height to apply the tolerance to: ask again".to_string(),
                });
            }
        }
    }
    ans
}

fn within(a: &Reading, allowed: f64) -> bool {
    a.out_of_plumb_mm.abs() <= allowed + 1e-9
}

/// Preview columns `(x0, x1)` framing the stretch `a`–`b` (project frame) as
/// seen from a station at `pose`: the `subject` the planner takes. `az` is the
/// station's azimuth (rad) by scan column (NaN where it saw nothing). `x0` to
/// `x1` runs the short way round, across the seam when `x1 < x0`.
pub fn preview_span(az: &[f64], preview_w: usize, pose: &Pose, a: [f64; 2], b: [f64; 2]) -> Option<(usize, usize)> {
    let tau = std::f64::consts::TAU;
    let (s, c) = (-pose.turn_deg.to_radians()).sin_cos();
    let column = |p: [f64; 2]| -> Option<usize> {
        let (x, y) = (p[0] - pose.xyz_m[0], p[1] - pose.xyz_m[1]);
        let want = (s * x + c * y).atan2(c * x - s * y).rem_euclid(tau);
        let off = |k: usize| ((az[k] - want + tau / 2.0).rem_euclid(tau) - tau / 2.0).abs();
        (0..az.len()).filter(|&k| az[k].is_finite()).min_by(|&i, &j| off(i).total_cmp(&off(j)))
    };
    if preview_w == 0 {
        return None;
    }
    let to_px = |k: usize| k * preview_w / az.len();
    let (xa, xb) = (to_px(column(a)?), to_px(column(b)?));
    if (xb + preview_w - xa) % preview_w <= preview_w / 2 { Some((xa, xb)) } else { Some((xb, xa)) }
}

/// A mission question as the report gives it: the target, what was said on
/// site, and the same face judged again from the final registration.
#[derive(Debug, Clone)]
pub struct Asked {
    pub target_id: String,
    pub name: String,
    /// The reason, when the person declined it.
    pub declined: Option<String>,
    pub on_site: Question,
    pub again: Gate,
    pub again_reading: Option<Reading>,
    pub image: Option<String>,
    pub tolerance: Option<Tolerance>,
    /// The face's ends, and how far the detected wall runs on past each (m), when it does.
    pub face: Face,
    pub runs_on_m: Option<[f64; 2]>,
}

/// An answer recomputed from the final registration may move this much from
/// the one given on site before the report flags it.
pub const RECOMPUTE_FLAG_MM: f64 = 5.0;

/// The report's opening section: the mission's questions in mission order,
/// each with its verdict and evidence; numbers only on an answer.
pub fn questions_html(asked: &[Asked]) -> String {
    use std::fmt::Write;
    if asked.is_empty() {
        return String::new();
    }
    let label = |v: Option<Verdict>| match v {
        Some(Verdict::Answer) => "answer",
        Some(Verdict::NotYet) => "not yet",
        Some(Verdict::DoesNotFit) => "does not fit",
        None => "not asked",
    };
    let list = |s: &[u16]| s.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    let mut h = String::from("<div class=questions><h2>The questions</h2>\n");
    for a in asked {
        let q = &a.on_site;
        let _ = write!(h, "<section class=question id=\"q-{}\"><h3>{}</h3><p><span class=verdict>{}</span> ", esc(&a.target_id), esc(&a.name), label(q.verdict));
        match (q.verdict, &q.answer) {
            (Some(Verdict::Answer), Some(r)) => {
                let over = r.over_m.map(|m| format!(" over {m:.1} m")).unwrap_or_default();
                let _ = write!(h, "{:.1} mm ({}) out of plumb{over} ({:.1} mm/m lean), {:.1} mm RMS against one plane; stations {}</p>",
                               r.out_of_plumb_mm, inches(r.out_of_plumb_mm), r.lean_mm_per_m, r.flat_rms_mm, list(&r.stations));
                if let Some(t) = &a.tolerance {
                    let call = |r: &Reading| r.over_m.and_then(|m| t.allowed_mm(m)).map(|allowed| (allowed, if within(r, allowed) { "within" } else { "outside" }));
                    let _ = match call(r) {
                        Some((allowed, said)) => write!(h, "<p class=tolerance>Tolerance: “{}”; allowed {allowed:.1} mm ({}){over}: <strong>{said}</strong>.</p>", esc(&t.words), inches(allowed)),
                        None => write!(h, "<p class=tolerance>Tolerance: “{}”; it gives no allowance for this height.</p>", esc(&t.words)),
                    };
                    if let (Gate::Pass, Some(f)) = (&a.again, &a.again_reading) {
                        if let (Some((_, before)), Some((allowed, after))) = (call(r), call(f)) {
                            if before != after {
                                let _ = write!(h, "<p class=flag>From the final registration: {:.1} mm, <strong>{after}</strong> the {allowed:.1} mm allowed.</p>", f.out_of_plumb_mm);
                            }
                        }
                    }
                }
                match (&a.again, &a.again_reading) {
                    (Gate::Pass, Some(f)) => {
                        let moved = (f.out_of_plumb_mm - r.out_of_plumb_mm).abs();
                        if moved > RECOMPUTE_FLAG_MM {
                            let _ = write!(h, "<p class=flag>From the final registration: {:.1} mm out of plumb (stations {}); moved {moved:.1} mm since it was given on site.</p>",
                                           f.out_of_plumb_mm, list(&f.stations));
                        }
                    }
                    (Gate::NotYet(why), _) | (Gate::DoesNotFit(why), _) => {
                        let _ = write!(h, "<p class=flag>The final registration no longer supports this answer: {}.</p>", esc(why));
                    }
                    (Gate::Pass, None) => {
                        let _ = write!(h, "<p class=flag>The final registration no longer supports this answer: too few points to fit.</p>");
                    }
                }
            }
            _ => {
                let _ = write!(h, "{}; stations {}</p>", esc(&q.reason), list(&q.stations));
            }
        }
        if let Some([past_from, past_to]) = a.runs_on_m {
            let end = |m: f64, p: [f64; 2]| (m > 1.0).then(|| format!("{m:.1} m past ({:.1}, {:.1})", p[0], p[1]));
            let past: Vec<String> = [end(past_from, a.face.from), end(past_to, a.face.to)].into_iter().flatten().collect();
            let _ = write!(h, "<p class=extent>This covers the face asked about, {:.1} m; the wall detected along it runs on {}.</p>",
                           (a.face.to[0] - a.face.from[0]).hypot(a.face.to[1] - a.face.from[1]), past.join(" and "));
        }
        if let Some(note) = &a.declined {
            let _ = write!(h, "<p class=declined>declined: {}</p>", esc(note));
        }
        if let Some(img) = &a.image {
            let _ = write!(h, "<img src=\"{}\" alt=\"{} — evidence\" loading=lazy>", esc(img), esc(&a.name));
        }
        h.push_str("</section>\n");
    }
    h.push_str("</div>\n");
    h
}

/// One look at a fixed face: its coverage (`face_coverage` of the trusted
/// stations) and, when the gate could pass, plumb and flatness fitted on the
/// full-resolution points of the stations that saw it (`files`: station,
/// pose, file). The fit is kept for drawing the evidence.
pub fn look(target_id: &str, f: &Face, coverage: &Value, files: &[(u16, Pose, PathBuf)], opts: &Opts) -> anyhow::Result<(Check, Option<FaceFit>)> {
    let mut stations: Vec<u16> = coverage["stations"].as_array().map(|a| a.iter().filter_map(|s| s["station"].as_u64().map(|n| n as u16)).collect()).unwrap_or_default();
    stations.sort_unstable();
    let mut c = Check { seen_share: coverage["seen_share"].as_f64().unwrap_or(0.0), length_m: coverage["length_m"].as_f64().unwrap_or(0.0), stations, fit: None };
    let z = (coverage["height_m"][0].as_f64(), coverage["height_m"][1].as_f64());
    let (Some(lo), Some(hi)) = z else { return Ok((c, None)) };
    if c.seen_share < FACE_COVERED || c.length_m < MIN_LENGTH_M || hi <= lo {
        return Ok((c, None));
    }
    let mine: Vec<(u16, Pose, PathBuf)> = files.iter().filter(|(n, _, _)| c.stations.contains(n)).cloned().collect();
    let fit = measure_faces(&[face_wall(target_id, f, [lo, hi], c.stations.clone())], &mine, opts)?.pop();
    c.fit = fit.as_ref().and_then(|ff| {
        let r = &ff.result;
        Some(Reading {
            out_of_plumb_mm: r.out_of_plumb_mm?,
            lean_mm_per_m: r.lean_mm_per_m?,
            flat_rms_mm: r.flat_rms_mm?,
            flat_p95_mm: r.flat_p95_mm,
            straightedge_ok_pct: r.straightedge_ok_pct,
            worst_strip_mm_per_m: r.worst_strip_mm_per_m,
            over_m: r.plumb_over_m,
            stations: c.stations.clone(),
        })
    });
    Ok((c, fit))
}

/// The evidence image for a verdict: the face's heat map, its title carrying
/// numbers only for an answer.
pub fn evidence(fit: &mut FaceFit, q: &Question, opts: &Opts) -> Vec<u8> {
    mark(&mut fit.result, q);
    fit.png(opts)
}

/// The verdict on the wall it draws: anything but an answer loses its numbers.
pub(crate) fn mark(r: &mut crate::analyze::WallResult, q: &Question) {
    let v = q.verdict.map_or("not_yet", |v| match v {
        Verdict::Answer => "answer",
        Verdict::NotYet => "not_yet",
        Verdict::DoesNotFit => "does_not_fit",
    });
    withhold(r, v, (v != "answer").then(|| q.reason.clone()));
}

/// What `ask` keeps between counts: the last verdict and the reading waiting
/// for its second pass.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Kept {
    pub question: Option<Question>,
    pub settling: Option<Reading>,
}

const USAGE: &str = "usage: x9-bridge ask [--state STATE.json | --reference] [--png OUT.png] [--id ID] REGISTER_DIR x0,y0,x1,y1\n\
  REGISTER_DIR is a `x9-bridge register` output (merged.bin, stations.json). Asks whether the face\n\
  x0,y0 to x1,y1 (project frame, m) is plumb, as the bridge does on site; --state carries the last\n\
  verdict and the settling reading from one count of stations to the next. --reference judges this\n\
  registration alone (the gate and the fit, no settling) and prints the reading whenever it passes.";

pub fn cli(args: &[String]) -> anyhow::Result<()> {
    let (mut state, mut png, mut id, mut rest, mut reference) = (None, None, "face".to_string(), Vec::new(), false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--state" => state = Some(it.next().context(USAGE)?.clone()),
            "--png" => png = Some(it.next().context(USAGE)?.clone()),
            "--id" => id = it.next().context(USAGE)?.clone(),
            "--reference" => reference = true,
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            _ => rest.push(a.clone()),
        }
    }
    let [reg, face] = &rest[..] else { anyhow::bail!(USAGE) };
    let v: Vec<f64> = face.split(',').map(str::parse).collect::<Result<_, _>>().context(USAGE)?;
    anyhow::ensure!(v.len() == 4, USAGE);
    let face = Face { from: [v[0], v[1]], to: [v[2], v[3]] };
    let t0 = std::time::Instant::now();
    let listed: Vec<Value> = serde_json::from_slice(&std::fs::read(format!("{reg}/stations.json")).with_context(|| format!("{reg}/stations.json: run `x9-bridge register` again"))?)?;
    let merged = Merged::from_bytes(&std::fs::read(format!("{reg}/merged.bin"))?).map_err(anyhow::Error::msg)?;
    let mut scans = std::collections::HashMap::new();
    let mut files = Vec::new();
    for s in listed.iter().filter(|s| s["trusted"] == true) {
        let n = s["station"].as_u64().context("stations.json: station")? as u16;
        let pose: Pose = serde_json::from_value(s["pose"].clone())?;
        scans.insert(n, n.to_string());
        files.push((n, pose, PathBuf::from(s["file"].as_str().context("stations.json: file")?)));
    }
    let kept: Kept = match &state {
        Some(p) if std::path::Path::new(p).exists() => serde_json::from_slice(&std::fs::read(p)?)?,
        _ => Kept::default(),
    };
    let coverage = face_coverage(&merged, &face, &scans);
    anyhow::ensure!(coverage["error"].is_null(), "{}", coverage["error"]);
    let (c, fit) = look(&id, &face, &coverage, &files, &Opts::default())?;
    if reference {
        let q = judged(&gate(c.seen_share, c.length_m, c.fit.as_ref().map(|r| r.flat_rms_mm)), KINDS[0]);
        let reading = if q.verdict == Some(Verdict::Answer) { c.fit.clone() } else { None };
        println!("{}", json!({"target_id": id, "verdict": q.verdict, "reason": q.reason, "seen_share": c.seen_share, "stations": c.stations, "reading": reading}));
        return Ok(());
    }
    let (mut q, settling) = decide(kept.question.as_ref(), kept.settling.as_ref(), &c);
    q.asked = true;
    q.checked_at = crate::util::now_rfc3339();
    if let (Some(p), Some(mut fit)) = (&png, fit) {
        std::fs::write(p, evidence(&mut fit, &q, &Opts::default()))?;
    }
    let mut out = public(&id, &q, None);
    let walls: Vec<crate::register::Wall> = std::fs::read(format!("{reg}/walls.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| serde_json::from_value(if v["walls"].is_array() { v["walls"].clone() } else { v }).ok()).unwrap_or_default();
    out["face"] = face_said(&face, c.length_m, &walls);
    out["seconds"] = json!((t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    out["unseen"] = coverage["unseen"].clone();
    println!("{}", serde_json::to_string(&out)?);
    if let Some(p) = &state {
        std::fs::write(p, serde_json::to_vec_pretty(&Kept { question: Some(q), settling })?)?;
    }
    Ok(())
}

// ------------------------------------------------------------- on the bridge ---

/// Ask (`by` a caller) or re-check (`by` "bridge") one question target on the
/// project's registration: its fixed face's coverage, a fit from the files of
/// the trusted stations that saw it, the verdict recorded in the mission and
/// logged. Returns what the caller may see.
pub async fn on_bridge(st: &std::sync::Arc<AppState>, project_id: &str, target_id: &str, by: &str) -> Result<Value, ApiError> {
    let t0 = std::time::Instant::now();
    let key = (project_id.to_string(), target_id.to_string());
    // One look at a target at a time: an ask and the re-check after a station
    // would otherwise each write over the other's verdict.
    let lock = st.inner.lock().unwrap().asking.entry(key.clone()).or_default().clone();
    let _one = lock.lock().await;
    let (face, name, prev, settling, reg, tolerance) = {
        let inner = st.inner.lock().unwrap();
        let m = inner.missions.get(project_id).ok_or_else(|| ApiError::not_found("mission"))?;
        let t = m.targets.iter().find(|t| t.target_id == target_id).ok_or_else(|| ApiError::not_found("target"))?;
        let q = t.question.clone().ok_or_else(|| ApiError::invalid(format!("target {target_id} asks no question: set its question to \"plumb\" first")))?;
        let face = t.face.ok_or_else(|| ApiError::invalid(format!("target {target_id} has no wall face")))?;
        let settling = settling_for(&q, &face, inner.settling.get(&key)).cloned();
        (face, t.name.clone(), q, settling, inner.registrations.get(project_id).cloned(), t.tolerance.clone())
    };
    let geo = st.geo(project_id);
    let scans = reg.as_ref().map(|r| r.station_scans()).unwrap_or_default();
    let coverage = geo.judge(&face).unwrap_or_else(|| json!({"seen_share": 0.0, "length_m": face_wall(target_id, &face, [0.0, 0.0], vec![]).length_m, "stations": [], "unseen": []}));
    if let Some(e) = coverage["error"].as_str() {
        return Err(ApiError::invalid(format!("target {target_id}: {e}")));
    }
    // Only the files of the stations that saw it, and only if a fit could pass.
    let saw: Vec<u16> = coverage["stations"].as_array().map(|a| a.iter().filter_map(|s| s["station"].as_u64().map(|n| n as u16)).collect()).unwrap_or_default();
    let could = coverage["seen_share"].as_f64().unwrap_or(0.0) >= FACE_COVERED && coverage["length_m"].as_f64().unwrap_or(0.0) >= MIN_LENGTH_M;
    let mut files = Vec::new();
    if could {
        for s in reg.iter().flat_map(|r| r.stations.iter()).filter(|s| saw.contains(&s.station) && scans.contains_key(&s.station)) {
            let Some(pose) = s.pose else { continue };
            files.push((s.station, pose, crate::files::scan_file(st, &s.scan_id, crate::files::FileKind::Tzf).await?));
        }
    }
    let (tid, cov) = (target_id.to_string(), coverage.clone());
    let (check, fit) = tokio::task::spawn_blocking(move || look(&tid, &face, &cov, &files, &Opts::default()))
        .await
        .map_err(|e| ApiError::cache(format!("asking: {e}")))?
        .map_err(|e| ApiError::cache(format!("asking: {e:#}")))?;
    let (mut q, keep) = decide(Some(&prev), settling.as_ref(), &check);
    q.asked = prev.asked || by != "bridge";
    let png = fit.filter(|_| shows_evidence(q.verdict)).map(|mut f| evidence(&mut f, &q, &Opts::default()));
    let changed = {
        let mut inner = st.inner.lock().unwrap();
        // Changed while it was being looked at: the verdict is for another question.
        let same = inner.missions.get(project_id).and_then(|m| m.targets.iter().find(|t| t.target_id == target_id)).is_some_and(|t| t.face == Some(face) && t.question.as_ref().is_some_and(|tq| tq.kind == q.kind));
        if !same {
            return Err(ApiError::conflict(format!("target {target_id} changed while it was being asked; ask again")));
        }
        match keep {
            Some(r) => inner.settling.insert(key.clone(), (face, r)),
            None => inner.settling.remove(&key),
        };
        match &png {
            Some(p) => inner.evidence.insert(key.clone(), p.clone()),
            None => inner.evidence.remove(&key),
        };
        inner.missions.get_mut(project_id).map(|m| (m.record_question(target_id, q.clone(), &scans), m.clone()))
    };
    let secs = t0.elapsed().as_secs_f64();
    let mut out = public(target_id, &q, tolerance.as_ref());
    out["face"] = face_said(&face, check.length_m, &geo.walls);
    out["seconds"] = json!((secs * 10.0).round() / 10.0);
    if q.verdict == Some(Verdict::NotYet) && q.reason != "settling" {
        out["unseen"] = unseen_with_subjects(st, reg.as_ref(), &coverage).await;
    }
    if png.is_some() {
        out["evidence_url"] = json!(format!("/v1/projects/{project_id}/mission/targets/{target_id}/ask/image"));
    }
    let stations = q.stations.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    let said = match (q.verdict, &q.answer) {
        (Some(Verdict::Answer), Some(a)) => format!("answer: {:+.1} mm out of plumb, {:.1} mm RMS flat", a.out_of_plumb_mm, a.flat_rms_mm),
        (Some(Verdict::DoesNotFit), _) => format!("does not fit: {}", q.reason),
        _ => format!("not yet: {}", q.reason),
    };
    let what = if by == "bridge" { "Re-checked" } else { "Asked" };
    st.log("note", format!("{what} {name} ({}): {said}; stations {}; {secs:.1} s", q.kind, if stations.is_empty() { "none".into() } else { stations }), by, Some(project_id), &[("target_id", target_id)]);
    if let Some((line, m)) = changed {
        st.save_missions();
        st.emit("mission", json!(m));
        if let Some(l) = line {
            st.log("note", format!("Ledger from the question: {l}"), "bridge", Some(project_id), &[("mission_id", &m.mission_id)]);
        }
    }
    Ok(out)
}

/// The unseen stretches, each with the preview columns that frame it from the
/// latest trusted station (`subject` for the planner).
async fn unseen_with_subjects(st: &std::sync::Arc<AppState>, reg: Option<&crate::register::Registration>, coverage: &Value) -> Value {
    let mut gaps = coverage["unseen"].as_array().cloned().unwrap_or_default();
    let scans = reg.map(|r| r.station_scans()).unwrap_or_default();
    let latest = reg.and_then(|r| r.stations.iter().filter(|s| s.pose.is_some() && scans.contains_key(&s.station)).max_by_key(|s| s.station).cloned());
    let Some(s) = latest else { return json!(gaps) };
    let (Ok(tzf), Ok(jpg)) = (
        crate::files::scan_file(st, &s.scan_id, crate::files::FileKind::Tzf).await,
        crate::files::scan_file(st, &s.scan_id, crate::files::FileKind::Preview).await,
    ) else {
        return json!(gaps);
    };
    let pw = tokio::fs::read(jpg).await.ok().and_then(|j| crate::aim::jpeg_size(&j).ok()).map_or(0, |p| p.0);
    let az = tokio::task::spawn_blocking(move || column_azimuths(&tzf)).await.ok().flatten();
    let (Some(az), Some(pose)) = (az, s.pose) else { return json!(gaps) };
    for g in gaps.iter_mut() {
        let p = |k: &str| Some([g[k][0].as_f64()?, g[k][1].as_f64()?]);
        if let (Some(a), Some(b)) = (p("from"), p("to")) {
            if let Some((x0, x1)) = preview_span(&az, pw, &pose, a, b) {
                g["subject"] = json!({"scan_id": s.scan_id, "station": s.station, "x0": x0, "x1": x1, "query": format!("{x0},{x1}")});
            }
        }
    }
    json!(gaps)
}

/// A scan's azimuth (rad) by column, NaN where it saw nothing.
fn column_azimuths(tzf: &std::path::Path) -> Option<Vec<f64>> {
    let t = trimble_tzf::Tzf::open(tzf).ok()?;
    let meta = t.scan_meta().ok()?;
    let mut az = vec![f64::NAN; meta.grid_cols as usize];
    for g in &t.grid_points().ok()? {
        let c = g.col as usize;
        if c < az.len() && az[c].is_nan() && (g.point.x != 0.0 || g.point.y != 0.0) {
            az[c] = (g.point.y as f64).atan2(g.point.x as f64).rem_euclid(std::f64::consts::TAU);
        }
    }
    Some(az)
}

/// After a registration: every question target judged again, whether or not
/// it was asked. Returns how long it took (s).
pub async fn recheck(st: &std::sync::Arc<AppState>, project_id: &str) -> f64 {
    let t0 = std::time::Instant::now();
    let ids: Vec<String> = st.inner.lock().unwrap().missions.get(project_id).map(|m| m.targets.iter().filter(|t| t.question.is_some() && t.face.is_some()).map(|t| t.target_id.clone()).collect()).unwrap_or_default();
    if ids.is_empty() {
        return 0.0;
    }
    for id in &ids {
        if let Err(e) = on_bridge(st, project_id, id, "bridge").await {
            st.log("check", format!("Could not re-check target {id}: {}", e.detail.as_deref().unwrap_or(&e.title)), "bridge", Some(project_id), &[("target_id", id)]);
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    st.log("note", format!("Re-checked {} question{} after the registration in {secs:.1} s", ids.len(), if ids.len() == 1 { "" } else { "s" }), "bridge", Some(project_id), &[]);
    secs
}

/// A face as `analyze` measures walls: the wall it stands for.
pub fn face_wall(id: &str, f: &Face, z_m: [f64; 2], seen_by: Vec<u16>) -> crate::register::Wall {
    let length_m = (f.to[0] - f.from[0]).hypot(f.to[1] - f.from[1]);
    crate::register::Wall { wall_id: id.into(), from: f.from, to: f.to, length_m, z_m, seen_by, kind: "building".into(), flatness: 0.0, height_m: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(plumb: f64, stations: &[u16]) -> Reading {
        Reading { out_of_plumb_mm: plumb, lean_mm_per_m: plumb / 10.0, flat_rms_mm: 8.0, flat_p95_mm: Some(16.0), straightedge_ok_pct: Some(90.0),
                  worst_strip_mm_per_m: Some(1.0), over_m: Some(5.0), stations: stations.to_vec() }
    }
    fn tms(words: &str) -> Tolerance {
        let step = |up_to_m: Option<f64>, mm: f64| Step { up_to_m, mm };
        Tolerance { words: words.into(), steps: vec![step(Some(3.05), 6.4), step(Some(6.10), 9.5), step(None, 12.7)], mm_per_m: None, max_mm: None }
    }
    fn aci() -> Tolerance {
        Tolerance { words: "0.3% of the height, 1 in at most".into(), steps: vec![], mm_per_m: Some(3.0), max_mm: Some(25.4) }
    }

    #[test]
    fn a_tolerance_is_the_allowance_for_the_height_measured() {
        let t = tms("¼ in in 10 ft, ⅜ in in 20 ft, ½ in max");
        assert_eq!([2.9, 5.0, 8.0].map(|h| t.allowed_mm(h)), [Some(6.4), Some(9.5), Some(12.7)]);
        assert_eq!(t.allowed_mm(3.05), Some(6.4));
        assert_eq!([5.0, 10.0].map(|h| aci().allowed_mm(h)), [Some(15.0), Some(25.4)]);
        // A cap applies to steps too; a face taller than every step has none.
        let capped = Tolerance { max_mm: Some(8.0), ..t.clone() };
        assert_eq!(capped.allowed_mm(8.0), Some(8.0));
        let short = Tolerance { steps: vec![Step { up_to_m: Some(3.05), mm: 6.4 }], ..t.clone() };
        assert_eq!(short.allowed_mm(5.0), None);
        assert_eq!(Tolerance { max_mm: Some(12.7), ..short }.allowed_mm(5.0), Some(12.7));
        assert!(t.check().is_ok() && aci().check().is_ok());
    }

    #[test]
    fn a_tolerance_needs_words_and_one_form() {
        let t = tms("¼ in in 10 ft");
        let refused = |x: Tolerance, why: &str| {
            let e = x.check().unwrap_err();
            assert!(e.contains(why), "{e}");
        };
        refused(Tolerance { words: "  ".into(), ..t.clone() }, "words");
        refused(Tolerance { words: "x".repeat(201), ..t.clone() }, "words");
        assert!(Tolerance { words: "x".repeat(200), ..t.clone() }.check().is_ok());
        refused(Tolerance { mm_per_m: Some(3.0), ..t.clone() }, "one of");
        refused(Tolerance { steps: vec![], ..t.clone() }, "one of");
        refused(Tolerance { steps: vec![Step { up_to_m: Some(6.1), mm: 9.5 }, Step { up_to_m: Some(3.05), mm: 6.4 }], ..t.clone() }, "order");
        refused(Tolerance { steps: vec![Step { up_to_m: None, mm: 9.5 }, Step { up_to_m: Some(3.05), mm: 6.4 }], ..t.clone() }, "last");
        refused(Tolerance { steps: vec![Step { up_to_m: Some(3.05), mm: 0.0 }], ..t.clone() }, "positive");
        refused(Tolerance { max_mm: Some(-1.0), ..t.clone() }, "positive");
        refused(Tolerance { mm_per_m: Some(f64::NAN), ..aci() }, "positive");
        // A misspelt field is refused, not dropped: a lost cap would pass a wall it shouldn't.
        assert!(serde_json::from_value::<Tolerance>(json!({"words": "x", "mm_per_m": 3, "max": 25.4})).is_err());
        assert!(serde_json::from_value::<Tolerance>(json!({"words": "x", "steps": [{"upto_m": 3.05, "mm": 6.4}]})).is_err());
        assert!(serde_json::from_value::<Tolerance>(json!({"words": "x", "mm_per_m": 3, "max_mm": 25.4})).is_ok());
    }

    #[test]
    fn an_answer_says_within_or_outside_its_tolerance_in_mm_and_inches() {
        let (settling, _) = decide(None, None, &check(0.9, 8.0, 8.0, 9.5, &[1]));
        let (q, _) = decide(Some(&settling), Some(&reading(9.5, &[1])), &check(0.9, 8.0, 8.0, 9.5, &[1, 2]));
        let t = tms("¼ in in 10 ft, ⅜ in in 20 ft, ½ in max");
        let v = public("t1", &q, Some(&t));
        assert_eq!(v["answer"]["allowed_mm"], 9.5);
        assert_eq!(v["answer"]["within"], true);
        assert_eq!(v["answer"]["out_of_plumb_in"], "3/8 in");
        assert_eq!(v["answer"]["leans"], "toward the side it was scanned from");
        assert_eq!(v["tolerance"]["words"], t.words);
        let (q2, _) = decide(Some(&settling), Some(&reading(-9.6, &[1])), &check(0.9, 8.0, 8.0, -9.6, &[1, 2]));
        let v2 = public("t1", &q2, Some(&t));
        assert_eq!(v2["answer"]["within"], false);
        assert_eq!(v2["answer"]["leans"], "away from the side it was scanned from");
        // No tolerance: the figure, and no within.
        assert!(public("t1", &q, None)["answer"].get("within").is_none());
        // Not an answer: the words are echoed, nothing is judged.
        let (rough, _) = decide(None, None, &check(0.9, 8.0, 22.0, 104.0, &[1]));
        for q in [&settling, &rough] {
            let v = public("t1", q, Some(&t));
            assert!(!v.to_string().contains("within") && !v.to_string().contains("allowed"), "{v}");
            assert_eq!(v["tolerance"]["words"], t.words);
        }
        assert_eq!([0.0, 1.6, 6.4, 12.7, 25.4, 30.0].map(inches), ["0 in", "1/16 in", "1/4 in", "1/2 in", "1 in", "1 3/16 in"]);
        // Dead plumb leans no way.
        assert!(reading_said(&reading(0.0, &[1]), None).get("leans").is_none());
        // A tolerance with no allowance for this height says so, rather than nothing.
        let short = Tolerance { steps: vec![Step { up_to_m: Some(3.05), mm: 6.4 }], ..t.clone() };
        let v3 = reading_said(&reading(4.0, &[1]), Some(&short));
        assert!(v3["allowed_mm"].is_null() && v3.get("within").is_none() && v3["tolerance_note"].as_str().unwrap().contains("5.0 m"), "{v3}");
        let unmeasured = Reading { over_m: None, ..reading(4.0, &[1]) };
        assert!(reading_said(&unmeasured, Some(&t))["tolerance_note"].as_str().unwrap().contains("ask again"));
    }

    #[test]
    fn a_detected_wall_that_runs_on_past_the_face() {
        let face = Face { from: [0.0, 0.0], to: [10.0, 0.0] };
        let wall = |a: [f64; 2], b: [f64; 2]| face_wall("w", &Face { from: a, to: b }, [0.0, 3.0], vec![1]);
        assert_eq!(runs_on(&face, &[wall([0.0, 0.1], [13.0, 0.1])]), Some([0.0, 3.0]));
        assert_eq!(runs_on(&face, &[wall([-2.0, 0.0], [4.0, 0.0]), wall([5.0, 0.0], [12.5, 0.0])]), Some([2.0, 2.5]));
        // Either end, not the two added up.
        assert_eq!(runs_on(&face, &[wall([-0.6, 0.0], [10.6, 0.0])]), None);
        // A short wall 12° across, its ends within 0.3 m of the line: not on it.
        let (s12, c12) = 12f64.to_radians().sin_cos();
        assert_eq!(runs_on(&face, &[wall([10.0, -0.12], [10.0 + 1.2 * c12, -0.12 + 1.2 * s12])]), None);
        assert_eq!(runs_on(&face, &[wall([0.0, 0.5], [13.0, 0.5])]), None);
        let (s, c) = 20f64.to_radians().sin_cos();
        assert_eq!(runs_on(&face, &[wall([0.0, 0.0], [13.0 * c, 13.0 * s])]), None);
        assert_eq!(runs_on(&face, &[wall([0.0, 0.0], [10.8, 0.0])]), None);
        assert_eq!(runs_on(&face, &[wall([20.0, 0.0], [30.0, 0.0])]), None);
        // Exactly 1 m past an end is not running on; one end alone is enough.
        assert_eq!(runs_on(&face, &[wall([0.0, 0.0], [11.0, 0.0])]), None);
        assert_eq!(runs_on(&face, &[wall([-1.0, 0.0], [10.0, 0.0])]), None);
        assert_eq!(runs_on(&face, &[wall([-3.0, 0.0], [10.0, 0.0])]), Some([3.0, 0.0]));
        // A wall that starts within 1 m of an end counts; one ending short of it, too.
        assert_eq!(runs_on(&face, &[wall([10.5, 0.0], [13.0, 0.0])]), Some([0.0, 3.0]));
        assert_eq!(runs_on(&face, &[wall([-4.0, 0.0], [-0.5, 0.0])]), Some([4.0, 0.0]));
        // A short piece 12° across past the end, its ends within 0.3 m of the line.
        assert_eq!(runs_on(&face, &[wall([10.8, 0.0], [10.8 + 0.49 * c12, 0.49 * s12])]), None);
        // The same cases anywhere in the frame, the face turned 30° and moved.
        let (s30, c30) = 30f64.to_radians().sin_cos();
        let at = |p: [f64; 2]| [5.0 + p[0] * c30 - p[1] * s30, -3.0 + p[0] * s30 + p[1] * c30];
        let moved = Face { from: at(face.from), to: at(face.to) };
        let mwall = |a: [f64; 2], b: [f64; 2]| wall(at(a), at(b));
        assert_eq!(runs_on(&moved, &[mwall([0.0, 0.1], [13.0, 0.1])]), Some([0.0, 3.0]));
        assert_eq!(runs_on(&moved, &[mwall([-2.0, 0.0], [4.0, 0.0]), mwall([5.0, 0.0], [12.5, 0.0])]), Some([2.0, 2.5]));
        assert_eq!(runs_on(&moved, &[mwall([0.0, 0.5], [13.0, 0.5])]), None);
        assert_eq!(runs_on(&moved, &[mwall([0.0, -0.5], [13.0, -0.5])]), None);
        assert_eq!(runs_on(&moved, &[mwall([10.0, -0.12], [10.0 + 1.2 * c12, -0.12 + 1.2 * s12])]), None);
        assert_eq!(runs_on(&moved, &[mwall([-3.0, 0.0], [10.0, 0.0])]), Some([3.0, 0.0]));
        assert_eq!(runs_on(&Face { from: [2.0, 2.0], to: [2.0, 2.0] }, &[mwall([0.0, 0.0], [13.0, 0.0])]), None);
        // As the response gives it.
        let said = face_said(&moved, 10.0, &[mwall([0.0, 0.1], [13.0, 0.1])]);
        assert_eq!(said["length_m"], 10.0);
        assert_eq!(said["runs_on"], json!({"past_from_m": 0.0, "past_to_m": 3.0}));
        assert_eq!(said["from"], json!(moved.from));
        assert!(face_said(&moved, 10.0, &[])["runs_on"].is_null());
    }
    fn check(seen: f64, len: f64, rms: f64, plumb: f64, stations: &[u16]) -> Check {
        let mut r = reading(plumb, stations);
        r.flat_rms_mm = rms;
        Check { seen_share: seen, length_m: len, stations: stations.to_vec(), fit: Some(r) }
    }
    fn verdict(q: &Question) -> (Verdict, &str) {
        (q.verdict.unwrap(), q.reason.as_str())
    }

    #[test]
    fn under_80_percent_seen_is_not_yet_whatever_else() {
        assert!(matches!(gate(0.79, 10.0, Some(8.0)), Gate::NotYet(_)));
        // Rough and short too: still the data that is missing first.
        assert!(matches!(gate(0.79, 3.0, Some(25.0)), Gate::NotYet(_)));
        let (q, keep) = decide(None, None, &check(0.79, 3.0, 25.0, 40.0, &[1]));
        assert_eq!(q.verdict, Some(Verdict::NotYet));
        assert!(keep.is_none() && q.answer.is_none());
    }

    #[test]
    fn short_or_rough_faces_do_not_fit() {
        assert!(matches!(gate(0.80, 3.9, Some(8.0)), Gate::DoesNotFit(r) if r.contains("4 m")));
        assert!(matches!(gate(0.80, 10.0, Some(15.1)), Gate::DoesNotFit(r) if r.contains("15 mm")));
        // No points to fit is data that is missing, not a surface that doesn't fit.
        assert!(matches!(gate(0.95, 10.0, None), Gate::NotYet(_)));
        assert_eq!(gate(0.80, 4.0, Some(15.0)), Gate::Pass);
    }

    #[test]
    fn an_answer_needs_a_second_pass_after_a_station_that_saw_the_face() {
        // First pass: settling, and the reading is kept, not published.
        let (q1, keep) = decide(None, None, &check(0.80, 4.0, 15.0, 10.0, &[1, 2]));
        assert_eq!(verdict(&q1), (Verdict::NotYet, "settling"));
        assert!(q1.answer.is_none());
        assert_eq!(keep.as_ref().unwrap().out_of_plumb_mm, 10.0);
        // A station that didn't see the face: still settling, whatever the plumb.
        let (q2, keep2) = decide(Some(&q1), keep.as_ref(), &check(0.80, 4.0, 15.0, 30.0, &[1, 2]));
        assert_eq!(verdict(&q2), (Verdict::NotYet, "settling"));
        assert_eq!(keep2, keep);
        // A new station that saw it, plumb moved 2.1 mm: settling again, on the new reading.
        let (q3, keep3) = decide(Some(&q2), keep2.as_ref(), &check(0.85, 4.0, 15.0, 12.1, &[1, 2, 3]));
        assert_eq!(verdict(&q3), (Verdict::NotYet, "settling"));
        assert_eq!(keep3.as_ref().unwrap().out_of_plumb_mm, 12.1);
        // Another new station, plumb moved 2.0 mm: the answer.
        let (q4, keep4) = decide(Some(&q3), keep3.as_ref(), &check(0.85, 4.0, 15.0, 14.1, &[1, 2, 3, 4]));
        assert_eq!(q4.verdict, Some(Verdict::Answer));
        assert_eq!(q4.answer.as_ref().unwrap().out_of_plumb_mm, 14.1);
        assert!(keep4.is_none());
        // It stays the answer while no station adds to it.
        let (q5, _) = decide(Some(&q4), None, &check(0.85, 4.0, 15.0, 30.0, &[1, 2, 3, 4]));
        assert_eq!(q5.answer, q4.answer);
    }

    #[test]
    fn an_answer_that_new_data_moves_goes_back_to_settling() {
        let (q, keep) = decide(None, None, &check(0.9, 8.0, 8.0, 5.0, &[1]));
        assert_eq!(q.verdict, Some(Verdict::NotYet));
        let (q, _) = decide(Some(&q), keep.as_ref(), &check(0.9, 8.0, 8.0, 6.0, &[1, 2]));
        assert_eq!(q.verdict, Some(Verdict::Answer));
        let (q, keep) = decide(Some(&q), None, &check(0.9, 8.0, 8.0, 9.0, &[1, 2, 3]));
        assert_eq!(verdict(&q), (Verdict::NotYet, "settling"));
        assert!(q.answer.is_none());
        assert_eq!(keep.unwrap().out_of_plumb_mm, 9.0);
        // And a face that drops below 80% loses its answer.
        let (q, _) = decide(None, None, &check(0.9, 8.0, 8.0, 5.0, &[1]));
        let (q, _) = decide(Some(&q), Some(&reading(5.0, &[1])), &check(0.9, 8.0, 8.0, 5.5, &[1, 2]));
        let (q, keep) = decide(Some(&q), None, &check(0.5, 8.0, 8.0, 5.5, &[1, 2, 3]));
        assert_eq!(q.verdict, Some(Verdict::NotYet));
        assert!(q.answer.is_none() && keep.is_none());
    }

    /// Criterion 2: only an answer carries plumb or flatness.
    #[test]
    fn only_an_answer_shows_numbers() {
        let numeric = ["out_of_plumb_mm", "lean_mm_per_m", "flat_rms_mm", "flat_p95_mm", "straightedge_ok_pct", "worst_strip_mm_per_m", "answer"];
        let has_any = |v: &Value| {
            let s = v.to_string();
            numeric.iter().any(|k| s.contains(k))
        };
        let (settling, _) = decide(None, None, &check(0.9, 8.0, 8.0, 5.0, &[1]));
        let (not_yet, _) = decide(None, None, &check(0.5, 8.0, 8.0, 5.0, &[1]));
        let (rough, _) = decide(None, None, &check(0.9, 8.0, 22.0, 104.0, &[1]));
        for q in [&settling, &not_yet, &rough] {
            let v = public("t1", q, None);
            assert!(!has_any(&v), "{v}");
            assert!(!v.to_string().contains("104"), "{v}");
            assert_eq!(v["target_id"], "t1");
            assert!(v["verdict"].is_string() && v["reason"].is_string(), "{v}");
        }
        assert_eq!(public("t1", &rough, None)["verdict"], "does_not_fit");
        let (q, _) = decide(Some(&settling), Some(&reading(5.0, &[1])), &check(0.9, 8.0, 8.0, 5.5, &[1, 2]));
        let v = public("t1", &q, None);
        assert_eq!(v["verdict"], "answer");
        assert_eq!(v["answer"]["out_of_plumb_mm"], 5.5);
        assert_eq!(v["answer"]["stations"], json!([1, 2]));
    }

    /// Criterion 5: the stretch as preview columns, the short way round, and
    /// no wider than its ends plus 15° each side.
    #[test]
    fn an_unseen_stretch_becomes_a_preview_span() {
        let tau = std::f64::consts::TAU;
        let (cols, w) = (720usize, 360usize);
        let up: Vec<f64> = (0..cols).map(|c| c as f64 / cols as f64 * tau).collect();
        let down: Vec<f64> = (0..cols).map(|c| (tau - c as f64 / cols as f64 * tau).rem_euclid(tau)).collect();
        let still = Pose { turn_deg: 0.0, xyz_m: [0.0; 3] };
        // A station at (3, 2), turned 90°: the same bearings as `still` sees
        // from the origin, once the stretch is moved and turned with it.
        let moved = Pose { turn_deg: 90.0, xyz_m: [3.0, 2.0, 0.0] };
        let turn = |p: [f64; 2]| [3.0 - p[1], p[0] + 2.0];
        let px = |az: &[f64], x: f64, y: f64| -> f64 {
            let a = y.atan2(x).rem_euclid(tau);
            let c = (0..cols).min_by(|&i, &j| {
                let d = |k: usize| ((az[k] - a + tau / 2.0).rem_euclid(tau) - tau / 2.0).abs();
                d(i).total_cmp(&d(j))
            });
            c.unwrap() as f64 * w as f64 / cols as f64
        };
        for az in [&up, &down] {
            for (a, b) in [([10.0, 1.0], [10.0, 5.0]), ([10.0, 5.0], [10.0, 1.0]), ([10.0, -2.0], [10.0, 2.0]), ([-4.0, 3.0], [-4.0, -3.0])] {
                for (pose, a2, b2) in [(&still, a, b), (&moved, turn(a), turn(b))] {
                    let (x0, x1) = preview_span(az, w, pose, a2, b2).unwrap();
                    let width = (x1 + w - x0) % w;
                    assert!(width <= w / 2, "{a:?}-{b:?}: ({x0}, {x1}) runs the long way");
                    let inside = |x: f64| {
                        let d = (x - x0 as f64).rem_euclid(w as f64);
                        d <= width as f64 + 1.0 || d >= w as f64 - 1.0
                    };
                    for p in [a, b] {
                        let e = px(az, p[0], p[1]);
                        assert!(inside(e), "{a:?}-{b:?}: end {p:?} at {e} outside ({x0}, {x1})");
                    }
                    let ends = {
                        let (ea, eb) = (a[1].atan2(a[0]), b[1].atan2(b[0]));
                        ((ea - eb + tau / 2.0).rem_euclid(tau) - tau / 2.0).abs()
                    };
                    let max = (ends + 2.0 * 15f64.to_radians()) / tau * w as f64;
                    assert!(width as f64 <= max + 1.0, "{a:?}-{b:?}: width {width} px over {max:.1}");
                }
            }
        }
        assert!(preview_span(&[f64::NAN; 8], w, &still, [1.0, 0.0], [1.0, 1.0]).is_none());
    }

    /// The reading kept for a second pass belongs to the face and the
    /// settling it came from: a new face, or a question asked afresh, starts over.
    #[test]
    fn a_kept_reading_counts_only_for_its_face_while_settling() {
        let face = Face { from: [0.0, 0.0], to: [8.0, 0.0] };
        let moved = Face { from: [0.0, 0.0], to: [9.0, 0.0] };
        let kept = (face, reading(5.0, &[1]));
        let (settling, _) = decide(None, None, &check(0.9, 8.0, 8.0, 5.0, &[1]));
        assert_eq!(settling_for(&settling, &face, Some(&kept)), Some(&kept.1));
        assert_eq!(settling_for(&settling, &moved, Some(&kept)), None);
        assert_eq!(settling_for(&Question::new("plumb"), &face, Some(&kept)), None);
        let (not_yet, _) = decide(None, None, &check(0.5, 8.0, 8.0, 5.0, &[1]));
        assert_eq!(settling_for(&not_yet, &face, Some(&kept)), None);
        assert_eq!(settling_for(&settling, &face, None), None);
    }

    /// The heat map goes with an answer or a face that doesn't fit, never
    /// with not yet: its colours would give flatness away.
    /// Criterion 4, the images: a question not answered on site gets no
    /// picture with numbers, even when the final registration would answer it.
    #[test]
    fn the_report_draws_numbers_only_for_an_answer_given_on_site() {
        let on = |v: Verdict| Question { verdict: Some(v), reason: "why".into(), ..Question::new("plumb") };
        assert_eq!(report_drawing(&on(Verdict::NotYet), &Gate::Pass), None);
        assert_eq!(report_drawing(&on(Verdict::Answer), &Gate::Pass).unwrap().verdict, Some(Verdict::Answer));
        let lost = report_drawing(&on(Verdict::Answer), &Gate::DoesNotFit("rough".into())).unwrap();
        assert_eq!((lost.verdict, lost.reason.as_str()), (Some(Verdict::DoesNotFit), "rough"));
        assert_eq!(report_drawing(&on(Verdict::Answer), &Gate::NotYet("62%".into())), None);
        let rough = report_drawing(&on(Verdict::DoesNotFit), &Gate::Pass).unwrap();
        assert_eq!((rough.verdict, rough.reason.as_str()), (Some(Verdict::DoesNotFit), "why"));
        assert_eq!(report_drawing(&Question::new("plumb"), &Gate::Pass), None);
    }

    #[test]
    fn one_look_judged_alone() {
        let q = judged(&Gate::DoesNotFit("rough".into()), "plumb");
        assert_eq!((q.verdict, q.reason.as_str(), q.kind.as_str()), (Some(Verdict::DoesNotFit), "rough", "plumb"));
        assert_eq!(judged(&Gate::NotYet("62%".into()), "plumb").verdict, Some(Verdict::NotYet));
        let a = judged(&Gate::Pass, "plumb");
        assert_eq!((a.verdict, a.reason.as_str()), (Some(Verdict::Answer), ""));
    }

    #[test]
    fn evidence_only_for_an_answer_or_a_face_that_does_not_fit() {
        assert!(shows_evidence(Some(Verdict::Answer)) && shows_evidence(Some(Verdict::DoesNotFit)));
        assert!(!shows_evidence(Some(Verdict::NotYet)) && !shows_evidence(None));
    }

    /// Criterion 4: the report opens with the questions, in mission order.
    #[test]
    fn the_report_opens_with_the_questions() {
        let q = |v: Verdict, reason: &str, answer: Option<Reading>| Question { verdict: Some(v), reason: reason.into(), asked: true, answer, ..Question::new("plumb") };
        let asked = vec![
            Asked { target_id: "long".into(), name: "Long facade".into(), declined: None,
                    on_site: q(Verdict::Answer, "", Some(reading(4.0, &[3, 4]))), again: Gate::Pass, again_reading: Some(reading(10.0, &[3, 4, 9])),
                    image: Some("question-long.png".into()), tolerance: Some(tms("¼ in in 10 ft, ⅜ in in 20 ft, ½ in max")),
                    face: Face { from: [0.0, 0.0], to: [30.2, 0.0] }, runs_on_m: Some([1.0, 14.1]) },
            Asked { target_id: "back".into(), name: "Back wall".into(), declined: None,
                    on_site: q(Verdict::NotYet, "62% of the face is seen; 80% needed", None), again: Gate::NotYet("62% of the face is seen; 80% needed".into()),
                    again_reading: None, image: None, tolerance: Some(tms("¼ in in 10 ft")),
                    face: Face { from: [0.0, 0.0], to: [8.0, 0.0] }, runs_on_m: None },
            Asked { target_id: "porch".into(), name: "Porch".into(), declined: Some("stepped facade: piers proud of the wall".into()),
                    on_site: q(Verdict::DoesNotFit, "rough", None), again: Gate::DoesNotFit("rough".into()), again_reading: None,
                    image: Some("question-porch.png".into()), tolerance: None,
                    face: Face { from: [0.0, 0.0], to: [8.0, 0.0] }, runs_on_m: None },
        ];
        let h = questions_html(&asked);
        let at = |s: &str| h.find(s).unwrap_or_else(|| panic!("{s} missing from {h}"));
        assert!(at("Long facade") < at("Back wall") && at("Back wall") < at("Porch"));
        // Each has its verdict and its evidence.
        assert!(at("Long facade") < at(">answer<") && at(">answer<") < at("Back wall"));
        assert!(at("Back wall") < at(">not yet<") && at(">not yet<") < at("Porch"));
        assert!(at("Porch") < at(">does not fit<"));
        assert!(h.contains("question-long.png") && h.contains("question-porch.png"));
        assert!(h.contains("declined: stepped facade"));
        // Only the answer has a number; it moved 6 mm on the final registration.
        let entries: Vec<&str> = h.split("<section class=question").skip(1).collect();
        assert_eq!(entries.len(), 3, "{h}");
        assert!(entries[0].contains("4.0 mm") && entries[0].contains("moved 6.0 mm"), "{}", entries[0]);
        for e in &entries[1..] {
            assert!(!e.contains(" mm out of plumb") && !e.contains("moved"), "{e}");
        }
        // The tolerance in the person's words, the allowance and the call; the
        // part of the wall the answer covers.
        assert!(entries[0].contains("¼ in in 10 ft, ⅜ in in 20 ft, ½ in max") && entries[0].contains("allowed 9.5 mm") && entries[0].contains("within"), "{}", entries[0]);
        assert!(entries[0].contains("30.2 m") && entries[0].contains("runs on 14.1 m past (30.2, 0.0)") && !entries[0].contains("past (0.0, 0.0)"), "{}", entries[0]);
        // 4.0 mm on site was within 9.5; the final 10.0 mm is outside: the report says so.
        assert!(entries[0].contains("<strong>outside</strong> the 9.5 mm allowed"), "{}", entries[0]);
        assert!(!entries[1].contains("within") && !entries[1].contains("allowed"), "{}", entries[1]);
        // Within 5 mm: no flag.
        let mut calm = asked[0].clone();
        calm.again_reading = Some(reading(9.0, &[3, 4, 9]));
        assert!(!questions_html(&[calm]).contains("moved"));
        // An answer the final registration no longer supports says so.
        let mut lost = asked[0].clone();
        lost.again = Gate::DoesNotFit("rough".into());
        lost.again_reading = None;
        assert!(questions_html(&[lost]).contains("no longer"));
    }
}
