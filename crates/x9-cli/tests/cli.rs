//! The `x9` binary against an in-process mock (`x9_mock::spawn`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use x9_mock::Config;

/// A running mock: keep it alive for the length of the test.
struct Mock {
    _rt: tokio::runtime::Runtime,
    base: String,
}

fn mock(cfg: Config) -> Mock {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let base = rt.block_on(x9_mock::spawn(cfg));
    Mock { _rt: rt, base }
}

fn fast() -> Config {
    Config { speed: 1000.0, cancel_linger: 0.0, ..Config::default() }
}

fn x9(base: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_x9"))
        .env_remove("X9_BASE")
        .arg("--base")
        .arg(base)
        .args(args)
        .output()
        .expect("running x9")
}

/// Run, require success, return stdout.
fn ok(base: &str, args: &[&str]) -> String {
    let out = x9(base, args);
    assert!(
        out.status.success(),
        "x9 {args:?} failed: {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn json(base: &str, args: &[&str]) -> Value {
    serde_json::from_str(&ok(base, args)).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "x9-cli-test-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fixtures(with_jpg: bool) -> PathBuf {
    let d = temp_dir("fixtures");
    std::fs::write(d.join("dome.tzf"), b"fake tzf").unwrap();
    if with_jpg {
        std::fs::write(d.join("dome.jpg"), b"fake jpg!").unwrap();
    }
    d
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn read_only_subcommands_print_json() {
    let m = mock(Config { battery: 64, on_ac: true, ..fast() });
    let b = &m.base;

    let st = json(b, &["status"]);
    assert_eq!(st["batteryLevel"], 64);
    assert_eq!(st["pluggedToAC"], true);

    assert_eq!(json(b, &["level"])["compensatorStatus"], "on");

    let scans = json(b, &["scans"]);
    assert_eq!(scans.as_array().unwrap().len(), 1);
    assert_eq!(json(b, &["scan", "0"])["scanMode"], "STD_L1");

    let modes = json(b, &["scan-modes"]);
    assert_eq!(modes.as_array().unwrap().len(), 4);
    assert_eq!(modes[0]["scanMode"], "STD_L0");

    assert_eq!(json(b, &["projects"])[0]["projectName"], "mock_project");
    assert_eq!(json(b, &["tasks"]), Value::Array(vec![]));
    assert_eq!(json(b, &["errors"])[0]["type"], "NoError");
    assert_eq!(json(b, &["diagnostics"]), Value::Array(vec![]));
    assert_eq!(json(b, &["tzf", "0"])["nbPoints"], 24_844_238u64);
}

#[test]
fn base_can_come_from_the_environment() {
    let m = mock(fast());
    let out = Command::new(env!("CARGO_BIN_EXE_x9"))
        .env("X9_BASE", &m.base)
        .arg("errors")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn download_tzf_and_preview() {
    let fx = fixtures(true);
    let m = mock(Config { fixtures: Some(s(&fx).into()), ..fast() });
    let out = temp_dir("dl");

    let dest = out.join("a.tzf");
    let stdout = ok(&m.base, &["download-tzf", "0", s(&dest)]);
    assert_eq!(stdout.trim(), format!("downloaded 8 bytes -> {}", s(&dest)));
    assert_eq!(std::fs::read(&dest).unwrap(), b"fake tzf");

    let dest = out.join("a.jpg");
    let stdout = ok(&m.base, &["preview", "0", s(&dest)]);
    assert_eq!(stdout.trim(), format!("downloaded 9 bytes -> {}", s(&dest)));
    assert_eq!(std::fs::read(&dest).unwrap(), b"fake jpg!");

    std::fs::remove_dir_all(out).unwrap();
    std::fs::remove_dir_all(fx).unwrap();
}

#[test]
fn pull_dry_run_filters_and_limits() {
    let m = mock(fast());
    let out = temp_dir("dry");
    let o = s(&out);

    let stdout = ok(&m.base, &["pull", "--out", o, "--dry-run"]);
    assert!(stdout.starts_with("1 scans selected, 102.1 MB total"), "{stdout}");
    assert!(stdout.contains("[dry] tzf 0 -> "), "{stdout}");
    assert!(stdout.contains("mock_project/1.tzf (102.1 MB)"), "{stdout}");
    assert!(!out.join("mock_project").exists(), "a dry run writes nothing");

    let stdout = ok(&m.base, &["pull", "--out", o, "--dry-run", "--project", "elsewhere"]);
    assert_eq!(stdout.trim(), "0 scans selected, 0.0 MB total");

    let stdout = ok(&m.base, &["pull", "--out", o, "--dry-run", "--project", "mock_project", "--limit", "0"]);
    assert_eq!(stdout.trim(), "0 scans selected, 0.0 MB total");

    std::fs::remove_dir_all(out).unwrap();
}

#[test]
fn pull_downloads_with_previews() {
    let fx = fixtures(true);
    let m = mock(Config { fixtures: Some(s(&fx).into()), ..fast() });
    let out = temp_dir("pull");

    let stdout = ok(&m.base, &["pull", "--out", s(&out), "--previews", "--limit", "5"]);
    assert!(stdout.contains("pulled "), "{stdout}");
    assert!(stdout.contains("(8 bytes)"), "{stdout}");
    assert!(stdout.contains("preview 9 bytes"), "{stdout}");
    assert_eq!(std::fs::read(out.join("mock_project/1.tzf")).unwrap(), b"fake tzf");
    assert_eq!(std::fs::read(out.join("mock_project/1.preview.jpg")).unwrap(), b"fake jpg!");

    std::fs::remove_dir_all(out).unwrap();
    std::fs::remove_dir_all(fx).unwrap();
}

#[test]
fn pull_reports_a_failed_preview_and_carries_on() {
    let fx = fixtures(false);
    let m = mock(Config { fixtures: Some(s(&fx).into()), ..fast() });
    let out = temp_dir("pullfail");

    let stdout = ok(&m.base, &["pull", "--out", s(&out), "--previews"]);
    assert!(stdout.contains("preview failed: instrument returned status 404"), "{stdout}");
    assert!(out.join("mock_project/1.tzf").exists());

    std::fs::remove_dir_all(out).unwrap();
    std::fs::remove_dir_all(fx).unwrap();
}

#[test]
fn pull_fails_when_a_download_fails() {
    // No fixtures: the TZF itself is 404.
    let m = mock(fast());
    let out = temp_dir("pull404");
    let o = x9(&m.base, &["pull", "--out", s(&out)]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("404"));
    std::fs::remove_dir_all(out).unwrap();
}

#[test]
fn launch_scan_prints_the_task() {
    let m = mock(fast());
    let t = json(
        &m.base,
        &["launch-scan", "--project-name", "mock_project", "--project-uuid", "39187455-8278-428c-89ad-8a4835f95be6", "--scan-mode", "HS_L1"],
    );
    assert_eq!(t["type"], "scan");
    assert_eq!(t["state"], "initializing");
    assert_eq!(t["scanRequest"]["scanMode"], "HS_L1");
    assert_eq!(t["scanRequest"]["projectName"], "mock_project");

}

#[test]
fn launch_scan_refused_exits_nonzero() {
    // A replay with no recorded stations refuses every full dome: a
    // deterministic 409 with no timing involved.
    let rec = temp_dir("replay");
    std::fs::write(rec.join("manifest.json"), b"[]").unwrap();
    let m = mock(Config { replay: Some(s(&rec).into()), ..fast() });
    let o = x9(&m.base, &["launch-scan"]);
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("instrument returned status 409 for /api/v1/scans"), "{err}");
    assert!(err.contains("all 0 recorded stations"), "{err}");
    std::fs::remove_dir_all(rec).unwrap();
}

#[test]
fn failures_exit_nonzero() {
    let m = mock(fast());

    // API error: unknown scan.
    let o = x9(&m.base, &["scan", "99"]);
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("instrument returned status 404 for /api/v1/scans/99"), "{err}");
    assert!(o.stdout.is_empty());

    // Download of a missing file.
    let out = temp_dir("fail");
    let o = x9(&m.base, &["download-tzf", "7", s(&out.join("x.tzf"))]);
    assert_eq!(o.status.code(), Some(1));
    std::fs::remove_dir_all(out).unwrap();

    // Unreachable host.
    let o = x9("http://127.0.0.1:9/api/v1", &["status"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("HTTP transport error"));

    // Bad base URL.
    let o = x9("not a url", &["status"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("invalid base URL"));

    // Usage errors come from clap.
    let o = x9(&m.base, &[]);
    assert_eq!(o.status.code(), Some(2));
    let o = x9(&m.base, &["scan", "not-a-number"]);
    assert_eq!(o.status.code(), Some(2));
}
