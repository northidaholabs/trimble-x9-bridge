# Spec: Question checks on the laptop, and only the checks that changed

Owner: Jim · Date: 2026-10-28 · Status: draft, not approved

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

Three parts. Part A helps every visit, laptop or not. Part B is the
laptop. Part C keeps the Pi's disk from filling.

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

Then the targets that are left are judged at up to two at once on the Pi
(`BRIDGE_CHECK_PARALLEL`, default 2). Decoding a scan briefly needs about
0.9 GB, so the cap is memory, not cores. A target's stations are decoded
once per re-check, not once per target.

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

**Laptops:** Apple Silicon and Intel Macs both go to site. Every release
carries both macOS builds; a release with either missing isn't installed
on a kit.

`scanner_ask` and `scanner_processing` say when a check ran on the laptop.
Nothing else about the tools changes.

### C. A budget for the Pi's scan cache

`BRIDGE_CACHE_MB` (default: whatever leaves 1 GB free on the data disk at
start, at most 4096). When a fetch would pass it, the bridge drops the
cached scan files of other projects first, oldest first, then this
project's stations whose 5 cm clouds are already prepared and whose files
a laptop worker holds. It never drops a file a running job is reading. The
scanner keeps every scan, so a dropped file is fetched again if it is
needed. When the cache can't get under budget, the field log says so once
and suggests connecting the laptop.

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
   count fitted on an Apple Silicon Mac, an Intel Mac and the Pi gives the same verdict,
   and plumb, lean and flatness to 0.1 mm. The bridge is deterministic for
   the same input; the margin is for floating point on two architectures.
   Any verdict that differs fails the spec, not just the run.
6. metric: The kit, with the laptop on the cable and its cache warm, Jim's
   timings, three runs, slowest given: the 13-wall re-check after station
   12 of the held-out survey is under 30 s, and one ask on w6 under 10 s,
   from the registration's end to the verdicts being served. Run once on
   an Apple Silicon Mac and once on an Intel Mac; the Intel run must also
   be under `on-site-answers.md` criterion 8's limits (90 s and 120 s).
   This is how criterion 8 is shown for the kit.
7. test: With `BRIDGE_CACHE_MB` set to 300 and the admin survey replayed
   station by station, the cache never passes 300 MB, every station still
   registers, and the log carries the budget note once.

## Kill-if

- Criterion 5 fails after one fix: the laptop path is dropped. Checks stay
  on the Pi with Part A.
- Criterion 6 fails on either Mac on 2026-11-20: `on-site-answers.md`'s
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
- Cache drops on the Pi, and the free space at sign-off.

Jim reads them after the fifth visit with questions or on 2027-03-31,
whichever comes first. If the laptop fitted fewer than a quarter of the
checks on those visits, Part B is reviewed for removal.

## Decided

| Question | Answer | By |
| --- | --- | --- |
| Is criterion 8 of `on-site-answers.md` for the Pi alone or the kit? | The kit, with the laptop. | Jim, 2026-10-28 |
| Which laptops go to site? | Apple Silicon and Intel Macs; binaries are built as needed. | Jim, 2026-10-28 |
| An ask while a registration runs: Pi now, or wait? | Wait, and check on the new registration (Part B). | Spec, 2026-10-28 |
