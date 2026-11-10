//! `x9-bridge bench`: measure registration on recorded surveys, so a change
//! can be shown to be better rather than believed to be.
//!
//! Registration is deterministic for given inputs (the same files in the
//! same order give byte-identical poses), so what varies on site is the
//! input: the order stations arrive in, and the scans themselves. The bench
//! fixes the scans and varies the order on purpose: the given order,
//! reversed, and seeded shuffles. It then plants wrong placements the audit
//! must catch, and scores everything by what each mistake costs:
//!
//! - a **wrong accept** (trusted but wrong, or a planted error missed) is the
//!   expensive one: the person has already left. A build passes only with
//!   none.
//! - a **wrong reject** (right but not trusted, or a good station flagged
//!   because of a planted one) costs a re-scan or a question.
//! - **unplaced** stations, the **error** of trusted stations against the
//!   reference, solve **time** and peak **memory**.
//!
//! It is gentle on the machine it runs on: each station is decoded once and
//! prepared once (the decoded cloud is dropped), cases run one after
//! another, and the solver uses `--threads` threads (default: a quarter of
//! the cores, at most 8). It refuses to start when memory is short.

use crate::register::{self, AuditCache, Cloud, Pose, Prepared};
use anyhow::Context;
use serde::Serialize;
use serde_json::json;

/// A placement this close to the reference counts as right.
pub const RIGHT_MM: f64 = 30.0;
pub const RIGHT_DEG: f64 = 0.1;
/// Memory the bench wants free before it starts (it needs about the decode
/// peak, ~1 GB, plus 40 MB per prepared station).
const MIN_FREE_MB: u64 = 3072;

type P4 = [f64; 4];
/// Per station: (distance mm, turn °) from the reference, if both exist.
type Errors = Vec<Option<(f64, f64)>>;
/// The given order's poses, pairs and which stations were trusted.
type Solved = (Vec<Option<Pose>>, Vec<register::Pair>, Vec<bool>);

fn p4(p: &Pose) -> P4 {
    [p.turn_deg.to_radians(), p.xyz_m[0], p.xyz_m[1], p.xyz_m[2]]
}

fn compose(a: P4, b: P4) -> P4 {
    let (s, c) = a[0].sin_cos();
    [a[0] + b[0], a[1] + c * b[1] - s * b[2], a[2] + s * b[1] + c * b[2], a[3] + b[3]]
}

fn invert(r: P4) -> P4 {
    let th = -r[0];
    let (s, c) = th.sin_cos();
    [th, -(c * r[1] - s * r[2]), -(s * r[1] + c * r[2]), -r[3]]
}

fn wrap_deg(d: f64) -> f64 {
    (d + 180.0).rem_euclid(360.0) - 180.0
}

/// How far each placed station is from the reference (mm, °), after bringing
/// the solve's frame onto the reference's. The frame is fixed through the
/// station that agrees with the most others, so one bad station can't make
/// all the rest look wrong.
pub fn errors(poses: &[Option<Pose>], truth: &[Option<Pose>]) -> Errors {
    let n = poses.len().min(truth.len());
    let err_with = |t: P4| -> Errors {
        (0..n)
            .map(|k| {
                let (p, r) = (poses[k].as_ref()?, truth[k].as_ref()?);
                let e = compose(t, p4(p));
                let r = p4(r);
                let mm = ((e[1] - r[1]).powi(2) + (e[2] - r[2]).powi(2) + (e[3] - r[3]).powi(2)).sqrt() * 1000.0;
                Some((mm, wrap_deg((e[0] - r[0]).to_degrees()).abs()))
            })
            .collect()
    };
    let mut best: Option<(usize, f64, Errors)> = None;
    for a in 0..n {
        let (Some(p), Some(r)) = (&poses[a], &truth[a]) else { continue };
        let e = err_with(compose(p4(r), invert(p4(p))));
        let right = e.iter().flatten().filter(|(mm, deg)| *mm <= RIGHT_MM && *deg <= RIGHT_DEG).count();
        let sum: f64 = e.iter().flatten().map(|(mm, _)| mm.min(10_000.0)).sum();
        if best.as_ref().is_none_or(|(br, bs, _)| right > *br || (right == *br && sum < *bs)) {
            best = Some((right, sum, e));
        }
    }
    let mut out = best.map(|b| b.2).unwrap_or_else(|| vec![None; n]);
    out.resize(poses.len(), None);
    out
}

/// The orders to run: the given one, reversed, then seeded shuffles.
pub fn orders(n: usize, count: usize) -> Vec<(String, Vec<usize>)> {
    let mut out = vec![("given".to_string(), (0..n).collect::<Vec<_>>())];
    if count > 1 {
        out.push(("reversed".into(), (0..n).rev().collect()));
    }
    for seed in 1..count.saturating_sub(1) as u64 + 1 {
        if out.len() >= count {
            break;
        }
        let mut v: Vec<usize> = (0..n).collect();
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for i in (1..n).rev() {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let j = (s.wrapping_mul(0x2545_F491_4F6C_DD1D) % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
        out.push((format!("shuffle {seed}"), v));
    }
    out
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub struct Score {
    pub stations: usize,
    pub wrong_accepts: usize,
    pub wrong_rejects: usize,
    pub unplaced: usize,
    pub trusted_right: usize,
    pub no_reference: usize,
}

/// Score one solve: `trusted[k]` from the audit, `err[k]` against the reference.
pub fn score(trusted: &[bool], placed: &[bool], err: &[Option<(f64, f64)>]) -> Score {
    let mut s = Score { stations: trusted.len(), ..Default::default() };
    for k in 0..trusted.len() {
        if !placed[k] {
            s.unplaced += 1;
            continue;
        }
        match err[k] {
            None => s.no_reference += 1,
            Some((mm, deg)) => {
                let right = mm <= RIGHT_MM && deg <= RIGHT_DEG;
                match (trusted[k], right) {
                    (true, true) => s.trusted_right += 1,
                    (true, false) => s.wrong_accepts += 1,
                    (false, true) => s.wrong_rejects += 1,
                    (false, false) => {}
                }
            }
        }
    }
    s
}

#[derive(Serialize)]
struct OrderResult {
    order: String,
    sequence: Vec<String>,
    score: Score,
    verdicts: Vec<String>,
    /// Each station's counting ties (station numbers).
    ties: Vec<Vec<String>>,
    error_mm: Vec<Option<f64>>,
    solve_s: f64,
    audit_s: f64,
}

#[derive(Serialize)]
struct PlantResult {
    station: String,
    moved: [f64; 3],
    caught: bool,
    collateral: Vec<String>,
}

fn mem_available_mb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let l = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
    l.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb / 1024)
}

fn peak_mb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let l = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    l.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb / 1024)
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

const USAGE: &str = "usage: x9-bridge bench [--truth poses.json] [--orders N] [--plant N:dx,dy,deg]… [--no-plants] \
[--threads T] [--json OUT.json] [--scoreboard FILE] [--label TEXT] [--write-reference poses.json] SURVEY_DIR | STATION.tzf…";

/// `x9-bridge bench …`: see the module notes.
pub fn cli(args: &[String]) -> anyhow::Result<()> {
    let mut truth: Option<(String, Vec<Option<Pose>>)> = None;
    let mut n_orders = 4usize;
    let mut plants: Vec<(String, [f64; 3])> = Vec::new();
    let mut no_plants = false;
    let cores = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(1);
    let mut threads = (cores / 4).clamp(1, 8);
    let (mut json_out, mut board, mut label, mut write_ref) = (None, None, String::new(), None);
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().context(USAGE);
        match a.as_str() {
            "--truth" => {
                let p = val()?;
                truth = Some((p.clone(), serde_json::from_slice(&std::fs::read(&p).with_context(|| format!("reading {p}"))?)?));
            }
            "--orders" => n_orders = val()?.parse::<usize>()?.max(1),
            "--plant" => {
                let v = val()?;
                let (n, d) = v.split_once(':').context("--plant N:dx,dy,deg")?;
                let d: Vec<f64> = d.split(',').map(str::parse).collect::<Result<_, _>>()?;
                anyhow::ensure!(d.len() == 3, "--plant N:dx,dy,deg");
                plants.push((n.to_string(), [d[0], d[1], d[2]]));
            }
            "--no-plants" => no_plants = true,
            "--threads" => threads = val()?.parse::<usize>()?.max(1),
            "--json" => json_out = Some(val()?),
            "--scoreboard" => board = Some(val()?),
            "--label" => label = val()?,
            "--write-reference" => write_ref = Some(val()?),
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            _ => rest.push(a.clone()),
        }
    }
    // A folder means its station_*.tzf (or *.tzf) files, sorted.
    let mut files: Vec<String> = Vec::new();
    for r in &rest {
        let p = std::path::Path::new(r);
        if p.is_dir() {
            let mut v: Vec<String> = std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|q| q.extension().is_some_and(|x| x == "tzf"))
                .map(|q| q.to_string_lossy().into_owned())
                .collect();
            v.sort();
            files.extend(v);
        } else {
            files.push(r.clone());
        }
    }
    anyhow::ensure!(files.len() >= 2, "{USAGE}\n(need at least two stations)");
    if let Some(free) = mem_available_mb() {
        let want = MIN_FREE_MB + 40 * files.len() as u64;
        anyhow::ensure!(free >= want, "only {free} MB of memory free; the bench wants {want} MB. Stop something heavy first.");
    }
    register::set_thread_cap(threads);

    let ids: Vec<String> = files.iter().enumerate().map(|(i, f)| station_number(i, f).to_string()).collect();
    let fingerprint = fingerprint(&files)?;
    eprintln!("bench: {} stations, {} orders, {} threads (of {cores})", files.len(), n_orders, threads);

    // Decode and prepare once; keep only the prepared stations.
    let t0 = std::time::Instant::now();
    let mut prep: Vec<Prepared> = Vec::new();
    for f in &files {
        let t = trimble_tzf::Tzf::open(std::path::Path::new(f)).with_context(|| format!("opening {f}"))?;
        let meta = t.scan_meta().with_context(|| format!("{f}: metadata"))?;
        let pts = t.grid_points().with_context(|| format!("{f}: decoding"))?;
        let cloud = Cloud::from_grid(&pts, crate::view::step_rad(&meta));
        drop(pts);
        prep.push(register::prepare(&cloud));
    }
    let prepare_s = t0.elapsed().as_secs_f64();
    eprintln!("prepared {} stations in {prepare_s:.0} s", prep.len());

    let mut results: Vec<OrderResult> = Vec::new();
    let mut given: Option<Solved> = None;
    for (name, order) in orders(files.len(), n_orders) {
        let o_ids: Vec<String> = order.iter().map(|&i| ids[i].clone()).collect();
        let o_prep: Vec<&Prepared> = order.iter().map(|&i| &prep[i]).collect();
        let t = std::time::Instant::now();
        let mut pairs = Vec::new();
        let mut o_poses = Vec::new();
        for k in 1..=order.len() {
            let (p2, groups) = register::register_all(&o_ids[..k], &o_prep[..k], pairs, &|_, _| {});
            pairs = p2;
            o_poses = groups.poses();
        }
        let solve_s = t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        let o_audits = register::audit(&o_ids, &o_ids, &o_prep, &o_poses, &pairs, &mut AuditCache::new());
        let audit_s = t.elapsed().as_secs_f64();
        // Back to file order.
        let mut poses: Vec<Option<Pose>> = vec![None; files.len()];
        let mut verdicts = vec![String::new(); files.len()];
        let mut trusted = vec![false; files.len()];
        let mut ties = vec![Vec::new(); files.len()];
        for (j, &i) in order.iter().enumerate() {
            poses[i] = o_poses[j];
            verdicts[i] = o_audits[j].verdict.clone();
            ties[i] = o_audits[j].ties.clone();
            trusted[i] = o_audits[j].trusted();
        }
        if truth.is_none() {
            truth = Some(("own solve, given order".into(), poses.clone()));
        }
        let reference = &truth.as_ref().unwrap().1;
        let err = errors(&poses, reference);
        let placed: Vec<bool> = poses.iter().map(Option::is_some).collect();
        let sc = score(&trusted, &placed, &err);
        eprintln!(
            "  {name:<10} wrong accepts {} · wrong rejects {} · unplaced {} · solve {solve_s:.0} s, audit {audit_s:.0} s",
            sc.wrong_accepts, sc.wrong_rejects, sc.unplaced
        );
        for k in 0..files.len() {
            let bad = match (trusted[k], err[k]) {
                (true, Some((mm, deg))) => mm > RIGHT_MM || deg > RIGHT_DEG,
                (false, Some((mm, deg))) => mm <= RIGHT_MM && deg <= RIGHT_DEG,
                _ => !placed[k],
            };
            if bad {
                eprintln!("      station {}: {} {} (ties {})", ids[k], verdicts[k], err[k].map(|(mm, d)| format!("{mm:.0} mm, {d:.2}° off")).unwrap_or_default(), ties[k].join(", "));
            }
        }
        if given.is_none() {
            given = Some((poses.clone(), pairs.clone(), trusted.clone()));
        }
        results.push(OrderResult {
            order: name,
            sequence: o_ids,
            score: sc,
            verdicts,
            ties,
            error_mm: err.iter().map(|e| e.map(|(mm, _)| (mm * 10.0).round() / 10.0)).collect(),
            solve_s,
            audit_s,
        });
    }
    if let Some(p) = &write_ref {
        let (poses, _, _) = given.as_ref().unwrap();
        std::fs::write(p, serde_json::to_vec_pretty(poses)?)?;
        eprintln!("wrote the given order's poses to {p}");
    }

    // Planted errors on the given order's solve: the moved station must not
    // be trusted, and stations trusted before should stay trusted.
    let (g_poses, g_pairs, g_trusted) = given.unwrap();
    if plants.is_empty() && !no_plants {
        let n = files.len();
        let picks: Vec<usize> = {
            let mut v = vec![1usize.min(n - 1), n / 2, n - 1];
            v.dedup();
            v
        };
        for &k in &picks {
            for d in [[0.03, 0.0, 0.0], [0.0, 0.3, 0.0], [0.0, 0.0, 1.0]] {
                plants.push((ids[k].clone(), d));
            }
        }
    }
    let prep_refs: Vec<&Prepared> = prep.iter().collect();
    let mut plant_results = Vec::new();
    for (st, d) in &plants {
        let Some(k) = ids.iter().position(|i| i == st) else {
            eprintln!("  plant: no station {st}");
            continue;
        };
        let mut poses = g_poses.clone();
        let Some(p) = poses[k].as_mut() else {
            eprintln!("  plant: station {st} was not placed");
            continue;
        };
        p.xyz_m[0] += d[0];
        p.xyz_m[1] += d[1];
        p.turn_deg += d[2];
        let a = register::audit(&ids, &ids, &prep_refs, &poses, &g_pairs, &mut AuditCache::new());
        let caught = !a[k].trusted();
        let collateral: Vec<String> = (0..ids.len()).filter(|&j| j != k && g_trusted[j] && !a[j].trusted()).map(|j| ids[j].clone()).collect();
        eprintln!(
            "  plant station {st} moved ({}, {}) m, {}°: {}{}",
            d[0], d[1], d[2],
            if caught { "caught" } else { "MISSED" },
            if collateral.is_empty() { String::new() } else { format!(", also flagged {}", collateral.join(", ")) }
        );
        plant_results.push(PlantResult { station: st.clone(), moved: *d, caught, collateral });
    }

    // Totals.
    let mut total = Score::default();
    for r in &results {
        total.stations += r.score.stations;
        total.wrong_accepts += r.score.wrong_accepts;
        total.wrong_rejects += r.score.wrong_rejects;
        total.unplaced += r.score.unplaced;
        total.trusted_right += r.score.trusted_right;
        total.no_reference += r.score.no_reference;
    }
    let missed = plant_results.iter().filter(|p| !p.caught).count();
    let collateral: usize = plant_results.iter().map(|p| p.collateral.len()).sum();
    let mut errs: Vec<f64> = results
        .iter()
        .flat_map(|r| r.error_mm.iter().zip(&r.verdicts).filter(|(_, v)| matches!(v.as_str(), "confirmed" | "single" | "frame")).filter_map(|(e, _)| *e))
        .collect();
    let err_max = errs.iter().cloned().fold(None, |m: Option<f64>, e| Some(m.map_or(e, |m| m.max(e))));
    let err_median = median(&mut errs);
    let mut solve: Vec<f64> = results.iter().map(|r| r.solve_s).collect();
    let solve_median = median(&mut solve);
    let pass = total.wrong_accepts == 0 && missed == 0;
    let summary = json!({
        "pass": pass,
        "wrong_accepts": total.wrong_accepts + missed,
        "wrong_accepts_from_orders": total.wrong_accepts,
        "planted_missed": missed,
        "wrong_rejects": total.wrong_rejects + collateral,
        "wrong_rejects_from_orders": total.wrong_rejects,
        "planted_collateral": collateral,
        "unplaced": total.unplaced,
        "station_solves": total.stations,
        "plants": plant_results.len(),
        "error_mm_median": err_median.map(|e| (e * 10.0).round() / 10.0),
        "error_mm_max": err_max.map(|e| (e * 10.0).round() / 10.0),
        "solve_s_median": solve_median.map(|s| s.round()),
        "prepare_s": prepare_s.round(),
        "peak_mb": peak_mb(),
    });
    eprintln!(
        "{}: wrong accepts {} (orders {}, planted missed {}/{}), wrong rejects {} (orders {}, collateral {}), unplaced {}, error median {} mm max {} mm, solve median {} s, peak {} MB",
        if pass { "PASS" } else { "FAIL" },
        total.wrong_accepts + missed, total.wrong_accepts, missed, plant_results.len(),
        total.wrong_rejects + collateral, total.wrong_rejects, collateral, total.unplaced,
        err_median.map_or("-".into(), |e| format!("{e:.0}")), err_max.map_or("-".into(), |e| format!("{e:.0}")),
        solve_median.map_or("-".into(), |s| format!("{s:.0}")), peak_mb().map_or("-".into(), |m| m.to_string()),
    );
    let reference = truth.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "label": label,
        "survey": fingerprint,
        "reference": reference,
        "right_if_within": {"mm": RIGHT_MM, "deg": RIGHT_DEG},
        "threads": threads,
        "summary": summary,
        "orders": results,
        "plants": plant_results,
    });
    if let Some(p) = json_out {
        std::fs::write(&p, serde_json::to_vec_pretty(&report)?)?;
    }
    if let Some(p) = board {
        let when = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let line = json!({"when_unix": when, "version": env!("CARGO_PKG_VERSION"), "label": report["label"], "survey": report["survey"],
                          "reference": reference, "orders": n_orders, "threads": threads, "summary": summary});
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        writeln!(f, "{line}")?;
    }
    if !pass {
        std::process::exit(1);
    }
    Ok(())
}

/// Station numbers from the file names (station_07.tzf, 7.tzf), else their order.
fn station_number(i: usize, f: &str) -> u16 {
    let stem = std::path::Path::new(f).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let digits: String = stem.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse().unwrap_or(i as u16 + 1)
}

/// Which survey this was: its folder, station count and a hash of the file
/// names and sizes, so scores are only compared on the same set.
fn fingerprint(files: &[String]) -> anyhow::Result<serde_json::Value> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut bytes = 0u64;
    for f in files {
        let len = std::fs::metadata(f)?.len();
        bytes += len;
        let name = std::path::Path::new(f).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for b in name.bytes().chain(len.to_le_bytes()) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    let dir = std::path::Path::new(&files[0]).parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(json!({"name": dir, "stations": files.len(), "bytes": bytes, "hash": format!("{h:016x}")}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn pose(deg: f64, x: f64, y: f64) -> Option<Pose> {
        Some(Pose { turn_deg: deg, xyz_m: [x, y, 0.0] })
    }

    #[test]
    fn orders_are_permutations_and_repeatable() {
        let o = orders(12, 4);
        assert_eq!(o.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["given", "reversed", "shuffle 1", "shuffle 2"]);
        for (_, v) in &o {
            let mut s = v.clone();
            s.sort();
            assert_eq!(s, (0..12).collect::<Vec<_>>());
        }
        assert_ne!(o[2].1, o[3].1, "different seeds, different orders");
        assert_eq!(orders(12, 4)[3].1, o[3].1, "the same seed gives the same order");
        assert_eq!(orders(5, 1).len(), 1);
    }

    #[test]
    fn errors_ignore_the_frame_and_one_bad_station() {
        let truth = vec![pose(0.0, 0.0, 0.0), pose(90.0, 5.0, 0.0), pose(0.0, 5.0, 5.0), pose(10.0, 0.0, 8.0)];
        // The same layout seen from station 2's frame, turned 30°, with
        // station 3 placed 0.5 m wrong. Build it by mapping truth through a
        // frame change, then spoil station 3.
        let t = [30f64.to_radians(), 2.0, -1.0, 0.0];
        let mut poses: Vec<Option<Pose>> = truth
            .iter()
            .map(|p| {
                let q = compose(t, p4(p.as_ref().unwrap()));
                Some(Pose { turn_deg: q[0].to_degrees(), xyz_m: [q[1], q[2], q[3]] })
            })
            .collect();
        poses[2].as_mut().unwrap().xyz_m[0] += 0.5;
        let e = errors(&poses, &truth);
        assert!(e[0].unwrap().0 < 0.01 && e[1].unwrap().0 < 0.01 && e[3].unwrap().0 < 0.01, "{e:?}");
        assert!((e[2].unwrap().0 - 500.0).abs() < 0.01, "only the spoiled station is off: {e:?}");
    }

    #[test]
    fn score_counts_by_cost() {
        let trusted = [true, true, false, false, false];
        let placed = [true, true, true, true, false];
        let err = [Some((5.0, 0.01)), Some((80.0, 0.01)), Some((4.0, 0.0)), Some((400.0, 2.0)), None];
        let s = score(&trusted, &placed, &err);
        assert_eq!(s, Score { stations: 5, wrong_accepts: 1, wrong_rejects: 1, unplaced: 1, trusted_right: 1, no_reference: 0 });
        // A small turn error alone also makes a placement wrong.
        assert_eq!(score(&[true], &[true], &[Some((1.0, 0.5))]).wrong_accepts, 1);
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_command_line_refuses_what_it_cannot_run() {
        cli(&args(&["--help"])).unwrap();
        let err = |v: &[&str]| format!("{:#}", cli(&args(v)).unwrap_err());
        assert!(err(&[]).contains("need at least two stations"));
        assert!(err(&["one.tzf"]).contains("need at least two stations"));
        assert!(err(&["--orders"]).starts_with("usage: x9-bridge bench"));
        assert!(err(&["--orders", "many", "a", "b"]).contains("invalid digit"));
        assert!(err(&["--threads", "-1", "a", "b"]).contains("invalid digit"));
        assert!(err(&["--plant", "3", "a", "b"]).contains("--plant N:dx,dy,deg"));
        assert!(err(&["--plant", "3:0.1,0", "a", "b"]).contains("--plant N:dx,dy,deg"));
        assert!(err(&["--plant", "3:x,0,0", "a", "b"]).contains("invalid float"));
        assert!(err(&["--truth", "/nonexistent/poses.json", "a", "b"]).contains("reading /nonexistent/poses.json"));
        // Files that don't exist, or aren't scans.
        assert!(err(&["/nonexistent/1.tzf", "/nonexistent/2.tzf"]).contains("No such file"));
        let dir = crate::export::tests::scratch("x9-bench-junk");
        for f in ["station_02.tzf", "station_01.tzf", "notes.txt"] {
            std::fs::write(dir.join(f), b"not a scan").unwrap();
        }
        let e = err(&[dir.to_str().unwrap(), "--label", "x", "--json", "/nonexistent/out.json", "--scoreboard", "/nonexistent/b", "--no-plants", "--write-reference", "/nonexistent/r"]);
        assert!(e.contains("opening") && e.contains("station_01.tzf"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stations_are_numbered_from_their_file_names_else_by_order() {
        assert_eq!(station_number(0, "/s/station_07.tzf"), 7);
        assert_eq!(station_number(4, "/s/12.tzf"), 12);
        assert_eq!(station_number(2, "/s/north.tzf"), 3);
        assert_eq!(station_number(0, ""), 1);
    }

    #[test]
    fn a_fingerprint_changes_with_names_and_sizes_only() {
        let dir = crate::export::tests::scratch("x9-bench-fp");
        let survey = dir.join("yard");
        std::fs::create_dir_all(&survey).unwrap();
        let f = |n: &str, len: usize| {
            let p = survey.join(n);
            std::fs::write(&p, vec![7u8; len]).unwrap();
            p.to_string_lossy().into_owned()
        };
        let files = vec![f("1.tzf", 10), f("2.tzf", 20)];
        let a = fingerprint(&files).unwrap();
        assert_eq!((a["name"].as_str(), a["stations"].as_u64(), a["bytes"].as_u64()), (Some("yard"), Some(2), Some(30)));
        std::fs::write(&files[1], vec![9u8; 20]).unwrap();
        assert_eq!(fingerprint(&files).unwrap()["hash"], a["hash"], "contents don't count");
        std::fs::write(&files[1], vec![9u8; 21]).unwrap();
        assert_ne!(fingerprint(&files).unwrap()["hash"], a["hash"]);
        assert!(fingerprint(&["/nonexistent/1.tzf".to_string()]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn small_helpers_compose_invert_wrap_and_take_medians() {
        let a = [0.7, 1.0, -2.0, 0.5];
        let back = compose(a, invert(a));
        assert!(back.iter().all(|v| v.abs() < 1e-12), "{back:?}");
        assert_eq!(wrap_deg(190.0), -170.0);
        assert_eq!(wrap_deg(-190.0), 170.0);
        assert_eq!(median(&mut Vec::new()), None);
        assert_eq!(median(&mut [5.0, 1.0, 3.0, 9.0]), Some(5.0));
        // No station with both a pose and a reference: nothing to compare.
        assert_eq!(errors(&[None, pose(0.0, 1.0, 0.0)], &[pose(0.0, 0.0, 0.0), None, pose(0.0, 0.0, 0.0)]), vec![None, None]);
        assert_eq!(orders(3, 2).iter().map(|o| o.1.clone()).collect::<Vec<_>>(), vec![vec![0, 1, 2], vec![2, 1, 0]]);
    }

    /// The bench on two synthetic stations of one walled yard, a metre
    /// apart: the given order only (so its own solve is the reference), a
    /// plant on a station that doesn't exist, every output written; then
    /// again against the reference it wrote.
    #[test]
    fn the_bench_scores_a_small_survey_and_writes_its_reports() {
        use crate::export::tests::{room_dome, scratch};
        let dir = scratch("x9-bench-run");
        let survey = dir.join("yard");
        std::fs::create_dir_all(&survey).unwrap();
        std::fs::write(survey.join("station_01.tzf"), room_dome(720, [-6.0, -4.0, -1.5], [5.0, 6.0, 3.0])).unwrap();
        std::fs::write(survey.join("station_02.tzf"), room_dome(720, [-7.0, -4.5, -1.5], [4.0, 5.5, 3.0])).unwrap();
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        let (json_out, board, reference) = (p("bench.json"), p("scores.jsonl"), p("reference.json"));
        let survey = survey.to_string_lossy().into_owned();
        cli(&args(&["--orders", "1", "--threads", "2", "--plant", "9:0.1,0,0", "--label", "synthetic", "--json", &json_out,
                    "--scoreboard", &board, "--write-reference", &reference, &survey])).unwrap();
        let r: Value = serde_json::from_slice(&std::fs::read(&json_out).unwrap()).unwrap();
        assert_eq!(r["label"], "synthetic");
        assert_eq!(r["survey"]["stations"], 2);
        assert_eq!(r["reference"], "own solve, given order");
        assert_eq!(r["summary"]["pass"], true);
        assert_eq!(r["summary"]["plants"], 0, "no station 9 to plant on");
        assert_eq!(r["orders"].as_array().unwrap().len(), 1);
        assert_eq!(r["orders"][0]["sequence"], json!(["1", "2"]));
        let poses: Vec<Option<Pose>> = serde_json::from_slice(&std::fs::read(&reference).unwrap()).unwrap();
        assert_eq!(poses.len(), 2);
        // Against the written reference, with a plant on each station; the scoreboard grows a line a run.
        cli(&args(&["--orders", "1", "--truth", &reference, "--no-plants", "--scoreboard", &board, &survey])).unwrap();
        let lines = std::fs::read_to_string(&board).unwrap();
        assert_eq!(lines.lines().count(), 2);
        let last: Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
        assert_eq!(last["reference"], reference.as_str());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
