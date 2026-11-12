//! x9-bridge — the one service that talks to the X9. Every client (MCP agents,
//! the web UI, the `x9` CLI, scripts) uses its HTTP API
//! (`api/bridge-v1.openapi.yaml`), and the rules live here.
//!
//! Environment:
//! - `X9_BASE`         scanner HTTP API, default the X9 over USB
//! - `BRIDGE_ADDR`     listen address, default `127.0.0.1:8795`
//! - `BRIDGE_TOKENS`   token file (required): one `name role token` per line,
//!   roles `viewer` | `agent` | `operator` | `admin`, `#` comments
//! - `BRIDGE_POLL_MS`  scanner poll interval while scanning, default 1000
//! - `BRIDGE_DATA`     cache for scan files, previews and exports,
//!   default `$HOME/.local/share/x9-bridge`
//! - `BRIDGE_RECIPES`  recipe directory (`*.toml`), default `$BRIDGE_DATA/recipes`

mod aim;
mod analyze;
mod ask;
mod bench;
mod count;
mod export;
mod files;
mod list;
mod map;
mod measure;
mod minimap;
mod mission;
mod model;
mod openings;
mod points;
mod planner;
mod problem;
mod recipe;
mod register;
mod routes;
mod rules;
mod runs;
mod save;
mod state;
mod station;
mod ui;
mod util;
mod view;
mod view3d;
mod work;
mod worker;
mod workmode;
#[cfg(test)]
mod testkit;

use anyhow::{bail, Context};
use model::{Caller, Role, Rules};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long open connections get to close after Ctrl-C.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

fn load_tokens(path: &str) -> anyhow::Result<HashMap<String, Caller>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading token file {path}"))?;
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [name, role, token] = parts[..] else { bail!("{path}:{}: expected `name role token`", n + 1) };
        let role: Role = serde_json::from_value(serde_json::json!(role)).with_context(|| format!("{path}:{}: unknown role {role:?}", n + 1))?;
        if token.len() < 16 {
            bail!("{path}:{}: token for {name} is shorter than 16 characters", n + 1);
        }
        out.insert(token.to_string(), Caller { name: name.to_string(), role });
    }
    if out.is_empty() {
        bail!("{path} holds no tokens");
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("register") => return register::cli(&args[1..]),
        Some("bench") => return bench::cli(&args[1..]),
        Some("analyze") => return analyze::cli(&args[1..]),
        Some("ask") => return ask::cli(&args[1..]),
        Some("plan") => return planner::cli(&args[1..]),
        Some("worker") => return workmode::run(&args[1..]).await,
        _ => {}
    }
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let base = env("X9_BASE", x9_client::DEFAULT_BASE);
    let addr = env("BRIDGE_ADDR", "127.0.0.1:8795");
    let tokens_path = std::env::var("BRIDGE_TOKENS").context("BRIDGE_TOKENS must point at a token file")?;
    let poll = Duration::from_millis(env("BRIDGE_POLL_MS", "1000").parse().context("BRIDGE_POLL_MS")?);

    let data_dir = std::path::PathBuf::from(std::env::var("BRIDGE_DATA").unwrap_or_else(|_| {
        format!("{}/.local/share/x9-bridge", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
    }));
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let recipes_dir = std::env::var("BRIDGE_RECIPES").map(std::path::PathBuf::from).unwrap_or_else(|_| data_dir.join("recipes"));
    std::fs::create_dir_all(&recipes_dir).with_context(|| format!("creating {}", recipes_dir.display()))?;
    let tokens = load_tokens(&tokens_path)?;
    let scanner = x9_client::Client::connect(&base)?;
    let st = Arc::new(state::AppState::new(scanner, tokens, Rules::default(), poll, data_dir, recipes_dir.clone()));
    st.log("rule", format!("Bridge {} started; scanner at {base}", routes::VERSION), "bridge", None, &[]);
    let missions = st.load_missions();
    if missions > 0 {
        st.log("note", format!("{missions} mission{} kept from before the restart", if missions == 1 { "" } else { "s" }), "bridge", None, &[]);
    }
    let regs = st.load_registrations();
    if regs > 0 {
        st.log("note", format!("{regs} project registration{} kept from before the restart", if regs == 1 { "" } else { "s" }), "bridge", None, &[]);
    }
    let saves = st.load_saves();
    if saves > 0 {
        st.log("note", format!("The last save of {saves} project{} kept from before the restart", if saves == 1 { "" } else { "s" }), "bridge", None, &[]);
    }
    let kept = st.load_grants();
    if kept > 0 {
        st.log("approval", format!("{kept} grant{} kept from before the restart", if kept == 1 { "" } else { "s" }), "bridge", None, &[]);
    }

    let (recipes, bad) = recipe::load_dir(&recipes_dir);
    for (file, why) in &bad {
        st.log("rule", format!("Recipe {file} skipped: {why}"), "bridge", None, &[]);
    }
    eprintln!("{} recipe(s) in {} ({} skipped)", recipes.len(), recipes_dir.display(), bad.len());
    tokio::spawn(worker::run(st.clone()));
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    eprintln!("x9-bridge {} listening on http://{addr}/v1 (scanner {base})", routes::VERSION);
    // On Ctrl-C, finish requests in flight, but an event stream never
    // finishes: give open connections a few seconds, then stop anyway.
    let stopping = Arc::new(tokio::sync::Notify::new());
    let signal = stopping.clone();
    let server = axum::serve(listener, routes::router(st)).with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.notify_one();
    });
    tokio::select! {
        served = server => served?,
        _ = async {
            stopping.notified().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => eprintln!("x9-bridge: open connections did not close within {} s; stopping", SHUTDOWN_GRACE.as_secs()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_file(text: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(util::new_id("x9-tokens"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens");
        std::fs::write(&path, text).unwrap();
        let p = path.to_string_lossy().into_owned();
        (dir, p)
    }

    #[test]
    fn load_tokens_reads_name_role_token_lines_and_skips_comments() {
        let (dir, p) = token_file("# the crew\n\n  crew operator operator-token-0123456789\nbot   agent\tagent-token-0123456789abc  \n");
        let t = load_tokens(&p).unwrap();
        assert_eq!(t.len(), 2);
        let crew = &t["operator-token-0123456789"];
        assert_eq!((crew.name.as_str(), crew.role), ("crew", Role::Operator));
        assert_eq!(t["agent-token-0123456789abc"].role, Role::Agent);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_tokens_names_the_line_that_is_wrong() {
        for (text, want) in [
            ("crew operator\n", "tokens:1: expected `name role token`"),
            ("# ok\ncrew operator token-0123456789abcdef extra\n", "tokens:2: expected `name role token`"),
            ("crew root token-0123456789abcdef\n", "tokens:1: unknown role \"root\""),
            ("crew viewer short\n", "tokens:1: token for crew is shorter than 16 characters"),
            ("# nobody\n\n", "holds no tokens"),
        ] {
            let (dir, p) = token_file(text);
            let e = format!("{:#}", load_tokens(&p).unwrap_err());
            assert!(e.contains(want), "{text:?}: {e}");
            std::fs::remove_dir_all(dir).unwrap();
        }
        let e = format!("{:#}", load_tokens("/nonexistent/x9-tokens").unwrap_err());
        assert!(e.starts_with("reading token file /nonexistent/x9-tokens"), "{e}");
    }

    /// `main` as the service starts, up to binding its address: every step
    /// before it runs, and a bad address stops it there. (Skipped when the
    /// test filter happens to be a subcommand name, which `main` would run.)
    #[test]
    fn main_reads_its_environment_and_stops_at_the_first_problem() {
        let subcommands = ["register", "bench", "analyze", "ask", "plan", "worker"];
        if std::env::args().nth(1).is_some_and(|a| subcommands.contains(&a.as_str())) {
            return;
        }
        let vars = ["BRIDGE_TOKENS", "BRIDGE_POLL_MS", "BRIDGE_DATA", "BRIDGE_RECIPES", "BRIDGE_ADDR", "X9_BASE"];
        let clear = || vars.iter().for_each(|k| std::env::remove_var(k));
        clear();
        let err = |r: anyhow::Result<()>| format!("{:#}", r.unwrap_err());
        assert!(err(main()).contains("BRIDGE_TOKENS must point at a token file"));

        let (dir, tokens) = token_file("crew operator operator-token-0123456789\n");
        std::env::set_var("BRIDGE_TOKENS", &tokens);
        std::env::set_var("BRIDGE_POLL_MS", "soon");
        assert!(err(main()).contains("BRIDGE_POLL_MS"));

        // Everything in place but the address: state kept from before a
        // restart is loaded, a bad recipe is skipped, then binding fails.
        let (data, recipes) = (dir.join("data"), dir.join("recipes"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&recipes).unwrap();
        std::fs::write(recipes.join("broken.toml"), "id = \"x\"\n").unwrap();
        std::fs::write(data.join("grants.json"), r#"[{"project_id": "p", "expires_at": "2099-01-01T00:00:00Z", "grant_id": "g1", "granted_by": "crew", "scans_used": 0}]"#).unwrap();
        std::fs::write(data.join("registrations.json"), r#"[{"project_id": "p", "frame_scan_id": null, "stations": [], "pairs": [], "walls": [], "summary": "", "updated_at": ""}]"#).unwrap();
        std::fs::write(data.join("missions.json"), r#"[{"mission_id": "m1", "project_id": "p", "goal": "Scan the yard", "kind": "custom", "done_when": "seen",
                                                        "state": "active", "created_by": "crew", "created_at": "2026-11-10T10:00:00Z", "targets": []}]"#).unwrap();
        std::env::set_var("BRIDGE_POLL_MS", "50");
        std::env::set_var("BRIDGE_DATA", &data);
        std::env::set_var("BRIDGE_RECIPES", &recipes);
        std::env::set_var("X9_BASE", "http://127.0.0.1:9/api/v1");
        std::env::set_var("BRIDGE_ADDR", "not-an-address");
        assert!(err(main()).starts_with("binding not-an-address"));
        // BRIDGE_RECIPES defaults to a folder in the data directory.
        std::env::remove_var("BRIDGE_RECIPES");
        std::env::set_var("BRIDGE_DATA", dir.join("fresh"));
        assert!(err(main()).starts_with("binding not-an-address"));
        assert!(dir.join("fresh").join("recipes").is_dir());
        clear();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
