//! Recipes v0: TOML files describing a scanning job as steps.
//!
//! ```toml
//! id = "window-measurement"
//! title = "Measure a window"
//! version = "0.1.0"
//!
//! [[inputs]]
//! name = "purpose"
//! type = "choice"
//! choices = ["replacement window", "blinds"]
//!
//! [[steps]]
//! id = "overview"
//! kind = "action"
//! operation = "startScan"
//! arguments = { kind = "full_dome", mode = "STD_L0" }
//!
//! [[steps]]
//! id = "box"
//! kind = "person"
//! instructions = "Draw a box around the window on the overview preview."
//! needs = ["box"]
//! ```
//!
//! Step kinds: `action` (calls an operation), `person` (waits for `advance`
//! with the `needs` fields), `check` (named checks on step `of` must pass).
//! Arguments may use `{{input.<name>}}`, `{{project_id}}` and
//! `{{steps.<id>.<field>…}}`; a string that is exactly one placeholder takes the
//! referenced value's type (an object, a number).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// Operations an `action` step may call.
pub const OPERATIONS: &[&str] = &["startScan", "aimFromPreview", "exportPoints", "measureOpening", "addLogNote"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeInput {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeStep {
    #[serde(alias = "id")]
    pub step_id: String,
    #[serde(default)]
    pub title: String,
    pub kind: String,
    #[serde(default, alias = "operation", skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// For `person` steps: fields the person must supply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// For `person` steps: show this earlier step's scan preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_of: Option<String>,
    /// For `person` steps: when the step is reached, the bridge proposes its
    /// `needs` from this earlier step's output (same-named fields), so the
    /// person only confirms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub propose_from: Option<String>,
    /// For `person` steps: when a person looks at the answer. `now` (the
    /// default): the step waits for a person (the agent may propose). `final`:
    /// the agent answers it itself and a person reviews it at the run's next
    /// `now` person step, so the run asks for one sign-off instead of two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// For `check` steps: the earlier step whose checks are inspected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    /// For `check` steps: names of checks that must be ok.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    #[serde(alias = "id")]
    pub recipe_id: String,
    pub title: String,
    pub version: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub inputs: Vec<RecipeInput>,
    pub steps: Vec<RecipeStep>,
}

/// Every `{{…}}` placeholder in a JSON value.
pub fn placeholders(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(a) = rest.find("{{") {
                let Some(b) = rest[a..].find("}}") else { break };
                out.push(rest[a + 2..a + b].trim().to_string());
                rest = &rest[a + b + 2..];
            }
        }
        Value::Array(xs) => xs.iter().for_each(|x| placeholders(x, out)),
        Value::Object(m) => m.values().for_each(|x| placeholders(x, out)),
        _ => {}
    }
}

impl Recipe {
    pub fn parse(text: &str) -> Result<Recipe, String> {
        let r: Recipe = toml::from_str(text).map_err(|e| e.to_string())?;
        r.validate()?;
        Ok(r)
    }

    pub fn validate(&self) -> Result<(), String> {
        let ok_id = |s: &str| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok_id(&self.recipe_id) {
            return Err(format!("recipe id {:?} must be letters, digits, '-' or '_'", self.recipe_id));
        }
        if self.steps.is_empty() {
            return Err("a recipe needs at least one step".into());
        }
        let inputs: HashSet<&str> = self.inputs.iter().map(|i| i.name.as_str()).collect();
        let mut seen: HashSet<&str> = HashSet::new();
        for (n, s) in self.steps.iter().enumerate() {
            let where_ = format!("step {:?}", s.step_id);
            if !ok_id(&s.step_id) || !seen.insert(&s.step_id) {
                return Err(format!("{where_}: ids must be unique letters, digits, '-' or '_'"));
            }
            match s.kind.as_str() {
                "action" => {
                    let op = s.operation_id.as_deref().unwrap_or("");
                    if !OPERATIONS.contains(&op) {
                        return Err(format!("{where_}: unknown operation {op:?} (allowed: {})", OPERATIONS.join(", ")));
                    }
                }
                "person" => {
                    if s.instructions.as_deref().unwrap_or("").trim().is_empty() {
                        return Err(format!("{where_}: a person step needs instructions"));
                    }
                }
                "check" => {
                    let of = s.of.as_deref().unwrap_or("");
                    if !seen.contains(of) || of == s.step_id {
                        return Err(format!("{where_}: `of` must name an earlier step"));
                    }
                    if s.expect.is_empty() {
                        return Err(format!("{where_}: a check step needs `expect`"));
                    }
                }
                k => return Err(format!("{where_}: unknown kind {k:?} (action, person, check)")),
            }
            if let Some(p) = &s.preview_of {
                if !seen.contains(p.as_str()) || p == &s.step_id {
                    return Err(format!("{where_}: `preview_of` must name an earlier step"));
                }
            }
            if let Some(r) = &s.review {
                if s.kind != "person" {
                    return Err(format!("{where_}: `review` is for person steps"));
                }
                if r != "now" && r != "final" {
                    return Err(format!("{where_}: `review` must be \"now\" or \"final\""));
                }
                if r == "final" && !self.steps[n + 1..].iter().any(|t| t.kind == "person" && t.review.as_deref() != Some("final")) {
                    return Err(format!("{where_}: `review = \"final\"` needs a later person step (review now) where a person reviews it"));
                }
            }
            if let Some(p) = &s.propose_from {
                if s.kind != "person" {
                    return Err(format!("{where_}: `propose_from` is for person steps"));
                }
                if !seen.contains(p.as_str()) || p == &s.step_id {
                    return Err(format!("{where_}: `propose_from` must name an earlier step"));
                }
            }
            let mut refs = Vec::new();
            if let Some(a) = &s.arguments {
                placeholders(a, &mut refs);
            }
            for r in refs {
                let mut parts = r.split('.');
                let ok = match parts.next() {
                    Some("project_id") => parts.next().is_none(),
                    Some("input") => parts.next().is_some_and(|n| inputs.contains(n)),
                    Some("steps") => parts.next().is_some_and(|n| n != s.step_id && seen.contains(n)),
                    _ => false,
                };
                if !ok {
                    return Err(format!("{where_}: placeholder {{{{{r}}}}} must refer to an input, project_id, or an earlier step"));
                }
            }
        }
        Ok(())
    }
}

/// Load every valid `*.toml` recipe in `dir`. Invalid files are returned as
/// (file name, reason) so the caller can report them.
pub fn load_dir(dir: &Path) -> (Vec<Recipe>, Vec<(String, String)>) {
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    let Ok(rd) = std::fs::read_dir(dir) else { return (ok, bad) };
    let mut paths: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|t| Recipe::parse(&t)) {
            Ok(r) if ok.iter().any(|o: &Recipe| o.recipe_id == r.recipe_id) => bad.push((name, format!("duplicate recipe id {:?}", r.recipe_id))),
            Ok(r) => ok.push(r),
            Err(e) => bad.push((name, e)),
        }
    }
    (ok, bad)
}

/// Look up `input.x`, `project_id` or `steps.<id>.<field>…` in the run context.
pub fn lookup(ctx: &Value, path: &str) -> Option<Value> {
    let mut v = ctx;
    for part in path.split('.') {
        v = v.get(part)?;
    }
    Some(v.clone())
}

/// Replace placeholders in a JSON value from the run context.
pub fn resolve(v: &Value, ctx: &Value) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => {
            let t = s.trim();
            if t.starts_with("{{") && t.ends_with("}}") && t.matches("{{").count() == 1 {
                let path = t[2..t.len() - 2].trim();
                lookup(ctx, path).ok_or_else(|| format!("{{{{{path}}}}} has no value yet"))?
            } else {
                let mut out = String::new();
                let mut rest = s.as_str();
                while let Some(a) = rest.find("{{") {
                    let Some(b) = rest[a..].find("}}") else { break };
                    out.push_str(&rest[..a]);
                    let path = rest[a + 2..a + b].trim();
                    match lookup(ctx, path).ok_or_else(|| format!("{{{{{path}}}}} has no value yet"))? {
                        Value::String(x) => out.push_str(&x),
                        other => out.push_str(&other.to_string()),
                    }
                    rest = &rest[a + b + 2..];
                }
                out.push_str(rest);
                Value::String(out)
            }
        }
        Value::Array(xs) => Value::Array(xs.iter().map(|x| resolve(x, ctx)).collect::<Result<_, _>>()?),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| Ok((k.clone(), resolve(x, ctx)?))).collect::<Result<_, String>>()?),
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GOOD: &str = r#"
id = "t"
title = "Test"
version = "0.1.0"
[[inputs]]
name = "purpose"
type = "string"
[[steps]]
id = "overview"
kind = "action"
operation = "startScan"
arguments = { kind = "full_dome", mode = "STD_L0", note = "for {{input.purpose}}" }
[[steps]]
id = "box"
kind = "person"
instructions = "Draw a box."
needs = ["box"]
preview_of = "overview"
[[steps]]
id = "aim"
kind = "action"
operation = "aimFromPreview"
arguments = { reference_scan_id = "{{steps.overview.scan_id}}", box = "{{steps.box.box}}" }
[[steps]]
id = "export"
kind = "action"
operation = "exportPoints"
arguments = { scan_id = "{{steps.overview.scan_id}}", format = "ply" }
[[steps]]
id = "checks"
kind = "check"
of = "export"
expect = ["point_count"]
"#;

    #[test]
    fn parses_and_validates() {
        let r = Recipe::parse(GOOD).unwrap();
        assert_eq!(r.steps.len(), 5);
        assert_eq!(r.steps[2].operation_id.as_deref(), Some("aimFromPreview"));
    }

    #[test]
    fn rejects_bad_recipes() {
        let bad_op = GOOD.replace("operation = \"exportPoints\"", "operation = \"rmRf\"");
        assert!(Recipe::parse(&bad_op).unwrap_err().contains("unknown operation"));
        let forward_ref = GOOD.replace("{{steps.overview.scan_id}}\", box", "{{steps.export.scan_id}}\", box");
        assert!(Recipe::parse(&forward_ref).unwrap_err().contains("earlier step"));
        let dup = GOOD.replace("id = \"box\"", "id = \"overview\"");
        assert!(Recipe::parse(&dup).unwrap_err().contains("unique"));
        let bad_input = GOOD.replace("{{input.purpose}}", "{{input.nope}}");
        assert!(Recipe::parse(&bad_input).is_err());
    }

    #[test]
    fn review_final_needs_a_later_person_step() {
        let final_box = GOOD.replace("preview_of = \"overview\"", "preview_of = \"overview\"\nreview = \"final\"");
        assert!(Recipe::parse(&final_box).unwrap_err().contains("later person step"));
        let with_signoff = format!("{final_box}[[steps]]\nid = \"confirm\"\nkind = \"person\"\ninstructions = \"Confirm.\"\nneeds = [\"ok\"]\n");
        let r = Recipe::parse(&with_signoff).unwrap();
        assert_eq!(r.steps[1].review.as_deref(), Some("final"));
        let bad = GOOD.replace("preview_of = \"overview\"", "preview_of = \"overview\"\nreview = \"later\"");
        assert!(Recipe::parse(&bad).unwrap_err().contains("now"));
    }

    #[test]
    fn resolves_placeholders() {
        let ctx = json!({"project_id": "p1", "input": {"purpose": "blinds"},
                         "steps": {"overview": {"scan_id": "s1"}, "box": {"box": {"x0": 1, "y0": 2, "x1": 3, "y1": 4}}}});
        let v = resolve(&json!({"a": "{{steps.box.box}}", "b": "for {{input.purpose}} in {{project_id}}", "c": 5}), &ctx).unwrap();
        assert_eq!(v, json!({"a": {"x0": 1, "y0": 2, "x1": 3, "y1": 4}, "b": "for blinds in p1", "c": 5}));
        assert!(resolve(&json!("{{steps.later.x}}"), &ctx).is_err());
    }

    #[test]
    fn each_rule_a_recipe_breaks_is_named() {
        let with = |from: &str, to: &str| {
            assert!(GOOD.contains(from), "{from}");
            Recipe::parse(&GOOD.replacen(from, to, 1)).unwrap_err()
        };
        assert!(with("id = \"t\"", "id = \"t t\"").contains("recipe id"));
        assert!(with("id = \"t\"", &format!("id = \"{}\"", "x".repeat(65))).contains("recipe id"));
        assert!(with("instructions = \"Draw a box.\"", "instructions = \"  \"").contains("needs instructions"));
        assert!(with("of = \"export\"", "of = \"checks\"").contains("`of` must name an earlier step"));
        assert!(with("of = \"export\"", "of = \"later\"").contains("`of` must name an earlier step"));
        assert!(with("expect = [\"point_count\"]", "expect = []").contains("needs `expect`"));
        assert!(with("kind = \"check\"", "kind = \"wait\"").contains("unknown kind \"wait\""));
        assert!(with("preview_of = \"overview\"", "preview_of = \"aim\"").contains("`preview_of` must name an earlier step"));
        assert!(with("operation = \"startScan\"", "operation = \"startScan\"\nreview = \"now\"").contains("`review` is for person steps"));
        assert!(with("operation = \"startScan\"", "operation = \"startScan\"\npropose_from = \"box\"").contains("`propose_from` is for person steps"));
        assert!(with("preview_of = \"overview\"", "propose_from = \"box\"").contains("`propose_from` must name an earlier step"));
        assert!(with("{{input.purpose}}", "{{project_id.x}}").contains("placeholder"));
        assert!(with("{{input.purpose}}", "{{weather}}").contains("placeholder"));
        assert!(Recipe::parse("id = \"t\"\ntitle = 3").is_err(), "not a recipe at all");
        assert_eq!(Recipe::parse("id = \"t\"\ntitle = \"T\"\nversion = \"1\"\nsteps = []\n").unwrap_err(), "a recipe needs at least one step");
        // `review = "now"` and `propose_from` on a person step are fine.
        let ok = GOOD.replacen("preview_of = \"overview\"", "preview_of = \"overview\"\nreview = \"now\"", 1);
        assert_eq!(Recipe::parse(&ok).unwrap().steps[1].review.as_deref(), Some("now"));
        let ok = format!("{GOOD}[[steps]]\nid = \"confirm\"\nkind = \"person\"\ninstructions = \"Confirm.\"\nneeds = [\"box\"]\npropose_from = \"box\"\n");
        assert_eq!(Recipe::parse(&ok).unwrap().steps[5].propose_from.as_deref(), Some("box"));
    }

    #[test]
    fn placeholders_are_found_in_strings_lists_and_tables() {
        let mut out = Vec::new();
        placeholders(&json!({"a": ["{{ input.x }} and {{project_id}}", 3], "b": {"c": "{{steps.s.f}}"}, "d": "{{unclosed"}), &mut out);
        out.sort();
        assert_eq!(out, ["input.x", "project_id", "steps.s.f"]);
    }

    #[test]
    fn resolving_keeps_text_around_placeholders_and_writes_values_as_json() {
        let ctx = json!({"project_id": "p1", "input": {"n": 3, "box": {"x0": 1}}, "steps": {"a": {"list": [1, 2]}}});
        assert_eq!(resolve(&json!("n={{input.n}}, box={{input.box}}"), &ctx).unwrap(), json!("n=3, box={\"x0\":1}"));
        assert_eq!(resolve(&json!(" {{ steps.a.list }} "), &ctx).unwrap(), json!([1, 2]));
        assert_eq!(resolve(&json!(["{{project_id}}", true, null]), &ctx).unwrap(), json!(["p1", true, null]));
        assert_eq!(resolve(&json!("left {{open"), &ctx).unwrap(), json!("left {{open"));
        assert_eq!(resolve(&json!("{{a}} and {{b}}"), &json!({"a": "x", "b": "y"})).unwrap(), json!("x and y"));
        assert_eq!(resolve(&json!("x {{input.missing}}"), &ctx).unwrap_err(), "{{input.missing}} has no value yet");
        assert!(resolve(&json!({"k": ["{{nope}}"]}), &ctx).is_err());
        assert_eq!(lookup(&ctx, "steps.a.list"), Some(json!([1, 2])));
        assert_eq!(lookup(&ctx, "steps.b"), None);
    }

    #[test]
    fn a_recipe_folder_loads_good_files_and_reports_the_rest() {
        let dir = std::env::temp_dir().join(crate::util::new_id("x9-recipes"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.toml"), GOOD).unwrap();
        std::fs::write(dir.join("b.toml"), GOOD).unwrap();
        std::fs::write(dir.join("c.toml"), "not = [toml").unwrap();
        std::fs::write(dir.join("d.toml"), include_str!("../tests/recipes/window-measurement.toml")).unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let (ok, bad) = load_dir(&dir);
        assert_eq!(ok.iter().map(|r| r.recipe_id.as_str()).collect::<Vec<_>>(), ["t", "window-measurement"]);
        assert_eq!(bad.len(), 2);
        assert_eq!(bad[0], ("b.toml".to_string(), "duplicate recipe id \"t\"".to_string()));
        assert_eq!(bad[1].0, "c.toml");
        assert_eq!(load_dir(&dir.join("missing")).0.len(), 0);
        std::fs::remove_dir_all(dir).unwrap();
        // A recipe serializes without its empty optional fields.
        let v = serde_json::to_value(&ok[0]).unwrap();
        assert!(v["steps"][0].get("needs").is_none() && v["steps"][1].get("operation_id").is_none());
    }
}
