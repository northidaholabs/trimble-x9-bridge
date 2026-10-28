#!/usr/bin/env python3
"""End-to-end check of x9-bridge against x9-mock (no hardware).

usage: e2e.py <target/release dir>
Starts the mock and the bridge on spare ports, walks the main flows, and
prints PASS/FAIL per check. Exit code 1 if anything failed.
"""
import json, os, re, shutil, subprocess, sys, tempfile, threading, time, uuid, urllib.request, urllib.error

BIN = sys.argv[1]
MOCK, BRIDGE = "127.0.0.1:34599", "127.0.0.1:8799"
# Optional real scans for file / aim / export checks: a directory with
# dome.tzf + dome.jpg (the 2026-10-19 full dome) and area.tzf + area.jpg.
FIX = os.environ.get("E2E_FIXTURES")
TOK = {"viewer": "viewer-token-0123456789", "agent": "agent-token-0123456789", "agent2": "agent2-token-0123456789",
       "operator": "operator-token-0123456789"}
ROLE = {"agent2": "agent"}  # a second agent, to check what one may do to another's work
results = []

def check(name, cond, detail=""):
    cond = bool(cond)
    results.append(cond)
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""))

def call(method, path, who=None, body=None, headers=None, base=BRIDGE):
    req = urllib.request.Request(f"http://{base}/v1{path}", method=method,
                                 data=json.dumps(body).encode() if body is not None else None)
    req.add_header("Content-Type", "application/json")
    if who:
        req.add_header("Authorization", f"Bearer {TOK[who]}")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, (json.loads(raw) if raw else None)

def fetch(path, who):
    req = urllib.request.Request(f"http://{BRIDGE}/v1{path}")
    req.add_header("Authorization", f"Bearer {TOK[who]}")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type", ""), e.read()

def wait_run(run_id, pred, timeout=120):
    t0 = time.time()
    while time.time() - t0 < timeout:
        _, r = call("GET", f"/runs/{run_id}", "viewer")
        if pred(r) or r["state"] in ("failed", "cancelled", "succeeded"):
            return r
        time.sleep(0.2)
    return r

def wait_job(job_id, states, timeout=60):
    t0 = time.time()
    while time.time() - t0 < timeout:
        _, j = call("GET", f"/jobs/{job_id}", "viewer")
        if j["state"] in states:
            return j
        time.sleep(0.2)
    return j

def wait_up(base, path):
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://{base}{path}", timeout=1)
            return
        except Exception:
            time.sleep(0.1)
    raise SystemExit(f"{base} did not come up")

tmp = tempfile.mkdtemp()
recipes = os.path.join(tmp, "recipes")
os.makedirs(recipes)
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "recipes", "window-measurement.toml")) as f:
    open(os.path.join(recipes, "window-measurement.toml"), "w").write(f.read())
open(os.path.join(recipes, "log-note.toml"), "w").write('id = "log-note"\ntitle = "Note"\nversion = "1"\n'
    '[[inputs]]\nname = "text"\ntype = "string"\n'
    '[[steps]]\nid = "note"\nkind = "action"\noperation = "addLogNote"\narguments = { text = "{{input.text}}" }\n')
open(os.path.join(recipes, "broken.toml"), "w").write('id = "broken"\ntitle = "x"\nversion = "0"\n[[steps]]\nid = "a"\nkind = "action"\noperation = "rmRf"\n')
tokfile = os.path.join(tmp, "tokens")
with open(tokfile, "w") as f:
    f.write("# name role token\n")
    for who, t in TOK.items():
        f.write(f"{who}-1 {ROLE.get(who, who)} {t}\n")

procs = []
try:
    menv = {**os.environ, "X9_MOCK_ADDR": MOCK, "X9_MOCK_SPEED": "20", "X9_MOCK_FALLBACK_MODE": "HS_L1",
            "X9_MOCK_CANCEL_LINGER": "40"}  # a cancelled task stays listed 2 s
    if FIX:
        menv["X9_MOCK_FIXTURES"] = FIX
    procs.append(subprocess.Popen([f"{BIN}/x9-mock"], env=menv,
                                  stderr=subprocess.DEVNULL))
    wait_up(MOCK, "/api/v1/status")
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": f"http://{MOCK}/api/v1",
                                  "BRIDGE_ADDR": BRIDGE, "BRIDGE_TOKENS": tokfile, "BRIDGE_POLL_MS": "200",
                                  "BRIDGE_DATA": os.path.join(tmp, "data"), "BRIDGE_RECIPES": recipes},
                                  stderr=subprocess.DEVNULL))
    wait_up(BRIDGE, "/v1/health")

    # Event stream in the background.
    events = []
    def listen():
        req = urllib.request.Request(f"http://{BRIDGE}/v1/events")
        req.add_header("Authorization", f"Bearer {TOK['viewer']}")
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                for line in r:
                    line = line.decode().strip()
                    if line.startswith("event:"):
                        events.append(line.split(":", 1)[1].strip())
        except Exception:
            pass
    threading.Thread(target=listen, daemon=True).start()

    def saw(kind, timeout=3.0):
        # Events arrive a moment after the call that caused them returns.
        deadline = time.time() + timeout
        while kind not in events and time.time() < deadline:
            time.sleep(0.05)
        return kind in events

    s, b = call("GET", "/health")
    check("health needs no token", s == 200 and b["ok"])
    s, b = call("GET", "/status")
    check("status without a token is 401 unauthorized", s == 401 and b["code"] == "unauthorized", b)
    s, b = call("GET", "/status", "viewer")
    check("status maps the scanner", s == 200 and b["scanner"]["connected"] and b["scanner"]["battery_pct"] == 87
          and b["scanner"]["leveling"]["state"] == "ok", b)
    s, b = call("GET", "/whoami", "agent")
    check("whoami names the caller and role", s == 200 and b["role"] == "agent", b)

    def page(path):
        try:
            with urllib.request.urlopen(f"http://{BRIDGE}{path}", timeout=10) as r:
                return r.status, r.headers.get("content-type", ""), r.read().decode("utf-8", "replace"), r.url
        except urllib.error.HTTPError as e:
            return e.code, "", "", path
    st, ct, body, url = page("/")
    check("/ leads to the web UI without a token", st == 200 and url.endswith("/ui/") and "app.js" in body, (st, url))
    st, ct, body, _ = page("/ui/app.js")
    check("web UI script is served", st == 200 and "javascript" in ct and "/whoami" in body, (st, ct))
    st, ct, body, _ = page("/ui/nope.js")
    check("unknown web UI file is 404", st == 404, st)

    s, b = call("GET", "/scan-modes", "viewer")
    check("scan modes are translated", s == 200 and any(m["mode"] == "STD_L2" and m["fixed_seconds"] == 42 for m in b), b)

    s, b = call("POST", "/projects", "agent", {"name": "e2e-window"})
    check("agent creates a project", s == 201 and b["name"] == "e2e-window", b)
    pid = b["project_id"]
    s, b = call("POST", "/projects", "agent", {"name": "e2e-window"})
    check("duplicate project name is a conflict", s == 409 and b["code"] == "conflict", b)

    dome = {"project_id": pid, "kind": "full_dome", "mode": "STD_L0", "note": "station 1"}
    s, b = call("POST", "/scans", "viewer", dome)
    check("viewer can't scan (403 forbidden)", s == 403 and b["code"] == "forbidden", b)
    s, b = call("POST", "/scans", "viewer", {"bogus": True})
    check("viewer's malformed scan request is 403 forbidden, not a validation error", s == 403 and b["code"] == "forbidden", b)
    s, b = call("POST", "/scans", "agent", {**dome, "kind": "area"})
    check("area scan without a window is 400 invalid_request", s == 400 and b["code"] == "invalid_request", b)
    s, b = call("POST", "/scans", "operator", {**dome, "mode": "STD_L9"})
    check("unknown scan mode is refused before it reaches the scanner", s == 400 and "STD_L0" in b.get("detail", ""), b)
    # Another client starts a scan directly on the scanner.
    other = urllib.request.Request(f"http://{MOCK}/api/v1/scans", method="POST", headers={"Content-Type": "application/json"},
                                   data=json.dumps({"scanMode": "STD_L0", "projectName": "mock_project",
                                                    "projectUUID": "39187455-8278-428c-89ad-8a4835f95be6"}).encode())
    urllib.request.urlopen(other).read()
    s, jb = call("POST", "/scans", "operator", dome)
    jb = wait_job(jb["job_id"], ("failed", "succeeded"))
    check("busy scanner (another client's task) fails the job with scanner_busy",
          jb["state"] == "failed" and jb["problem"]["code"] == "scanner_busy", jb)
    for _ in range(200):
        if json.loads(urllib.request.urlopen(f"http://{MOCK}/api/v1/tasks").read()) == []:
            break
        time.sleep(0.1)
    s, jf = call("POST", "/scans", "operator", {**dome, "mode": "HS_L1"})
    jf = wait_job(jf["job_id"], ("failed", "succeeded"))
    tasks = json.loads(urllib.request.urlopen(f"http://{MOCK}/api/v1/tasks").read())
    check("a request the scanner echoes differently is cancelled at once",
          jf["state"] == "failed" and "did not accept" in jf["problem"].get("detail", ""), jf)
    check("the cancelled task stays listed for a while, exitResult none (as on a real X9)",
          len(tasks) == 1 and tasks[0]["exitResult"] == "none", tasks)
    s, job = call("POST", "/scans", "agent", dome)
    check("agent scan waits for approval", s == 202 and job["state"] == "awaiting_approval", job)
    check("a waiting scan shows its request and project", job.get("request", {}).get("mode") == dome["mode"]
          and job.get("project_name") == "e2e-window", job)
    s, b = call("POST", f"/jobs/{job['job_id']}/approve", "agent")
    check("agent can't approve (403)", s == 403, b)
    s, b = call("POST", f"/jobs/{job['job_id']}/approve", "operator")
    check("operator approves", s == 200 and b["state"] == "queued", b)
    s, b = call("POST", f"/jobs/{job['job_id']}/approve", "operator")
    check("approving twice is a conflict naming the state in API spelling", s == 409 and b["code"] == "conflict"
          and re.fullmatch(r"job is (queued|running|succeeded), not awaiting approval", b.get("detail", "")), b)
    seen_phases = set()
    t0 = time.time()
    while time.time() - t0 < 60:
        _, j = call("GET", f"/jobs/{job['job_id']}", "viewer")
        if j.get("phase"):
            seen_phases.add(j["phase"])
        if j["state"] in ("succeeded", "failed", "cancelled"):
            break
        time.sleep(0.1)
    check("full-dome job succeeds", j["state"] == "succeeded" and j["result"]["scan_id"], j)
    s, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("queued while the bridge's cancelled task lingered: waited instead of scanner_busy",
          any("clear the cancelled task" in e["text"] and e["refs"].get("job_id") == job["job_id"] for e in log), [e["text"] for e in log])
    check("phases reported: waiting (for the cancelled task), leveling, sweeping",
          {"waiting", "leveling", "sweeping"} <= seen_phases, seen_phases)
    station_scan = j["result"]["scan_id"]
    s, sc = call("GET", f"/scans/{station_scan}", "viewer")
    check("scan record: full dome, station 1, timings", s == 200 and sc["kind"] == "full_dome" and sc["station"] == 1
          and sc["project_id"] == pid and sc["timings"].get("geometry_ready_s"), sc)

    s, g = call("POST", "/grants", "agent", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z"})
    check("agent can't create grants (403)", s == 403, g)
    s, g = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z",
                                                "max_scans": 1, "allowed_modes": ["STD_L2"]})
    check("operator grants one STD_L2 scan", s == 201 and g["scans_used"] == 0, g)
    window = {"start_ha": 5.163, "delta_ha": 0.455, "start_va": 1.261, "delta_va": 0.44}
    area = {"project_id": pid, "kind": "area", "mode": "STD_L2", "window": window}
    s, job2 = call("POST", "/scans", "agent", area)
    check("granted area scan is queued without asking", s == 202 and job2["state"] == "queued", job2)
    j2 = wait_job(job2["job_id"], ("succeeded", "failed", "cancelled"))
    check("area job succeeds", j2["state"] == "succeeded", j2)
    s, sc2 = call("GET", f"/scans/{j2['result']['scan_id']}", "viewer")
    check("area scan record: kind area, parent = station scan, window kept", s == 200 and sc2["kind"] == "area"
          and sc2["parent_scan_id"] == station_scan and abs(sc2["window"]["start_ha"] - 5.163) < 1e-9, sc2)
    if FIX:
        s, ctype, body = fetch(f"/scans/{station_scan}/preview", "viewer")
        check("preview comes through the bridge", s == 200 and ctype == "image/jpeg" and body[:2] == b"\xff\xd8", (s, ctype))
        s, _, body = fetch(f"/scans/{sc2['scan_id']}/tzf", "viewer")
        check("scan file comes through the bridge", s == 200 and len(body) == os.path.getsize(os.path.join(FIX, "area.tzf")), (s, len(body)))
        s, b = call("POST", "/aim", "agent", {"reference_scan_id": station_scan, "box": {"x0": 523, "y0": 418, "x1": 674, "y1": 564}, "margin_rad": 0})
        want = {"start_ha": 5.157144, "delta_ha": 0.45571, "start_va": 1.261974, "delta_va": 0.440785}
        check("aim reproduces the 2026-10-19 window shot", s == 200 and all(abs(b["window"][k] - v) < 1e-4 for k, v in want.items()), b)
        s, b = call("POST", "/aim", "agent", {"reference_scan_id": station_scan, "box": {"x0": 10, "y0": 500, "x1": 20, "y1": 400}})
        check("aim rejects a box with y0 > y1", s == 400 and b["code"] == "invalid_request", b)
        s, b = call("POST", f"/scans/{sc2['scan_id']}/exports", "viewer", {"format": "ply"})
        check("viewer can't export (403)", s == 403, b)
        s, ej = call("POST", f"/scans/{sc2['scan_id']}/exports", "agent", {"format": "ply"})
        ej = wait_job(ej["job_id"], ("succeeded", "failed"), timeout=120)
        s, ex = call("GET", f"/exports/{ej['result']['export_id']}", "viewer") if ej["state"] == "succeeded" else (0, ej)
        check("area export: every point, all checks pass, preview intensity", s == 200 and ex["point_count"] == 777580
              and all(c["ok"] for c in ex["checks"]) and any("preview" in c.get("detail", "") for c in ex["checks"]), ex)
        s, _, body = fetch(f"/exports/{ex.get('export_id')}/file", "viewer")
        check("PLY file is complete", s == 200 and body.startswith(b"ply") and len(body) == body.index(b"end_header\n") + 11 + 15 * 777580, (s, len(body)))
        t_full = time.time()
        s, ej2 = call("POST", f"/scans/{station_scan}/exports", "agent", {"format": "las", "intensity": "none"})
        _, st_ = call("GET", "/status", "viewer")
        check("a running export is not the scanner's current job", st_["scanner"]["current_job_id"] is None, st_)
        ej2 = wait_job(ej2["job_id"], ("succeeded", "failed"), timeout=180)
        t_full = time.time() - t_full
        s, ex2 = call("GET", f"/exports/{ej2['result']['export_id']}", "viewer") if ej2["state"] == "succeeded" else (0, ej2)
        s3, _, body = fetch(f"/exports/{ex2.get('export_id')}/file", "viewer")
        check("full-dome LAS export: 6,281,859 points, levelled", s == 200 and ex2["point_count"] == 6281859
              and next(c for c in ex2["checks"] if c["name"] == "levelled")["ok"] and len(body) == 227 + 20 * 6281859, (ex2, len(body)))
        exports_dir = os.path.join(tmp, "data", "exports")
        n_files = len(os.listdir(exports_dir))
        t_cancel = time.time()
        s, ej3 = call("POST", f"/scans/{station_scan}/exports", "agent", {"format": "las", "intensity": "none"})
        s, b = call("POST", f"/jobs/{ej3['job_id']}/cancel", "agent")
        ej3 = wait_job(ej3["job_id"], ("succeeded", "failed", "cancelled"), timeout=180)
        t_cancel = time.time() - t_cancel
        check("cancelling an export stops it before it finishes, leaving no file", ej3["state"] == "cancelled"
              and "result" not in ej3 and len(os.listdir(exports_dir)) == n_files and t_cancel < t_full, (ej3, t_cancel, t_full))
        # COPC, the viewer's points and coverage.
        s, ej4 = call("POST", f"/scans/{sc2['scan_id']}/exports", "agent", {"format": "copc"})
        ej4 = wait_job(ej4["job_id"], ("succeeded", "failed"), timeout=180)
        s, ex4 = call("GET", f"/exports/{ej4['result']['export_id']}", "viewer") if ej4["state"] == "succeeded" else (0, ej4)
        s2, _, body = fetch(f"/exports/{ex4.get('export_id')}/file", "viewer")
        check("area COPC export: every point, LAS 1.4 with the COPC record first, compressed",
              s == 200 and ex4["format"] == "copc" and ex4["point_count"] == 777580 and all(c["ok"] for c in ex4["checks"])
              and s2 == 200 and body[:4] == b"LASF" and body[24:26] == bytes([1, 4]) and body[377:381] == b"copc"
              and len(body) < 777580 * 15, (ex4, s2, body[:4], len(body)))
        import struct
        # Every octree node reachable from the root, and the nodes hold every point
        # (copc-rs 0.5 alone leaves empty parents out; readers then lose points).
        h_off, h_size = struct.unpack_from("<QQ", body, 375 + 54 + 40) if s2 == 200 else (0, 0)
        nodes = {struct.unpack_from("<iiii", body, i): struct.unpack_from("<i", body, i + 28)[0] for i in range(h_off, h_off + h_size, 32)}
        orphans = [k for k in nodes if k[0] > 0 and (k[0] - 1, k[1] >> 1, k[2] >> 1, k[3] >> 1) not in nodes]
        check("COPC hierarchy: no orphan nodes, node counts add up to every point",
              nodes and not orphans and sum(v for v in nodes.values() if v > 0) == 777580 and h_off + h_size == len(body),
              (len(nodes), len(orphans), sum(v for v in nodes.values() if v > 0)))
        t_view = time.time()
        s, ctype, body = fetch(f"/scans/{station_scan}/view", "viewer")
        t_view = time.time() - t_view
        n = struct.unpack_from("<I", body, 4)[0] if s == 200 else 0
        stride = struct.unpack_from("<I", body, 8)[0] if s == 200 else 0
        check("dome view: X9V1, about 2 M points, every 2nd row and column", s == 200 and body[:4] == b"X9V1"
              and 1_200_000 < n <= 2_100_000 and stride == 2 and len(body) == 16 + 14 * n
              and max(body[16 + 13 * n:]) <= 3, (s, ctype, n, stride, len(body)))
        t0 = time.time()
        s, _, again = fetch(f"/scans/{station_scan}/view", "viewer")
        check("a second view request is served from the cache", s == 200 and again == body and time.time() - t0 < t_view / 2 + 0.2, (t_view, time.time() - t0))
        s, _, small = fetch(f"/scans/{station_scan}/view?max_points=100000", "viewer")
        n2 = struct.unpack_from("<I", small, 4)[0] if s == 200 else 0
        check("max_points thins harder", s == 200 and 50_000 < n2 <= 105_000, (s, n2))
        s, cov = call("GET", f"/scans/{station_scan}/coverage", "viewer")
        shares = sum(c["share"] for c in cov["classes"]) if s == 200 else 0
        check("dome coverage: spacing classes add up, plan has seen floor and shadows, image linked",
              s == 200 and abs(shares - 1) < 0.01 and cov["point_count"] == 6281859 and cov["plan"]["seen_m2"] > 10
              and cov["plan"]["shadow_m2"] > 0 and cov["spacing_mm"]["p50"] > 0
              and cov["image_url"] == f"/v1/scans/{station_scan}/coverage/image", cov)
        s, ctype, png = fetch(f"/scans/{station_scan}/coverage/image", "viewer")
        check("coverage image is a PNG the plan's size", s == 200 and ctype == "image/png" and png[1:4] == b"PNG"
              and struct.unpack(">II", png[16:24]) == (cov["plan"]["width"], cov["plan"]["height"]), (s, ctype))
        print(f"      (dome view built in {t_view:.1f} s; spacing p50 {cov['spacing_mm']['p50']} mm; shadow {cov['plan']['shadow_m2']} m²)")
        s, _ = call("GET", f"/scans/{sc2['scan_id']}/coverage", "viewer")
        check("area coverage builds too", s == 200, s)
        s, b = call("GET", f"/scans/{uuid.uuid4()}/view", "viewer")
        check("view of a scan the scanner doesn't list is 404", s == 404, (s, b))
        # A window measured straight from the full dome, cropped to the box that framed it.
        def measure(body):
            s, j = call("POST", f"/scans/{station_scan}/measurements", "agent", body)
            if s != 202:
                return s, j
            j = wait_job(j["job_id"], ("succeeded", "failed"), timeout=120)
            if j["state"] != "succeeded":
                return 0, j
            return call("GET", f"/measurements/{j['result']['measurement_id']}", "viewer")
        s, m = measure({"kind": "opening", "box": {"x0": 523, "y0": 418, "x1": 674, "y1": 564}})
        check("a window measured from the full dome by a box: within 10 mm of the close-up (1185 x 1150), all checks pass",
              s == 200 and abs(m["opening_width_mm"] - 1185) <= 10 and abs(m["opening_height_mm"] - 1150) <= 10
              and all(c["ok"] for c in m["checks"]) and m["crop"]["box"]["x0"] == 523 and m["crop"]["margin_rad"] == 0.017
              and 50_000 < m["crop"]["points"] < 200_000, m)
        s, j = measure({"kind": "opening", "box": {"x0": 523, "y0": 564, "x1": 674, "y1": 418}})
        check("a box with y0 > y1 fails the measurement with measure_failed", s == 0 and j["state"] == "failed"
              and j["problem"]["code"] == "measure_failed" and "y0 < y1" in j["problem"]["detail"], j)
        # Next-station proposal: drawn on the preview, kept, listed, shown.
        prop = {"marker": {"x": 900, "y": 600, "label": "by the door"}, "note": "Carry the tripod past the window, by the door, on the rug.",
                "labels": [{"x": 600, "y": 450, "text": "Window", "kind": "landmark"}],
                "boxes": [{"x0": 523, "y0": 418, "x1": 674, "y1": 564, "text": "Seen: the window", "kind": "seen"}]}
        s, b = call("PUT", f"/scans/{station_scan}/next-station", "viewer", prop)
        check("viewer can't propose a station (403)", s == 403, b)
        s, b = call("PUT", f"/scans/{station_scan}/next-station", "agent", {**prop, "marker": {"x": 99999, "y": 1}})
        check("a marker off the preview is refused (400)", s == 400 and "marker" in b["detail"], b)
        s, b = call("PUT", f"/scans/{station_scan}/next-station", "agent", {**prop, "note": "Turn right at the window, by the door."})
        check("directions by left or right are refused (400)", s == 400 and "\"right\"" in b["detail"], b)
        s, b = call("PUT", f"/scans/{station_scan}/next-station", "agent",
                    {**prop, "labels": [{"x": 1900, "y": 450, "text": "Window", "kind": "landmark"}]})
        check("a landmark out of the tripod's view is refused (400)", s == 400 and "one view" in b["detail"], b)
        s, ns = call("PUT", f"/scans/{station_scan}/next-station", "agent", prop)
        s2, ctype, png = fetch(f"/scans/{station_scan}/next-station/image", "viewer")
        _, lst = call("GET", "/next-stations", "viewer")
        check("an agent proposes the next station; the bridge draws it, keeps it and lists it",
              s == 200 and ns["proposed_by"] == "agent-1" and ns["preview_size"] == [2082, 815] and ns["marker"]["x"] == 900
              and s2 == 200 and ctype == "image/png" and png[1:4] == b"PNG" and lst and lst[0]["scan_id"] == station_scan
              and saw("next_station"), (s, ns, s2, ctype))
        s, _ = call("PUT", f"/scans/{sc2['scan_id']}/next-station", "agent",
                    {**prop, "marker": {"x": 100, "y": 100}, "boxes": [],
                     "labels": [{"x": 110, "y": 90, "text": "Window", "kind": "landmark"}]})
        check("a proposal on the area scan too", s == 200, s)
        # Next-station plan: computed from the dome's points, cached, per subject.
        t0 = time.time()
        s, pl = call("GET", f"/scans/{station_scan}/next-station/plan", "viewer")
        t_plan = time.time() - t0
        cands = pl.get("candidates", []) if s == 200 else []
        check("the bridge plans the next station from the dome: picks a step out, on the preview, each with a pixel",
              s == 200 and pl["preview_size"] == [2082, 815] and pl["min_step_m"] >= 2
              and all(c["distance_m"] >= pl["min_step_m"] and 0 <= c["preview_px"][0] < 2082 and 0 <= c["preview_px"][1] < 815
                      and c["walk_m"] >= c["distance_m"] - 0.5 for c in cands)
              and (cands or pl.get("reason")), pl)
        t0 = time.time()
        s, again = call("GET", f"/scans/{station_scan}/next-station/plan", "viewer")
        check("a second plan is served from the cache", s == 200 and again == pl and time.time() - t0 < t_plan / 2 + 0.2, (t_plan, time.time() - t0))
        s, sub = call("GET", f"/scans/{station_scan}/next-station/plan?subject=523,674", "viewer")
        check("a plan toward a subject says which, and counts only its hidden ground",
              s == 200 and sub["subject_px"] == [523, 674] and sub["subject_hidden_m2"] <= sub["hidden_m2"], sub)
        for bad in ("523", "523,523", "a,b", "523,2082", "-1,5"):
            s, b = call("GET", f"/scans/{station_scan}/next-station/plan?subject={bad}", "viewer")
            check(f"a bad subject {bad!r} is refused (400)", s == 400, (s, b))
        views = os.listdir(os.path.join(tmp, "data", "views"))
        check("only the plan without a subject is cached", [v for v in views if ".plan" in v] == [f"{station_scan}.plan.json"], views)
        s, b = call("GET", f"/scans/{sc2['scan_id']}/next-station/plan", "viewer")
        check("an area scan is not planned from: it says it needs a full dome", s == 200 and not b["candidates"] and "full dome" in b["reason"], b)
        print(f"      (plan in {t_plan:.1f} s: {len(cands)} picks, min step {pl['min_step_m']} m"
              + (f", first {cands[0]['distance_m']} m" if cands else f", {pl.get('reason')}") + ")")
    # Registration: each full dome lands in the project frame by itself.
    _, in_project = call("GET", f"/scans?project_id={pid}", "viewer")
    domes = [x for x in in_project if x["kind"] == "full_dome"]
    deadline, reg = time.time() + 180, None
    while time.time() < deadline:
        _, reg = call("GET", f"/projects/{pid}/registration", "viewer")
        if not reg["registering_job_id"] and reg.get("updated_at") and (len(reg["stations"]) == len(domes) or not FIX):
            break
        time.sleep(1)
    check("the bridge registers the project's full domes after each scan", reg.get("updated_at") and not reg["registering_job_id"], reg)
    s, b = call("POST", f"/projects/{pid}/registration", "viewer")
    check("viewer can't start a registration (403)", s == 403, b)
    s, ctype, png = fetch(f"/projects/{pid}/minimap", "viewer")
    s2, ctype2, svg = fetch(f"/projects/{pid}/minimap?format=svg", "viewer")
    check("the minimap is drawn, as PNG and as SVG", s == 200 and ctype == "image/png" and png[1:4] == b"PNG"
          and s2 == 200 and ctype2.startswith("image/svg") and svg.startswith(b"<svg") and b"</svg>" in svg, (s, ctype, s2, ctype2))
    if FIX:
        check("every full dome is placed in one frame, with walls", len(reg["stations"]) == len(domes) >= 1
              and all(x["placed"] for x in reg["stations"]) and reg["walls"] and saw("registration"), reg["summary"])
        check("each station has a verdict, and the placed ones count for coverage",
              all(x["verdict"] in ("frame", "confirmed", "single") and x["counts_for_coverage"] for x in reg["stations"]),
              [(x["station"], x["verdict"], x["reasons"]) for x in reg["stations"]])
        s, ctype, png = fetch(f"/projects/{pid}/registration/plan", "viewer")
        check("the registered plan is drawn", s == 200 and ctype == "image/png" and png[1:4] == b"PNG", (s, ctype))
        s, fc = call("POST", f"/projects/{pid}/registration/face", "viewer", {"wall_id": reg["walls"][0]["wall_id"]})
        check("a wall face is judged: share of its length seen, and by which stations", s == 200 and fc["seen_share"] > 0.5 and fc["stations"], fc)
        s, b = call("POST", f"/projects/{pid}/registration/face", "viewer", {"wall_id": "w999"})
        check("an unknown wall is refused (400)", s == 400, b)
        check("registrations are saved for a restart", os.path.exists(os.path.join(tmp, "data", "registrations.json")))
    # Workers: a faster computer can take registration; the bridge takes it back.
    s, _ = call("POST", "/work/claim", "viewer", {"worker": "e2e-laptop"})
    check("viewer can't do work for the bridge (403)", s == 403, s)
    s, _ = call("POST", "/work/claim", "agent", {"worker": "e2e-laptop"})
    _, ws = call("GET", "/workers", "viewer")
    check("an idle worker gets 204 and is listed", s == 204 and [w["worker"] for w in ws] == ["e2e-laptop"], (s, ws))
    s, rj = call("POST", f"/projects/{pid}/registration", "agent")
    job, deadline = None, time.time() + 15
    while job is None and time.time() < deadline:
        s, b = call("POST", "/work/claim", "agent", {"worker": "e2e-laptop"})
        job = b if s == 200 else None
        time.sleep(0.2)
    check("the worker claims the registration job, with the stations and pairs", job and job["job_id"] == rj["job_id"]
          and job["kind"] == "register" and isinstance(job["stations"], list) and isinstance(job["pairs"], list), job)
    s, b = call("POST", f"/work/{rj['job_id']}/result", "agent2", {"pairs": [], "poses": [], "audits": [], "walls": []})
    check("only the claimer may post the result (409)", s == 409 and "not claimed" in b["detail"], b)
    s, _ = call("POST", f"/work/{rj['job_id']}/progress", "agent", {"phase": "decoding on e2e-laptop", "progress": 0.2})
    _, jb = call("GET", f"/jobs/{rj['job_id']}", "viewer")
    check("the worker's progress shows on the job", s == 204 and jb["phase"] == "decoding on e2e-laptop", jb)
    s, _ = call("POST", f"/work/{rj['job_id']}/fail", "agent", {"error": "laptop lid closed"})
    jb = wait_job(rj["job_id"], ("succeeded", "failed"), timeout=180)
    _, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("a failed worker hands the job back and the bridge registers it", s == 204 and jb["state"] == "succeeded"
          and any("came back to the bridge" in e["text"] and "laptop lid closed" in e["text"] for e in log), (jb["state"], [e["text"] for e in log][-3:]))
    # Missions: the goal, the ledger, ready to leave, one sign-off.
    s, b = call("PUT", f"/projects/{pid}/mission", "viewer", {"goal": "x", "kind": "exterior", "done_when": "y"})
    check("viewer can't set a mission (403)", s == 403, b)
    s, ms = call("PUT", f"/projects/{pid}/mission", "agent", {"goal": "Scan the exterior of the house", "kind": "exterior",
                 "done_when": "Every facade seen, no shadow on the house walls",
                 "targets": [{"target_id": "north", "name": "North facade", "kind": "facade"}]})
    check("an agent sets the mission with its first target", s == 200 and ms["state"] == "active" and ms["targets"][0]["status"] == "open"
          and saw("mission"), ms)
    if FIX:
        w = reg["walls"][0]
        s, ms = call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [
            {"target_id": "longest-wall", "name": "Longest wall", "kind": "wall", "wall_id": w["wall_id"]}]})
        t = ms["targets"][-1] if s == 200 else ms
        check("a target on a registered wall is covered by the bridge once seen", s == 200 and t["status"] == "covered"
              and t["stations"] and t["face"]["from"] == w["from"] and t["geometry"]["seen_share"] >= 0.8, t)
        far = {"target_id": "far-wall", "name": "Far wall", "kind": "wall", "face": {"from": [100, 100], "to": [110, 100]}}
        s, ms = call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [far]})
        s2, b = call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [{**far, "status": "covered", "stations": [station_scan]}]})
        check("a wall face nobody saw stays open and can't be called covered", s == 200 and ms["targets"][-1]["status"] == "open"
              and s2 == 400 and "only 0%" in b["detail"], (s, s2, b))
        call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [{**far, "status": "declined", "note": "not on this site"}]})
    s, b = call("POST", f"/projects/{pid}/mission/ready", "agent", {"summary": "done"})
    check("ready to leave is refused while a target is open, naming it", s == 409 and "North facade" in b["detail"], b)
    s, b = call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [{"target_id": "north", "status": "covered", "stations": ["nope"]}]})
    check("a covered target must name scans of this project", s == 400 and "not in this project" in b["detail"], b)
    s, ms = call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [
        {"target_id": "north", "status": "covered", "stations": [station_scan]},
        {"target_id": "south", "name": "South facade", "status": "declined", "note": "neighbour's yard, locked"}]})
    s2, ms = call("POST", f"/projects/{pid}/mission/ready", "agent", {"summary": "North from station 1; south declined (locked yard)."})
    s3, b = call("POST", f"/projects/{pid}/mission/sign-off", "agent", {})
    check("agents can't sign a job off (403)", s3 == 403, b)
    s4, ms = call("POST", f"/projects/{pid}/mission/sign-off", "operator", {"confirmed_via": "web UI"})
    check("ledger settled, ready, then the operator signs it off", s == 200 and s2 == 200 and s4 == 200 and ms["state"] == "signed_off"
          and ms["signed_off_by"] == "operator-1" and ms["confirmed_via"] == "web UI", (s, s2, s4, ms))
    s, b = call("POST", f"/projects/{pid}/mission/targets", "agent", {"targets": [{"target_id": "west", "name": "West facade"}]})
    check("a signed-off mission doesn't change (409)", s == 409, b)
    _, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("the mission's sign-off is in the field log with its tally", any("Mission signed off" in e["text"] and ("2 covered, 2 declined" if FIX else "1 covered, 1 declined") in e["text"]
          for e in log), [e["text"] for e in log][-4:])
    missions_file = os.path.join(tmp, "data", "missions.json")
    check("missions are saved for a restart", os.path.exists(missions_file)
          and json.load(open(missions_file))[0]["state"] == "signed_off", os.path.exists(missions_file))

    # --- recipes and runs ---
    s, rl = call("GET", "/recipes", "viewer")
    check("recipes listed, the broken one skipped", s == 200 and sorted(r["recipe_id"] for r in rl) == ["log-note", "window-measurement"], rl)
    s, rc = call("GET", "/recipes/window-measurement", "viewer")
    check("recipe served as data", s == 200 and [st["step_id"] for st in rc["steps"]][:3] == ["overview", "box", "aim"], rc)
    s, b = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "nonsense"}})
    check("run rejects an input outside its choices", s == 400 and b["code"] == "invalid_request", b)
    s, b = call("POST", "/runs", "viewer", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
    check("viewer can't start runs (403)", s == 403, b)
    s, runc = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
    r = wait_run(runc["run_id"], lambda r: r["waiting_for"] == "approval")
    check("run waits for approval of its overview scan", r["state"] == "waiting_for_person" and r["current_step_id"] == "overview", r)
    s, b = call("POST", f"/runs/{runc['run_id']}/advance", "agent2", {"step_id": "box", "input": {}})
    check("another agent can't advance the run (403)", s == 403 and b["code"] == "forbidden", b)
    s, b = call("POST", f"/runs/{runc['run_id']}/cancel", "agent2")
    check("another agent can't cancel the run (403)", s == 403 and b["code"] == "forbidden", b)
    _, r = call("GET", f"/runs/{runc['run_id']}", "viewer")
    check("... and the run carries on", r["state"] == "waiting_for_person", r)
    s, b = call("POST", f"/runs/{runc['run_id']}/cancel", "agent")
    r = wait_run(runc["run_id"], lambda r: False, timeout=10)
    _, oj = call("GET", f"/jobs/{r['steps'][0]['job_id']}", "viewer")
    check("cancelling the run cancels its pending scan", r["state"] == "cancelled" and oj["state"] == "cancelled", (r["state"], oj["state"]))
    s, runo = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "trim"}})
    wait_run(runo["run_id"], lambda r: r["waiting_for"] == "approval")
    s, b = call("POST", f"/runs/{runo['run_id']}/cancel", "operator")
    r = wait_run(runo["run_id"], lambda r: False, timeout=10)
    check("an operator may cancel an agent's run", s == 200 and r["state"] == "cancelled", (s, r["state"]))
    s, runn = call("POST", "/runs", "agent", {"recipe_id": "log-note", "project_id": pid, "inputs": {"text": "   "}})
    r = wait_run(runn["run_id"], lambda r: False, timeout=10)
    check("addLogNote with blank text fails the run with invalid_request", r["state"] == "failed"
          and r["problem"]["code"] == "invalid_request" and "1–4000" in r["problem"]["detail"], r)
    s, runn = call("POST", "/runs", "agent", {"recipe_id": "log-note", "project_id": pid, "inputs": {"text": "Door wedged open"}})
    r = wait_run(runn["run_id"], lambda r: False, timeout=10)
    _, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("addLogNote with text writes the note", r["state"] == "succeeded"
          and any(e["text"] == "Door wedged open" and e["refs"].get("run_id") == runn["run_id"] for e in log), r)

    if FIX:
        s, run = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
        rid = run["run_id"]
        r = wait_run(rid, lambda r: r["waiting_for"] == "approval")
        call("POST", f"/jobs/{r['steps'][0]['job_id']}/approve", "operator")
        r = wait_run(rid, lambda r: r["waiting_for"] == "input")
        check("after approval the run scans, then waits for the box", r["current_step_id"] == "box" and r["steps"][0]["state"] == "done", r)
        s, g = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 1, "run_id": rid})
        s, b = call("POST", f"/runs/{rid}/advance", "agent", {"step_id": "box", "input": {}})
        check("advance without the box is refused", s == 400 and "box" in b.get("detail", ""), b)
        box = {"x0": 523, "y0": 418, "x1": 674, "y1": 564}
        s, b = call("POST", f"/runs/{rid}/propose", "agent2", {"step_id": "box", "input": {"box": box}})
        check("another agent can't propose in the run (403)", s == 403 and b["code"] == "forbidden", b)
        s, b = call("POST", f"/runs/{rid}/propose", "agent", {"step_id": "box", "input": {}})
        check("a proposal without the box is refused", s == 400 and "box" in b.get("detail", ""), b)
        s, b = call("POST", f"/runs/{rid}/propose", "agent", {"step_id": "box", "input": {"box": box}, "note": "window found at the right of the overview"})
        prop = {x["step_id"]: x for x in b["steps"]}["box"].get("proposal", {}) if s == 200 else {}
        check("agent proposes the box; the step keeps waiting with the proposal", s == 200 and prop.get("input", {}).get("box") == box
              and prop.get("by") == b["started_by"] and b["waiting_for"] == "input", b)
        s, b = call("POST", f"/runs/{rid}/advance", "operator", {"step_id": "box", "input": {"box": box}})
        r = wait_run(rid, lambda r: r["current_step_id"] == "confirm" and r["waiting_for"] == "input", timeout=300)
        steps = {x["step_id"]: x for x in r["steps"]}
        check("run aims, scans the close-up under its grant, exports and passes checks",
              r["current_step_id"] == "confirm" and all(steps[k]["state"] == "done" for k in ("aim", "closeup", "export", "checks"))
              and steps["export"]["output"]["point_count"] == 777580, {k: v["state"] for k, v in steps.items()} if r else r)
        check("the person's confirmation is recorded as accepting the proposal", steps["box"]["output"].get("proposal_accepted") is True, steps["box"])
        _, cj = call("GET", f"/jobs/{steps['closeup']['job_id']}", "viewer")
        check("close-up job carries the run id and ran without approval", cj.get("run_id") == rid and cj["state"] == "succeeded", cj)
        m = steps["measure"]["output"]
        check("the run measures the opening: about 1185 x 1150 mm, with checks and evidence",
              steps["measure"]["state"] == "done" and abs(m["opening_width_mm"] - 1185) <= 10 and abs(m["opening_height_mm"] - 1150) <= 10
              and steps["measure_checks"]["state"] == "done" and m["evidence_url"].endswith("/evidence"), m)
        sc = m.get("site_check", {})
        check("site check: wall on every side of the close-up, so no rescan", sc.get("ok") is True and sc.get("rescans") == 0
              and next(c for c in m["checks"] if c["name"] == "margins")["ok"], sc)
        s, _, png = fetch(m["evidence_url"].removeprefix("/v1"), "viewer")
        check("the evidence image is a PNG", s == 200 and png[:4] == b"\x89PNG", s)
        prop = {x["step_id"]: x for x in r["steps"]}["confirm"].get("proposal", {})
        check("the bridge proposes the measurement for a person to confirm", prop.get("by") == "bridge"
              and prop.get("input") == {"opening_width_mm": m["opening_width_mm"], "opening_height_mm": m["opening_height_mm"]}, prop)
        s, b = call("POST", f"/runs/{rid}/advance", "operator", {"step_id": "confirm", "input": prop.get("input", {})})
        r = wait_run(rid, lambda r: False, timeout=10)
        check("run finishes when the person hands in the measurement", r["state"] == "succeeded" and r["finished_at"], r)
        iv = r.get("interventions", {})
        check("the run counts its person interventions: 1 approval, 2 confirmations, no corrections",
              iv.get("total") == 3 and iv.get("approvals") == 1 and iv.get("confirmed") == 2 and iv.get("corrected") == 0, iv)
        s, log = call("GET", f"/log?project_id={pid}", "viewer")
        check("run steps are in the field log", any(e["kind"] == "run" and "started" in e["text"] for e in log)
              and any("Waiting for a person" in e["text"] for e in log), [e["text"] for e in log][-6:])

        # AI-first, one intervention: a standing grant given at setup, the agent
        # marks the window itself (review = "final"), one sign-off at the end.
        s, gp = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 3})
        s, run1 = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "blinds"}})
        rid1 = run1["run_id"]
        r = wait_run(rid1, lambda r: r["waiting_for"] in ("approval", "input"), timeout=120)
        check("one-intervention run: the overview runs under the project grant, no approval", r["waiting_for"] == "input"
              and r["current_step_id"] == "box" and r["interventions"]["approvals"] == 0, r)
        s, b = call("POST", f"/runs/{rid1}/advance", "agent", {"step_id": "box", "input": {"box": box}})
        r = wait_run(rid1, lambda r: r["current_step_id"] == "confirm" and r["waiting_for"] == "input", timeout=300)
        check("the agent's own box is not an intervention; the run goes straight to the sign-off",
              r["current_step_id"] == "confirm" and r["interventions"]["total"] == 0, r.get("interventions"))
        prop1 = {x["step_id"]: x for x in r["steps"]}["confirm"].get("proposal", {})
        s, b = call("POST", f"/runs/{rid1}/advance", "operator", {"step_id": "confirm", "input": prop1.get("input", {})})
        r = wait_run(rid1, lambda r: False, timeout=10)
        iv = r.get("interventions", {})
        check("one-intervention run: 1 person intervention (the sign-off), scans under the operator's grant",
              r["state"] == "succeeded" and iv.get("total") == 1 and iv.get("confirmed") == 1 and iv.get("approvals") == 0
              and [g["grant_id"] for g in r.get("authorized_by", [])] == [gp["grant_id"]]
              and r["authorized_by"][0]["granted_by"] == "operator-1", (iv, r.get("authorized_by")))
        _, log = call("GET", f"/log?project_id={pid}", "viewer")
        check("the run's last log line names the grant", any(e["kind"] == "run" and rid1 in e["text"] and "succeeded after 1 person intervention" in e["text"]
              and "a grant from operator-1" in e["text"] for e in log), [e["text"] for e in log if rid1 in e["text"]])
        call("DELETE", f"/grants/{gp['grant_id']}", "operator")

        # Without a grant, one approval can cover the whole run.
        s, run2 = call("POST", "/runs", "agent", {"recipe_id": "window-measurement", "project_id": pid, "inputs": {"purpose": "trim"}})
        rid2 = run2["run_id"]
        r = wait_run(rid2, lambda r: r["waiting_for"] == "approval")
        s, b = call("POST", f"/jobs/{r['steps'][0]['job_id']}/approve", "agent", {"run": True})
        check("an agent can't approve its run (403)", s == 403, b)
        s, b = call("POST", f"/jobs/{r['steps'][0]['job_id']}/approve", "operator", {"run": True})
        _, gl = call("GET", "/grants", "viewer")
        rg = [g for g in gl if g.get("run_id") == rid2]
        check("approving the run grants its later scans: the close-up and one site-check rescan",
              s == 200 and b["state"] == "queued" and len(rg) == 1 and rg[0]["max_scans"] == 2 and rg[0]["granted_by"] == "operator-1", (b, rg))
        r = wait_run(rid2, lambda r: r["waiting_for"] == "input")
        check("... counted as one approval", r["interventions"]["approvals"] == 1 and r["interventions"]["total"] == 1, r["interventions"])
        call("POST", f"/runs/{rid2}/cancel", "agent")
        wait_run(rid2, lambda r: False, timeout=10)

    s, job3 = call("POST", "/scans", "agent", area)
    check("grant used up: next scan waits for approval", s == 202 and job3["state"] == "awaiting_approval", job3)
    s, b = call("POST", f"/jobs/{job3['job_id']}/cancel", "agent")
    check("agent cancels its own waiting job", s == 200 and b["state"] == "cancelled", b)

    s, job4 = call("POST", "/scans", "operator", {**dome, "color": True})
    check("operator scan is queued directly", s == 202 and job4["state"] == "queued", job4)
    t0 = time.time()
    while time.time() - t0 < 30:
        _, j4 = call("GET", f"/jobs/{job4['job_id']}", "viewer")
        if j4.get("phase") == "sweeping" or j4["state"] != "running" and j4["state"] != "queued":
            break
        time.sleep(0.05)
    # While job4 runs, a granted agent scan waits in the queue; cancelling it gives the grant use back.
    s, g2 = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z",
                                                 "max_scans": 1, "allowed_modes": ["STD_L0"]})
    s, jq = call("POST", "/scans", "agent", dome)
    _, gl = call("GET", "/grants", "viewer")
    used = next(x["scans_used"] for x in gl if x["grant_id"] == g2["grant_id"])
    s, b = call("POST", f"/jobs/{jq['job_id']}/cancel", "agent")
    _, gl = call("GET", "/grants", "viewer")
    check("a granted scan cancelled before it starts gives its grant use back", jq["state"] == "queued" and used == 1
          and b["state"] == "cancelled" and next(x["scans_used"] for x in gl if x["grant_id"] == g2["grant_id"]) == 0, (jq, used, gl))
    call("DELETE", f"/grants/{g2['grant_id']}", "operator")
    s, b = call("POST", f"/jobs/{job4['job_id']}/cancel", "operator")
    j4 = wait_job(job4["job_id"], ("cancelled", "failed", "succeeded"))
    check("scan cancelled mid-sweep reads cancelled at once", j4["state"] == "cancelled" and "partial_scan_id" not in j4, j4)
    # Queued while the cancelled task is still listed.
    s, job5 = call("POST", "/scans", "operator", dome)
    t0 = time.time()
    while time.time() - t0 < 15 and "partial_scan_id" not in j4:
        time.sleep(0.2)
        _, j4 = call("GET", f"/jobs/{job4['job_id']}", "viewer")
    s, ps = call("GET", f"/scans/{j4.get('partial_scan_id')}", "viewer")
    check("cancelled job points at the partial scan the scanner filed", s == 200 and 0 < ps["point_count"] < 6281859
          and ps["project_id"] == pid and ps["created_at"], (j4, ps))
    s, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("field log says the partial scan was kept", any(e["refs"].get("scan_id") == j4.get("partial_scan_id")
          and "partial" in e["text"] for e in log), [e["text"] for e in log][-4:])
    j5 = wait_job(job5["job_id"], ("succeeded", "failed", "cancelled"))
    check("scan queued right after a cancel runs once the scanner clears", j5["state"] == "succeeded"
          and j5["result"]["scan_id"] != j4.get("partial_scan_id"), j5)
    tasks = json.loads(urllib.request.urlopen(f"http://{MOCK}/api/v1/tasks").read())
    check("scanner has no task left", tasks == [], tasks)

    # Plant cached copies (unless real fixtures cached them already) to see what a delete does to them.
    cache = os.path.join(tmp, "data", "scans")
    os.makedirs(cache, exist_ok=True)
    cached = [os.path.join(cache, f"{sc2['scan_id']}.{ext}") for ext in ("tzf", "jpg")]
    for c in cached:
        if not os.path.exists(c):
            open(c, "wb").write(b"\xff\xd8 cached")
    s, _, body = fetch(f"/scans/{sc2['scan_id']}/preview", "viewer")
    check("a listed scan's cached preview is served", s == 200 and body == open(cached[1], "rb").read(), s)
    ghost = os.path.join(cache, f"{uuid.uuid4()}.jpg")
    open(ghost, "wb").write(b"\xff\xd8 ghost")
    s, _, body = fetch(f"/scans/{os.path.basename(ghost)[:-4]}/preview", "viewer")
    check("a cached file for a scan the scanner doesn't list is refused and evicted", s == 404 and not os.path.exists(ghost), s)
    s, b = call("DELETE", f"/scans/{sc2['scan_id']}", "agent")
    check("agent can't delete (403)", s == 403, b)
    s, b = call("DELETE", f"/scans/{sc2['scan_id']}", "operator")
    check("delete without X-Confirm is refused", s == 412 and b["code"] == "confirm_mismatch", b)
    s, _ = call("DELETE", f"/scans/{sc2['scan_id']}", "operator", headers={"X-Confirm": sc2["scan_id"]})
    s2, gone = call("GET", f"/scans/{sc2['scan_id']}", "viewer")
    check("confirmed delete removes the scan", s == 204 and s2 == 404, (s, s2))
    s3, _, _ = fetch(f"/scans/{sc2['scan_id']}/preview", "viewer")
    s4, _, _ = fetch(f"/scans/{sc2['scan_id']}/tzf", "viewer")
    check("delete evicts the cached file and preview; neither is served", s3 == 404 and s4 == 404
          and not any(os.path.exists(c) for c in cached), (s3, s4, [os.path.exists(c) for c in cached]))
    if FIX:
        s, _ = call("GET", f"/exports/{ex.get('export_id')}", "viewer")
        s2, _, _ = fetch(f"/exports/{ex.get('export_id')}/file", "viewer")
        check("delete also removes exports made from the scan", s == 404 and s2 == 404, (s, s2))
        s, _ = call("GET", f"/scans/{sc2['scan_id']}/next-station", "viewer")
        check("delete also removes the scan's next-station proposal", s == 404, s)
        views = os.listdir(os.path.join(tmp, "data", "views"))
        check("delete also removes the scan's view and coverage files", not any(v.startswith(sc2["scan_id"]) for v in views)
              and any(v.startswith(station_scan) for v in views), views)
    else:
        s, b = call("GET", f"/scans/{station_scan}/tzf", "viewer")
        check("a scanner error status (no file on the mock) is 502 scanner_error", s == 502 and b["code"] == "scanner_error"
              and "404" in b["title"], b)
        s, ej = call("POST", f"/scans/{station_scan}/exports", "agent", {"format": "ply"})
        ej = wait_job(ej["job_id"], ("succeeded", "failed", "cancelled"))
        check("... and fails an export with scanner_error", ej["state"] == "failed" and ej["problem"]["code"] == "scanner_error"
              and ej["problem"]["status"] == 502, ej)
    s, sc1 = call("GET", f"/scans/{station_scan}", "viewer")
    check("other scans survive the renumbering", s == 200, sc1)

    s, log = call("GET", f"/log?project_id={pid}", "viewer")
    check("field log records the project's scans", s == 200 and sum(e["kind"] == "scan" for e in log) >= 4, len(log or []))
    check("approvals and finished jobs are in the project's log", any(e["kind"] == "approval" for e in log)
          and any("finished" in e["text"] for e in log), [e["text"] for e in log])
    s, b = call("POST", "/log", "agent", {"text": "Plant moved out of the way", "project_id": pid})
    check("agent adds a log note", s == 201 and b["author"] == "agent-1", b)
    time.sleep(0.3)
    check("event stream delivered job and log events", "job" in events and "log" in events, sorted(set(events)))

    # Grants survive a restart: a second bridge on the same data directory has them.
    s, gk = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 7})
    s, gr = call("POST", "/grants", "operator", {"project_id": pid, "expires_at": "2099-01-01T00:00:00Z", "max_scans": 5})
    call("DELETE", f"/grants/{gr['grant_id']}", "operator")
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": f"http://{MOCK}/api/v1",
                                  "BRIDGE_ADDR": "127.0.0.1:8796", "BRIDGE_TOKENS": tokfile, "BRIDGE_DATA": os.path.join(tmp, "data")},
                                  stderr=subprocess.DEVNULL))
    wait_up("127.0.0.1:8796", "/v1/health")
    s, gl = call("GET", "/grants", "viewer", base="127.0.0.1:8796")
    ids = [g["grant_id"] for g in gl] if s == 200 else []
    check("grants survive a restart (a revoked one doesn't)", gk["grant_id"] in ids and gr["grant_id"] not in ids
          and next(g for g in gl if g["grant_id"] == gk["grant_id"])["max_scans"] == 7, (s, ids))
    call("DELETE", f"/grants/{gk['grant_id']}", "operator")

    # A bridge whose scanner is unreachable.
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": "http://127.0.0.1:9/api/v1",
                                  "BRIDGE_ADDR": "127.0.0.1:8798", "BRIDGE_TOKENS": tokfile}, stderr=subprocess.DEVNULL))
    wait_up("127.0.0.1:8798", "/v1/health")
    s, b = call("GET", "/status", "viewer", base="127.0.0.1:8798")
    check("disconnected scanner shows as connected: false", s == 200 and b["scanner"]["connected"] is False, b)
    s, b = call("POST", "/scans", "operator", dome, base="127.0.0.1:8798")
    check("scan with no scanner is 503 scanner_disconnected", s == 503 and b["code"] == "scanner_disconnected", b)

    # A bridge that can't write its cache: `scans` is a file, not a directory.
    bad_data = os.path.join(tmp, "bad-data")
    os.makedirs(bad_data)
    open(os.path.join(bad_data, "scans"), "w").write("not a directory")
    procs.append(subprocess.Popen([f"{BIN}/x9-bridge"], env={**os.environ, "X9_BASE": f"http://{MOCK}/api/v1",
                                  "BRIDGE_ADDR": "127.0.0.1:8797", "BRIDGE_TOKENS": tokfile, "BRIDGE_DATA": bad_data},
                                  stderr=subprocess.DEVNULL))
    wait_up("127.0.0.1:8797", "/v1/health")
    s, b = call("GET", f"/scans/{station_scan}/preview", "viewer", base="127.0.0.1:8797")
    check("a cache the bridge can't write is 500 cache_error", s == 500 and b["code"] == "cache_error", b)

    # The simulator serves the item routes the x9 CLI uses.
    for args, key in ((["scan", "0"], "scanUUID"), (["tzf", "0"], "nbPoints")):
        p = subprocess.run([f"{BIN}/x9", "--base", f"http://{MOCK}/api/v1", *args], capture_output=True, text=True)
        out = json.loads(p.stdout) if p.returncode == 0 else {}
        check(f"x9 {args[0]} works against the mock", p.returncode == 0 and out.get(key) is not None, (p.returncode, p.stderr))
finally:
    for p in procs:
        p.terminate()
    for p in procs:
        p.wait()
    # Each run's data directory holds scans and exports (half a gigabyte with fixtures).
    shutil.rmtree(tmp, ignore_errors=True)

print(f"\n{sum(results)}/{len(results)} checks passed")
sys.exit(0 if all(results) else 1)
