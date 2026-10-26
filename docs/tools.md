# Tools: the `x9` CLI, the `x9-mock` simulator, and the tests

This page covers the programs that ship next to the bridge, and how the
project is tested. See [guide.md](guide.md) for the bridge itself.

## The `x9` CLI

`x9` (crate `crates/x9-cli`) is a small smoke-test CLI over `x9-client`. It
prints the scanner API's answers as pretty JSON.

> **`x9` talks to the scanner API directly, not to the bridge.** None of the
> bridge's rules apply to it: no roles, approvals, battery or busy checks, no
> echo check, no field log. Use it to look at a scanner, to pull files, and to
> test. Do not use it to scan while the bridge is running: the bridge sees
> that task as another client's, and a bridge scan that is due to start while
> it runs fails with `scanner_busy`.

```text
x9 [--base <URL>] <command>
```

| Option | Default | What |
| --- | --- | --- |
| `--base <URL>` | `http://192.168.42.1:34567/api/v1`, or `$X9_BASE` when set | The scanner API base URL |

### Commands

| Command | Scanner request | What |
| --- | --- | --- |
| `status` | `GET /status` | Full scanner status: battery, levelling, storage, motion sensor, Wi-Fi, errors |
| `level` | `GET /status` | Only the levelling part of the status |
| `scans` | `GET /scans` | All scans, with the scanner's numeric ids, UUIDs and file names |
| `scan <ID>` | `GET /scans/{id}` | One scan by the scanner's numeric (positional) id |
| `scan-modes` | `GET /scans/modes` | Scan modes with range, spacing, point counts and times |
| `projects` | `GET /projects` | All projects |
| `tasks` | `GET /tasks` | Tasks the scanner is running or still lists |
| `errors` | `GET /errors` | The scanner's current error list |
| `diagnostics` | `GET /diagnostics` | The scanner's diagnostics list |
| `tzf <ID>` | `GET /tzfs/{id}` | Metadata of one scan file (path, size, point count) |
| `download-tzf <ID> <DEST>` | `GET /tzfs/{id}/file` | Save a scan file to `DEST` |
| `preview <ID> <DEST>` | `GET /tzfs/{id}/preview` | Save a scan's preview JPEG to `DEST` |
| `pull [options]` | `GET /scans`, then files | Download many scan files into a directory tree |
| `launch-scan [options]` | `POST /scans` | **Starts a scan: the head turns and the laser fires.** Prints the scanner's task. |

`<ID>` for `tzf`, `download-tzf` and `preview` is the scan file's id (`tzf.id`
or `tzfId` in `x9 scans`), not the scan's UUID.

### `pull`

```bash
x9 pull --out x9-pull --project kitchen --previews
```

| Option | Default | What |
| --- | --- | --- |
| `--out <DIR>` | `x9-pull` | Where to write. Files keep the scanner's paths: `<DIR>/<project>/<N>.tzf`. |
| `--project <TEXT>` | all | Only scans whose file path contains this text (for example a project name) |
| `--limit <N>` | all | Only the first `N` scans after sorting by file path |
| `--previews` | off | Also save each preview as `<N>.preview.jpg` next to the scan file. A failed preview is reported and skipped. |
| `--dry-run` | off | List what would be downloaded, with sizes, and download nothing |

`pull` first prints how many scans were selected and their total size. Scans
without a file are skipped. Existing files are overwritten.

### `launch-scan`

```bash
x9 launch-scan --project-name kitchen --project-uuid <uuid> --scan-mode STD_L0
```

| Option | Default | What |
| --- | --- | --- |
| `--project-name <NAME>` | none | The project's name |
| `--project-uuid <UUID>` | none | The project's UUID |
| `--scan-mode <MODE>` | `STD_L0` | The scan mode |

Pass **both** `--project-name` and `--project-uuid` to choose a project. With
only one, or neither, the scanner scans into its last project. The scan is a
full dome with the scanner's defaults (it sends no color or window settings).
`launch-scan` does not wait; follow it with `x9 tasks`.

## `x9-mock`

`x9-mock` (crate `crates/x9-mock`) simulates the scanner API, so the bridge,
clients and tests run without hardware. It serves `/api/v1` with the same
shapes as a real scanner and keeps state in memory.

```bash
x9-mock                                    # http://127.0.0.1:34567/api/v1
X9_MOCK_SPEED=50 X9_MOCK_ADDR=127.0.0.1:34599 x9-mock
```

### Environment

| Variable | Default | What |
| --- | --- | --- |
| `X9_MOCK_ADDR` | `127.0.0.1:34567` | Listen address (the real scanner's port) |
| `X9_MOCK_SPEED` | `10` | Time compression: every simulated duration is divided by this. At 10, a scan that takes 54 s on a scanner takes about 5 s. |
| `X9_MOCK_BATTERY` | `87` | Battery percent reported in `/status`. Fixed; it does not drain. |
| `X9_MOCK_ON_AC` | `0` | `1` reports AC power; anything else reports battery |
| `X9_MOCK_CANCEL_LINGER` | `13` | Seconds a cancelled task stays listed in `/tasks`, divided by the speed factor like every other duration |
| `X9_MOCK_FALLBACK_MODE` | unset | A scan mode name. Requests for this mode are echoed, and run, as a default color full dome in `STD_L0`, to test a client's echo check. |
| `X9_MOCK_FIXTURES` | unset | A directory with `dome.tzf`, `dome.jpg`, `area.tzf`, `area.jpg`. Served as the file and preview of every full-dome and area scan. Without it, file and preview requests answer `404`. |

Unparsable numbers fall back to the default.

### What it simulates

Routes: `GET /status`, `GET`/`POST /scans`, `GET /scans/modes`,
`GET`/`DELETE /scans/{id}`, `GET`/`POST /projects`, `GET /tasks`,
`DELETE /tasks/{id}`, `GET /tzfs/{id}` (the `tzf` object `/scans` embeds),
`GET /tzfs/{id}/file`, `GET /tzfs/{id}/preview`, `GET /errors`,
`GET /diagnostics`. Every CLI command works against it. Item routes take the
positional ids (`id` and `tzfId` in `/scans`) and answer `404` for an id the
mock does not have.

Start state: one project, `mock_project`, with one full-dome scan
`mock_project/1.tzf`. Status: levelled, compensator on, not moved,
about 24 GB free, battery and AC as configured. Four scan modes: `STD_L0`,
`STD_L1`, `STD_L2`, `HS_L1`.

A scan (`POST /scans`) answers `202` with a task whose `scanRequest` echoes
the request, then walks through these phases (real-scanner seconds, divided by
`X9_MOCK_SPEED`):

| Phase | Duration | Task |
| --- | --- | --- |
| Levelling | 18 s | `state: initializing`, `percentage: 0` |
| Sweep | 22 s full dome, 8 s area | `state: running`, `percentage` 0.1 → 1.0 in ten steps |
| File ready | 6 s after the sweep | The scan appears in `/scans`; the task's `file` is set |
| Color imaging (color on only) | about 150 s | `percentage` 0 → 1 in steps of 1/30 |
| Clear | 2 s | The task leaves `/tasks` |

Other behaviour, matching what was observed on a real scanner:

- **One task at a time.** A scan request while a task is listed answers `409`.
- **File names.** A full dome is filed as `<project>/<N>.tzf` and advances
  the project's station counter. An area scan is filed as
  `<project>/<N>_<k>.tzf` under the latest station `N`.
- **Point counts.** 6,281,859 points for a full dome, 777,580 for an area
  scan, whatever the mode; file size is 9 bytes per point.
- **Cancel** (`DELETE /tasks/{id}`): `200` for the current task (also when
  repeated), `404` for any other id. The task stays listed for
  `X9_MOCK_CANCEL_LINGER` with `exitResult: "none"`, and new scans are refused
  meanwhile. If it was cancelled during the sweep, a scan is filed when it
  clears, with the points swept so far (at least 1 %). Cancelled while
  levelling, nothing is filed.
- **Positional ids.** Deleting a scan renumbers the ids of the others.
- **Fallback mode.** With `X9_MOCK_FALLBACK_MODE`, a request for that mode is
  echoed as `STD_L0`, `FullDome`, color `ON`, `startHA`/`startVA` `-1`, and
  runs that way.
- `POST /projects` stores the name and UUID it is given.
- Fixture files are chosen by the scan's file name: names with `_` get
  `area.*`, others `dome.*`.

## Registration offline: `x9-bridge register` and `x9-bridge bench`

Both read recorded stations (`.tzf` files) and run the same registration
the bridge runs on site, one station more each time, then the audit.

```bash
x9-bridge register [--perturb N:dx,dy,deg] [--truth poses.json] OUT_DIR station_*.tzf
```

`register` writes `poses.json`, `pairs.json`, `audit.json`, `walls.json`, a
plan and a minimap to `OUT_DIR`, and prints each station's verdict and
checks. `--perturb` moves a station after the solve (a planted error the
audit should catch); `--truth` reports each station's distance from known
poses.

```bash
x9-bridge bench [--truth poses.json] [--orders N] [--plant N:dx,dy,deg]… [--no-plants] \
  [--threads T] [--json OUT.json] [--scoreboard FILE] [--label TEXT] SURVEY_DIR
```

`bench` tells whether a change to registration is better. The registration
is deterministic (the same files in the same order give the same poses), so
what varies on site is the input. The bench keeps the scans fixed and varies
the rest on purpose:

- **Orders:** the stations in the given order, reversed, and seeded shuffles
  (`--orders`, default 4). Each order is a separate on-site story: different
  stations exist when each one is placed, and groups form and join
  differently.
- **Planted errors:** on the given order's result, a station is moved and the
  audit is run again. By default three stations (the second, the middle, the
  last) are each moved 3 cm, 30 cm and 1°; `--plant` sets your own.

Each result is scored by what the mistake costs, against `--truth` (by
default the given order's own solve; a placement within 30 mm and 0.1° is
right):

| | |
| --- | --- |
| **Wrong accepts** | Trusted but wrong, or a planted error not caught. The person has already left, so this is the costly one. The bench **fails** on any. |
| **Wrong rejects** | Right but not trusted, or a good station flagged because of a planted one: a re-scan or a question. |
| **Unplaced** | Stations no order could place. |
| **Error** | Median and worst distance of trusted stations from the reference. |
| **Time, memory** | Solve time per order, and the run's peak memory. |

To compare two builds, run both on the same surveys with the same options;
the survey's fingerprint (station count and a hash of names and sizes) is in
the report, so scores are only compared on the same set. `--scoreboard`
appends one line per run to a file, to see a slow drift over many changes.

It is gentle on the machine: each station is decoded and prepared once,
cases run one after another, it uses `--threads` threads (default a quarter
of the cores, at most 8), and it won't start with less than about 3 GB of
memory free. On a shared machine, run it under `nice -n 19 ionice -c3`.

## Deliverables: `x9-bridge analyze`

Turns a registered survey into numbers and pictures for construction and
site work. It reads a `register` output folder (poses, audit, walls) and the
same station files, and goes back to each station's full-resolution points,
one station at a time (about 1 GB at most; 34 s for a 19-station site).
Only stations the audit trusts are used.

```bash
x9-bridge register REG_DIR station_*.tzf
x9-bridge analyze [--straightedge 3] [--tolerance-mm 6] [--section x1,y1,x2,y2]… REG_DIR OUT_DIR station_*.tzf
x9-bridge analyze --compare [--min-change-m 0.02] BEFORE_OUT AFTER_OUT CHANGE_DIR
```

| Output | What |
| --- | --- |
| `report.html` | Everything below on one page, with the pictures |
| `wall-<id>.png`, `walls.csv` | Each building wall from the stations' side: departure from its best-fit plane (red proud, blue set back, grey openings and reliefs more than 50 mm off), its lean over its height and per metre-wide strip, flatness (RMS, 95th percentile), a straight-line check (windows of `--straightedge` metres along rows and columns, each within `--tolerance-mm` of its own straight line), and its top above the ground at its foot |
| `ground.png` | The ground and floors: the lowest level surface in each 10 cm cell, cars and other things more than 25 cm above their surroundings left out, with contours |
| `flatness.png` | Bumps and dips: each cell against the ground within half a straightedge around it, so slopes drop out and ruts, gutters, kerbs and potholes show |
| `low_spots.png`, `low_spots.csv` | Where water would stand before spilling over the ground around it (0.5 m² or more, 10 mm or deeper): position, area, depth, volume. A drain the scan didn't see empties a low spot |
| `section-N.png`, `section-N.csv` | The ground along a line, with the building walls it crosses at their measured height |
| `plan.dxf` | Walls (by kind), stations and low spots, in metres, for CAD |
| `ground.f32`, `ground.json` | The height grid, for `--compare` |
| `analysis.json` | All of the above as data |

`--compare` measures cut and fill between two visits' height grids in the
same frame, counting cells that changed by `--min-change-m` or more (a scan's
millimetres of noise, summed over a whole site, would otherwise add up to
cubic metres), and draws the change. On the admin building's ground with a
planted 18.85 m³ stockpile and a 2.40 m³ pit (synthetic), it reports 18.84
and 2.40 m³.

What the numbers are and aren't:

- Coordinates are the project frame (station 1's), not north or site
  coordinates. Two visits compare only when they share a frame.
- A wall's plane mixes every station that saw it, so a few millimetres of
  registration mismatch show up as roughness. Profiled cladding (ribbed
  metal, siding) measures as its profile.
- Heights are to the top of what was seen from the ground: a parapet or roof
  edge, not a roof behind it.
- Precision, not accuracy: nothing here is checked against an independent
  measurement (tape, level, total station). Do that before using a number
  for acceptance or payment.

## Tests

### Unit tests

```bash
cargo test
```

`x9-bridge` has 13 unit tests:

| Module | Covers |
| --- | --- |
| `rules` | Viewers refused; agents wait for approval unless granted; battery (and on-AC), storage, connection and queue-full refusals; grant limits (used up, mode, color, expiry) |
| `aim` | A recorded preview box reproduces the window of a real area scan; seam wrap; bad boxes refused |
| `recipe` | A recipe parses and validates; unknown operations, forward references, duplicate ids and unknown inputs are refused; placeholder resolution keeps types and interpolates text |
| `worker` | The echo check (float tolerance, the fallback case); telling a task the bridge cancelled from another client's |
| `map` | Scan file names → project, station and area index |
| `util` | RFC 3339 formatting and parsing (offsets, fractions) |

`x9-mock` has 2: a task cancelled mid-sweep lingers, refuses new scans, then
files a partial scan; a task cancelled while levelling files nothing.

### End-to-end test

```bash
cargo build --release
python3 crates/x9-bridge/tests/e2e.py target/release
```

`e2e.py` takes the directory holding the built `x9-mock` and `x9-bridge`. It
needs only Python 3's standard library. It:

- starts `x9-mock` on `127.0.0.1:34599` (speed 20, fallback mode `HS_L1`,
  cancel linger 40, which is 2 s at speed 20);
- writes a token file with a viewer, an agent and an operator, and a recipe
  directory with the window recipe and one broken recipe;
- starts the bridge on `127.0.0.1:8799` with `BRIDGE_POLL_MS=200` and a
  temporary `BRIDGE_DATA`, and later a second bridge on `127.0.0.1:8798`
  whose scanner is unreachable and a third on `127.0.0.1:8797` whose
  `BRIDGE_DATA` cannot be written;
- runs `x9 scan` and `x9 tzf` from the same directory against the simulator;
- listens to the event stream, walks the main flows, and prints `PASS` or
  `FAIL` per check, then `N/M checks passed`. It exits with `1` if any check
  failed.

Those ports must be free. Without fixtures it runs 65 checks: health and
auth, status and modes, projects, refusals (viewer, missing window, unknown
mode), another client's task (`scanner_busy`), the echo check, approval, scan
phases, waiting out a cancelled task, grants (and a cancelled scan giving its
use back) and area scans, recipe listing and validation, a run waiting for
approval, who may advance and cancel it, `addLogNote` text, cancel mid-sweep
with `partial_scan_id`, the scan file cache (served while listed, evicted on
delete or when the scanner no longer lists the scan), confirmed deletes,
`scanner_error` and `cache_error`, the field log, events, a disconnected
scanner, and `x9 scan` / `x9 tzf` against the simulator.

### `E2E_FIXTURES`

```bash
E2E_FIXTURES=/path/to/fixtures python3 crates/x9-bridge/tests/e2e.py target/release
```

`E2E_FIXTURES` names a directory with `dome.tzf`, `dome.jpg`, `area.tzf` and
`area.jpg`: a real full-dome scan with its preview, and a real area scan with
its preview. It is passed to the simulator as `X9_MOCK_FIXTURES`. It adds 17
checks and skips the 2 that need a scanner without files (80 in all):
previews and scan files through the bridge, aiming, PLY and LAS exports with
their checks, a running export not counted as the scanner's job, cancelling
an export, a delete removing the scan's exports, and a full run of the window
recipe.

The fixture checks compare against the author's own scans, which are not in
the repository: the aim check expects one particular window, and the export
checks expect 777,580 and 6,281,859 points. With other scans those checks
fail on the numbers even when the bridge works.

### CI

`.github/workflows/ci.yml` runs `cargo test --locked`, a release build, and
`e2e.py` without fixtures on every push and pull request.
`.github/workflows/release.yml` builds the release archives for tags `v*`.
