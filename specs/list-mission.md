# Spec: A mission from a list

Owner: Jim · Date: 2026-11-09, revised twice on 2026-11-10 · Status: approved
(Jim, 2026-11-11). Built for the next version, as far as
[`construction-counts.md`](construction-counts.md) part A needs: part A (the
list and the record, from JSON or a door and window schedule), part B's
binding, and part C's measured lines (`plumb` and `flatness` through
questions, `opening` through the count). Not built: the ITP spreadsheet and
part B's sorting, seen and attested answers, declines by a person only, and
parts D to F (criterion 10 comes first).

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
  points, where a named party's release is owed. The record has nowhere to
  put that.
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
ticked. What the bridge can do that such an app cannot is know whether the
evidence carries a line, say where to stand when it does not, and print each
line as what it is: measured, seen, stated, or left open by a named person
with their reason.

## The rule: tell, don't stop

The bridge refuses a person nothing that is theirs to decide. It says what
is open and what the evidence carries. The person decides, and the record
prints the decision as what it was, with their name.

Where a person goes past what the bridge said, the bridge asks for one
sentence of why: leaving with lines open, declining a line, and a call on
anything that is not a clean pass. That sentence is the only thing it asks
for. Whether it is required or only asked for is an open question; the
draft requires it.

A block gets routed around, and the record is then less true. A crew that
cannot leave with a line open declines the line, and the record says
`declined` where the truth was "left".

Three things stay fixed. None of them is a person's choice being refused:

- **The record prints what it has.** Evidence carries the grade it was
  given, and a pass is there only where the tolerance or a named person gave
  one. A statement never prints as a measurement.
- **An agent is not a person.** It signs nothing, attests nothing, marks no
  picture and declines no line. It can propose any of those, and relay a
  person's words, marked as relayed.
- **The instrument's own guards are not in this spec.** Approvals and grants
  stay as [`any-agent.md`](any-agent.md) and
  [`control-plane.md`](control-plane.md) have them. They guard what an agent
  may do to the scanner, not what a person may decide about a line.

## Ship

Six parts. A to C make a list something a mission can run. D to F are the
signatures and what leaves. Every part follows the rule above.

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

A new revision of the list starts a new record. It carries over the answer
and signatures of every line whose `requirement`, `tolerance` and `question`
did not change. The other lines start `open`, because their answers were to
different words. Nothing is deleted: the earlier revision's record is kept
as it was. Each record says which revision it answers.

### B. Sorting, and binding to a place

A spreadsheet row says what must be true. It does not say what the bridge is
to be asked. Before the first scan:

- The agent proposes, per line, the `evidence` grade, the `question`, and
  the arithmetic of the tolerance from its words ("a quarter inch in ten
  feet" as `steps`). It asks the person about a line it cannot sort, or a
  measured line with no tolerance ("plumb to what?"). The person confirms or
  changes the proposals, as a recipe's person step works today. One yes can
  cover the whole sorting, and any line can be sorted again later. Nothing
  is sorted without a person's yes.
- A line is bound to a place by a person, on a station's view: a wall for a
  measured line (the existing `wall_id` to face), a station and a box on its
  preview for a seen line. It can be bound on site or beforehand, and bound
  again. The record keeps who bound it. A line with no place is `open` and
  says so.

### C. What a line can be answered with, in v1

| Evidence | Question | Answered by |
| --- | --- | --- |
| `measured` | `plumb`, `flatness` | The bridge, as a question target is today: `not_yet`, `does_not_fit` or `answer`, re-checked after every station |
| `seen` | `photo` | A picture from the project, attached by anyone, and marked as showing it by a person. The agent does not judge a picture in v1 |
| `attested` | `statement` | A person's statement. The document it rests on is asked for; a statement with none is kept and printed as such. An agent cannot give it |

`conformance` on a measured line comes from the tolerance's call the bridge
already makes: `within` true is `conforming`, `within` false is
`non_conforming`, and no allowance for the case (`allowed_mm` null) is
`neutral` with the bridge's reason. A measured line with no tolerance is
`not_applicable`: it records a figure.

A line says the grade it asks for. An answer says the grade it has. A person
may answer any line at a lower grade: a picture or a statement on a measured
line, a statement on a seen line. The line is `answered`, the conformance is
the person's own call, and the report prints it as answered below the grade
asked, with both grades. The bridge never raises a grade. A `measured`
answer is the bridge's alone: it carries a reading, and nobody edits its
figure or its call. A person who disagrees says so in their signature.

Every answer that is not the bridge's names its person. A seen answer names
who marked the picture, an attested one who gave the statement, and a
decline who declined. The agent may propose a decline ("the face never came
back into view"); a person says yes, and the record names them and that the
agent proposed it.

The other measured kinds the schema names (`height`, `opening`, `clearance`,
`low_spots`, `map`) are accepted in a list and stay `open`, with the reason
that the bridge cannot ask them yet. A person can decline them, leave them
open, or answer them at a lower grade.

### D. Signatures

A party is a named token of role `operator` or `viewer`, tied to a party of
the list. A signature on a line is a party, an action (`done`, `witnessed`,
`released`, `accepted`, `accepted_with_note`, `rejected`), who, when, and
how it reached the bridge (`via`, as the mission's sign-off records it). The
bridge keeps every signature a person makes, and says what it counts for.
For every client:

- Any party of the list may sign any line. A signature from a party with no
  point on the line, or a release from a party that does not hold it, is
  kept and shown. It does not count toward accepted.
- A party may make its call on any line, whatever its status. Plain
  `accepted` is the bridge's word for a `conforming` or `not_applicable`
  answer with nothing to explain. On anything else (a `neutral` or
  `non_conforming` answer, or a line that is `declined`, `open`, `not_yet`
  or `does_not_fit`) the call is `accepted_with_note` or `rejected`, and the
  report prints the note beside what the bridge had.
- An `agent` token signs nothing. There is no tool for it and the route
  answers 403. A signature the agent relays from the chat is marked relayed,
  as the mission's sign-off is.
- A later change to a line's `status` or `conformance`, including from
  `open` to `answered`, or a move of its figure by more than the 5 mm
  `on-site-answers.md` scores a miss at, marks the calls on that line as
  made on the earlier answer. They stay in the record, stop
  counting, and the record and the report flag the line.

### E. Ready to leave, and accepted

Two statements. Neither stops anyone.

- **Ready to leave** always answers. It names every required line that is
  `open`, `not_yet` or `does_not_fit`, and what each one needs, from `say`.
  The operator signs the capture off when they choose. With lines open, the
  sign-off takes one reason and names those lines as left open, and the
  report opens with them. A line left open keeps its status. It is not
  turned into a decline.
- **Accepted** is new and separate. Nothing is owed: on every line, each
  hold party has released it or made a call, and each review party has
  made a call. And no call that counts is a rejection. A rejected line
  keeps the work from accepted until its answer changes, which marks the
  rejection as made on the earlier answer, and the party calls it again.
  Accepted can come days later and from another place.
- **A hold point keeps its ITP meaning.** The trade's next work waits for
  the release, and the contract enforces that, not the bridge. The bridge
  never holds the scanning crew. It shows the release as owed until it is
  given.

This changes what is built. `on-site-answers.md` criterion 3 has
`scanner_ready_to_leave` refuse while an asked target is `not_yet`, or
`does_not_fit` and not declined, and 0.15.0 does that. If this part is
approved, that criterion is amended for every mission, not only a mission
from a list, so there is one rule. That spec's "Unanswered" count changes
with it: it assumes sign-off refuses an open `not_yet`, and becomes
questions declined or left open while their last verdict was `not_yet`.

### F. What the person sees, and what leaves

- The web UI's Now view shows one line: the next to document, with its
  requirement, and where to stand. The whole list is one tap away. Every
  sentence comes from `say`.
- The report's first line counts the lines the bridge answered, the lines
  a person answered below the grade asked, and the lines declined or left
  open, before any acceptance. A record accepted with nothing measured says
  so at the top.
- The report then gives the list, in order: each line's requirement, grade,
  status, conformance, evidence and signatures. It carries names, companies
  and roles. It carries no licence number and is not a stamped document.
- A rejected line can be exported as a BCF topic (BCF API 3.0 JSON):
  `title` from the line number, activity and requirement, `description` the
  bridge's sentence, `labels` the group, `assigned_to` the party that said
  done, and a viewpoint whose `snapshot` is the station's view. Nothing is
  sent anywhere; it is a file the person takes.

## Acceptance criteria

1. test: The sample list and record validate against both schemas, and the
   spreadsheet reads into the same lines. The bridge treats each change
   `list-mission/check.py` makes to the record as the checker does. Ten
   are things the record may not say, and are refused with the reason: a
   sign-off that does not name the lines it left open, a sign-off that
   leaves lines open with no reason, plain acceptance where there is
   something to explain, a measured answer with no reading, a pass on a
   measured answer with no tolerance, answered with no evidence, a signature
   from a party the list does not name, a decline with nobody named, a
   statement with nobody named, a person named on a measured answer. Six
   are things a person may do, and
   are kept and shown: leaving with lines open, accepting a declined line
   with a note, accepting a neutral answer with a note, signing a line with
   no point on it, answering below the grade asked, a statement with no
   document.
2. test: `PUT list` makes a mission with one target per line. Revision B
   of a list starts a record that names B. It carries over the answer and
   signatures of an unchanged line, and starts a line whose tolerance
   changed as `open` with no signatures. Revision A's record is unchanged.
3. test: A measured `plumb` line gives the verdicts a question target gives
   today, on the same fixtures. With `within` true the record says
   `conforming`; false, `non_conforming`; `allowed_mm` null, `neutral` with
   the bridge's reason. The demonstration replay's long facade, asked
   against "a quarter inch in ten feet" alone, is `answered` and `neutral`.
4. test: An `attested` line is answered with a person's statement, and with
   no document named the record says so. An `agent` token's attempt is 403,
   and so is its attempt to decline a line; it can propose one.
   A `seen` line is answered when a person marks a picture of that project;
   an agent may attach the picture and cannot mark it. A `height` line stays
   `open` with the reason. A person can decline it, or answer it with a
   statement, and the report then prints it as attested where measured was
   asked. No request sets the grade `measured` or changes a measured
   answer's figure or call.
5. test: Signatures follow part D's four rules, one test each. A signature
   stores party, action, who, when and `via`. A signature from a party with
   no point is stored and does not change `accepted`. A re-check that flips
   a line from `conforming` to `non_conforming` marks its acceptance as made
   on the earlier answer, it stops counting, and the report flags it.
6. test: Ready to leave answers in every state and names each open line
   with what it needs. A sign-off with a required line open succeeds with a
   reason: the record names the lines left open, their status is unchanged,
   and the report opens with them. `accepted` is false while a release or
   call is owed or a call that counts is a rejection, and true after the
   last one with none rejected: the checker's three acceptance cases. No
   test in this spec has a person refused for the state of a line.
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
  sign that a photo does not already give them: parts D, E's accepted and
  F's export are killed. Parts A to C stay if criterion 9 holds, as a better
  way to state a mission.
- Either model, in any of criterion 11's runs and after one revision of the
  playbook, proposes `measured` for a line whose method is `visual`, `test`
  or `review`: the agent stops proposing grades. The method's default
  stands, and a person changes it.
- Criterion 9's re-check cannot stay inside the laptop-offload limits at the
  held-out list's size: a list is capped at the number of measured lines
  that fits, and the spec says the number.
- At the fifth list from a real job, fewer than half of its measured lines
  ended answered by the bridge; the rest were left open, declined, or
  answered by picture or statement. The list mission is then a checklist
  with a scanner beside it. Parts D to F stop until a field visit shows why,
  and the rule on leaving is reopened.
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
calls overtaken by a later answer.

What the rule costs is read from four more counts: lines left open at
sign-off, by reason; lines answered below the grade asked; calls made with a
note, by the line's status; signatures that did not count. The kill-if on
measured lines reads from them.

Read at the fifth list from a real job, or on 2027-03-31.

## Decided

| Date | Question | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-09 | Which format to model? | The inspection and test plan's columns, in our own schema. | No open machine format for a site list was found. The ITP is the list a quality manager already reads, and its columns are the fields a line needs: criterion, method, points per party, proof. |
| 2026-11-09 | One document or two? | Two: a list with no results, and a record that answers it by line number and revision. | One list runs on many jobs, and a revised list must not rewrite what was already answered. It is the split FHIR Questionnaire makes, borrowed without the format. |
| 2026-11-09 | Is an answer a pass? | No. `status` says whether the evidence carries the line; `conformance` says what it came to. | The demonstration facade answered with no allowance at its height. A record that can only say pass or fail would have to invent one. |
| 2026-11-09 | Which pass and fail words? | `conforming`, `non_conforming`, `neutral`, `not_applicable`. | They are the states field inspection software already stores, so a record maps onto a customer's system without a translation table. |
| 2026-11-09 | Three grades of evidence, or one? | Three: `measured`, `seen`, `attested`. The line fixing the answer's grade was changed on 2026-11-10, below. | A list mixes all three. A record that prints them alike lets a statement pass for a measurement. |
| 2026-11-09 | Who signs? | People, through named tokens. Never an agent. | The same rule as approvals and grants: the model reasons, and the bridge keeps what only a person may do. |
| 2026-11-09 | Does acceptance hold the crew on site? | No. Ready to leave and accepted are separate. | The checker is often not there. The capture closes on its own; the calls can follow. |
| 2026-11-10 | Does the bridge stop a person? | No. It says what is open, the person decides, and the record prints the decision with their name and reason. | Jim: people need room to work, and blocking is not good practice. A block also gets routed around: a line that was left gets declined to pass the gate, and the record is less true. |
| 2026-11-10 | Does a hold point hold the crew on site? | No. It keeps its ITP meaning: the trade's next work waits for the release, by contract. The bridge shows the release as owed. | The scanning crew is not the trade. It closes the open question of 2026-11-09. |
| 2026-11-10 | Who declines a line? | A person, named in the record. The agent may propose a decline. | Declining is a decision on a line, as a signature is. |
| 2026-11-10 | Is the work accepted once every call is in? | Only if no call that counts is a rejection. A release settles what a hold party owes. | A record where every party rejected every line read as accepted in the first revision. |
| 2026-11-10 | Must an answer match the grade the line asks for? | No. The line says the grade asked, the answer the grade it has, and the report prints both. The bridge never raises a grade. | A person on site may have only a picture or their word. Refusing it loses the line; printing it at its own grade keeps the record true. |
| 2026-11-11 | Approve? | Approved. | Jim. |
| 2026-11-11 | What is a line's id? | An ITP item number (`1.2`) or a schedule's mark (`W1`, `D-3`): letters and digits joined by `.` or `-`, up to 32. | A schedule's mark is how everyone on the job names the opening, and renumbering the rows would break revisions. |
| 2026-11-11 | Who binds a line, in this version? | A person with an operator token, on site or beforehand. The agent proposes, and its MCP tool asks the person first. | Party tokens come with part D. The bridge's existing person-only actions (the sign-off) use the operator token the same way. |
| 2026-11-10 | What stays fixed? | What the record prints, that an agent is not a person, and the instrument's guards. | None of the three refuses a person's choice. They keep the record worth signing. |

## Open questions

| Question | Who | By |
| --- | --- | --- |
| The leave gate is built and tested as a refusal (`on-site-answers.md` criterion 3, the README, 0.15.0). Amend it for every mission when this is approved, or only for a mission from a list? The draft says every mission | Jim | 2026-11-30 |
| When a person goes past what the bridge said (leaves with lines open, accepts a neutral answer), is the reason required or only asked for? The draft requires one sentence, which can cover many lines and can be spoken | Jim | 2026-11-30 |
| May a signed-off mission be picked up again on a return visit? Today it cannot change and a new one is started | Jim | 2026-11-30 |
| The draft lets the operator on site or the list's author beforehand bind a line, and either bind it again. Is there a job where one of them must not? | Jim | 2026-11-30 |
| Who are the two readers for criterion 10? | Jim | 2026-11-30 |
| When may the agent judge a `seen` line from the picture, and against what test? | Jim | after criterion 10 |
