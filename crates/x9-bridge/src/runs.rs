//! Runs: execute a recipe's steps in order, under the role of whoever started
//! the run (so approvals and grants apply exactly as for a direct request).

use crate::export::ExportRequest;
use crate::model::{Caller, JobState, ScanRequest};
use crate::problem::ApiError;
use crate::recipe::{lookup, resolve, Recipe, RecipeStep};
use crate::routes::{check_note, compute_aim, submit_export, submit_measure, submit_scan, AimReq, MeasureRequest};
use crate::state::AppState;
use crate::util::{new_id, now_rfc3339};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize)]
pub struct StepState {
    pub step_id: String,
    pub state: String,
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// A person step's input as proposed by someone else (usually the agent),
    /// for the person to confirm or change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Proposal {
    pub input: Value,
    pub by: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub run_id: String,
    pub recipe_id: String,
    pub recipe_version: String,
    pub project_id: String,
    /// running | waiting_for_person | succeeded | failed | cancelled
    pub state: String,
    /// When waiting: `approval` (a scan needs an operator) or `input` (a person step).
    pub waiting_for: Option<String>,
    pub current_step_id: Option<String>,
    pub started_by: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub steps: Vec<StepState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Value>,
    /// How often a person had to step in. AI-first aims for one: the final
    /// confirmation.
    pub interventions: Interventions,
    /// Grants the run's scans ran under (`grant_id`, `granted_by`): standing
    /// permission given before or during the run, which `interventions` does
    /// not count as a per-run approval.
    pub authorized_by: Vec<Value>,
}

/// A run's person interventions. An agent completing its own run's step is
/// not one; an operator or admin (or a person-held token) acting on it is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Interventions {
    pub total: u32,
    /// A person approved one of the run's scans.
    pub approvals: u32,
    /// A person accepted a proposal unchanged.
    pub confirmed: u32,
    /// A person changed a proposal; see `corrections`.
    pub corrected: u32,
    /// A person answered a step that had no proposal.
    pub answered: u32,
    /// Of `total`, how many the agent relayed from the chat (the person said
    /// it to the agent; the chat client couldn't ask them directly). Marked
    /// by `confirmed_via` starting with "relayed" in the step's input.
    pub relayed: u32,
    /// What was changed: `{step_id, field, proposed, given, delta?}`, with
    /// `delta` = given − proposed for numbers.
    pub corrections: Vec<Value>,
}

impl Interventions {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (n, one, many) in [(self.approvals, "approval", "approvals"), (self.confirmed, "confirmation", "confirmations"),
                               (self.corrected, "correction", "corrections"), (self.answered, "answer", "answers")] {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        }
        if self.relayed > 0 {
            parts.push(format!("{} relayed by the agent", self.relayed));
        }
        let what = if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) };
        format!("{} person intervention{}{what}", self.total, if self.total == 1 { "" } else { "s" })
    }
}

/// Record the grant a run's scan job was admitted under, if any (once per grant).
fn note_grant(st: &AppState, run_id: &str, job_id: &str) {
    let g = {
        let inner = st.inner.lock().unwrap();
        let gid = inner.jobs.iter().find(|j| j.job.job_id == job_id).and_then(|j| j.grant_id.clone());
        gid.and_then(|gid| {
            inner.grants.iter().find(|g| g.grant_id == gid).map(|g| {
                let mut v = json!({"grant_id": g.grant_id, "granted_by": g.granted_by});
                if let Some(n) = &g.req.note {
                    v["note"] = json!(n);
                }
                v
            })
        })
    };
    if let Some(g) = g {
        with_run(st, run_id, |rec| {
            if !rec.run.authorized_by.iter().any(|x| x["grant_id"] == g["grant_id"]) {
                rec.run.authorized_by.push(g);
            }
        });
    }
}

/// Scans the run may still start from its current step on: one per
/// `startScan` step, plus each `measureOpening` step's `rescans`.
pub fn scans_left(st: &AppState, run_id: &str) -> u32 {
    with_run(st, run_id, |rec| {
        let from = rec.run.current_step_id.as_ref().and_then(|c| rec.recipe.steps.iter().position(|s| &s.step_id == c)).unwrap_or(0);
        rec.recipe.steps[from..]
            .iter()
            .map(|s| match s.operation_id.as_deref() {
                Some("startScan") => 1,
                Some("measureOpening") => s.arguments.as_ref().and_then(|a| a.get("rescans")).and_then(Value::as_u64).unwrap_or(0) as u32,
                _ => 0,
            })
            .sum()
    })
    .unwrap_or(0)
}

/// Count a person's approval of one of the run's scans.
pub fn note_approval(st: &AppState, run_id: &str) {
    with_run(st, run_id, |rec| {
        rec.run.interventions.approvals += 1;
        rec.run.interventions.total += 1;
    });
}

pub struct RunRecord {
    pub run: Run,
    pub recipe: Recipe,
    pub ctx: Value,
    pub caller: Caller,
    pub advance: Option<(String, Value)>,
    pub cancel: bool,
    pub wake: Arc<Notify>,
}

fn cancelled() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "cancelled", "The run was cancelled")
}

/// Apply a change to a run and publish it.
fn with_run<R>(st: &AppState, run_id: &str, f: impl FnOnce(&mut RunRecord) -> R) -> Option<R> {
    let (r, run) = {
        let mut inner = st.inner.lock().unwrap();
        let rec = inner.runs.get_mut(run_id)?;
        let r = f(rec);
        (r, rec.run.clone())
    };
    st.emit("run", serde_json::to_value(&run).unwrap_or_default());
    Some(r)
}

pub fn snapshot(st: &AppState, run_id: &str) -> Option<Run> {
    st.inner.lock().unwrap().runs.get(run_id).map(|r| r.run.clone())
}

/// Validate inputs, record the run and start driving it.
pub fn start(st: &Arc<AppState>, caller: Caller, recipe: Recipe, project_id: String, inputs: Value) -> Result<Run, ApiError> {
    let inputs = if inputs.is_null() { json!({}) } else { inputs };
    for i in &recipe.inputs {
        let v = inputs.get(&i.name).ok_or_else(|| ApiError::invalid(format!("input {:?} is required", i.name)))?;
        if !i.choices.is_empty() && !v.as_str().is_some_and(|s| i.choices.iter().any(|c| c == s)) {
            return Err(ApiError::invalid(format!("input {:?} must be one of: {}", i.name, i.choices.join(", "))));
        }
    }
    let run = Run {
        run_id: new_id("run"),
        recipe_id: recipe.recipe_id.clone(),
        recipe_version: recipe.version.clone(),
        project_id: project_id.clone(),
        state: "running".into(),
        waiting_for: None,
        current_step_id: None,
        started_by: caller.name.clone(),
        created_at: now_rfc3339(),
        finished_at: None,
        steps: recipe.steps.iter().map(|s| StepState { step_id: s.step_id.clone(), state: "pending".into(), job_id: None, output: None, proposal: None }).collect(),
        problem: None,
        interventions: Interventions::default(),
        authorized_by: Vec::new(),
    };
    let ctx = json!({"project_id": project_id, "input": inputs, "steps": {}});
    let name = caller.name.clone();
    st.inner.lock().unwrap().runs.insert(
        run.run_id.clone(),
        RunRecord { run: run.clone(), recipe: recipe.clone(), ctx, caller, advance: None, cancel: false, wake: Arc::new(Notify::new()) },
    );
    st.log("run", format!("Recipe {:?} v{} started", recipe.title, recipe.version), &name, Some(&project_id), &[("run_id", &run.run_id)]);
    tokio::spawn(drive(st.clone(), run.run_id.clone()));
    Ok(run)
}

/// Only whoever started a run, or an operator or admin, may advance or cancel it.
pub fn may_steer(st: &AppState, run_id: &str, c: &Caller, what: &str) -> Result<(), ApiError> {
    let inner = st.inner.lock().unwrap();
    let rec = inner.runs.get(run_id).ok_or_else(|| ApiError::not_found("run"))?;
    if may_steer_run(&rec.run, c) {
        Ok(())
    } else {
        Err(ApiError::forbidden(what))
    }
}

fn may_steer_run(run: &Run, c: &Caller) -> bool {
    run.started_by == c.name || c.role.is_operator()
}

/// The waiting person step `step_id`, with `input` holding every field it needs.
fn waiting_step(rec: &RunRecord, step_id: &str, input: &Value) -> Result<usize, ApiError> {
    let i = rec.run.steps.iter().position(|s| s.step_id == step_id).ok_or_else(|| ApiError::not_found("step"))?;
    if rec.run.steps[i].state != "waiting" || rec.run.waiting_for.as_deref() != Some("input") {
        return Err(ApiError::conflict(format!("step {step_id:?} is not waiting for input")));
    }
    if !input.is_object() {
        return Err(ApiError::invalid("input must be an object"));
    }
    let missing: Vec<&String> = rec.recipe.steps[i].needs.iter().filter(|n| input.get(n.as_str()).is_none()).collect();
    if !missing.is_empty() {
        return Err(ApiError::invalid(format!("missing: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
    }
    Ok(i)
}

/// Supply a person step's input. Errors if the run isn't waiting there. When
/// the step had a proposal, its output records whether it was accepted as is.
pub fn advance(st: &AppState, run_id: &str, step_id: &str, mut input: Value, by_person: bool) -> Result<Run, ApiError> {
    let res = with_run(st, run_id, |rec| -> Result<(Run, Option<(String, bool)>), ApiError> {
        let i = waiting_step(rec, step_id, &input)?;
        if by_person {
            let iv = &mut rec.run.interventions;
            iv.total += 1;
            if input.get("confirmed_via").and_then(Value::as_str).is_some_and(|v| v.starts_with("relayed")) {
                iv.relayed += 1;
            }
            match &rec.run.steps[i].proposal {
                None => iv.answered += 1,
                Some(p) => {
                    let mut changed = Vec::new();
                    for n in &rec.recipe.steps[i].needs {
                        let (given, proposed) = (input.get(n.as_str()), p.input.get(n.as_str()));
                        let same = match (given, proposed) {
                            (Some(Value::Number(a)), Some(Value::Number(b))) => a.as_f64() == b.as_f64(),
                            _ => given == proposed,
                        };
                        if !same {
                            let mut c = json!({"step_id": step_id, "field": n, "proposed": proposed, "given": given});
                            if let (Some(a), Some(b)) = (given.and_then(Value::as_f64), proposed.and_then(Value::as_f64)) {
                                c["delta"] = json!(((a - b) * 1000.0).round() / 1000.0);
                            }
                            changed.push(c);
                        }
                    }
                    if changed.is_empty() {
                        iv.confirmed += 1;
                    } else {
                        iv.corrected += 1;
                        iv.corrections.extend(changed);
                    }
                }
            }
        }
        let verdict = rec.run.steps[i].proposal.as_ref().map(|p| {
            // Numbers compare by value (1185 and 1185.0 are the same answer).
            let eq = |a: Option<&Value>, b: Option<&Value>| match (a, b) {
                (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
                _ => a == b,
            };
            let same = rec.recipe.steps[i].needs.iter().all(|n| eq(input.get(n.as_str()), p.input.get(n.as_str())));
            (p.by.clone(), same)
        });
        if let Some((_, same)) = &verdict {
            input["proposal_accepted"] = json!(same);
        }
        rec.advance = Some((step_id.to_string(), input));
        rec.wake.notify_one();
        Ok((rec.run.clone(), verdict))
    });
    let (run, verdict) = res.ok_or_else(|| ApiError::not_found("run"))??;
    if let Some((by, same)) = verdict {
        let text = if same { format!("Step {step_id:?}: {by}'s proposal accepted") } else { format!("Step {step_id:?}: {by}'s proposal changed by a person") };
        st.log("run", text, "bridge", Some(&run.project_id), &[("run_id", run_id), ("step_id", step_id)]);
    }
    Ok(run)
}

/// Propose a waiting person step's input (an agent's box, a measurement) for
/// a person to confirm or change. The step keeps waiting; a newer proposal
/// replaces an older one.
pub fn propose(st: &AppState, run_id: &str, step_id: &str, input: Value, note: Option<String>, by: &str) -> Result<Run, ApiError> {
    let res = with_run(st, run_id, |rec| -> Result<Run, ApiError> {
        let i = waiting_step(rec, step_id, &input)?;
        rec.run.steps[i].proposal = Some(Proposal { input, by: by.to_string(), at: now_rfc3339(), note });
        Ok(rec.run.clone())
    });
    let run = res.ok_or_else(|| ApiError::not_found("run"))??;
    st.log("run", format!("{by} proposed an answer for step {step_id:?}; waiting for a person to confirm"), by, Some(&run.project_id), &[("run_id", run_id), ("step_id", step_id)]);
    Ok(run)
}

/// Ask a run to stop. Its job in flight is cancelled too: a scan or an export
/// stops at its next check, and the run settles once the job does.
pub fn cancel(st: &AppState, run_id: &str) -> Result<Run, ApiError> {
    let job_id = with_run(st, run_id, |rec| {
        if !matches!(rec.run.state.as_str(), "succeeded" | "failed" | "cancelled") {
            rec.cancel = true;
            rec.wake.notify_one();
        }
        rec.run.steps.iter().find(|s| s.state == "running" || s.state == "waiting").and_then(|s| s.job_id.clone())
    })
    .ok_or_else(|| ApiError::not_found("run"))?;
    if let Some(job_id) = job_id {
        let job = st.inner.lock().unwrap().cancel_job(&job_id);
        st.save_grants();
        if let Some(j) = job {
            st.emit_job(&j);
        }
    }
    snapshot(st, run_id).ok_or_else(|| ApiError::not_found("run"))
}

fn finish(st: &AppState, run_id: &str, state: &str, problem: Option<&ApiError>) {
    let project = with_run(st, run_id, |rec| {
        rec.run.state = state.into();
        rec.run.waiting_for = None;
        rec.run.finished_at = Some(now_rfc3339());
        rec.run.problem = problem.map(|p| p.to_json());
        for s in rec.run.steps.iter_mut() {
            if s.state == "running" || s.state == "waiting" {
                s.state = if state == "cancelled" { "skipped".into() } else { "failed".into() };
            }
        }
        rec.run.project_id.clone()
    });
    let iv = snapshot(st, run_id)
        .map(|r| {
            let by: Vec<String> = r
                .authorized_by
                .iter()
                .filter_map(|g| g["granted_by"].as_str().map(|n| format!("a grant from {n}{}", g["note"].as_str().map(|x| format!(", {x}")).unwrap_or_default())))
                .collect();
            format!("{}{}", r.interventions.summary(), if by.is_empty() { String::new() } else { format!("; scans under {}", by.join(", ")) })
        })
        .unwrap_or_default();
    let text = match (state, problem) {
        ("failed", Some(p)) => format!("Run {run_id} failed: {} ({iv})", p.detail.as_deref().unwrap_or(&p.title)),
        _ => format!("Run {run_id} {state} after {iv}"),
    };
    st.log(if state == "failed" { "error" } else { "run" }, text, "bridge", project.as_deref(), &[("run_id", run_id)]);
}

async fn drive(st: Arc<AppState>, run_id: String) {
    let n = snapshot(&st, &run_id).map(|r| r.steps.len()).unwrap_or(0);
    for i in 0..n {
        let Some(Ok((step, ctx, caller))) = with_run(&st, &run_id, |rec| {
            if rec.cancel {
                return Err(());
            }
            rec.run.current_step_id = Some(rec.recipe.steps[i].step_id.clone());
            rec.run.steps[i].state = "running".into();
            rec.run.state = "running".into();
            rec.run.waiting_for = None;
            Ok((rec.recipe.steps[i].clone(), rec.ctx.clone(), rec.caller.clone()))
        }) else {
            return finish(&st, &run_id, "cancelled", None);
        };
        let out = match step.kind.as_str() {
            "action" => action(&st, &run_id, i, &step, &ctx, &caller).await,
            "person" => person(&st, &run_id, i, &step).await,
            "check" => check(&step, &ctx),
            k => Err(ApiError::invalid(format!("unknown step kind {k:?}"))),
        };
        match out {
            Ok(v) => {
                with_run(&st, &run_id, |rec| {
                    rec.ctx["steps"][&step.step_id] = v.clone();
                    rec.run.steps[i].state = "done".into();
                    rec.run.steps[i].output = Some(v);
                });
            }
            Err(e) if e.code == "cancelled" => return finish(&st, &run_id, "cancelled", None),
            Err(e) => return finish(&st, &run_id, "failed", Some(&e)),
        }
    }
    with_run(&st, &run_id, |rec| rec.run.current_step_id = None);
    finish(&st, &run_id, "succeeded", None);
}

async fn action(st: &Arc<AppState>, run_id: &str, i: usize, step: &RecipeStep, ctx: &Value, caller: &Caller) -> Result<Value, ApiError> {
    let args = resolve(step.arguments.as_ref().unwrap_or(&json!({})), ctx).map_err(ApiError::invalid)?;
    let bad = |e: serde_json::Error| ApiError::invalid(format!("step {:?}: {e}", step.step_id));
    match step.operation_id.as_deref().unwrap_or("") {
        "startScan" => {
            let mut a = args;
            a["project_id"] = ctx["project_id"].clone();
            let req: ScanRequest = serde_json::from_value(a).map_err(bad)?;
            let job = submit_scan(st, caller, req, Some(run_id)).await?;
            note_grant(st, run_id, &job.job_id);
            let job = wait_job(st, run_id, i, &job.job_id).await?;
            Ok(json!({"job_id": job.job_id, "scan_id": job.result.as_ref().and_then(|r| r.get("scan_id")).cloned()}))
        }
        "exportPoints" => {
            let scan_id = args.get("scan_id").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("exportPoints needs scan_id"))?.to_string();
            let req: ExportRequest = serde_json::from_value(args.clone()).map_err(bad)?;
            let job = submit_export(st, caller, scan_id, req, Some(run_id)).await?;
            let job = wait_job(st, run_id, i, &job.job_id).await?;
            let export_id = job.result.as_ref().and_then(|r| r.get("export_id")).and_then(Value::as_str).unwrap_or("").to_string();
            st.inner.lock().unwrap().exports.get(&export_id).cloned().ok_or_else(|| ApiError::not_found("export"))
        }
        "measureOpening" => {
            let mut scan_id = args.get("scan_id").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("measureOpening needs scan_id"))?.to_string();
            // Site check: with `window` (the close-up's) and `rescans` > 0, a
            // close-up short of wall on a side is rescanned wider, under the
            // run, instead of asking a person.
            let rescans = args.get("rescans").and_then(Value::as_u64).unwrap_or(0);
            let mut window: Option<crate::model::Window> = match args.get("window") {
                Some(w) if !w.is_null() => Some(serde_json::from_value(w.clone()).map_err(bad)?),
                _ => None,
            };
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("STD_L2").to_string();
            let project_id = ctx["project_id"].as_str().unwrap_or("").to_string();
            let mut attempts = Vec::new();
            loop {
                let job = submit_measure(st, caller, scan_id.clone(), MeasureRequest::opening(), Some(run_id)).await?;
                let job = wait_job(st, run_id, i, &job.job_id).await?;
                let id = job.result.as_ref().and_then(|r| r.get("measurement_id")).and_then(Value::as_str).unwrap_or("").to_string();
                let mut m = st.inner.lock().unwrap().measurements.get(&id).cloned().ok_or_else(|| ApiError::not_found("measurement"))?;
                let short: Vec<String> = m["details"]["sides"]
                    .as_object()
                    .map(|o| o.iter().filter(|(_, v)| v["ok"] == false).map(|(k, _)| k.clone()).collect())
                    .unwrap_or_default();
                attempts.push(json!({"scan_id": scan_id, "measurement_id": id, "short_sides": short,
                                     "opening_width_mm": m["opening_width_mm"], "opening_height_mm": m["opening_height_mm"]}));
                let done = short.is_empty() || attempts.len() as u64 > rescans;
                let Some(w) = window.filter(|_| !done) else {
                    m["site_check"] = json!({"rescans": attempts.len() - 1, "attempts": attempts, "ok": short.is_empty()});
                    return Ok(m);
                };
                let d = m["details"]["distance_m"].as_f64().unwrap_or(1.0);
                let (nw, a) = widen(w, &short, d);
                st.log("check", format!("Site check: not enough wall at the {} of the opening; rescanning {:.1}° wider on each side of that axis", short.join(", "), a.to_degrees()),
                       "bridge", Some(&project_id), &[("run_id", run_id), ("measurement_id", &id)]);
                let req = ScanRequest { project_id: project_id.clone(), kind: crate::model::ScanKind::Area, mode: mode.clone(), color: false,
                                        window: Some(nw), note: Some("site check rescan, wider".into()) };
                let job = submit_scan(st, caller, req, Some(run_id)).await?;
                note_grant(st, run_id, &job.job_id);
                let job = wait_job(st, run_id, i, &job.job_id).await?;
                scan_id = job.result.as_ref().and_then(|r| r.get("scan_id")).and_then(Value::as_str).unwrap_or("").to_string();
                window = Some(nw);
            }
        }
        "aimFromPreview" => {
            let req: AimReq = serde_json::from_value(args).map_err(bad)?;
            compute_aim(st, req).await
        }
        "addLogNote" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            check_note(&text).map_err(|e| ApiError::invalid(format!("step {:?}: {}", step.step_id, e.detail.unwrap_or_default())))?;
            let project = ctx["project_id"].as_str().unwrap_or("").to_string();
            let e = st.log("note", text, &caller.name, Some(&project), &[("run_id", run_id)]);
            Ok(json!({"entry_id": e.entry_id}))
        }
        op => Err(ApiError::invalid(format!("unknown operation {op:?}"))),
    }
}

/// A close-up window widened on its short axis (left/right: horizontal,
/// top/bottom: vertical) by the angle that adds ~15 cm at the wall's distance
/// `d`, on both sides. Returns the window and that angle.
pub fn widen(w: crate::model::Window, short: &[String], d: f64) -> (crate::model::Window, f64) {
    use std::f64::consts::{PI, TAU};
    let a = (0.15 / d.max(0.3)).atan();
    let mut nw = w;
    if short.iter().any(|s| s == "left" || s == "right") {
        nw.start_ha = (w.start_ha - a).rem_euclid(TAU);
        nw.delta_ha = (w.delta_ha + 2.0 * a).min(TAU);
    }
    if short.iter().any(|s| s == "top" || s == "bottom") {
        let (lo, hi) = ((w.start_va - a).max(0.0), (w.start_va + w.delta_va + a).min(PI));
        nw.start_va = lo;
        nw.delta_va = hi - lo;
    }
    (nw, a)
}

/// Follow a job to the end, mirroring "waiting for approval" onto the run.
async fn wait_job(st: &AppState, run_id: &str, i: usize, job_id: &str) -> Result<crate::model::Job, ApiError> {
    with_run(st, run_id, |rec| rec.run.steps[i].job_id = Some(job_id.to_string()));
    let mut last_wait: Option<bool> = None;
    loop {
        let (job, cancel) = {
            let inner = st.inner.lock().unwrap();
            let job = inner.jobs.iter().find(|j| j.job.job_id == job_id).map(|j| j.job.clone());
            let cancel = inner.runs.get(run_id).is_none_or(|r| r.cancel);
            (job, cancel)
        };
        let job = job.ok_or_else(|| ApiError::not_found("job"))?;
        match job.state {
            JobState::Succeeded => return Ok(job),
            JobState::Failed => {
                let why = job.problem.as_ref().and_then(|p| p.get("detail").or(p.get("title"))).and_then(Value::as_str).unwrap_or("failed");
                return Err(ApiError::conflict(format!("job {job_id} failed: {why}")));
            }
            JobState::Cancelled => return Err(if cancel { cancelled() } else { ApiError::conflict(format!("job {job_id} was cancelled")) }),
            s => {
                let waiting = s == JobState::AwaitingApproval;
                if last_wait != Some(waiting) {
                    last_wait = Some(waiting);
                    with_run(st, run_id, |rec| {
                        rec.run.state = if waiting { "waiting_for_person" } else { "running" }.into();
                        rec.run.waiting_for = waiting.then(|| "approval".into());
                    });
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn person(st: &AppState, run_id: &str, i: usize, step: &RecipeStep) -> Result<Value, ApiError> {
    let wake = with_run(st, run_id, |rec| {
        rec.run.steps[i].state = "waiting".into();
        rec.run.state = "waiting_for_person".into();
        rec.run.waiting_for = Some("input".into());
        rec.wake.clone()
    })
    .ok_or_else(cancelled)?;
    let project = snapshot(st, run_id).map(|r| r.project_id).unwrap_or_default();
    st.log("run", format!("Waiting for a person: {}", step.instructions.as_deref().unwrap_or("")), "bridge", Some(&project), &[("run_id", run_id), ("step_id", &step.step_id)]);
    if let Some(from) = &step.propose_from {
        // Fill the step's needs from the earlier step's output, by name.
        let source = with_run(st, run_id, |rec| rec.ctx["steps"][from.as_str()].clone()).unwrap_or(Value::Null);
        let mut input = serde_json::Map::new();
        for n in &step.needs {
            if let Some(v) = source.get(n.as_str()) {
                input.insert(n.clone(), v.clone());
            }
        }
        if input.len() == step.needs.len() {
            let note = match (source.get("opening_width_mm"), source.get("opening_height_mm")) {
                (Some(w), Some(h)) => Some(format!("Measured by the bridge from step {from:?}: {w} × {h} mm (± {} mm)", source.get("uncertainty_mm").cloned().unwrap_or(Value::Null))),
                _ => Some(format!("Taken from step {from:?}")),
            };
            let _ = propose(st, run_id, &step.step_id, Value::Object(input), note, "bridge");
        }
    }
    loop {
        wake.notified().await;
        let got = with_run(st, run_id, |rec| {
            if rec.cancel {
                return Err(cancelled());
            }
            match rec.advance.take() {
                Some((sid, input)) if sid == step.step_id => Ok(Some(input)),
                _ => Ok(None),
            }
        })
        .ok_or_else(cancelled)?;
        if let Some(input) = got? {
            return Ok(input);
        }
    }
}

fn check(step: &RecipeStep, ctx: &Value) -> Result<Value, ApiError> {
    let of = step.of.as_deref().unwrap_or("");
    let checks = lookup(ctx, &format!("steps.{of}.checks")).and_then(|c| c.as_array().cloned()).unwrap_or_default();
    let mut failed = Vec::new();
    for want in &step.expect {
        match checks.iter().find(|c| c["name"] == want.as_str()) {
            Some(c) if c["ok"] == true => {}
            Some(c) => failed.push(format!("{want} ({})", c["detail"].as_str().unwrap_or("not ok"))),
            None => failed.push(format!("{want} (missing)")),
        }
    }
    if failed.is_empty() {
        Ok(json!({"ok": true, "checked": step.expect}))
    } else {
        Err(ApiError::new(StatusCode::CONFLICT, "check_failed", "A recipe check failed").detail(failed.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn run(started_by: &str) -> Run {
        Run {
            run_id: "run_1".into(),
            recipe_id: "r".into(),
            recipe_version: "1".into(),
            project_id: "p".into(),
            state: "waiting_for_person".into(),
            waiting_for: Some("input".into()),
            current_step_id: None,
            started_by: started_by.into(),
            created_at: now_rfc3339(),
            finished_at: None,
            steps: vec![],
            problem: None,
            interventions: Interventions::default(),
            authorized_by: Vec::new(),
        }
    }

    #[test]
    fn only_the_starter_or_an_operator_steers_a_run() {
        let who = |name: &str, role| Caller { name: name.into(), role };
        let r = run("agent-1");
        assert!(may_steer_run(&r, &who("agent-1", Role::Agent)));
        assert!(!may_steer_run(&r, &who("agent-2", Role::Agent)));
        assert!(may_steer_run(&r, &who("op", Role::Operator)));
        assert!(may_steer_run(&r, &who("root", Role::Admin)));
    }

    #[test]
    fn interventions_summary_reads_well() {
        let mut iv = Interventions::default();
        assert_eq!(iv.summary(), "0 person interventions");
        iv.total = 3;
        iv.approvals = 1;
        iv.confirmed = 2;
        assert_eq!(iv.summary(), "3 person interventions (1 approval, 2 confirmations)");
        iv.total = 1;
        iv.relayed = 1;
        assert!(iv.summary().ends_with("1 relayed by the agent)"), "{}", iv.summary());
        iv.relayed = 0;
        iv.approvals = 0;
        iv.confirmed = 0;
        iv.corrected = 1;
        assert_eq!(iv.summary(), "1 person intervention (1 correction)");
    }

    #[test]
    fn widen_adds_about_15_cm_on_the_short_axis_only() {
        use crate::model::Window;
        let w = Window { start_ha: 0.05, delta_ha: 1.4, start_va: 0.7, delta_va: 1.35 };
        let (nw, a) = widen(w, &["left".into()], 0.85);
        assert!((a - (0.15f64 / 0.85).atan()).abs() < 1e-12);
        assert!((nw.delta_ha - (1.4 + 2.0 * a)).abs() < 1e-12);
        assert!((nw.start_ha - (0.05 - a).rem_euclid(std::f64::consts::TAU)).abs() < 1e-12, "wraps past zero");
        assert_eq!((nw.start_va, nw.delta_va), (0.7, 1.35));
        let (nw, _) = widen(w, &["top".into()], 0.85);
        assert_eq!((nw.start_ha, nw.delta_ha), (0.05, 1.4));
        assert!(nw.start_va < 0.7 && nw.start_va + nw.delta_va <= std::f64::consts::PI + 1e-12);
        nw.validate().unwrap();
    }

    // ------------------------------------------------- runs on a bridge ---

    use crate::recipe::RecipeInput;
    use crate::testkit::{fast_mock, Bridge, OPERATOR};

    /// The mock's project.
    const PROJECT: &str = "39187455-8278-428c-89ad-8a4835f95be6";

    fn who(name: &str, role: Role) -> Caller {
        Caller { name: name.into(), role }
    }

    /// A step the way a recipe file gives one.
    fn step(v: Value) -> RecipeStep {
        serde_json::from_value(v).unwrap()
    }

    /// A recipe as given, unchecked (runs take what they are handed).
    fn recipe(steps: Vec<Value>) -> Recipe {
        Recipe { recipe_id: "t".into(), title: "Test".into(), version: "1".into(), summary: String::new(),
                 inputs: vec![RecipeInput { name: "purpose".into(), kind: "choice".into(), choices: vec!["blinds".into(), "trim".into()], prompt: None }],
                 steps: steps.into_iter().map(step).collect() }
    }

    fn person(id: &str, needs: &[&str]) -> Value {
        json!({"id": id, "kind": "person", "instructions": format!("Answer {id}."), "needs": needs})
    }

    /// How much longer than its stated time a wait may take before it fails.
    /// The times are what an ordinary build needs; an instrumented build on CI
    /// (the coverage job) runs the measurement many times slower. A passing
    /// test never waits this out, so the slack only delays a real failure.
    const SLACK: f64 = 12.0;

    /// Wait (up to `secs`, times `SLACK`) for the run to satisfy `pred`.
    async fn until(st: &AppState, run_id: &str, secs: f64, pred: impl Fn(&Run) -> bool) -> Run {
        let end = std::time::Instant::now() + Duration::from_secs_f64(secs * SLACK);
        loop {
            let r = snapshot(st, run_id).expect("run");
            if pred(&r) {
                return r;
            }
            if std::time::Instant::now() > end {
                let log: Vec<String> = st.inner.lock().unwrap().log.iter().rev().take(12).map(|e| e.text.clone()).collect();
                panic!("run {run_id} stuck: {}\nlog (newest first): {log:#?}", serde_json::to_string(&r).unwrap());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn waiting_at(st: &AppState, run_id: &str, step_id: &str) -> Run {
        until(st, run_id, 5.0, |r| r.waiting_for.as_deref() == Some("input") && r.current_step_id.as_deref() == Some(step_id)).await
    }

    async fn finished(st: &AppState, run_id: &str, secs: f64) -> Run {
        until(st, run_id, secs, |r| r.finished_at.is_some()).await
    }

    fn logged(st: &AppState, needle: &str) -> bool {
        st.inner.lock().unwrap().log.iter().any(|e| e.text.contains(needle))
    }

    /// No scanner needed: a bridge whose scanner never answers.
    fn offline() -> Bridge {
        Bridge::on_scanner("http://127.0.0.1:9/api/v1")
    }

    #[tokio::test]
    async fn a_run_needs_each_input_and_one_of_its_choices() {
        let b = offline();
        let r = recipe(vec![person("p", &["x"])]);
        let go = |inputs: Value| start(&b.st, who("agent", Role::Agent), r.clone(), PROJECT.into(), inputs);
        assert_eq!(go(Value::Null).unwrap_err().detail.unwrap(), "input \"purpose\" is required");
        assert_eq!(go(json!({"purpose": "paint"})).unwrap_err().detail.unwrap(), "input \"purpose\" must be one of: blinds, trim");
        assert!(go(json!({"purpose": 3})).is_err());
        let run = go(json!({"purpose": "trim"})).unwrap();
        assert_eq!((run.state.as_str(), run.steps.len(), run.started_by.as_str()), ("running", 1, "agent"));
        assert!(logged(&b.st, "Recipe \"Test\" v1 started"));
        // Who may steer it.
        assert!(may_steer(&b.st, &run.run_id, &who("agent", Role::Agent), "advance runs").is_ok());
        assert_eq!(may_steer(&b.st, &run.run_id, &who("other", Role::Agent), "advance runs").unwrap_err().code, "forbidden");
        assert_eq!(may_steer(&b.st, "run_nope", &who("op", Role::Operator), "advance runs").unwrap_err().code, "not_found");
        assert_eq!(scans_left(&b.st, "run_nope"), 0);
        assert!(snapshot(&b.st, "run_nope").is_none());
        // A run that doesn't exist can't be advanced, proposed to or cancelled.
        assert_eq!(advance(&b.st, "run_nope", "p", json!({}), true).unwrap_err().code, "not_found");
        assert_eq!(propose(&b.st, "run_nope", "p", json!({}), None, "agent").unwrap_err().code, "not_found");
        assert_eq!(cancel(&b.st, "run_nope").unwrap_err().code, "not_found");
        cancel(&b.st, &run.run_id).unwrap();
        assert_eq!(finished(&b.st, &run.run_id, 5.0).await.state, "cancelled");
    }

    #[tokio::test]
    async fn person_steps_count_confirmations_corrections_and_answers() {
        let b = offline();
        let r = recipe(vec![
            person("box", &["width"]),
            person("size", &["w", "label"]),
            person("note", &["text"]),
            person("agent_only", &["checks"]),
            json!({"id": "verify", "kind": "check", "of": "agent_only", "expect": ["seen"]}),
            json!({"id": "log", "kind": "action", "operation": "addLogNote", "arguments": {"text": "Done for {{input.purpose}} in {{project_id}}"}}),
        ]);
        let run = start(&b.st, who("agent", Role::Agent), r, PROJECT.into(), json!({"purpose": "blinds"})).unwrap();
        let id = run.run_id.clone();
        waiting_at(&b.st, &id, "box").await;
        // Nothing to advance but the waiting step, with an object holding what it needs.
        assert_eq!(advance(&b.st, &id, "nope", json!({}), true).unwrap_err().code, "not_found");
        assert_eq!(advance(&b.st, &id, "size", json!({}), true).unwrap_err().code, "conflict");
        assert_eq!(advance(&b.st, &id, "box", json!([1]), true).unwrap_err().detail.unwrap(), "input must be an object");
        assert_eq!(advance(&b.st, &id, "box", json!({"w": 1}), true).unwrap_err().detail.unwrap(), "missing: width");
        assert_eq!(propose(&b.st, &id, "box", json!({}), None, "agent").unwrap_err().code, "invalid_request");
        // The agent proposes; a person accepts it (1185 and 1185.0 are one answer).
        let p = propose(&b.st, &id, "box", json!({"width": 1185}), Some("from the preview".into()), "agent").unwrap();
        assert_eq!(p.steps[0].proposal.as_ref().unwrap().note.as_deref(), Some("from the preview"));
        assert!(logged(&b.st, "agent proposed an answer for step \"box\""));
        advance(&b.st, &id, "box", json!({"width": 1185.0}), true).unwrap();
        assert!(logged(&b.st, "Step \"box\": agent's proposal accepted"));

        // A person changes a proposal, relayed by the agent from the chat.
        waiting_at(&b.st, &id, "size").await;
        propose(&b.st, &id, "size", json!({"w": 10, "label": "left"}), None, "agent").unwrap();
        advance(&b.st, &id, "size", json!({"w": 12.5, "label": "right", "confirmed_via": "relayed by the agent: it's the right one"}), true).unwrap();
        assert!(logged(&b.st, "Step \"size\": agent's proposal changed by a person"));

        // A person answers a step nobody proposed for; the agent fills its own.
        waiting_at(&b.st, &id, "note").await;
        advance(&b.st, &id, "note", json!({"text": "north side"}), true).unwrap();
        waiting_at(&b.st, &id, "agent_only").await;
        advance(&b.st, &id, "agent_only", json!({"checks": [{"name": "seen", "ok": true}]}), false).unwrap();

        let done = finished(&b.st, &id, 5.0).await;
        assert_eq!(done.state, "succeeded", "{done:?}");
        assert!(done.current_step_id.is_none() && done.waiting_for.is_none());
        assert!(done.steps.iter().all(|s| s.state == "done"));
        assert_eq!(done.steps[0].output.as_ref().unwrap()["proposal_accepted"], true);
        assert_eq!(done.steps[1].output.as_ref().unwrap()["proposal_accepted"], false);
        assert_eq!(done.steps[4].output.as_ref().unwrap()["checked"], json!(["seen"]));
        assert!(done.steps[5].output.as_ref().unwrap()["entry_id"].is_string());
        let iv = &done.interventions;
        assert_eq!((iv.total, iv.confirmed, iv.corrected, iv.answered, iv.relayed, iv.approvals), (3, 1, 1, 1, 1, 0));
        assert_eq!(iv.corrections, vec![
            json!({"step_id": "size", "field": "w", "proposed": 10, "given": 12.5, "delta": 2.5}),
            json!({"step_id": "size", "field": "label", "proposed": "left", "given": "right"}),
        ]);
        assert!(logged(&b.st, &format!("Done for blinds in {PROJECT}")));
        assert!(logged(&b.st, &format!("Run {id} succeeded after 3 person interventions (1 confirmation, 1 correction, 1 answer, 1 relayed by the agent)")));
        // Finished: nothing waits, and cancelling changes nothing.
        assert_eq!(advance(&b.st, &id, "note", json!({"text": "x"}), true).unwrap_err().code, "conflict");
        assert_eq!(cancel(&b.st, &id).unwrap().state, "succeeded");
    }

    /// Runs whose steps can't go on fail with the reason, before reaching a scanner.
    #[tokio::test]
    async fn a_step_that_cannot_run_fails_the_run_with_its_reason() {
        let b = offline();
        let check = |expect: Value| json!({"id": "verify", "kind": "check", "of": "p", "expect": expect});
        let action = |op: &str, args: Value| json!({"id": "a", "kind": "action", "operation": op, "arguments": args});
        let cases: Vec<(Vec<Value>, &str, &str)> = vec![
            (vec![json!({"id": "x", "kind": "dance"})], "invalid_request", "unknown step kind \"dance\""),
            (vec![action("rmRf", json!({}))], "invalid_request", "unknown operation \"rmRf\""),
            (vec![action("addLogNote", json!({"text": " "}))], "invalid_request", "step \"a\": text must be 1–4000 characters"),
            (vec![action("addLogNote", json!({"text": "{{steps.later.x}}"}))], "invalid_request", "{{steps.later.x}} has no value yet"),
            (vec![action("startScan", json!({"kind": "sideways", "mode": "STD_L0"}))], "invalid_request", "step \"a\": unknown variant"),
            (vec![action("exportPoints", json!({"format": "ply"}))], "invalid_request", "exportPoints needs scan_id"),
            (vec![action("exportPoints", json!({"scan_id": "s", "format": "e57"}))], "invalid_request", "step \"a\": unknown variant"),
            (vec![action("measureOpening", json!({}))], "invalid_request", "measureOpening needs scan_id"),
            (vec![action("measureOpening", json!({"scan_id": "s", "window": {"start_ha": "wide"}}))], "invalid_request", "step \"a\""),
            (vec![action("aimFromPreview", json!({"box": {}}))], "invalid_request", "step \"a\""),
            (vec![action("startScan", json!({"kind": "full_dome", "mode": "STD_L0"}))], "scanner_disconnected", ""),
        ];
        for (steps, code, detail) in cases {
            let run = start(&b.st, who("crew", Role::Operator), recipe(steps), PROJECT.into(), json!({"purpose": "trim"})).unwrap();
            let done = finished(&b.st, &run.run_id, 5.0).await;
            assert_eq!(done.state, "failed");
            let p = done.problem.as_ref().unwrap();
            assert_eq!(p["code"], code, "{p}");
            assert!(p["detail"].as_str().unwrap_or("").contains(detail), "{p}");
            assert!(done.steps.iter().all(|s| s.state == "failed"), "{done:?}");
        }
        // A check names what is missing and what isn't ok.
        let run = start(&b.st, who("crew", Role::Operator), recipe(vec![person("p", &["checks"]), check(json!(["a", "b", "c"]))]), PROJECT.into(), json!({"purpose": "trim"})).unwrap();
        waiting_at(&b.st, &run.run_id, "p").await;
        advance(&b.st, &run.run_id, "p", json!({"checks": [{"name": "a", "ok": true}, {"name": "b", "ok": false, "detail": "3 of 4"}, {"name": "x"}]}), true).unwrap();
        let done = finished(&b.st, &run.run_id, 5.0).await;
        let p = done.problem.unwrap();
        assert_eq!((p["code"].as_str(), p["detail"].as_str()), (Some("check_failed"), Some("b (3 of 4); c (missing)")));
        assert!(logged(&b.st, &format!("Run {} failed: b (3 of 4); c (missing) (1 person intervention (1 answer))", run.run_id)));
    }

    /// A close-up of a wall 1.2 m away, 2 × 1.8 m, with a 1.2 × 1.1 m opening
    /// (glass 12 cm behind), optionally cropped `crop_right` m beyond the
    /// opening on the scanner's right, and with a box standing 20 cm in front
    /// of the wall below and left of the opening when `in_front`.
    fn close_up(crop_right: Option<f64>, in_front: bool) -> Vec<u8> {
        use crate::export::tests::tzf_bytes;
        let (n, t) = ([0.6f64, -0.8], [0.8f64, 0.6]);
        let u_min = crop_right.map_or(-1.0, |c| -0.6 - c);
        let wall = move |va: f64, ha: f64| -> Option<f64> {
            let d = [va.sin() * ha.cos(), va.sin() * ha.sin(), va.cos()];
            let dn = n[0] * d[0] + n[1] * d[1];
            if dn <= 0.0 {
                return None;
            }
            let at = |dist: f64| {
                let r = dist / dn;
                (r, t[0] * d[0] * r + t[1] * d[1] * r, d[2] * r)
            };
            let (r, u, z) = at(1.2);
            if !(u_min..=1.0).contains(&u) || z.abs() > 0.9 {
                return None;
            }
            if in_front && (0.7..0.9).contains(&u) && (-0.85..-0.6).contains(&z) {
                return Some(at(1.0).0);
            }
            Some(if u.abs() < 0.6 && z > -0.5 && z < 0.6 { at(1.32).0 } else { r })
        };
        let ha_c = (-0.8f64).atan2(0.6).rem_euclid(std::f64::consts::TAU);
        let step = 0.0033;
        tzf_bytes(424, 455, std::f64::consts::FRAC_PI_2 - 0.7, ha_c - 0.75, step, wall)
    }

    /// The scanner's fixtures: a full dome of a walled yard and the close-up.
    fn fixtures(crop_right: Option<f64>) -> std::path::PathBuf {
        use crate::export::tests::{grey_jpeg, room_dome, scratch};
        let dir = scratch("x9-run-fixtures");
        std::fs::write(dir.join("dome.tzf"), room_dome(720, [-6.0, -4.0, -1.5], [5.0, 6.0, 3.0])).unwrap();
        std::fs::write(dir.join("area.tzf"), close_up(crop_right, true)).unwrap();
        for f in ["dome.jpg", "area.jpg"] {
            std::fs::write(dir.join(f), grey_jpeg()).unwrap();
        }
        dir
    }

    async fn bridge_with(fixtures: &std::path::Path) -> Bridge {
        let b = Bridge::with_mock(x9_mock::Config { fixtures: Some(fixtures.to_string_lossy().into_owned()), ..fast_mock() }).await;
        b.start_worker();
        b
    }

    /// The window-measurement recipe from end to end on the mock: the agent
    /// runs it, a person approves the first scan for the whole run and
    /// confirms the opening the bridge measured.
    #[tokio::test]
    async fn the_window_recipe_runs_with_one_approval_and_one_confirmation() {
        let fx = fixtures(None);
        let b = bridge_with(&fx).await;
        let r = Recipe::parse(include_str!("../tests/recipes/window-measurement.toml")).unwrap();
        let run = start(&b.st, who("agent", Role::Agent), r, PROJECT.into(), json!({"purpose": "blinds"})).unwrap();
        let id = run.run_id.clone();
        assert_eq!(scans_left(&b.st, &id), 3, "overview, close-up and one rescan");
        let w = until(&b.st, &id, 10.0, |r| r.waiting_for.as_deref() == Some("approval")).await;
        let job = w.steps[0].job_id.clone().unwrap();
        let (code, body) = b.call("POST", &format!("/v1/jobs/{job}/approve"), Some(OPERATOR), Some(json!({"run": true}))).await;
        assert!(code.is_success(), "{body}");
        waiting_at(&b.st, &id, "box").await;
        advance(&b.st, &id, "box", json!({"box": {"x0": 3, "y0": 2, "x1": 6, "y1": 5}}), false).unwrap();
        waiting_at(&b.st, &id, "confirm").await;
        let r = snapshot(&b.st, &id).unwrap();
        let measured = r.steps[6].output.clone().unwrap();
        assert_eq!(measured["site_check"]["rescans"], 0);
        assert!((measured["opening_width_mm"].as_f64().unwrap() - 1200.0).abs() < 15.0, "{measured}");
        let proposal = r.steps[8].proposal.clone().expect("the bridge proposes the measured size");
        assert_eq!(proposal.by, "bridge");
        assert!(proposal.note.unwrap().starts_with("Measured by the bridge from step \"measure\""));
        assert_eq!(r.authorized_by.len(), 1, "the close-up ran under the run's grant: {:?}", r.authorized_by);
        assert_eq!(r.authorized_by[0]["granted_by"], "crew");
        advance(&b.st, &id, "confirm", proposal.input.clone(), true).unwrap();
        let done = finished(&b.st, &id, 10.0).await;
        assert_eq!(done.state, "succeeded", "{done:?}");
        assert_eq!((done.interventions.approvals, done.interventions.confirmed, done.interventions.total), (1, 1, 2));
        assert!(done.steps[2].output.as_ref().unwrap()["window"]["delta_ha"].as_f64().unwrap() > 0.0);
        assert!(done.steps[4].output.as_ref().unwrap()["checks"].as_array().unwrap().iter().all(|c| c["ok"] == true));
        assert!(logged(&b.st, "scans under a grant from crew"));
        std::fs::remove_dir_all(fx).unwrap();
    }

    /// A close-up short of wall on one side is rescanned wider, once, under the run.
    #[tokio::test]
    async fn a_close_up_short_of_wall_is_rescanned_wider_once() {
        let fx = fixtures(Some(0.01));
        let b = bridge_with(&fx).await;
        let win = json!({"start_ha": 4.6, "delta_ha": 1.5, "start_va": 0.87, "delta_va": 1.4});
        let r = recipe(vec![
            json!({"id": "closeup", "kind": "action", "operation": "startScan", "arguments": {"kind": "area", "mode": "STD_L2", "window": win}}),
            json!({"id": "measure", "kind": "action", "operation": "measureOpening", "arguments": {"scan_id": "{{steps.closeup.scan_id}}", "window": win, "rescans": 1}}),
        ]);
        let run = start(&b.st, who("crew", Role::Operator), r, PROJECT.into(), json!({"purpose": "trim"})).unwrap();
        let done = finished(&b.st, &run.run_id, 20.0).await;
        assert_eq!(done.state, "succeeded", "{done:?}");
        let sc = &done.steps[1].output.as_ref().unwrap()["site_check"];
        assert_eq!((sc["rescans"].as_u64(), sc["ok"].as_bool()), (Some(1), Some(false)), "{sc}");
        assert_eq!(sc["attempts"][0]["short_sides"], json!(["right"]));
        assert_ne!(sc["attempts"][0]["scan_id"], sc["attempts"][1]["scan_id"]);
        assert!(logged(&b.st, "Site check: not enough wall at the right of the opening"));
        std::fs::remove_dir_all(fx).unwrap();
    }

    /// A job that fails fails its run; cancelling a run cancels the job it waits on.
    #[tokio::test]
    async fn a_failed_job_fails_the_run_and_a_cancel_stops_its_job() {
        let b = Bridge::new().await;
        b.start_worker();
        let scan = json!({"id": "overview", "kind": "action", "operation": "startScan", "arguments": {"kind": "full_dome", "mode": "STD_L0"}});
        let export = json!({"id": "export", "kind": "action", "operation": "exportPoints", "arguments": {"scan_id": "{{steps.overview.scan_id}}", "format": "ply"}});
        // No scan files on this scanner: the export fails.
        let run = start(&b.st, who("crew", Role::Operator), recipe(vec![scan.clone(), export]), PROJECT.into(), json!({"purpose": "trim"})).unwrap();
        let done = finished(&b.st, &run.run_id, 10.0).await;
        assert_eq!(done.state, "failed");
        assert_eq!(done.steps[0].state, "done");
        assert!(done.problem.unwrap()["detail"].as_str().unwrap().contains("failed"));
        // The agent's scan waits for approval; cancelling the run cancels it.
        let run = start(&b.st, who("agent", Role::Agent), recipe(vec![scan]), PROJECT.into(), json!({"purpose": "trim"})).unwrap();
        let w = until(&b.st, &run.run_id, 5.0, |r| r.waiting_for.as_deref() == Some("approval")).await;
        assert_eq!(w.state, "waiting_for_person");
        let job = w.steps[0].job_id.clone().unwrap();
        cancel(&b.st, &run.run_id).unwrap();
        let done = finished(&b.st, &run.run_id, 5.0).await;
        assert_eq!(done.state, "cancelled");
        assert_eq!(done.steps[0].state, "skipped");
        let (_, j) = b.call("GET", &format!("/v1/jobs/{job}"), Some(OPERATOR), None).await;
        assert_eq!(j["state"], "cancelled");
    }

    #[test]
    fn widen_keeps_a_window_on_the_sphere_near_the_poles() {
        use crate::model::Window;
        let w = Window { start_ha: 6.2, delta_ha: 6.0, start_va: 0.02, delta_va: 3.1 };
        let (nw, a) = widen(w, &["right".into(), "bottom".into()], 0.1);
        assert!((a - (0.15f64 / 0.3).atan()).abs() < 1e-12, "a close wall counts as 0.3 m away");
        assert_eq!(nw.delta_ha, std::f64::consts::TAU);
        assert_eq!((nw.start_va, nw.start_va + nw.delta_va), (0.0, std::f64::consts::PI));
        let (same, _) = widen(w, &[], 2.0);
        assert_eq!(same, w);
    }

    /// Found while building the run fixtures: with nothing in front of the
    /// wall and little range noise, the wall's points all fall in the first
    /// 5 mm bin of `measure::opening`'s depth histogram, which its search for
    /// the frontmost surface skips (`1..nb`), so the glass 12 cm behind is
    /// taken for the wall face and no opening is found.
    #[test]
    fn a_close_up_with_nothing_in_front_of_the_wall_measures_its_opening() {
        let t = trimble_tzf::Tzf::from_bytes(close_up(None, false)).unwrap();
        let pts: Vec<[f64; 3]> = t.grid_points().unwrap().iter().map(|g| [g.point.x as f64, g.point.y as f64, g.point.z as f64]).collect();
        // The same scene with a box in front of the wall measures fine.
        let t2 = trimble_tzf::Tzf::from_bytes(close_up(None, true)).unwrap();
        let with_box: Vec<[f64; 3]> = t2.grid_points().unwrap().iter().map(|g| [g.point.x as f64, g.point.y as f64, g.point.z as f64]).collect();
        assert!((crate::measure::opening(&with_box).unwrap().width_m - 1.2).abs() < 0.015);
        let o = crate::measure::opening(&pts).expect("the opening");
        assert!((o.width_m - 1.2).abs() < 0.015 && (o.height_m - 1.1).abs() < 0.015, "{} × {}", o.width_m, o.height_m);
    }
}
