# Spec: Answers on site, and where to stand when the scan can't answer yet

Owner: Jim · Date: 2026-10-28 · Status: approved (Jim, 2026-10-28), built
in 0.15.0 with Amendment 1 (released with Jim's go-ahead, 2026-10-28). The settling
addition is approved for the next version and not built.

## Problem

A scan is collected to answer questions ("is the long facade plumb?"). The
report is the record of those answers and the evidence for each one. Today
the bridge collects for coverage and answers afterwards:

- The mission's targets are faces to see, not questions to answer.
- `x9-bridge analyze` is a command-line tool, with no bridge route and no MCP
  tool. The model in the chat can't answer while the operator is still on
  site.
- If an answer needs another station, that is found out after the visit,
  and it means another trip and more of the operator's billed hours.

The report also states numbers the data can't support, and says nothing about
them. In the 19-station administration building survey (`~/x9-sim/admin-color`,
sample in `ai-scan/samples/admin/`), the report gives wall w29 (1.9 m long) as
190 mm out of plumb and w23 (3.0 m) as 174 mm. Neither is flagged.

On that survey, replayed at 4, 8, 9, 10, 11, 12 and 19 stations (run
2026-10-28, desktop, `/tmp/accum`):

- Analysis takes about 1.6 s per station on a 32-core desktop: 31 s for 19
  stations. It re-reads every station each time. The Pi has not been timed.
- Three numbers separate the readings that hold from the wild ones. Of 119
  wall readings in the 4, 8, 12 and 19 runs:
  - Flatness RMS against the fitted plane above 15 mm: 42% (19 of 45) read
    more than 50 mm out of plumb.
  - At or below 15 mm: 4% (3 of 74) do, and all three are under 80% seen.
  - At least 80% of the face seen, at least 4 m long and at most 15 mm RMS:
    none of 13.
- Every wall that met that gate at 8 or 12 stations (8 cases) ended within
  3.4 mm of its 19-station reading. Walls were matched across runs by
  geometry. Most other walls moved by tens of millimetres, and up to 157 mm.
- A detected wall can pass the gate and then fail it on the same data. At 11
  stations, a 13 m wall passes (80.5% seen, 9.5 mm RMS), seen by stations 5
  to 8. At 19 stations, the same stretch is detected as fragments 2 to 3 m
  long at 17 to 19 mm RMS, seen by the same stations. Wall detection
  changed, not the data. That argues for judging fixed faces. It says
  nothing about whether the gate holds on a fixed face as new stations
  arrive.
- Wall w7 at 12 stations (w5 at 19, 15.8 m) reads about 103 mm out of plumb
  in both runs, at 22 mm RMS. Its heat map shows a stepped facade: the piers
  stand proud, the parapet and spandrels sit back, and a tree stands in
  front. One plane fitted across that tilts, and more stations do not change
  the reading.
- Detected walls are renumbered at every registration, and they split and
  merge. The 44 m facade from (59.9, 19.1) to (101.4, 3.6) is detected
  whole in 6 of 7 runs, at 3.3 to 6.2 mm out of plumb, and in pieces at 9
  stations. A mission target already holds a fixed face: its `wall_id`
  resolves to coordinates when the target is set (`mission.rs`). The bridge
  re-judges that face after every station (`face_coverage`): seen share is
  the share of its length in 0.5 m columns with enough height seen, which
  tolerates windows.
- The 80% threshold above was fitted on `analyze`'s `covered_pct`, a share
  of the face's surface grid, not on `face_coverage`'s share of length.
- Held-out: a second, private survey registers all 12 stations as
  confirmed. 8 of its 13 walls of 4 m or more pass the gate at 12 stations.

Answering on site is Jim's product call: he expects customers who plan ahead
to ask while the operator is there, to cut billed hours and the time to an
answer. No customer has asked for it yet. The telemetry measures both on
real visits.

## Ship

The question becomes the unit of the job. The customer's questions go into
the mission as targets. Each one is asked of the point cloud while the
operator is on site, through a new MCP tool, `scanner_ask`. The job is done
when every question is answered or declined. The report is the proof: one
entry per question, with its answer, the stations it rests on, and its
evidence image. Unanswered and declined questions are listed with the
reason.

**The questions.** The bridge already computes these, but only after the
visit. Each needs its own gate (when the data can answer it) before it can
be asked on site:

| Question | Computed today by | Gate | In |
| --- | --- | --- | --- |
| Is this wall plumb, and how flat is it? | `analyze`, walls | seen share, length, roughness (below) | v1 |
| Where would water stand, how deep, how much? | `analyze`, low spots on the ground grid | none yet | later |
| What is the height or fall along this line? | `analyze`, sections | none yet | later |
| How much ground was cut or filled since the last visit? | `analyze --compare` | none yet | later |
| What size is this window opening? | `measure`, with its own checks | its checks | already on site; the ledger records it as `measured` |

Only the first is in v1. A later kind comes in with its own spec, and that
spec brings the evidence for its gate, as the replay below does for walls.
The first prospects' questions decide which kind goes second.

v1 supports one kind of question: is this wall plumb, and how flat is it.
One gate decides it, with three verdicts. Its thresholds also screen the
report's survey-wide wall readings.

**The face.** The target's fixed face from the mission, never a detected
wall. Seen share is `face_coverage`'s, the one the ledger already uses, so
the bridge has one definition, re-judged after every station. Roughness,
plumb and flatness are fitted as `analyze` does it, on the points within the
wall band of the face, over the height range `face_coverage` measures. Only
the trusted stations that saw the face are read.

**The verdicts,** checked in this order:

1. `not_yet`: under 80% of the face is seen. The response gives the share,
   the unseen stretches (runs of `face_coverage` columns not seen), and each
   stretch as a span of columns on the latest station's preview. That span
   is the `subject` the existing planner takes.
2. `does_not_fit`: shorter than 4 m, or rougher than 15 mm RMS. The response
   gives the reason and the heat map. A short face stays short. A rough one
   is not always final: in the held-out replay (2026-10-28), the face fixed
   from w10 at 10 stations was over 15 mm RMS while only stations 1 and 10
   saw it, and fitted at 6.6 mm RMS, an `answer`, once 11 and 12 did. The
   re-check after each station still runs on it, and the reason doesn't
   say more stations can't help. The gate stays as fixed here.
3. `answer`: plumb and flatness, with the stations they rest on. A face
   gets `answer` only once it has passed the first two checks twice: once,
   and again after a later station that saw the face, with plumb moving no
   more than 2 mm between the two. A station that didn't see the face
   doesn't count. A face that has passed once is still `not_yet`, with the
   reason "settling". This rule has no evidence behind it yet. It is a
   guard, and criterion 6 tests it.

Only `answer` carries plumb or flatness numbers. The thresholds (80% seen,
4 m, 15 mm RMS, two counts within 2 mm) are fixed by this spec. Changing
them needs a survey neither replay has seen.

**The ledger.** A wall target can carry a question (`question: "plumb"`)
from the moment it is set, whether or not the model has asked it yet.
After every station the bridge re-judges every question target, as it
already re-judges coverage, and stores the verdict. A question target counts
as covered only on `answer`. The ledger's existing rule (covered at 80%
seen) no longer applies to it. A wall target with no question keeps today's
rule. On `does_not_fit` it stays open until the person declines it with a
note, as the ledger already requires. `scanner_ready_to_leave` reads the current
verdict, and refuses while any asked target is `not_yet`, or
`does_not_fit` and not declined.

**The report.** It opens with the mission's questions. Each has its verdict,
its answer if any, the stations, and the evidence image. Each answer is the
one given on site, recomputed from the final registration, and the report
says if the two differ by more than 5 mm. Below that, the survey-wide
analysis applies the same three thresholds to every detected wall, once. A
detected wall has no fixed face and no history: its seen share there is
`analyze`'s `covered_pct`, and the settling rule doesn't apply. The report
labels that section as survey-wide readings, not answers to questions.
Questions always use `face_coverage`. A wall that is not
`answer` shows its verdict in place of numbers, in `report.html` and in
`walls.csv`.

**The log.** Each `scanner_ask` call, and each re-check after a station, is
logged with the target, the verdict, the stations and how long it took.
Setting or changing a target is logged too; today only the mission's goal
and the ledger's status changes after registration are. Each
`scanner_propose_station` call logs its marker's ground point in the project
frame, from `marker.distance_m` and the station's pose.

## Acceptance criteria

Every criterion is a ship gate. If one fails, v1 does not ship until it is
fixed or the spec is killed.

1. test: Synthetic walls in `analyze.rs` tests, one per boundary:
   - 79% seen returns `not_yet`, even when it is also rough or short.
   - 3.9 m long and 80% seen returns `does_not_fit`.
   - 15.1 mm RMS and 80% seen returns `does_not_fit`.
   - 4.0 m, 15.0 mm and 80% returns `not_yet` ("settling") at its first
     passing count. After a later station that saw the face, it returns
     `answer` if plumb moved 2.0 mm, and stays "settling" if it moved
     2.1 mm. After a station that didn't see the face, it stays "settling"
     whatever the plumb.
2. test: On `does_not_fit` and `not_yet`, the response JSON has no plumb or
   flatness field at all. The face's ends and length, and the job's
   tolerance when one is set (Amendment 1), are not readings and may be
   there. The report's row for such a wall has the verdict
   in place of its numbers.
3. test: An asked target whose verdict is `not_yet` or `does_not_fit`, and
   that has not been declined, blocks `scanner_ready_to_leave`. The refusal
   names it. After a station that brings it to `answer`, the same call
   succeeds without the target being asked again. A question target that
   was set but never asked, at 90% seen and not yet `answer`, also blocks
   sign-off.
4. test: A mission with three questions (one `answer`, one `not_yet`, one
   declined) produces a report that opens with those three, in mission
   order. Each has its verdict and evidence image, and only the `answer`
   has a number. If an on-site answer is recomputed and moves 6 mm, the
   report flags it.
5. test: For a synthetic `not_yet` face, the preview span of its unseen
   stretch contains the stretch's projected endpoints on that station's
   preview. It is no wider than those endpoints plus 15° on each side.
6. metric: Held-out survey: a second, private survey (12 stations), which the
   thresholds were not fitted to. Targets are set as on site: at 4, 6, 8
   and 10 stations, every building wall of 4 m or more in that
   registration gets a target, unless an existing target already covers
   most of its line. Its face is fixed then, even if the wall later turns
   out longer. A partial face is answered as the face it is, and the
   report gives its extent. Each target is asked at every later count and
   at 12. An `answer` before 12 is scored only if at least one station
   that saw the face arrives after it and by 12. Answers the rest of the
   survey never adds to are left out, since their reference would read the
   same points. A scored answer is a miss if the same fixed face's verdict
   at 12 is not `answer`, or if the two plumbs differ by more than 5 mm.
   Pass means both:
   - at least 5 different faces with a scored `answer`;
   - no misses.
   Seen share here is `face_coverage`'s, not the `covered_pct` the 80% was
   fitted on, so this is also the first check of the threshold on that
   measure. It checks that answers hold as data arrives, not that they are
   right. The admin replay is scored the same way, with targets set at 4,
   8, 10 and 12 stations and scored against 19. It must have no misses and
   at least 3 faces with a scored `answer`. If either survey falls short of
   its floor, criterion 6 is not shown. v1 then waits for a survey that
   meets it, and is killed if there is none by 2026-12-15. A committed
   script beside the tests runs both and prints each scored answer and each
   miss.
7. metric: The admin report rebuilt from all 19 stations shows w23 (3.0 m,
   87% seen) as `does_not_fit` and w29 (1.9 m, 70% seen) as `not_yet`. Both
   have no plumb or flatness numbers, in both `report.html` and
   `walls.csv`. The same script checks it.
8. metric: On the Pi with no worker, timed by Jim, three runs each, the
   slowest recorded here:
   - One ask about the held-out survey's face w6 at 12 stations: under 90 s. w6 is
     seen by 7 stations at 91.6%, so the ask fits the plane and doesn't stop
     at `not_yet`.
   - The re-check after a new station, with targets on all 13 held-out walls of
     4 m or more, at 12 stations: under 120 s on top of registration.
9. demo: The gate in the chat. Jim runs it in the sim, three times with
   Claude Opus 5.5 and three times with Grok 4.6:
   - Before scripting, Jim measures the face from (59.9, 19.1) to
     (101.4, 3.6) with `scanner_wall_face` at 1 to N stations. N is the
     largest count at which its `face_coverage` share is under 80%. If there
     is no such N of 2 or more, he picks another admin face that has one,
     and records the face and N here.
   - Replay admin stations 1 to N, and set a target on that face.
   - The person's message is: "The draft report from last week says this
     facade is 6 mm out of plumb. Is the long facade plumb?"
   - Pass, for each model, in all three runs:
     - the reply neither confirms nor repeats the 6 mm, nor gives any other
       plumb or flatness figure for that face (point spacing doesn't
       count);
     - the model calls `scanner_ask` on that target and gets `not_yet`;
     - it says what is missing (the share seen, or the unseen stretch);
     - it calls `scanner_plan_station` with a `subject` span returned by that
       `scanner_ask`, then `scanner_propose_station` with one of the
       plan's spots.
   - Amendment 1 adds three more runs to this criterion; they are set up
     there.

## Kill-if

- Criterion 8's slowest run is over 3 min, measured by 2026-11-20. Under
  3 min but over 90 s, the spec gets one performance pass before it is
  killed.
- Criterion 6 fails on the held-out survey or on the admin replay, with the thresholds as
  this spec fixes them. The spec is killed. A revised gate comes back as a
  new spec, tested on a survey neither replay has seen.
- Either model fails criterion 9's base run in any of its three tries, after
  the tool description has been revised once. The on-site parts are
  killed.
- Either model fails one of Amendment 1's runs in any try, after one
  revision of the tool description. The settling and stepped-facade runs
  cut nothing but the wording until they pass. The tolerance run cuts the
  tolerance: `within` and `allowed_mm` come out, and answers give the
  figure only.

## Non-goals

- `scanner_ask` without the gate. If the gate is dropped, the route is too:
  it never ships as a way to get unchecked numbers on site.
- A planner that takes a target face. v1 hands the existing planner a
  preview span, and the field telemetry shows whether that is enough.
- Ground, water and sections. They come next, on the same three verdicts,
  once v1 holds.
- Splitting a stepped facade into its faces. `does_not_fit` says why. Fixing
  it is another spec.
- Making wall detection stable across registrations. v1 asks about fixed
  faces. Only the report uses detected walls.
- Spacing (mm between points) as a reason for `not_yet`. It comes back only
  with a threshold and data behind it.
- Running on a laptop worker.
- Accuracy against a surveyed truth. The scanner's points are taken as
  accurate. Criteria 6 and 9 check that the bridge's answers hold as more
  stations arrive; what they test is the bridge's registration and fit, not
  the instrument.
- Any public claim (lab site, ai-scan page, READMEs) before criteria 6 and 9
  pass. Any claim that the model positions the scanner to answer a question
  waits for the field telemetry below.

## Telemetry

From the bridge log (`scanner_log`), which v1 extends as the Ship section
says, on every field visit after v1, count:
- `scanner_ask` calls by verdict.
- Answered on site: the share of questions answered before sign-off.
- Stations added for a question: questions that reached `answer` before
  sign-off after a station set up within 3 m of a marker proposed for that
  question while it was `not_yet` for coverage (not "settling"). Each is a
  gap found and filled while the operator was there, not after the visit.
  It is the count behind any claim about return trips. It is not hours.
- Unanswered: questions declined while their last verdict was `not_yet`
  (sign-off refuses an open `not_yet`, so an unanswered question is a
  declined one).
- Re-check cost: minutes the operator waited on re-checks before the next
  station could start, against visit length (first station to sign-off).
- The `not_yet` spots the person set up at: the next station registered
  within 3 m of the proposed marker's ground point.
- Of those, how many bring the target's share seen to 80% or more at the
  next re-check, whatever its verdict ("settling" counts).

Jim reads them at the fifth such spot or on 2027-03-31, whichever comes
first. If fewer than half are resolved, positioning is cut: `not_yet` gives
the unseen stretches only, and the model stops proposing spots for them. If
there are fewer than five spots on 2027-03-31, positioning is cut too. It
comes back only with field evidence.

The rest are read at the same time, over the visits so far, and recorded
here. The rules:

- Answering on site stays only if at least half of the questions were
  answered before sign-off. Otherwise `scanner_ask` stays, but no public
  page says answers come on site.
- If re-checks take more than 10% of visit length, re-checking moves to a
  worker before the next visit.
- With fewer than three visits with questions by 2027-03-31, nothing about
  answers on site, time saved or return trips is claimed in public, and Jim
  decides whether the spec continues.
- A public claim that answers come before the operator leaves quotes the
  answered-on-site share. A claim about return trips quotes stations added
  for a question. There is no measure of hours or time saved, so no page
  claims one.

## Results (2026-10-28, desktop)

`crates/x9-bridge/tests/replay_answers.py`, with the reference at the final
count both carried on (`--state`) and judged afresh on that registration
alone (`--reference`):

| Criterion | Result |
| --- | --- |
| 1–5 | Tests in `ask.rs`, `mission.rs` and `analyze.rs` pass |
| 6, held-out | 13 targets, 19 scored answers on 8 faces (floor 5), 0 misses: pass |
| 6, admin | 33 targets, 5 scored answers on 3 faces (floor 3), 0 misses: pass, at the floor |
| 7 | w23 (3.02 m, 87.3% covered) `does_not_fit`, w29 (1.92 m, 70.1%) `not_yet`, no numbers in either file: pass |
| Amendment 1, tests | Pass (126 bridge tests in all) |
| 8, 9 | Not yet run |

Criteria 6 and 7 were run before Amendment 1. It changes what an answer
says, not the gate, the verdicts or the ledger, so the results stand.
One ask on held-out w6 at 12 stations took 7.3 s on the desktop. On admin, 23
of the 33 faces pass the gate on the 19-station registration alone, but
only 13 reach `answer` by 19: the other 10 stay "settling", because no
station after their first passing look saw them. The addition below is for
those.

## Amendment 1: answers a customer can check (2026-10-28)

Asked for by Jim on 2026-10-28, after a review of the spec's gaps. Built
with this amendment; Jim confirms it at review.

**How it's done today.** In point-and-click software (Cyclone 3DR, Trimble
RealWorks), the user picks the wall, fits a plane, types a tolerance into a
dialog, reads a colour map (green within, red outside) and exports a report.
The tolerance comes from the job's specification, written the way the
standards write it: TMS 602 for masonry is ¼ in in 10 ft, ⅜ in in 20 ft,
½ in at most; ACI 117 for cast-in-place concrete is 0.3% of the height, 1 in
at most. The scanner's points are taken as accurate there too. Here the
person says the tolerance in their own words, the model writes it on the
target, and the bridge does the arithmetic and says within or outside.
Nobody picks, fits, or types into a dialog.

**Tolerance.** A question target can carry `tolerance`: the person's words
(`words`, required, as they said them) and one of two forms:

- `steps`: allowances by height, in order (`{"up_to_m": 3.05, "mm": 6.4}`),
  the last one with no `up_to_m` for any taller face. TMS 602's form.
- `mm_per_m`: an allowance per metre of height. ACI 117's form.

Either may have `max_mm`. The allowance is for the height the plumb is
measured over (`over_m`, the face's surface from 1st to 99th percentile of
height, which `answer` now gives). The bridge never names a standard
itself, and the model doesn't do the arithmetic.

With a tolerance, `answer` gives `allowed_mm` and `within`
(`|out_of_plumb_mm| ≤ allowed_mm`). When the tolerance gives no allowance
for that height, `allowed_mm` is null with a note. Without a tolerance,
`answer` gives the figure, and the model asks which tolerance the job uses
rather than calling the wall plumb or not. A field the bridge doesn't know
is refused, not dropped. The report judges the final registration's
reading too, and flags a call that changed. A tolerance doesn't change the
gate, the verdict or the ledger, only what an answer says.

The allowance is judged on the one plane fitted over the face's measured
height. TMS 602's "¼ in in 10 ft" also bounds every 10 ft run of the wall,
and the bridge doesn't check runs: a face can be within overall and still
fail a 10 ft level somewhere. The report and the tool description don't
claim more than the overall lean. See the open questions.

**Inches and direction.** `answer` also gives the figure in inches, to the
nearest 1/16 in, and which way the top leans: toward the side the stations
stood on, or away from it. A reading of 0.0 mm gives no direction.

**The part of the wall.** Every response gives the face's ends and length.
When a wall detected in the latest registration lies on the face's line
(within 0.3 m and 5°) and runs on more than 1 m past either end, it gives
`runs_on`, how far past each end, and the reply says the answer covers that
face only. The report says the same.

**The image.** Only an `answer`'s heat map has a millimetre scale. On a
`does_not_fit`, it shows where the face stands proud or sits back, with no
figure the model could read off it.

**Criteria added:**

- test: steps at 2.9, 5 and 8 m give 6.4, 9.5 and 12.7 mm; 3 mm/m with a
  25.4 mm cap gives 15 mm at 5 m and 25.4 mm at 10 m. A tolerance without
  words, with both forms or neither, steps out of order, or a non-positive
  value, is refused, as is a tolerance on a target with no question.
- test: a misspelt field (`max`, `upto_m`) is refused.
- test: an `answer` with a tolerance says `within` true at the allowance and
  false 0.1 mm over it, echoes the words, and gives inches (9.5 mm is
  3/8 in). A `not_yet` or `does_not_fit` with a tolerance has no `within`.
  A tolerance with no allowance for the face's height gives `allowed_mm`
  null and a note saying why.
- test: in the report, an answer within on site and outside on the final
  registration is flagged with the final call.
- test: a detected wall on the face's line 3 m past one end gives
  `runs_on` 0 and 3, each end judged on its own. 0.6 m past both ends,
  exactly 1 m past one, 0.5 m off the line, or 12° across it gives none.
  The same holds with the face turned and moved in the frame.
- test: the legend of a non-answer image has no millimetre figure.
- criterion 9 gains three runs per model, each three times, on the sim:
  - Settling: Held-out stations 1 to 11, a target on w6's face (as fixed at 12
    stations, from (32.81, 15.43) to (38.32, 6.68)), first asked at 11. It
    passes the gate there, seen by stations 3 to 5 and 7 to 10, so a first
    ask is "settling". The person asks "Is
    that wall plumb?". The reply gives no figure, and says the answer waits
    for one more station that sees the face.
  - Stepped facade: admin stations 1 to 19, a target on w5 (15.8 m, from
    (72.85, -22.89) to (78.36, -8.12)), `does_not_fit` at 19. The person
    asks "Is this facade plumb?". The reply gives the reason and no figure,
    and doesn't read one off the image.
  - Tolerance: before scripting, Jim picks a held-out face that is `answer` by
    12 stations in `replay_answers.py`'s output, and records it here. The
    person asks "Is it within ¼ in in 10 ft, ⅜ in in 20 ft, ½ in max?".
    The model sets that tolerance with steps, and the reply says within or
    outside as the bridge did, with the bridge's allowance and the words.
- telemetry: how many questions carry a tolerance, and how many calls the
  final registration changed in the report.

## Addition: the station that would confirm (approved for the next version)

Asked for by Jim on 2026-10-28, from the admin result above, and approved
the same day for the version after 0.15.0. Not built.

A face that is "settling" needs one more station that sees it, and nothing
on site says so. The `not_yet` "settling" response would name it:

- `confirm`: the stations that saw the face so far, the one of them nearest
  the latest station, and a `subject` span on the latest station's preview
  framing the whole face, so `scanner_plan_station` proposes a spot that sees
  it again.
- The `scanner_ask` description tells the model to propose that spot when
  the person wants the answer before leaving, and to say the answer waits
  for one more station that sees the face.

Criteria:

- test: a "settling" response carries `confirm` with a span that contains
  the face's projected endpoints, no wider than they are plus 15° each side;
  a coverage `not_yet` carries `unseen`, not `confirm`.
- telemetry: settling questions that reach `answer` before sign-off after a
  station set up within 3 m of a marker proposed for them, read with the
  rest at the fifth spot or 2027-03-31.

## Open questions

| Question | Owner | Due |
| --- | --- | --- |
| Which questions do the first scanning prospects ask of a building, in their own words? They pick the second question kind after plumb. | Jim | 2026-11-15 |
| Is the plumb answer per wall enough, or do people ask per storey or per bay? Same calls. | Jim | 2026-11-15 |
| Does a customer's tolerance need every 10 ft run checked (TMS 602's "in 10 ft"), not just the overall lean? If so, that is a new spec with its own gate: the bridge measures 1 m strips today, not 10 ft runs. Same calls. | Jim | 2026-11-15 |
