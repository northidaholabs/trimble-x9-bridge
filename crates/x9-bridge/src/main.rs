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
mod export;
mod files;
mod map;
mod measure;
mod minimap;
mod mission;
mod model;
mod planner;
mod problem;
mod recipe;
mod register;
mod routes;
mod rules;
mod runs;
mod state;
mod station;
mod ui;
mod util;
mod view;
mod view3d;
mod work;
mod worker;
mod workmode;

use anyhow::{bail, Context};
use model::{Caller, Role, Rules};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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
    axum::serve(listener, routes::router(st))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
