//! API types (see `api/bridge-v1.openapi.yaml`). JSON is `snake_case`;
//! lengths in metres, angles in radians, times RFC 3339 UTC.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Agent,
    Operator,
    Admin,
}

impl Role {
    /// Approve jobs, create grants, delete with confirmation.
    pub fn is_operator(self) -> bool {
        matches!(self, Role::Operator | Role::Admin)
    }
}

#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rules {
    pub min_battery_pct_to_scan: u32,
    pub min_storage_free_bytes: u64,
    pub require_approval_for_scans: bool,
    pub refuse_aim_after_move: bool,
    pub max_queue_length: usize,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            min_battery_pct_to_scan: 15,
            min_storage_free_bytes: 2_000_000_000,
            require_approval_for_scans: true,
            refuse_aim_after_move: true,
            max_queue_length: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub start_ha: f64,
    pub delta_ha: f64,
    pub start_va: f64,
    pub delta_va: f64,
}

impl Window {
    pub fn validate(&self) -> Result<(), String> {
        let ok = (0.0..TAU).contains(&self.start_ha)
            && self.delta_ha > 0.0
            && self.delta_ha <= TAU
            && (0.0..=PI).contains(&self.start_va)
            && self.delta_va > 0.0
            && self.start_va + self.delta_va <= PI + 1e-9;
        if ok {
            Ok(())
        } else {
            Err("window out of range: start_ha in [0, 2π), 0 < delta_ha ≤ 2π, start_va in [0, π], 0 < delta_va, start_va + delta_va ≤ π".into())
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    FullDome,
    Area,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    pub project_id: String,
    pub kind: ScanKind,
    pub mode: String,
    #[serde(default)]
    pub color: bool,
    #[serde(default)]
    pub window: Option<Window>,
    #[serde(default)]
    pub note: Option<String>,
}

impl ScanRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode.trim().is_empty() {
            return Err("mode is required".into());
        }
        match (self.kind, &self.window) {
            (ScanKind::Area, None) => Err("an area scan needs a window".into()),
            (ScanKind::Area, Some(w)) => w.validate(),
            (ScanKind::FullDome, Some(_)) => Err("a full-dome scan takes no window".into()),
            (ScanKind::FullDome, None) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    AwaitingApproval,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_final(self) -> bool {
        matches!(self, JobState::Succeeded | JobState::Failed | JobState::Cancelled)
    }

    /// The API spelling, e.g. `awaiting_approval`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::AwaitingApproval => "awaiting_approval",
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub job_id: String,
    pub kind: String,
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    pub requested_by: String,
    pub run_id: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Value>,
    /// Cancelled scan jobs: the scan the scanner filed from the partial sweep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_scan_id: Option<String>,
    /// Scan jobs: the request as admitted, so a person can see what they approve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<ScanRequest>,
    /// The project the job works in, by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRequest {
    pub project_id: String,
    pub expires_at: String,
    #[serde(default)]
    pub max_scans: Option<u32>,
    #[serde(default)]
    pub allowed_modes: Vec<String>,
    #[serde(default)]
    pub allow_color: bool,
    #[serde(default)]
    pub run_id: Option<String>,
    /// How the person gave it, when not in the bridge's own UI: e.g. "asked
    /// in the chat", or "relayed by the agent: <their words>".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    #[serde(flatten)]
    pub req: GrantRequest,
    pub grant_id: String,
    pub granted_by: String,
    pub scans_used: u32,
    #[serde(skip)]
    pub expires_unix: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub entry_id: String,
    pub at: String,
    pub kind: String,
    pub text: String,
    pub author: String,
    pub project_id: Option<String>,
    pub refs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leveling_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry_ready_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_s: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn job_state_spelling_matches_serde() {
        use JobState::*;
        for s in [AwaitingApproval, Queued, Running, Succeeded, Failed, Cancelled] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
    }


    #[test]
    fn only_operators_and_admins_are_operators() {
        assert_eq!([Role::Viewer, Role::Agent, Role::Operator, Role::Admin].map(Role::is_operator), [false, false, true, true]);
        assert_eq!(serde_json::to_value(Role::Admin).unwrap(), "admin");
        assert!(serde_json::from_value::<Role>(serde_json::json!("root")).is_err());
    }

    #[test]
    fn default_rules_ask_for_approval_and_keep_a_reserve() {
        let r = Rules::default();
        assert!(r.require_approval_for_scans && r.refuse_aim_after_move);
        assert_eq!((r.min_battery_pct_to_scan, r.max_queue_length), (15, 20));
        assert_eq!(serde_json::to_value(&r).unwrap()["min_storage_free_bytes"], 2_000_000_000u64);
    }

    #[test]
    fn a_window_must_lie_on_the_sphere() {
        let w = |start_ha, delta_ha, start_va, delta_va| Window { start_ha, delta_ha, start_va, delta_va };
        assert!(w(0.0, TAU, 0.0, PI).validate().is_ok());
        assert!(w(6.0, 0.5, 1.0, 0.5).validate().is_ok());
        for bad in [w(TAU, 0.5, 1.0, 0.5), w(-0.1, 0.5, 1.0, 0.5), w(1.0, 0.0, 1.0, 0.5), w(1.0, 7.0, 1.0, 0.5),
                    w(1.0, 0.5, -0.1, 0.5), w(1.0, 0.5, 1.0, 0.0), w(1.0, 0.5, 3.0, 0.5)] {
            assert!(bad.validate().unwrap_err().contains("out of range"), "{bad:?}");
        }
    }

    #[test]
    fn a_scan_request_needs_a_mode_and_a_window_only_for_an_area() {
        let win = Window { start_ha: 1.0, delta_ha: 0.5, start_va: 1.0, delta_va: 0.5 };
        let req = |kind, mode: &str, window| ScanRequest { project_id: "p".into(), kind, mode: mode.into(), color: false, window, note: None };
        assert!(req(ScanKind::FullDome, "STD_L0", None).validate().is_ok());
        assert!(req(ScanKind::Area, "STD_L2", Some(win)).validate().is_ok());
        assert_eq!(req(ScanKind::FullDome, "  ", None).validate().unwrap_err(), "mode is required");
        assert_eq!(req(ScanKind::Area, "STD_L2", None).validate().unwrap_err(), "an area scan needs a window");
        assert_eq!(req(ScanKind::FullDome, "STD_L0", Some(win)).validate().unwrap_err(), "a full-dome scan takes no window");
        assert!(req(ScanKind::Area, "STD_L2", Some(Window { delta_va: 9.0, ..win })).validate().is_err());
        let parsed: ScanRequest = serde_json::from_value(serde_json::json!({"project_id": "p", "kind": "full_dome", "mode": "STD_L0"})).unwrap();
        assert!(!parsed.color && parsed.window.is_none() && parsed.note.is_none());
    }

    #[test]
    fn only_succeeded_failed_and_cancelled_are_final() {
        use JobState::*;
        let finals: Vec<bool> = [AwaitingApproval, Queued, Running, Succeeded, Failed, Cancelled].iter().map(|s| s.is_final()).collect();
        assert_eq!(finals, [false, false, false, true, true, true]);
    }

    #[test]
    fn a_job_leaves_out_what_it_does_not_have() {
        let job = Job { job_id: "job_1".into(), kind: "scan".into(), state: JobState::Queued, phase: None, progress: None, requested_by: "agent".into(),
                        run_id: None, created_at: "t".into(), started_at: None, finished_at: None, result: None, problem: None,
                        partial_scan_id: None, request: None, project_name: None };
        let v = serde_json::to_value(&job).unwrap();
        for absent in ["phase", "progress", "result", "problem", "partial_scan_id", "request", "project_name"] {
            assert!(v.get(absent).is_none(), "{absent}");
        }
        assert_eq!((v["state"].as_str(), v["run_id"].clone()), (Some("queued"), Value::Null));
        let t = serde_json::to_value(Timings { sweep_s: Some(1.5), ..Default::default() }).unwrap();
        assert_eq!(t, serde_json::json!({"sweep_s": 1.5}));
    }

    #[test]
    fn a_grant_flattens_its_request_and_keeps_its_expiry_private() {
        let g: Grant = serde_json::from_value(serde_json::json!({"project_id": "p", "expires_at": "2030-01-01T00:00:00Z", "grant_id": "g1", "granted_by": "op", "scans_used": 2})).unwrap();
        assert_eq!((g.req.max_scans, g.req.allow_color, g.expires_unix), (None, false, 0));
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["project_id"], "p");
        assert!(v.get("expires_unix").is_none() && v.get("note").is_none());
    }
}
