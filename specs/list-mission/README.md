# The list model

The formats behind [`../list-mission.md`](../list-mission.md), samples, and
a checker. The bridge reads lists in this format, and an ITP spreadsheet and
a door and window schedule into it (`crates/x9-bridge/src/list.rs`), and
writes records in it.

The model refuses a person nothing that is theirs to decide. It refuses a
record that is not whole, prints more than it has, or does not name who
decided. What a person chose to do is kept and shown as what it was, and
going past what the bridge said takes one sentence of why.

| File | What |
| --- | --- |
| `list.schema.json` | A list: parties and lines, no results |
| `record.schema.json` | A record: what each line came to, and who signed |
| `samples/demo-closeout.list.json` | Eight lines. 1.1 to 4.1 are the missions on the public visit page; 5.1 to 6.1 are illustrative and say so |
| `samples/demo-closeout.csv` | The same list as a spreadsheet with an ITP's columns, the party columns naming their roles (`trade (contractor)`). It reads into the list's lines unsorted; sorted as the list is, it is the list. The bridge's tests hold the two together |
| `samples/demo-closeout.record.json` | Only what the visit page gives, quoted. No signatures: nobody signed a line |
| `samples/demo-schedule.csv` | A door and window schedule, written for the model: four openings |
| `samples/demo-schedule.list.json` | The list the bridge reads from it, one `opening` line per mark. The bridge's tests hold the two together |
| `check.py` | Both schemas, the rules a schema cannot say, the spreadsheet read as an unsorted list against the list, and the schedule against its list. Then sixteen changes to a copy of the record: ten things the record may not say, each refused, and six things a person may do, each kept and shown. Then three cases of acceptance of the work |

```bash
python3 specs/list-mission/check.py          # needs the jsonschema package
```

CI runs it on every push, as its own job.

## What is out there

A search on 2026-11-09 found no open, machine-readable standard for a
construction punch list or site checklist. Five things each cover part of
it:

| Candidate | What it is | Open | Gives us | Lacks |
| --- | --- | --- | --- | --- |
| Inspection and test plan (ITP) | The quality plan's table: item, activity, acceptance criteria, inspection type, hold, witness and review points per party, verifying record, sign-off | A practice, not a file format. ISO 10005 gives guidance on quality plans and fixes no layout | The meaning of every column | Any schema |
| FHIR Questionnaire and QuestionnaireResponse | HL7's standard for a list of typed questions and, held apart, one set of answers | Yes, CC0 | The split between a list and its answers, joined by a stable item id (`linkId`) | The domain: no tolerances, parties or place |
| XLSForm | The spreadsheet form standard behind ODK, KoboToolbox and Survey123 | Yes, a community standard; the converter is BSD | A list a person writes in a spreadsheet | Parties, sign-off, what evidence is worth |
| BCF | buildingSMART's open format for issues: a topic with a title, type, status and assignee, and viewpoints with a camera and a snapshot | Yes, CC BY-ND 4.0 | A place for each issue, and a way to hand a finding to tools a customer runs | Acceptance criteria |
| Field inspection software | Templates of sections and items, each answered from a response set | Proprietary | Four response states: conforming, non-conforming, neutral, not applicable | Openness |

## Against the bridge at 0.15.0

(Kept as it was found. The next version reads lists and writes records; see the
spec's status line for what is built.)

| Model | Bridge today | Gap |
| --- | --- | --- |
| Line | `Target`: `target_id`, `name`, `kind`, `note` | `requirement`, `reference`, `method`, `record` are not held |
| `tolerance` | `Target.tolerance`, the same shape | None |
| `plumb`, `flatness` | `Question` with a verdict, a reading and `said` | Live |
| `height`, `low_spots`, `map` | `x9-bridge analyze` and `register`, from finished scans | Not questions a mission can ask |
| `opening` | The window recipe's `measureOpening`; from 0.19.0 the count of openings on a wall face | Live, for a schedule's lines |
| `photo`, `statement` | Nothing | Not built |
| `status` | `Verdict` and the target's `Status` | Names only |
| `conformance` | `within` and `allowed_mm` inside `said` | No field of its own |
| `place.face` | `Target.face` | None |
| `signatures`, `points` | One sign-off for the whole mission | Not built |

## Sources

Read on 2026-11-09.

- BCF API 3.0, buildingSMART: https://github.com/buildingSMART/BCF-API
- FHIR R5 Questionnaire: https://hl7.org/fhir/R5/questionnaire.html, and its licence: https://hl7.org/fhir/R5/license.html
- XLSForm: https://xlsform.org/en/
- Response types for inspection items: https://support.procore.com/product-releases/new-releases/inspections-flexible-response-types-for-inspections
- ITP columns and points: https://blog.projectmaterials.com/quick-answers/epc/what-is-itp/ and https://www.qualityinconstruction.com/itp-in-construction
- ISO 10005, guidelines for quality plans: https://knowledge.bsigroup.com/products/quality-management-systems-guidelines-for-quality-plans
- The demonstration scan's figures: https://ai-scan.northidaholabs.com/visit.html
