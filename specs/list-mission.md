# Spec: A mission from a list

Owner: Jim · Date: 2026-11-09 · Status: draft, not approved

A mission today starts from one sentence. The people who would buy the work
hold lists: an inspection and test plan, a punch list, a closeout list. This
spec lets a mission start from a list, guides the person to document each
line, and records who signed what. It stands on
[`on-site-answers.md`](on-site-answers.md) for the verdicts and the gate,
[`any-agent.md`](any-agent.md) for the bridge's words, and
[`control-plane.md`](control-plane.md) for who decided under what.

The formats, a sample and a checker are in [`list-mission/`](list-mission/).

## Problem

- **A mission holds a sentence, not requirements.** A `Target` has a name, a
  kind, a face, a question and a tolerance. It has no place for the
  requirement's own words, the document it comes from, how it is to be
  checked, or what is kept as proof. A list has all four on every row.
- **One kind of line can be answered.** A wall face can be asked whether it
  is plumb and how flat it is. A list also has lines a picture settles
  ("doors hung and latching") and lines only a person can state ("anchors
  torqued to the manufacturer's figure"). The mission can only leave those
  out, or decline them.
- **One signature, for the capture.** The mission is signed off once, by the
  operator, and it means nothing is open. A list is signed per line, per
  party: the trade says done, the checker accepts, and some lines are hold
  points nobody may pass alone. The record has nowhere to put that.
- **An answer and a pass are the same field.** On the demonstration
  building the long facade answered: 4.1 mm out of plumb over 14.6 m. The
  tolerance, a quarter inch in ten feet, gives no allowance at that height,
  so there is an answer and no pass or fail. Today that lives in the words
  of `said` (`allowed_mm` is null). A person signing a line needs it as a
  state.
- **There is no format to adopt.** A search on 2026-11-09 found no open,
  machine-readable standard for a punch list or a site checklist: vendor
  templates, and one contract form. The pieces that exist each cover part.
  The inspection and test plan (ITP) has the meaning of every column and no
  schema. FHIR Questionnaire has the split between a list and its answers.
  BCF has issues with a place. Field inspection software has four response
  states. [`list-mission/README.md`](list-mission/README.md) has the
  comparison and the sources.

What a checklist app does with a list is accept any photo and let the box be
ticked. What the bridge can do that such an app cannot is refuse to close a
line until the evidence carries it, and say where to stand when it does not.

## Ship

Six parts. A to C make a list something a mission can run. D to F are the
signatures and what leaves.

### A. The list and the record

Two documents, as [`list-mission/list.schema.json`](list-mission/list.schema.json)
and [`list-mission/record.schema.json`](list-mission/record.schema.json)
define them:

- A **list** has parties and lines. A line is one row of an ITP: its number
  (`line_id`), `group`, `activity`, `requirement`, `reference`, `method`
  (`dimensional`, `visual`, `test`, `review`), the grade of `evidence` it
  can carry (`measured`, `seen`, `attested`), the `question` the bridge is
  asked, a `tolerance` in the bridge's existing shape, an inspection point
  per party (`H` hold, `W` witness, `R` review, `S` surveillance), and the
  `record` kept as proof. It holds no results.
- A **record** has one entry per line, by `line_id`, and names the list's
  revision: `status` (`open`, `not_yet`, `does_not_fit`, `answered`,
  `declined`), and with `answered` a `conformance` (`conforming`,
  `non_conforming`, `neutral`, `not_applicable`), the evidence, the place,
  the signatures and the agent.

`PUT /v1/projects/{id}/list` takes a list as JSON, or as a spreadsheet (CSV)
with the ITP's columns: `line`, `group`, `activity`, `requirement`,
`reference`, `method`, `tolerance`, one column per party, `record`. It makes
the project's mission: one target per line, `target_id` the `line_id`. A
mission from a sentence works as it does today; it is a list nobody wrote
down. `GET /v1/projects/{id}/record` gives the record, from the mission's
state.

A new revision of the list keeps the answer of every line whose
`requirement`, `tolerance` and `question` did not change, reopens the others,
and voids their signatures. The record says which revision it answers.

### B. Sorting, and binding to a place

A spreadsheet row says what must be true. It does not say what the bridge is
to be asked. Before the first scan:

- The agent proposes, per line, the `evidence` grade, the `question`, and
  the arithmetic of the tolerance from its words ("a quarter inch in ten
  feet" as `steps`). It asks the person about a line it cannot sort, or a
  measured line with no tolerance ("plumb to what?"). The person confirms or
  changes each proposal, as a recipe's person step works today. Nothing is
  sorted without a person's yes.
- A line is bound to a place once, by a person, on a station's view: a wall
  for a measured line (the existing `wall_id` to face), a station and a box
  on its preview for a seen line. The record keeps who bound it. A line with
  no place is `open` and says so.

### C. What a line can be answered with, in v1

| Evidence | Question | Answered by |
| --- | --- | --- |
| `measured` | `plumb`, `flatness` | The bridge, as a question target is today: `not_yet`, `does_not_fit` or `answer`, re-checked after every station |
| `seen` | `photo` | A picture from the project, attached by anyone, and marked as showing it by a person. The agent does not judge a picture in v1 |
| `attested` | `statement` | A person's statement, with the document it rests on named. An agent cannot give it |

`conformance` on a measured line comes from the tolerance's call the bridge
already makes: `within` true is `conforming`, `within` false is
`non_conforming`, and no allowance for the case (`allowed_mm` null) is
`neutral` with the bridge's reason. A measured line with no tolerance is
`not_applicable`: it records a figure.

The other measured kinds the schema names (`height`, `opening`, `clearance`,
`low_spots`, `map`) are accepted in a list and stay `open`, with the reason
that the bridge cannot ask them yet. They can be declined, not answered.

### D. Signatures

A party is a named token of role `operator` or `viewer`, tied to a party of
the list. A signature on a line is a party, an action (`done`, `witnessed`,
`released`, `accepted`, `accepted_with_note`, `rejected`), who, when, and
how it reached the bridge (`via`, as the mission's sign-off records it). The
bridge enforces, for every client:

- Only a party with a point on the line signs it. Only a hold is released.
- A call (`accepted`, `accepted_with_note`, `rejected`) needs an answered
  line. A line that is `not_yet`, `does_not_fit`, `declined` or `open`
  cannot be accepted.
- Plain acceptance needs a `conforming` answer, or `not_applicable`. A
  `neutral` or `non_conforming` answer takes a note, or a rejection.
- An `agent` token signs nothing. There is no tool for it and the route
  answers 403. A signature the agent relays from the chat is marked relayed,
  as the mission's sign-off is.
- A later answer that changes a line's `conformance`, or moves its figure by
  more than the 5 mm `on-site-answers.md` scores a miss at, voids the calls
  on that line and flags it in the record and the report.

### E. Two gates

- **Ready to leave** is the existing gate, per line: refused while a
  required line is `open`, `not_yet` or `does_not_fit`, naming the lines.
  The operator signs the capture off once, as now.
- **Accepted** is new and separate: every hold and review party has made a
  call on every line it has a point on. It can come days later and from
  another place. It never blocks the crew leaving.

### F. What the person sees, and what leaves

- The web UI's Now view shows one line: the next to document, with its
  requirement, and where to stand. The whole list is one tap away. Every
  sentence comes from `say`.
- The report opens with the list, in order: each line's requirement, grade,
  status, conformance, evidence and signatures. It carries names, companies
  and roles. It carries no licence number and is not a stamped document.
- A rejected line can be exported as a BCF topic (BCF API 3.0 JSON):
  `title` from the line number, activity and requirement, `description` the
  bridge's sentence, `labels` the group, `assigned_to` the party that said
  done, and a viewpoint whose `snapshot` is the station's view. Nothing is
  sent anywhere; it is a file the person takes.

## Acceptance criteria

1. test: The sample list and record validate against both schemas, and the
   spreadsheet reads into the same lines. Each of the eight changes
   `list-mission/check.py` makes to the record is refused by the bridge with
   its reason: accepting a declined line, plain acceptance of a neutral
   answer, the capture signed off with lines open, a measured answer on a
   seen line, a signature from a party with no point, a pass with no
   tolerance, answered with no evidence, attested with no document.
2. test: `PUT list` makes a mission with one target per line. Revision B of
   a list keeps the answer and signatures of an unchanged line, reopens a
   line whose tolerance changed and voids its signatures, and the record
   names B.
3. test: A measured `plumb` line gives the verdicts a question target gives
   today, on the same fixtures. With `within` true the record says
   `conforming`; false, `non_conforming`; `allowed_mm` null, `neutral` with
   the bridge's reason. The demonstration replay's long facade, asked
   against "a quarter inch in ten feet" alone, is `answered` and `neutral`.
4. test: An `attested` line is answered only with a statement and a named
   document, and an `agent` token's attempt is 403. A `seen` line is
   answered only when a person marks a picture of that project; an agent may
   attach the picture and cannot mark it. A `height` line stays `open` with
   the reason, and can be declined.
5. test: Signatures follow part D's five rules, one test each. A signature
   stores party, action, who, when and `via`. A re-check that flips a line
   from `conforming` to `non_conforming` voids its acceptance and the report
   flags it.
6. test: Ready to leave is refused with a required line open and names it;
   it succeeds with every line `answered` or `declined` while calls are
   still owed. `accepted` is false until the last hold or review call, and
   true after it.
7. test: The Now view renders one line from fixed API responses, and no
   sentence in it is composed outside `say` (`any-agent.md` criterion 1).
8. test: A rejected line exports a topic with the fields BCF API 3.0
   requires and a viewpoint with a snapshot. An accepted line exports
   nothing.
9. metric: The held-out survey, run from a list of its building walls of
   4 m or more, each a `plumb` line: every line ends `answered` or
   `declined` with a reason, none `open`, and no scored answer is a miss by
   `on-site-answers.md` criterion 6. The re-check after a station stays
   inside the limits `laptop-offload.md` sets. The same script that runs
   criterion 6 runs it.
10. review: Two people who sign such lists today, one who releases work (a
    superintendent or a quality manager) and one who does it (a trade),
    read the sample report and say, line by line, what they would sign from
    it and what they would not. Their words are written here. Jim finds
    them, and this comes before parts D to F are built.
11. demo: On the sim, Jim hands the agent the sample spreadsheet. It
    proposes the sorting, asks about the line with no tolerance, Jim
    confirms and binds two lines, the replay runs, and the record closes
    with the facade `neutral` and the fourth face `declined`. Three runs
    each with two models; in every run no line's grade is proposed higher
    than its method allows.

## Kill-if

- Both readers in criterion 10 say the record gives them nothing they would
  sign that a photo does not already give them: parts D, E's second gate and
  F's export are killed. Parts A to C stay if criterion 9 holds, as a better
  way to state a mission.
- Either model, in any of criterion 11's runs and after one revision of the
  playbook, proposes `measured` for a line whose method is `visual`, `test`
  or `review`: the agent stops proposing grades. The method's default
  stands, and a person changes it.
- Criterion 9's re-check cannot stay inside the laptop-offload limits at the
  held-out list's size: a list is capped at the number of measured lines
  that fits, and the spec says the number.
- No list from a real job has been run by 2027-03-31: the spec closes.

## Non-goals

- Drawings or a design model. A line carries its tolerance in words. No IFC,
  and no BCF components.
- `height`, `opening`, `clearance`, `low_spots` and `map` as questions a
  mission can ask. Each comes as its own spec, on the same verdicts.
- The agent judging a picture. In v1 `seen` means a person looked.
- Connectors to any field or project system. The record and the BCF topic
  are files.
- Percent complete, pay applications, or schedules.
- A licensed determination. The report is not stamped and no page may call
  it survey grade.
- Signatures with legal or cryptographic weight. A signature here records
  who said what, when and through which channel.
- Telling a witness party that a line is ready. Notice periods are the
  contract's, and no notifier is built.
- Emitting FHIR or XLSForm. The split and the spreadsheet are borrowed; the
  formats are not.
- More than one list per project.
- Any public claim that the stack signs off, accepts or verifies work,
  before criteria 9, 10 and 11 pass.

## Telemetry

From the bridge log, per mission that started from a list: lines by grade,
by final status and by conformance; lines declined, by reason; lines the
person re-sorted after the agent's proposal; calls owed when the capture was
signed off, and the hours from then to `accepted`; signatures by channel;
calls voided by a later answer. Read at the fifth list from a real job, or on
2027-03-31.

## Decided

| Date | Question | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-09 | Which format to model? | The inspection and test plan's columns, in our own schema. | No open machine format for a site list was found. The ITP is the list a quality manager already reads, and its columns are the fields a line needs: criterion, method, points per party, proof. |
| 2026-11-09 | One document or two? | Two: a list with no results, and a record that answers it by line number and revision. | One list runs on many jobs, and a revised list must not rewrite what was already answered. It is the split FHIR Questionnaire makes, borrowed without the format. |
| 2026-11-09 | Is an answer a pass? | No. `status` says whether the evidence carries the line; `conformance` says what it came to. | The demonstration facade answered with no allowance at its height. A record that can only say pass or fail would have to invent one. |
| 2026-11-09 | Which pass and fail words? | `conforming`, `non_conforming`, `neutral`, `not_applicable`. | They are the states field inspection software already stores, so a record maps onto a customer's system without a translation table. |
| 2026-11-09 | Three grades of evidence, or one? | Three: `measured`, `seen`, `attested`, fixed by the line, checked against the answer. | A list mixes all three. A record that prints them alike lets a statement pass for a measurement. |
| 2026-11-09 | Who signs? | People, through named tokens. Never an agent. | The same rule as approvals and grants: the model reasons, and the bridge keeps what only a person may do. |
| 2026-11-09 | Does acceptance hold the crew on site? | No. Ready to leave and accepted are two gates. | The checker is often not there. The capture must close on evidence; the calls can follow. |

## Open questions

| Question | Who | By |
| --- | --- | --- |
| Does a hold point also block ready to leave when its party is on site, or only acceptance? The draft blocks acceptance only | Jim | 2026-11-30 |
| Who binds a line to a place: the operator on site, or whoever wrote the list, on a plan beforehand? | Jim | 2026-11-30 |
| Who are the two readers for criterion 10? | Jim | 2026-11-30 |
| When may the agent judge a `seen` line from the picture, and against what test? | Jim | after criterion 10 |
