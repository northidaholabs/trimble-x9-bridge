# Spec: Question checks on the laptop, and only the checks that changed

Owner: Jim · Date: 2026-10-28, revised 2026-11-12 · Status: all four parts approved and built (A, B and C 2026-11-11; D 2026-11-11)

## Problem

Since 0.15.0 a mission target can carry a question ("is this face plumb?").
After every registration the bridge judges every question target again
(`ask::recheck`), one target after another. Each judgement (`ask::look`)
fits the face on the full-resolution points of every trusted station that
saw it, decoded from the cached scan files.

On the Pi 4 that is too slow for a site visit (2026-10-28, Jim's timings,
held-out survey at 12 stations, no worker, three runs each, slowest given):

- One ask, face w6, seen by 7 stations: 44.1 s.
- The re-check of all 13 building walls of 4 m or more: 512.0 s, 26 to 54 s
  a wall, one wall at a time on one core.

`on-site-answers.md` criterion 8 allows 90 s for one ask and 120 s for the
re-check, and kills the on-site parts if the slowest run is over 3 min on
2026-11-20. The re-check is four times over that line. The same ask takes
7.3 s on the 32-core desktop.

Those limits are for the kit as it goes to site, with the laptop on the
cable (Jim, 2026-10-28), not for the Pi alone. So the laptop can carry
criterion 8, and the Pi alone only has to stay usable when the laptop is
away.

Two things make it slow, and they are separate:

1. **The Pi does work it doesn't need to.** Every target is re-judged after
   every registration, including targets the new station didn't see and
   whose stations didn't move. By the settling rule ("after a station that
   didn't see the face, it stays settling"), most of those judgements can't
   change anything.
2. **The Pi is the slowest computer on site.** Since 0.12 a laptop on the
   USB-C cable can take registration off it (`x9-bridge worker`, `work.rs`),
   and it already holds every station's 5 cm cloud in its cache. Question
   checks don't go through it: the worker claims only registration jobs, and
   it deletes each scan file once it has decoded the 5 cm cloud.

There is also a storage limit. The Pi caches every scan file it fetches
under `$BRIDGE_DATA/scans` (about 95 MB per station on the admin survey:
1.8 GB for 19), and evicts one only when the scanner deletes the scan. The
SD card has 1.5 GB free, so a long visit runs the Pi's disk out around
station 15. Nothing in the log warns before it does.

## Ship

Four parts. Part A helps every visit, laptop or not. Part B is the
laptop. Part C keeps the Pi's disk from filling. Part D makes the laptop
the place the data lands.

### A. Re-check only what can change (the Pi alone)

After a registration, `recheck` skips a target when all of these hold:

- no station added since its last judgement saw the face (by
  `face_coverage`'s station list for the new registration);
- every station that saw it at the last judgement is still trusted and
  still in that list;
- none of those stations' poses moved by more than 1 mm or 0.001° in the
  new registration;
- its face, question and tolerance haven't changed.

A skipped target keeps its verdict and its settling ledger as they are,
which is what `decide` would produce for a station that didn't see the face.
The field log says how many targets were re-checked and how many skipped,
and why.

The targets that are left are fitted together: each station's file is
decoded once per re-check, not once per target, and only if one of the faces
left needs it. Decoding a scan briefly needs about 0.9 GB, so files are
decoded one at a time; the fits then run up to two at once
(`BRIDGE_CHECK_PARALLEL`, default 2). A face's fit takes points only from its
own stations, so fitting faces together gives each the fit it gets alone.

`x9-bridge ask --skip` applies the same rule from one count to the next, so
`tests/replay_answers.py --part-a` can show criterion 2 on the recordings.

Built 2026-11-11: the basis of each judgement is kept in memory, so after a
restart every target is judged once more before skipping starts.

### B. Checks on the laptop worker

A new job kind, `check`, offered through the same `POST /v1/work/claim`
route, after a registration and for a single ask:

- **The offer:** the project, the registration it belongs to, and per
  target the face, the coverage the bridge already computed (`seen_share`,
  `length_m`, `height_m`, the stations that saw it) and the stations' poses
  and scan ids. Only targets that pass the coverage part of the gate are
  offered; the rest are decided on the Pi with no fit, as now.
- **The work:** the worker fits each face (`measure_faces`, as `look` does)
  on its own cores and returns, per target, the `Reading` and, for a reading
  that could be an `answer`, the evidence heat map.
- **The record stays on the Pi:** the bridge runs `decide` (the settling
  ledger, the tolerance, the verdict) on what comes back, exactly as when it
  fits itself, logs "checked on <worker>", and serves the verdicts. One
  place decides; the laptop only measures.
- **Falling back:** the existing rules, with a shorter limit. The bridge
  fits the faces itself when no worker has been seen for 15 s, none claims
  within 10 s, the worker fails, or it takes longer than 120 s for a
  re-check or 60 s for one ask.
- **The scan files:** the worker keeps the full scan file of every station
  it decodes for registration (`--cache`, default `~/.cache/x9-worker`), so
  a check needs no transfer for stations already registered. It fetches any
  it is missing over the bridge, as registration does. The cache has a size
  budget (`--cache-gb`, default 20) and drops the oldest project first.

**An ask while a registration is running** waits for that registration
and is then checked on the new one, wherever the registration runs. The
newer registration may move the face's stations or add one that saw it, so
an answer from the older one could be out of date as it is given. The
chat says the bridge is placing the new station and that the answer
follows.

**Laptop:** the kit's laptop is an Apple Silicon Mac. A release without
the Apple Silicon build isn't installed on the kit.

`scanner_ask` and `scanner_processing` say when a check ran on the laptop.
Nothing else about the tools changes.

Built 2026-11-11:

- The worker sends back each face's reading and, for a face it fitted, the
  fit itself (its grid, compressed, uploaded with `PUT …/merged` before the
  result). The bridge draws the evidence from it exactly as when it fits
  itself, since the picture depends on the verdict only the bridge decides.
- Only faces that pass the coverage part of the gate are offered; a check
  carries the stations that saw them, with scan ids and poses.
- An answer says where it was fitted (`checked_on`), the log says "checked
  on <worker>", and `GET /v1/workers` gives each worker's `job_kind`.
- The worker keeps every scan file it fetches, for registration or a check,
  within `--cache-gb` (default 20), dropping the least recently used
  project's files first and never the one at work.
- A result for a job the bridge took back is refused (409) before its body
  is read.

Built 2026-11-12: counting openings goes the same way, as a `count` job
([`construction-counts.md`](construction-counts.md)): the worker finds each
bound face's openings and draws its picture, and the Pi checks what comes
back, matches, judges and records. A claimed count gets 3 minutes.

Built 2026-11-12: decoding the stations, not fitting them, was most of a
check's time on the laptop (a 10.9 m wall seen by 7 stations: about 9 s of
decoding, under 1 s of fitting). The worker now keeps decoded stations in
memory (`--mem-gb`, default 3 GB or a quarter of the computer's memory if
less; least recently used dropped first), each as it registers and each a
check or count decodes, and decodes up to four at once. On the held-out
survey, replayed through a bridge on the same laptop with the worker's real
path, against the 0.19.0 worker: the 15-wall re-check after station 12 in
5.9 s (20.8 s), and one question on that wall in 1.9 s right after it
(10.9 s), 6.2 s with nothing kept (10.0 s). The fits are byte-identical. The
kit's timings for criterion 6 are taken again with it.

A worker does one job at a time, and says nothing to the bridge while it
measures but its progress. A registration offered while a worker holds a
check or a count keeps waiting until that job is delivered, fails, is
taken back at its limit or loses its waiter (a count's request cut off),
however long since the worker was seen, and never past the job's limit from
its claim; it goes to the Pi only if no worker claims it within 10 s after
that. A count asked over the API runs on even if its request is cut off, and
its result is kept for `GET …/count`. Registrations
are claimed first, so the laptop takes it as soon as the short job ends. A
laptop that hangs on a count delays a registration by at most 3 minutes and
10 s; on a check, 2 minutes and 10 s.

### C. A budget for the Pi's scan cache

`BRIDGE_CACHE_MB` (default: whatever leaves 1 GB free on the data disk at
start, at most 4096). When a fetch would pass it, the bridge drops the
cached scan files of other projects first, oldest first, then this
project's stations whose 5 cm clouds are already prepared and whose files
a laptop worker holds. It never drops a file a running job is reading. The
scanner keeps every scan, so a dropped file is fetched again if it is
needed. When the cache can't get under budget, the field log says so once
and suggests connecting the laptop.

Built 2026-11-11, the first tier: other projects' files, oldest first, never
one of a project with a job running or a registration in progress, and none
of no known project while any job runs. The second tier came with part D:
any project's files that a worker seen in the last 15 s holds and whose 5 cm
cloud is prepared, oldest first. Neither tier drops a file being read on the
Pi (a fit, an export, a measurement, a viewer build) from before it is
fetched until the reading ends.

### D. The laptop is where the data lands

The scan data goes to the laptop anyway, so the finished point cloud can be
saved from there to its permanent destination (Jim, 2026-11-11). So:

- The worker keeps the full scan file of every station it registers, as
  part B already says, and tells the bridge which it holds.
- The Pi's cache is staging: once the laptop holds a station's file and the
  station's 5 cm cloud is prepared, the Pi may drop its copy (part C's
  second tier).
- At the end of a job, the registered point cloud is exported on the laptop
  and saved to its permanent destination: for now a folder on the laptop,
  one per project. Later it moves to the cloud; the folder is where that
  upload will start from.

Built 2026-11-11:

- Each claim (`POST /v1/work/claim`) carries `holds`: the scan files in the
  worker's cache. The bridge keeps them per worker and counts them only
  while the worker is seen.
- A `save` job is offered after the mission's sign-off and on
  `POST /v1/projects/{id}/save` (agent or operator). It is a bridge job
  (`kind: save`), running from the start with the phase "waiting for the
  laptop", then "saving on <worker>". One waits per project; asking again
  while it waits returns it. Its body is built when a worker claims it, so
  it carries the registration as it then stands, and it isn't handed out
  while the project registers or while another save of it runs.
- There is no fallback on the Pi. A save waits for a worker however long it
  takes; a worker that says nothing for 5 minutes loses it and it goes back
  on offer. A failed save is logged with the reason, and nothing on the Pi
  was dropped for it. A cancel takes it back.
- The worker writes the folder under `--save-dir` (default `~/X9 Projects`),
  named after the project and the start of its id: every station's scan
  file, as the scanner made it (a hard link to the cached file when they
  share a disk) and preview; each placed station's points moved by its pose
  into the project frame as COPC (`stations/station-NN.copc.laz`,
  `-unconfirmed` when the registration's checks don't trust it); the
  registration and mission as JSON; and last `manifest.json`, which lists
  every station and every file with its size and sha256. A station whose
  scan and pose are as at the last save keeps its cloud. A station that
  can't be saved is noted in the manifest and the log, and the rest are
  saved.
- The bridge logs "Saved the project on <worker>: <folder> (…)", keeps the
  latest save of each project (`saves.json`, across restarts), and gives it
  with any save running on `GET /v1/projects/{id}/save`.

The format is COPC, one file per station in the project frame, in the
project frame's own local CRS (the frame station's levelled scanner frame).
One merged cloud is left to the cloud copy, where memory isn't the Pi's or
the laptop's. Still open: who confirms the cloud copy arrived, once there is
one.

**What it opens up (a follow-on, its own spec, [`whole-cloud-questions.md`](whole-cloud-questions.md)):** with every station's file
and the merged cloud on the laptop, the agent can ask about the whole cloud,
not only the walls a mission fixed beforehand: a height, a distance, a
section, whether a floor is level. The laptop measures where the data and
the cores are; COPC's index lets a question read only its region at the
detail it needs, on site now and from the cloud copy later. The rules carry
over: the bridge records and judges each answer with its evidence, every
kind of question gets its own gate (no figure until the data carries one),
and nothing is claimed in public until it has been shown.

## Acceptance criteria

Every criterion is a ship gate.

1. test: A target is skipped after a registration when the new station
   didn't see it and its stations' poses didn't move; it is judged when
   any of the four conditions in Part A fails (one test each). A skipped
   target's verdict and settling ledger are byte-identical to what `decide`
   gives for a station that didn't see it.
2. metric: Replays (`tests/replay_answers.py`, held-out and admin, as
   criterion 6 of `on-site-answers.md`) with Part A on give the same
   verdict and the same plumb to 0.1 mm for every target at every count as
   with Part A off. The script prints both and the number of fits skipped.
3. metric: On the Pi with no worker, Jim's timings, three runs, slowest
   given, the held-out survey with targets on all 13 walls of 4 m or more:
   the re-check after station 12 is under 3 min, so a visit whose laptop
   goes away stays usable. This is not criterion 8; criterion 6 below is.
4. test: A `check` job offered with no worker connected is fitted on the
   Pi within 10 s of being offered. A worker that claims and goes silent
   loses the job at 120 s (re-check) and 60 s (ask), and the Pi fits it.
   A result for a job the bridge already took back is refused (409).
5. metric: Laptop and Pi agree. On both replays, every target at every
   count fitted on the Apple Silicon Mac and the Pi gives the same verdict,
   and plumb, lean and flatness to 0.1 mm. The bridge is deterministic for
   the same input; the margin is for floating point on two machines.
   Any verdict that differs fails the spec, not just the run.
6. metric: The kit, with the laptop on the cable and its cache warm, Jim's
   timings, three runs, slowest given: the 13-wall re-check after station
   12 of the held-out survey is under 30 s, and one ask on w6 under 10 s,
   from the registration's end to the verdicts being served, on the Apple
   Silicon Mac. This is how criterion 8 is shown for the kit.
7. test: With `BRIDGE_CACHE_MB` set to 300 and the admin survey replayed
   station by station, the cache never passes 300 MB, every station still
   registers, and the log carries the budget note once.
8. test: A save writes every station's scan file byte for byte, each placed
   station's cloud with its decoded points moved by its pose (bounds to
   1 mm), and a manifest whose sizes and checksums match the files. Saved
   again unchanged, no cloud is written again; a station no longer
   confirmed is written again under its `-unconfirmed` name and the old
   cloud goes. A station that can't be fetched is noted and the save
   finishes.
9. test: With a worker seen holding a station's file and the station's
   cloud prepared, the Pi drops that file when a fetch would pass the
   budget, and not before; never a file being read, and none at all with
   no worker seen.
10. metric: Jim, once: the held-out survey saved on the laptop after its
    sign-off. The time from sign-off to "Saved the project", the folder's
    size, and the twelve station clouds opened together in a viewer line up
    by eye at the walls.

## Kill-if

- Criterion 5 fails after one fix: the laptop path is dropped. Checks stay
  on the Pi with Part A.
- Criterion 6 fails on the Mac on 2026-11-20: `on-site-answers.md`'s
  kill line applies as written there.
- Criterion 3 is over 3 min after Part A: without the laptop, the on-site
  re-check is cut back to targets asked in the chat since the last station,
  and the rest are judged in the report.

## Non-goals

- Reports (`analyze`) and next-station plans on the laptop. The planner
  takes about 2 s a station on the Pi; reports are written after the
  visit, on whatever computer has the files.
- Making the laptop required. Every path works on the Pi alone, slower.
- Splitting the merged points into tiles for very large sites. That is a
  separate spec once a job needs more stations than the Pi can hold in
  `BRIDGE_PREPARED_MB`.
- Workers anywhere but on the cable. The token and transfer rules stay as
  they are.
- Speeding up the fit itself. If Part A and Part B together aren't enough,
  that is the next spec, with a profile behind it.

## Telemetry

From the field log, on every visit after this ships:

- Per re-check: targets judged, skipped, and fitted on the laptop or the
  Pi, and the seconds from the registration's end to the verdicts.
- Checks taken back from a worker, and why.
- Cache drops on the Pi (and which tier), and the free space at sign-off.
- Per save: seconds from the sign-off to "Saved the project", stations
  written and kept, and the folder's size.

Jim reads them after the fifth visit with questions or on 2027-03-31,
whichever comes first. If the laptop fitted fewer than a quarter of the
checks on those visits, Part B is reviewed for removal.

## Decided

| Question | Answer | By |
| --- | --- | --- |
| Is criterion 8 of `on-site-answers.md` for the Pi alone or the kit? | The kit, with the laptop. | Jim, 2026-10-28 |
| Which laptops go to site? | The one Apple Silicon Mac; criteria 5 and 6 are run on it alone. Releases still carry the Intel build. | Jim, 2026-11-12 (was Apple Silicon and Intel, 2026-10-28) |
| An ask while a registration runs: Pi now, or wait? | Wait, and check on the new registration (Part B). | Spec, 2026-10-28 |
| What first? | Parts A and C, which help every visit with or without the laptop. | Jim, 2026-11-11 |
| Where does the scan data end up? | On the laptop, and from there the point cloud goes to its permanent destination (part D). | Jim, 2026-11-11 |
| Where is the permanent destination? | A folder on the laptop for now; the cloud later. | Jim, 2026-11-11 |
| What format is the saved cloud? | COPC, one file per station in the project frame, beside the scan files; a merged cloud waits for the cloud copy. | Spec, 2026-11-11 |
| Does a save ever run on the Pi? | No: the Pi has neither the disk nor the place to keep it. It waits for the laptop and says so. | Spec, 2026-11-11 |
