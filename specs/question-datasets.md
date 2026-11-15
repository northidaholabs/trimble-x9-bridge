# Spec: What an answer rests on, and where it is

Owner: Jim · Date: 2026-11-15 · Status: draft, not approved

Every answer the bridge gives rests on some points, a fit and a place.
Today it keeps the verdict, the figure and the picture, and throws the rest
away. When the next station arrives, the question is worked out again from
the scans. When the report is written, it is worked out again from the
final registration, as `whole-cloud-questions.md` drafts it. When the
office asks the same thing, it would be worked out a third time.

This spec keeps what each answer rests on, as a dataset that grows with
the job, and gives every question a place that survives the registration
changing under it. It serves the questions in
[`whole-cloud-questions.md`](whole-cloud-questions.md) and
[`on-site-answers.md`](on-site-answers.md), the counts in
[`construction-counts.md`](construction-counts.md), and the office's
questions in the draft spec *Field and office on one job* (2026-11-13,
kept outside this repo).

## Problem

- **Nothing is reused.** Re-judging a question after a station fits it
  again from every station that saw it, though only the new station added
  anything.
- **A place is a set of coordinates.** A wall question holds a face in
  project-frame metres, and the project frame is an output of
  registration. When the frame moves to a bigger group, the bridge moves
  the stored faces along with it. That covers one case. A place seen only
  by a group that hasn't joined yet has no project-frame coordinates, so
  it can't be asked about until the join.
- **Wall ids don't last.** They are numbered again after every
  registration (`w1`, `w2`, … by kind, then length). A mission target named
  by `wall_id` is turned into a face when the mission loads, and the name
  is gone.
- **The same thing asked twice is two things.** "The north wall" asked in
  the chat and the same wall asked from the office have nothing joining
  them.
- **Nobody can ask what is already known.** There is no "what have we
  found near the back door?". The ledger lists targets, not places.

## The rule

A dataset is never changed, only followed by a new version. What a
question was pointed at is the record. Coordinates are worked out from it,
for each registration, and are never the record.

The bridge's ledger stays the record of verdicts. A stored dataset is only
used while everything it rests on is as it was. Otherwise the question is
judged again.

## Ship

### A. Each answer keeps what it rests on

- When the bridge (or a worker) judges a question, it writes a dataset
  version. Its name is a hash of what it rests on: the kind and its
  arguments, the anchor (part B), each station with its scan's sha256 and
  its pose version, and the version of the code for that kind.
- The version has a small envelope, the same for every kind, as JSON:
  `kind`, `anchor`, `thing`, `stations`, `frame`, `bounds`, `verdict`,
  `figure`, `uncertainty`, `parent` (the version it follows), `made`
  (time, and where it was measured). Nothing kind-specific goes in it.
- The rest is the payload, which is whatever the kind needs, with its own
  format version:
  - points are kept as references, not copies: for each station, the
    cells of its scan grid that fall in the region. A scan never moves
    relative to itself, so the references stay true through every
    registration, and they are small;
  - the fit as JSON (a plane, two faces and a gap, a line and its fall,
    a set of counted objects with their evidence);
  - the picture, as PNG.
- A new kind brings its own payload and needs no change to the envelope,
  the store or the index.

### B. A place has an anchor, a thing and coordinates

- **The anchor** is what was pointed at, in terms that never move. It
  takes one of three forms:
  - a pick: a station and a direction in that scanner's own frame (the
    preview pixel, its row and column);
  - a target: the mission target's id, and the wall it named;
  - words: the place as the person or agent said it ("the north
    facade"), kept as written beside one of the other two.
- **The thing** is what the question is about: a wall, an opening, a run
  of floor, an object. It has an identity that outlives detecting walls
  again. After each registration, a wall found again is matched to one
  before it by overlap (part C), and the new id is linked to the old.
  A question about the north wall follows the wall, not its number.
- **The coordinates** are worked out from the anchor for each pose
  version: in the project frame always; in a world frame (with its EPSG
  code) only when the project has one (part E). They are kept with the
  dataset version they belong to, never on their own.

### C. Reuse

- **After a station.** When a station sees a question's region, the new
  version starts from its parent: only the new station's cells in the
  region are read, and the fit is made again from the parent's references
  plus the new ones. A station that didn't see the region makes no new
  version, as today.
- **After a registration.** When poses move, the version's references are
  moved by the new poses and the fit made again. No scan is searched
  again for the region unless its bounds grew.
- **The same answer either way.** A version made from its parent gives
  the same verdict and figure, to 0.1 mm, as one made from the scans with
  nothing stored. If it ever doesn't, the stored path is wrong and is
  turned off for that kind.
- **Walls matched.** Two walls are the same thing when their faces
  overlap along at least half the shorter one's length and lie within
  10 cm and 5° of each other. A wall that matches two is split or joined
  in the link table, not guessed.
- **Questions matched.** A new question whose thing and kind match one
  already asked joins its line of versions and is not asked twice.
  The ledger shows who asked it, each time.

### D. Where it lives, and the index

- Datasets live in the project folder (`laptop-offload.md` part D) under
  `questions/`, one directory per version, named by its hash. They are
  listed in `manifest.json` with every other file, so the cloud copy
  receives them and checks them the same way.
- The bridge on the Pi keeps the envelopes and the fits of the job it is
  running. The pictures and anything bigger go to the laptop with the next
  save. With no laptop, the Pi keeps them until one is connected, within
  the cache limit it already has.
- **The index** is a copy, not a store. It is built from the envelopes
  (SQLite or DuckDB, with an R-tree over `bounds`), and can be deleted and
  built again at any time with the same results. It answers: what is known
  near here, what was asked about this thing, which answers rest on
  station N, which are out of date.
- The agent gets `scanner_known(project_id, near?, thing?, kind?)`: the
  latest version of each matching question, with its verdict, its
  stations, and whether it is current.

### E. Frames

- Every coordinate says its frame: the project frame of one registration
  version, or a world frame by EPSG code.
- A project has a world frame only from control: known points the scan
  sees, or a position given by the person. Nothing is placed in the world
  by guesswork.
- A transform between two frames (this job to a later visit, the scan to
  a drone survey) is a dataset like any other: what it rests on, its
  version, its uncertainty. A coordinate carried through transforms
  carries their uncertainty with it.

### F. Who sees what

- Datasets follow the project's disclosure (`control-plane.md`). With
  `minimal`, no world coordinates and no pictures leave the site.
- World coordinates of a home are not kept at all unless the person adds
  control. Scans of homes stay private, as `control-plane.md` says.

## Acceptance criteria

1. test: For every question in `whole-cloud-questions.md` criterion 1 and
   every `plumb` test, the version made from its parent and the one made
   with nothing stored give the same verdict and figure to 0.1 mm.
2. test: After a station that saw the region, the new version reads only
   that station's cells (counted), and its `parent` is the version before.
   After one that didn't, no version is written.
3. test: On a synthetic room scanned as two groups of stations that join
   later, a question asked by pick on a station of the second group before
   the join is accepted as `not_yet`, saying it waits for the join. After
   the join it is judged at the picked wall, within one grid cell. A
   question asked before the frame moves to a bigger group is judged at
   the same wall after, with no code that moves stored faces.
4. test: Over every registration of the admin recording, station by
   station, each building wall keeps one identity, or the link table says
   where it was split or joined. No two walls swap.
5. test: The index is deleted and built again. Every query in the test
   suite gives the same results.
6. test: A new kind is added in a test with its own payload. The envelope,
   store and index code are untouched.
7. test: A file in `questions/` whose sha256 doesn't match the manifest
   is refused by the cloud copy's check, as any other file is.
8. metric: Jim's timings on the held-out survey after station 12, three
   runs, slowest given. Re-judging every question after a station takes
   half the time it takes today or less. Stored data per question,
   without pictures, is under 1 MB.
9. demo: On the sim with the admin recording, after station 6, Jim asks
   the agent "what do we know near the north entrance?" The reply lists
   each question there with its verdict, its stations and whether it is
   current, and opens the picture of one. Two models, three runs each.
10. review: With disclosure `minimal`, nothing in `questions/` that leaves
    the site carries a world coordinate or a picture.

## Kill-if

- Criterion 1 fails for a kind after one fix: that kind is judged from
  the scans every time, and stores only its envelope.
- Criterion 4 swaps walls in more than one registration in twenty: wall
  questions are anchored by pick only, and named walls are resolved fresh
  each time, as today.
- Criterion 8's re-judging gets less than 20% faster and the office spec
  isn't approved by 2027-03-31: keep the envelopes and the index for
  "what do we know near here", and stop storing payloads.

## Non-goals

- A fixed table schema for payloads. Only the envelope is fixed.
- Replacing the ledger. Verdicts are recorded there, as now.
- One merged cloud, or a database of points. Points stay in the scans and
  the COPC files; datasets refer to them.
- Joining drone or other sources now. Part E leaves room for it; each
  source is its own spec.
- A search across every job a company has done. The index is per project.
  Many projects at once is the cloud copy's business.
- Turning the person's words into a place inside the bridge. The agent
  does that, as in `whole-cloud-questions.md`.

## Telemetry

Per job, from the envelopes: questions asked, versions per question, and
how many versions came from a parent; time to re-judge after each station;
megabytes in `questions/`; walls split or joined in the link table; how
often `scanner_known` was asked, and how often its answer was out of date.
Read at the fifth job with datasets, or on 2027-03-31.

## Decided

| Date | Question | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-15 | Keep what answers rest on? | Yes, as datasets that grow with the job. | Jim: "add a database that essentially caches datasets from questions". This revises `whole-cloud-questions.md`'s "Nothing new is stored" (2026-11-11). |
| 2026-11-15 | A table schema for them? | No. A fixed envelope for every kind; the payload is the kind's own. The index is built from the envelopes. | Jim: the datasets are dynamic, and a SQL schema won't cover them. Kinds keep arriving, datasets grow with each station, and coordinates move with each registration. |
| 2026-11-15 | What is a place? | An anchor that never moves, the thing it is about, and coordinates worked out per registration. The anchor is the record. | Coordinates are an output of registration. A pick in a scanner's own frame stays true however the registration changes. |

## Open questions

| Question | Who | By |
| --- | --- | --- |
| Does a code change for a kind make all its stored versions out of date, or only a change to its payload format? | Jim | 2026-11-30 |
| Do the overlap thresholds for matching walls (half the length, 10 cm, 5°) hold outdoors, with vegetation tight to the walls? | Jim | after criterion 4 on the admin recording |
| How long are datasets kept after a job is signed off: with the project folder, or only the latest version of each question? | Jim | 2026-11-30 |
| Where does a world frame come from first: control points, or a position from the laptop or the person? | Jim | 2026-11-30 |
