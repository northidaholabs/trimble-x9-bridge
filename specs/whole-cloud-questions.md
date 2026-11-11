# Spec: Questions about the whole cloud

Owner: Jim · Date: 2026-11-11 · Status: draft, not approved

On site, people ask about the place they are standing in, not only about
the walls a mission fixed beforehand. How high is the ceiling here? Is the
hall 36 inches clear? Does the garage floor fall to the drain? Today the
bridge answers one kind of question, whether a fixed wall face is plumb
and flat ([`on-site-answers.md`](on-site-answers.md)). Anything else is
answered after the mission, by hand in a viewer, or not at all.

With [`laptop-offload.md`](laptop-offload.md) part D, every station's
points land on the laptop in the project frame, indexed (COPC). This spec
lets the agent ask about any place in the cloud during the mission. Each
answer is judged, recorded and drawn the same way as a wall answer. Three
kinds come first, each with its own gate: a clear height, a clear width,
and the fall along a line.

## Problem

- **Only fixed faces can be asked about.** A question has to be a wall
  target set before the scanning. A question that comes up on site has
  nowhere to go.
- **The data is there, but the bridge can't reach it.** The laptop holds
  every station's full points, while the bridge reads only wall bands and
  5 cm clouds. A height or a width needs full-resolution points in a small
  region, from whichever stations saw it.
- **A figure from two stations carries the registration.** A width whose
  two sides were seen from different stations is only as good as the tie
  between them. Today nothing says so. The audit knows how well each
  station sits against its neighbours (`surface_mm`), but no answer uses
  it.
- **"Where" is the hard part of a question in a chat.** The person says
  "here" or "this hallway". The agent sees previews and the minimap, not
  coordinates. A figure measured at the wrong place is worse than none, and
  the person has to be able to see where it was measured.

## The rule

The rules of `on-site-answers.md` carry over. A kind of question gets a
figure only from its own gate. Every answer names the stations it rests
on, its uncertainty, and a picture showing where it was measured. When the
data can't answer, the verdict says what is missing and where to stand to
get it. Nothing is claimed in public until it has been shown (criteria 4
and 8).

## Ship

Four parts. Part A is how the agent says where. Part B is the three kinds
and their gates. Part C is where the points are read from. Part D is the
record.

### A. Saying where

A question names its place in one of three ways. The bridge turns each one
into a point or line in the project frame, and draws it back on a preview
so the person can check it.

- **A pick on a preview:** `{scan_id, x, y}`, a pixel on a station's
  preview. The preview is aligned with the scan's grid (as `aim` uses it),
  so the pixel names a grid cell. The surface point is the scan's point in
  that cell, moved into the project frame by the station's pose. A pick
  with no point under it (sky, a window, out of range) is refused, with the
  reason. This is how the agent marks a spot today with
  `scanner_propose_station`.
- **A named thing:** a wall (`w3`), a mission target, or a station ("at
  station 4": the floor under the tripod).
- **Coordinates:** `[x, y]` or a line `from`/`to` in the project frame, in
  metres, for the agent working from the minimap.

The answer's picture shows the place on the preview of the station that
saw it best: the measured segment, its end points and the figure, drawn
with `view3d` as the web UI draws tripods and faces today. If the place is
wrong, the person can see that before trusting the number.

### B. The kinds and their gates

Each kind gives the same three verdicts as a wall question (`answer`,
`not_yet` with what is missing, `does_not_fit` with why), checked in that
order.

| Kind | Asked as | Measured | Answer |
| --- | --- | --- | --- |
| `height` | a place | The floor and the ceiling (or the underside of whatever is overhead) in a 10 cm disc around the place: a plane fitted to each, and the vertical distance between them | The clear height, and each surface's roughness |
| `width` | a pick on one side, or two named walls, at a height (default 1 m above the floor) | Across, square to the first surface: the first surface opposite, in a 10 cm square at that height | The clear width, and the angle between the two sides |
| `fall` | a line on the floor or ground | The surface along the line, in a 10 cm band, as a profile | The fall from end to end, the steepest stretch (as % and inches per foot), and whether it runs the way the line points |

Figures are given in millimetres and inches, as wall answers are.

**The gate for every kind:**

1. **`not_yet`** when a surface the figure needs has under 50 points in
   its patch, from trusted stations only. The response says which surface
   is missing. It also gives the planner a `subject`: the patch as a span
   on the preview of the nearest station that has a clear line to it,
   which `view3d` decides.
2. **`does_not_fit`** when a surface is rougher than 15 mm RMS (an
   unfinished ceiling, clutter on the floor), or when the uncertainty
   (below) is more than the kind's limit: 5 mm for `height` and `width`,
   and 0.2% of the line's length for `fall`.
3. **`answer`** otherwise.

**The uncertainty** is the fits' own (each plane's residual over the
square root of its points), plus, when the surfaces come from different
stations, the tie between them. The tie is the larger `surface_mm` p90 of
the stations involved, from the registration's audit. A figure read from
one station's points alone has no registration term. So when two-station
evidence is too loose, the answer says that one station seeing both sides
would answer it. The planner then takes both patches together as its
subject.

**Settling.** A wall answer settles over two stations (`on-site-answers.md`)
because a new station can move the registration. Here that applies only
when the figure rests on more than one station: it settles the same way,
and can't move more than 2 mm between the two passes. A figure from one
station's points answers on the first pass. This has no evidence behind it
yet; criterion 2 tests it.

The thresholds above are fixed by this spec, as the wall gate's are.
Changing them needs a mission neither replay has seen.

### C. Where the points come from

- **On the laptop**, from the project folder. With a laptop connected, the
  bridge offers a save after each registration as well as at sign-off.
  Part D's save rewrites only the stations that changed, so the folder
  stays current for the whole mission. A question becomes a job for the
  worker, as question checks are (`laptop-offload.md` part B), with the
  same fallbacks: the Pi measures it itself if no worker takes it within
  10 s or the worker doesn't answer within 60 s. The worker reads only the
  COPC nodes that hold the region, at the finest level, from the stations
  whose bounds contain it. A station not in the folder yet is read from its
  scan file in the worker's cache. The fits, verdicts and pictures stay on
  the bridge. Saves are claimed after registrations and checks, so keeping
  the folder current never delays a question.
- **On the Pi**, when no laptop is connected: the same fits, from the scan
  files of the stations `view3d` says saw the place, decoded one at a time.
  This is slower. The response says where it was measured and how long it
  took, so the agent can suggest connecting the laptop. The question isn't
  refused.
- **The same answer either way.** The laptop and the Pi give the same
  verdict and figure to 0.1 mm, as `laptop-offload.md` criterion 5 requires
  for checks.

### D. The record

- **The ledger.** A question asked in the chat on a job with a mission is
  added to the ledger as a target, `kind: question`, with its place and
  verdict, marked "asked in the chat". It is re-judged after every later
  station, as wall questions are. It counts towards being ready to leave
  only if the person makes it a target. Otherwise it is listed and doesn't
  hold the job open. On a job with no mission, it goes to the field log.
- **The log** has each question, its place, verdict, figure, stations,
  where it was measured and how long it took.
- **The report** lists the questions asked on site after the mission's own,
  each with its picture. Each answer is recomputed from the final
  registration, and the report says when the two differ by more than its
  uncertainty.
- **The agent** gets `scanner_ask_at(project_id, kind, place)`, which
  returns the verdict, the figure, the uncertainty, the stations, the
  picture, and a `subject` for the planner when the verdict is `not_yet`.

## Acceptance criteria

1. test: On synthetic rooms (the ray-cast scenes the tests already use),
   with known heights, widths and slopes: each kind answers within 2 mm, or
   0.05% for a fall. A place no station saw is `not_yet`, with a subject. A
   rough ceiling is `does_not_fit`.
2. test: A width whose two sides come from two stations tied at 6 mm is
   `does_not_fit`, saying one station seeing both would answer it. The same
   width seen from one station is an `answer` on the first pass. A
   two-station figure settles over two passes, and doesn't answer if it
   moved more than 2 mm between them.
3. test: A pick on a preview gives the surface point under it, in the
   project frame, within one grid cell. A pick on the sky is refused with
   its reason. The answer's picture draws the measured segment where the
   pick was.
4. metric: In the admin building (pictures may be public), Jim measures 10
   heights and 10 widths with a laser distance meter, and 5 falls with a
   level and rule. For each kind, every `answer` is within its stated
   uncertainty plus 2 mm (or 0.1% for a fall) of Jim's figure, and at least
   7 of the 10 (4 of the 5 falls) are answers, not `not_yet` or
   `does_not_fit`. Jim's figures are kept with the recordings, out of git.
5. metric: Jim's timings, three runs, slowest given, on the held-out
   survey after station 12: one question of each kind in under 10 s on the
   laptop, from the call to the answer, and in under 90 s on the Pi alone.
6. test: The laptop and the Pi give the same verdict and figure to 0.1 mm
   for every question in criterion 1.
7. test: With a laptop connected, each station's cloud is in the project
   folder within 60 s of its registration finishing. A station whose pose
   moved is written again, and one that didn't move is kept.
8. demo: On the sim with the admin recording, Jim asks the agent in the
   chat "how high is the ceiling where station 3 is?", "is the corridor 36
   inches clear?" and "does the floor by the back door fall to the drain?".
   Each reply gives the figure or the verdict, the stations, and the
   picture, with the place right. Two models, three runs each.
9. test: A question asked in the chat is in the ledger as "asked in the
   chat", re-judged after the next station, and in the report with its
   picture. It doesn't hold `scanner_ready_to_leave` unless the person made
   it a target.

## Kill-if

- Criterion 4 fails for a kind after one fix: that kind is withdrawn from
  the chat. Its figures stay in the report as readings, not answers.
- Criterion 3's picks land on the wrong surface in the criterion 8 demo
  in more than one run in six: picks are dropped, and the agent names
  places by wall, target, station or coordinates only.
- Criterion 5 is over 90 s on the Pi alone: without a laptop, the
  questions wait for one and say so. The Pi doesn't fit them.

## Non-goals

- Turning the person's words into a place inside the bridge. The agent does
  that, and the bridge takes a structured place.
- Volumes, cut and fill, and where water would stand. They are later kinds
  in `on-site-answers.md`, each with its own spec.
- Floor flatness numbers (FF/FL) and any comparison with a design model.
- Questions about the cloud copy, from off site. The same kinds will run
  there once the cloud spec exists, with the same gates.
- One merged cloud. The stations stay separate files, read together.
- A figure without a picture of where it was measured.

## Telemetry

From the field log, per mission with questions asked in the chat: questions
by kind and verdict; where they were measured (laptop or Pi) and how long
they took; how often a `not_yet` was followed by a station that turned it
into an answer; and how often the person asked again about the same thing
at a different place, as a sign the first pick was wrong. Jim reads them at
the fifth mission with questions asked in the chat, or on 2027-03-31.

## Decided

| Date | Question | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-11 | Why now? | The whole cloud is on the laptop with part D, and asking about it is faster and more open than a fixed list of targets. | Jim. |
| 2026-11-11 | Which kinds first? | Clear height, clear width, and fall along a line. | They need only small regions and plane fits the bridge already makes. They match "the height or fall along this line" in `on-site-answers.md`'s later kinds. |
| 2026-11-11 | Settle every answer over two stations? | Only figures that rest on more than one station. | A one-station figure doesn't depend on the registration, which is what settling guards against. |
| 2026-11-11 | Where do the points come from? | The project folder on the laptop, kept current after each registration; the scan files on the Pi otherwise. | It reuses part D's save and its index. Nothing new is stored. |

## Open questions

| Question | Who | By |
| --- | --- | --- |
| Which questions do the first prospects ask on site? They decide the fourth kind. | Jim | 2026-11-30 |
| Does a question asked in the chat join the ledger by itself, or only when the person says so? | Jim | 2026-11-30 |
| Default height for a width: 1 m above the floor, or the person's to give each time? | Jim | 2026-11-30 |
| When does Jim measure the admin building for criterion 4? | Jim | 2026-11-30 |
