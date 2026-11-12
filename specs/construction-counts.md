# Spec: Counting what was built

Owner: Jim · Date: 2026-11-11 · Status: approved (Jim, 2026-11-11). Part A
built for the next version, counted on the Pi (the laptop doesn't count
yet), with the agent's tools for it (`scanner_count`,
`scanner_confirm_matches`). Criteria 1, 2 and 4 are tests; 3, 9 and 10 are
Jim's. Parts B and C not built.

The lists that buyers hold count things. A door and window schedule lists
every opening and its size; a rough-in inspection counts boxes; a stair
check counts risers. A mission today can measure a wall and one window. It
can't count. This spec adds counts of construction entities a terrestrial
scanner sees, in two kinds:

- **Defensible counts:** each object found, measured and checked against
  the list it is on. First: openings against the schedule.
- **Counts with a confidence:** objects the scan may find but can't always
  be sure of, each shown to the person with a confidence and its reasons,
  and confirmed by them. First: electrical boxes at rough-in.

It stands on [`list-mission.md`](list-mission.md) for lists, evidence
grades and conformance, on [`on-site-answers.md`](on-site-answers.md) for
the gate (no figure until the data carries one), and on the planner for
where to set up next. Counting runs where the full cloud is: the laptop when
it is on the cable ([`laptop-offload.md`](laptop-offload.md), parts B and
D), the Pi otherwise.

## Problem

- **A mission can't count.** It holds targets one by one. A schedule of
  40 openings would be 40 hand-made targets, each bound by hand.
- **A bare number can't be checked.** "38 windows" is wrong somewhere, or
  not; nobody can tell which. A count has to be a list of what was counted,
  each with where it is, its size and a picture.
- **A count overstates what was seen.** A face the scanner never saw has
  openings nobody counted. A count has to say which faces it covers.
- **A model's score isn't a confidence.** A detector's 0.83 says nothing a
  person can check or act on. A confidence has to come with its reasons,
  and the reasons have to say what would change it.

A photo app counts what is in its pictures. What the bridge can do that it
can't: say what it hasn't seen, and where to stand to see it.

## The rule

Nothing is counted on a detector's word alone. A defensible count rests on
a measurement the bridge makes and checks. A candidate counts only once a
person has confirmed it. Both say what the scan didn't see, and use "at
least" when coverage is partial.

## Ship

Three parts. A is openings. B is the confidence pattern, with boxes as its
first class. C is what the person and the agent see.

### A. Openings against the schedule

- **The schedule is a list** in the list-mission format. One line per
  opening mark (`W1`, `D3`): type (window, door, opening), rough opening
  width and height, tolerance, and the wall it is on in the customer's
  words. A spreadsheet with columns `mark`, `type`, `width`, `height`,
  `tolerance`, `wall` reads into the same lines.
- **Finding openings.** On each wall face bound to the list, the bridge
  finds the gaps in the face: areas with no points on the plane, at least
  0.4 m across and 0.4 m high, with the face around them seen. Each gets an
  id, its wall, its position along the wall, its sill height, and its width
  and height with an uncertainty, measured as the window recipe measures an
  opening (`measure::opening`). Each has a picture: the face's heat map with
  the opening outlined.
- **Matching to the schedule.** The agent proposes which found opening is
  which line, by wall and order along it. A person confirms the matches,
  as a list-mission line is bound to a place. A found opening on no line is
  "not on the schedule". A line with nothing found says why: its wall isn't
  seen enough, or no opening was found there.
- **Verdicts per line,** as `on-site-answers.md` gives them: `not_yet`
  while the wall face is under 80% seen; `does_not_fit` when the opening's
  outline can't be closed (part of it hidden, or something stands in it);
  `answer` with its size, and a conformance against the line's tolerance as
  `list-mission.md` defines it.
- **The count** is the list of found openings, by wall, with the faces it
  covers. "11 openings on the faces seen; the east face is 40% seen."

### B. Candidates with a confidence

The first class is electrical boxes at rough-in: small, common, often half
hidden behind framing, and checked on every inspection. Later classes come
one at a time, each with its own calibration (criterion 6).

- **Candidates.** The bridge finds boxes on the bound wall faces and gives
  each an id, its wall, its position and height, its size, and a picture: a
  crop of the station photo that saw it best and a slice of the points.
- **A confidence in words, with reasons.** Each candidate is `likely`,
  `possible` or `can't tell`. The band comes from what the scan shows for
  that object: the points on it, from how many stations, how far and at
  what angle, how much of its surface was seen, and whether something stood
  in front. A detector's score may feed it, never decide it alone. The
  reasons are given with the band ("seen from one station, 9 m away, at a
  steep angle"), so the person knows what would change it.
- **Better confidence by scanning.** A candidate below `likely` becomes
  something the next station can be aimed at, as an unseen stretch of wall
  is today: the planner takes it as its subject and picks a spot that sees
  it squarely and close. After the station the candidate is judged again,
  and the log says how its band moved and why.
- **The person decides.** They confirm, reject or leave each candidate.
  A confirmed candidate is `seen` evidence, as `list-mission.md` defines
  it. The model never confirms one.
- **The count** is the confirmed candidates. Unconfirmed ones are listed
  apart with their bands, and never added to it.

### C. What the person and the agent see

- **The agent** gets two tools: `scanner_count` (the openings of a list,
  or the candidates of a class, on the bound faces) and
  `scanner_candidate` (one candidate: its picture, band and reasons, and a
  proposed station to see it better). Proposing a station goes through
  `scanner_plan_station` and `scanner_propose_station` as today. Every
  sentence the agent relays comes from `say`.
- **The web UI** shows one line at a time, as the field UI does: the next
  candidate to look at, with its picture and three buttons (confirm,
  reject, leave). The whole list is one tap away.
- **The report** opens with the counts: each list's lines with their
  verdicts and conformance, then each class's confirmed objects, the
  rejected ones, and the unconfirmed candidates with their bands and
  reasons. Each object has its picture. The faces the counts cover, and
  those they don't, are named. The report is unstamped and never calls a
  count survey grade.

## Acceptance criteria

1. test: A schedule spreadsheet reads into list lines; a missing size, an
   unknown type or a duplicate mark is refused with its reason.
2. test: On synthetic wall faces (the ray-cast scenes the tests already
   use), three openings are found with sizes within 10 mm of the scene's;
   one with a box in front of it is `does_not_fit`; a face 40% seen is
   `not_yet`; a recess that isn't through the wall isn't an opening.
3. metric: On one real scan whose openings Jim has taped, every opening on
   a face at least 80% seen is found, every size is within 15 mm of the
   tape, and nothing is found that isn't an opening. The building and the
   tape list are kept with the recordings, out of git.
4. test: Matching: the agent's proposed matches are only applied when a
   person confirms them; an unmatched opening is "not on the schedule"; an
   unmatched line says whether its wall was seen.
5. test: A candidate's band and reasons follow from its evidence: the same
   box seen squarely from 2 m is `likely`, from 12 m at a steep angle
   `possible`, half hidden behind a stud `can't tell`, on synthetic scenes.
6. metric: Calibration, before anyone relies on a band. On at least three
   real rough-in scans with every box counted by hand, the share of
   candidates that are real is printed per band. `likely` is right at least
   90% of the time, `possible` between 40% and 90%, and `likely` is right
   more often than `possible`, which is right more often than `can't
   tell`.
7. test: A `possible` candidate becomes the planner's subject; after a
   station that sees it squarely, it is judged again and the log says how
   its band moved and why.
8. test: The report keeps confirmed, rejected and unconfirmed candidates
   apart, counts only the confirmed, names the faces not covered, and says
   "at least" when a bound face is under 80% seen.
9. demo: On the sim with a recorded scan, Jim asks the agent "how many
   windows on the north side?". It answers with the list and pictures, says
   which part of the side isn't seen, and proposes a station for it. Two
   models, three runs each.
10. review: The same two readers as `list-mission.md` criterion 10 read the
    sample report and say which counts they would sign from it, and which
    they would not.

## Kill-if

- Criterion 3 misses an opening on a face seen, or finds one that isn't
  there, after one tuning pass: part A stays measurement only (the window
  recipe), with no finding of openings.
- Criterion 6's bands aren't ordered after one revision: candidates are
  shown without a band, and the person decides each one.
- No real rough-in scan is taken by 2027-03-31: part B closes.

## Non-goals

- Overhead MEP: ducts, pipes, sprinkler heads, lights. The coverage model
  is a plan, and that work belongs to scan-to-BIM.
- Any design model, IFC or BIM. A schedule is a list.
- Quantities of material, percent complete, or progress billing.
- A count accepted by the model. A person confirms every candidate.
- Classes other than openings and boxes in v1. Framing (studs and their
  spacing), stairs (risers and their uniformity), and columns and anchor
  bolts against a grid come next, each as its own part with its own
  calibration.
- Any public claim that the stack counts construction, before criteria 3,
  6 and 9 pass.

## Telemetry

From the bridge log, per job with counts: openings found, matched, not on
the schedule, and lines not found, by reason; candidates by band at first
look and at sign-off; confirmed and rejected by band (live calibration);
stations set up for a candidate, and the band moves they caused. Read at
the fifth job with counts, or on 2027-03-31.

## Decided

| Date | Question | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-11 | Which objects? | Construction entities a terrestrial scanner sees: openings, framing, stairs, columns and anchors. Not overhead MEP. | Jim: for a terrestrial scanner it will be construction entities. Overhead work is out of the coverage model's reach and belongs to scan-to-BIM. |
| 2026-11-11 | Objects the scan can't always be sure of? | Allowed, with a confidence and its reasons, extracted so the person sees each one, and scan positions proposed to improve it. | Jim. |
| 2026-11-11 | What first? | Openings against the schedule, then boxes at rough-in. | Openings reuse the opening measurement and fit the list format with a pass or fail. Boxes are common, small and often half hidden: the case the confidence pattern is for. |
| 2026-11-11 | Approve? | Approved; part A first. | Jim. |
| 2026-11-11 | How does a schedule fit the list format? | Each mark is a line asking `opening`, with the size it expects in `question.opening` (type, width_mm, height_mm) and its tolerance as `max_mm` either way. | The list format had no place for the figure a line expects, and a schedule's tolerance is one figure either way. |
| 2026-11-11 | When is an opening's size a pass? | When both sides are within the tolerance by more than their uncertainty. Outside it by more than that is `non_conforming`; in between is `neutral`, with both figures. | A call the measurement can't make is the tolerance making no call, which `list-mission.md` prints as neutral. |
| 2026-11-11 | Recess or opening? | A gap backed by a surface less than 35 cm behind the face across 70% of it, with under 5% seen beyond, is a recess and isn't counted; the count lists it with why. | Criterion 2's recess. A window whose glass returns densely would read the same; criterion 3 shows whether that happens. |
| 2026-11-11 | Settle an opening over two stations, as a plumb answer does? | No. Every registration counts the faces again, and a match is found again by position (within 25 cm). | Nothing yet shows an opening's size moving with the registration; criterion 3 can. |
| 2026-11-11 | Where is it counted? | On the Pi, in this version. | Each station's file is read once for every bound face. The laptop takes counts next, as it takes question checks. |
| 2026-11-11 | A confidence as a number? | No: three bands in words, each with its reasons. | A reason says what would change it; a percentage doesn't, and invites more precision than the evidence has. |

## Open questions

| Question | Who | By |
| --- | --- | --- |
| What form do schedules arrive in: a spreadsheet, a PDF, a drawing table? | Jim | 2026-11-30 |
| How are doors and windows told apart: sill height alone, or the schedule's type? | Jim | 2026-11-30 |
| Which real building, and who tapes its openings for criterion 3? | Jim | 2026-11-30 |
| Which class after boxes: framing or stairs? | Jim | after criterion 6 |
