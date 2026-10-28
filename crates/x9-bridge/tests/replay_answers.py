#!/usr/bin/env python3
"""Criteria 6 and 7 of specs/on-site-answers.md: replay recorded surveys as
they arrive, ask each target's face at every count, and score the answers.

    replay_answers.py [--bin target/release/x9-bridge] [--work /tmp/answers] [howel] [admin]

Targets are set as on site: at each set count, every building wall of 4 m or
more in that registration gets one, unless an existing target already covers
most of its line; its face is fixed then. Each target is asked at every later
count with `x9-bridge ask --state`, so settling carries over as it does on
site. An answer before the final count is scored only if a station that saw
the face arrives after it. It is a miss if the face's verdict at the final
count is not an answer, either as carried on (`--state`) or judged afresh on
the final registration alone (`--reference`), or if its plumb is more than
5 mm from the fresh one.

Criterion 7 rebuilds the admin report from all 19 stations and checks w23 and
w29 carry their verdicts and no numbers.

Recordings are real properties: they stay outside git, under ~/x9-sim.
"""
import argparse
import csv
import json
import math
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SURVEYS = {
    # name: (recordings, set counts, final count, scored faces needed)
    "howel": (Path.home() / "x9-sim/howel", [4, 6, 8, 10], 12, 5),
    "admin": (Path.home() / "x9-sim/admin-color", [4, 8, 10, 12], 19, 3),
}
MIN_LENGTH_M = 4.0
MISS_MM = 5.0


def run(cmd, log=None):
    out = subprocess.run(cmd, capture_output=True, text=True)
    if log:
        Path(log).write_text(out.stderr)
    if out.returncode != 0:
        sys.exit(f"{' '.join(map(str, cmd))} failed:\n{out.stderr[-2000:]}")
    return out.stdout


def station_files(src, n):
    return [str(src / f"station_{k:02d}.tzf") for k in range(1, n + 1)]


def register(binary, src, work, n):
    out = work / f"reg{n}"
    if not (out / "merged.bin").exists():
        run([binary, "register", out, *station_files(src, n)], log=work / f"reg{n}.log")
    return out


def covers(face, wall):
    """Most of the wall's line within 0.5 m of an existing face."""
    (ax, ay), (bx, by) = face
    length = math.hypot(bx - ax, by - ay)
    if length < 1e-6:
        return False
    dx, dy = (bx - ax) / length, (by - ay) / length
    (wx0, wy0), (wx1, wy1) = wall["from"], wall["to"]
    steps = 20
    near = 0
    for i in range(steps + 1):
        t = i / steps
        px, py = wx0 + (wx1 - wx0) * t, wy0 + (wy1 - wy0) * t
        u = (px - ax) * dx + (py - ay) * dy
        off = abs(-(px - ax) * dy + (py - ay) * dx)
        if -0.5 <= u <= length + 0.5 and off <= 0.5:
            near += 1
    return near > (steps + 1) / 2


def replay(binary, name, work):
    src, set_at, final, floor = SURVEYS[name]
    work = work / name
    work.mkdir(parents=True, exist_ok=True)
    counts = list(range(min(set_at), final + 1))
    print(f"{name}: registering at {counts[0]}..{final} stations", flush=True)
    with ThreadPoolExecutor(3) as ex:
        regs = dict(zip(counts, ex.map(lambda n: register(binary, src, work, n), counts)))
    targets = []  # (id, face, set at)
    for n in set_at:
        walls = json.loads((regs[n] / "walls.json").read_text())
        for w in walls:
            if w["kind"] != "building" or w["length_m"] < MIN_LENGTH_M:
                continue
            if any(covers(t[1], w) for t in targets):
                continue
            targets.append((f"t{len(targets) + 1}", (w["from"], w["to"]), n, w["wall_id"]))
    print(f"{name}: {len(targets)} targets", flush=True)

    def ask_all(t):
        tid, face, n0, _ = t
        state = work / f"{tid}.state.json"
        state.unlink(missing_ok=True)
        out = []
        for n in range(n0, final + 1):
            f = ",".join(str(v) for v in (*face[0], *face[1]))
            v = json.loads(run([binary, "ask", "--state", state, "--id", tid, regs[n], f]))
            v["count"] = n
            out.append(v)
        ref = json.loads(run([binary, "ask", "--reference", "--id", tid, regs[final], f]))
        return tid, (out, ref)

    with ThreadPoolExecutor(8) as ex:
        both = dict(ex.map(ask_all, targets))
    asked = {k: v[0] for k, v in both.items()}
    refs = {k: v[1] for k, v in both.items()}
    (work / "asked.json").write_text(json.dumps({"asked": asked, "reference": refs}, indent=1))

    scored_faces, misses, scored = set(), [], 0
    for tid, face, n0, wall in targets:
        rows = asked[tid]
        last, ref = rows[-1], refs[tid]
        saw = set(ref.get("stations", []))
        for r in rows[:-1]:
            if r["verdict"] != "answer":
                continue
            if not any(s > r["count"] for s in saw):
                continue
            scored += 1
            scored_faces.add(tid)
            plumb = r["answer"]["out_of_plumb_mm"]
            if last["verdict"] != "answer":
                why = f"at {final}, carried on: {last['verdict']} ({last['reason']})"
            elif ref["verdict"] != "answer":
                why = f"at {final}, afresh: {ref['verdict']} ({ref['reason']})"
            elif abs(ref["reading"]["out_of_plumb_mm"] - plumb) > MISS_MM:
                why = f"at {final}, afresh: {ref['reading']['out_of_plumb_mm']} mm"
            else:
                why = None
            line = f"  {tid} ({wall} at {n0}) answer at {r['count']}: {plumb} mm, stations {r['answer']['stations']}"
            print(line + (f"  MISS {why}" if why else f"  holds ({ref['reading']['out_of_plumb_mm']} mm afresh at {final})"))
            if why:
                misses.append(line)
    for tid, face, n0, wall in targets:
        trail = " ".join(f"{r['count']}:{r['verdict'][0]}{'' if r['verdict'] != 'not_yet' or r['reason'] != 'settling' else 's'}" for r in asked[tid])
        print(f"  {tid} ({wall} at {n0}, {math.dist(*face):.1f} m): {trail}")
    ok = len(scored_faces) >= floor and not misses
    shown = len(scored_faces) >= floor
    print(f"{name}: {scored} scored answers on {len(scored_faces)} faces (floor {floor}), {len(misses)} misses: "
          + ("PASS" if ok else "floor not met: criterion 6 not shown" if not shown else "FAIL"))
    return ok, shown, regs


def criterion7(binary, work, regs):
    src = SURVEYS["admin"][0]
    out = work / "admin" / "report19"
    run([binary, "analyze", regs[19], out, *station_files(src, 19)], log=work / "admin" / "report19.log")
    rows = {r["wall_id"]: r for r in csv.DictReader(open(out / "walls.csv"))}
    html = (out / "report.html").read_text()
    numbers = ["out_of_plumb_mm", "lean_mm_per_m", "worst_strip_mm_per_m", "flat_rms_mm", "flat_p95_mm", "straightedge_ok_pct", "straightedge_worst_mm"]
    ok = True
    # Wall ids are renumbered by each registration: check these are the walls
    # the spec names (length m, covered %) before judging them.
    for wid, want, length, covered in (("w23", "does_not_fit", 3.0, 87.0), ("w29", "not_yet", 1.9, 70.0)):
        r = rows.get(wid)
        if r is None:
            print(f"  {wid}: not in walls.csv")
            ok = False
            continue
        if abs(float(r["length_m"]) - length) > 0.1 or abs(float(r["covered_pct"]) - covered) > 1.0:
            print(f"  {wid}: {r['length_m']} m, {r['covered_pct']}% covered: not the spec's {length} m at {covered}%: WRONG WALL")
            ok = False
            continue
        bare = all(r[k] == "" for k in numbers)
        row = html.split(f'<a href="#{wid}">{wid}</a>', 1)[1].split("</tr>", 1)[0] if f'<a href="#{wid}">' in html else ""
        shown = want.replace("_", " ") in row and "colspan" in row
        good = r["verdict"] == want and bare and shown
        ok &= good
        print(f"  {wid}: {r['length_m']} m, {r['covered_pct']}% covered: {r['verdict']} ({r['reason']}); "
              f"numbers in csv: {not bare}, verdict in report row: {shown}: {'ok' if good else 'WRONG'}")
    print(f"criterion 7: {'PASS' if ok else 'FAIL'}")
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=str(Path(__file__).resolve().parents[3] / "target/release/x9-bridge"))
    ap.add_argument("--work", default="/tmp/answers")
    ap.add_argument("surveys", nargs="*", default=["howel", "admin"])
    a = ap.parse_args()
    work = Path(a.work)
    results = {}
    for s in a.surveys:
        results[s] = replay(a.bin, s, work)
    c7 = criterion7(a.bin, work, results["admin"][2]) if "admin" in results else None
    shown = all(r[1] for r in results.values())
    passed = all(r[0] for r in results.values())
    print("criterion 6:", "PASS" if passed else "not shown (a floor not met)" if not shown else "FAIL")
    sys.exit(0 if passed and c7 is not False else 1)


if __name__ == "__main__":
    main()
