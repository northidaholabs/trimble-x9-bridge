"""Check a list and its record: both schemas, then the rules a schema cannot say.

    python3 check.py                      # the sample, then the refusals
    python3 check.py LIST.json RECORD.json

The rule is tell, don't stop. A problem is something the record may not say:
it is not whole, or it prints more than it has. A notice is something a
person chose to do. It is kept, and shown as what it was.
"""
import copy
import csv
import json
import sys
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker

HERE = Path(__file__).parent
CLOSED = {"answered", "declined"}
CALLS = {"accepted", "accepted_with_note", "rejected"}
RANK = {"attested": 0, "seen": 1, "measured": 2}
DEFAULT_EVIDENCE = {"dimensional": "measured", "visual": "seen", "test": "attested", "review": "attested"}


def load(path):
    return json.loads(Path(path).read_text(encoding="utf8"))


def schema_problems(doc, schema_name):
    validator = Draft202012Validator(load(HERE / schema_name), format_checker=FormatChecker())
    return [f"schema: {'/'.join(map(str, e.absolute_path)) or '.'}: {e.message[:110]}" for e in validator.iter_errors(doc)]


def rules(lst, rec):
    """Returns (problems, notices)."""
    out, notes = [], []
    if (rec["list_id"], rec["list_revision"]) != (lst["list_id"], lst["revision"]):
        out.append("the record answers another list or revision")
    lines = {l["line_id"]: l for l in lst["lines"]}
    parties = {p["id"] for p in lst["parties"]}
    seen = set()
    for r in rec["lines"]:
        lid = r["line_id"]
        if lid not in lines:
            out.append(f"{lid}: not a line of the list")
            continue
        if lid in seen:
            out.append(f"{lid}: answered twice")
        seen.add(lid)
        line, status, conf = lines[lid], r["status"], r.get("conformance")
        ev = r.get("evidence") or {}
        grade = ev.get("grade")
        if status == "answered":
            if RANK[grade] < RANK[line["evidence"]]:
                notes.append(f"{lid}: answered below the grade asked; the list asks for {line['evidence']}, the answer is {grade}")
            if grade == "attested" and not ev.get("document"):
                notes.append(f"{lid}: a statement with no document named")
            if grade == "measured":
                has_tol = "tolerance" in line
                if has_tol and conf == "not_applicable":
                    out.append(f"{lid}: the line has a tolerance, so the record makes a call or says why it cannot")
                if not has_tol and conf in ("conforming", "non_conforming"):
                    out.append(f"{lid}: no tolerance on the line, so a measured answer has nothing to conform to")
        for s in r.get("signatures", []):
            who, action = s["party"], s["action"]
            if who not in parties:
                out.append(f"{lid}: {who} is not a party to the list")
                continue
            if action == "accepted" and (status != "answered" or conf not in ("conforming", "not_applicable")):
                what = f"a {conf} answer" if status == "answered" else f"a line that is {status}"
                out.append(f"{lid}: plain acceptance of {what}; it takes a note or a rejection")
            elif action in CALLS and (status != "answered" or conf in ("neutral", "non_conforming")):
                what = f"a {conf} answer" if status == "answered" else f"a line that is {status}"
                notes.append(f"{lid}: {who} {action.replace('_', ' ')} on {what}; kept, with the note")
            if not counts(line, s):
                notes.append(f"{lid}: {who} signed {action}; kept, and it does not count toward accepted")
    for lid in lines.keys() - seen:
        out.append(f"{lid}: the record has no entry for this line")
    sign_off = rec.get("capture_sign_off")
    if sign_off:
        named, still = sorted(sign_off.get("left_open", [])), sorted(open_lines(lst, rec))
        if named != still:
            out.append("the sign-off does not name the lines it left open: " + (", ".join(still) or "none are open"))
        elif still:
            notes.append("signed off with lines left open: " + ", ".join(still))
    return out, notes


def counts(line, s):
    """Whether a signature counts toward accepted. One that does not is still kept."""
    point = line.get("points", {}).get(s["party"])
    if s.get("on_earlier_answer") or point in (None, "S"):
        return False
    if s["action"] == "released":
        return point == "H"
    if s["action"] == "witnessed":
        return point in ("H", "W")
    return True


def open_lines(lst, rec):
    """What ready to leave names: required lines with no answer and no decline. It stops nobody."""
    status = {r["line_id"]: r["status"] for r in rec["lines"]}
    return [l["line_id"] for l in lst["lines"] if l.get("required", True) and status.get(l["line_id"], "open") not in CLOSED]


def owed(lst, rec):
    """Calls still owed before the work is accepted: every hold and review party, on every line."""
    lines = {l["line_id"]: l for l in lst["lines"]}
    called = {(r["line_id"], s["party"]) for r in rec["lines"] for s in r.get("signatures", [])
              if r["line_id"] in lines and s["action"] in CALLS and counts(lines[r["line_id"]], s)}
    return [f"{l['line_id']}:{p}" for l in lst["lines"] for p, pt in l.get("points", {}).items()
            if pt in ("H", "R") and (l["line_id"], p) not in called]


def check(lst, rec):
    """Returns (problems, notices). Rules run only on a record both schemas accept."""
    problems = schema_problems(lst, "list.schema.json") + schema_problems(rec, "record.schema.json")
    return (problems, []) if problems else rules(lst, rec)


def sheet_problems(csv_path, lst):
    """The spreadsheet carries the ITP's columns. What it cannot carry is the agent's sorting step."""
    out, lines = [], {l["line_id"]: l for l in lst["lines"]}
    parties = [p["id"] for p in lst["parties"]]
    for row in csv.DictReader(Path(csv_path).open(encoding="utf8")):
        line = lines.get(row["line"])
        if not line:
            out.append(f"{row['line']}: in the sheet, not in the list")
            continue
        want = {"group": line.get("group", ""), "activity": line["activity"], "requirement": line["requirement"],
                "reference": line.get("reference", ""), "method": line["method"],
                "tolerance": line.get("tolerance", {}).get("words", ""), "record": line.get("record", "")}
        want.update({p: line.get("points", {}).get(p, "") for p in parties})
        out += [f"{row['line']}: {k} differs" for k, v in want.items() if row.get(k, "") != v]
        if line["evidence"] != DEFAULT_EVIDENCE[line["method"]]:
            out.append(f"{row['line']}: evidence is not the method's default; the sheet cannot say so")
    return out


def main(argv):
    if len(argv) == 3:
        problems, notices = check(load(argv[1]), load(argv[2]))
        print("\n".join(problems) or "ok")
        for n in notices:
            print("notice:", n)
        return 1 if problems else 0

    lst, rec = load(HERE / "samples/demo-closeout.list.json"), load(HERE / "samples/demo-closeout.record.json")
    problems, notices = check(lst, rec)
    problems += sheet_problems(HERE / "samples/demo-closeout.csv", lst)
    by = {}
    for r in rec["lines"]:
        by.setdefault(r["status"] + ("/" + r["conformance"] if "conformance" in r else ""), []).append(r["line_id"])
    print("sample:", "ok" if not problems else problems)
    for k, v in sorted(by.items()):
        print(f"  {k:28} {', '.join(v)}")
    for n in notices:
        print("  notice:", n)
    print("  ready to leave would name:", ", ".join(open_lines(lst, rec)) or "nothing")
    print("  calls owed before acceptance:", len(owed(lst, rec)))

    sig = {"by": {"name": "Test Signer"}, "at": "2026-01-01T00:00:00Z", "via": "web UI"}
    still = open_lines(lst, rec)

    def line(doc, lid):
        return next(r for r in doc["lines"] if r["line_id"] == lid)

    # What the record may not say.
    def sign_off_unnamed(d): d["capture_sign_off"] = dict(sig)
    def sign_off_no_reason(d): d["capture_sign_off"] = dict(sig, left_open=still)
    def accept_neutral(d): line(d, "1.1")["signatures"] = [dict(sig, party="checker", action="accepted")]
    def measured_no_reading(d): line(d, "5.1").update(status="answered", conformance="not_applicable", evidence={"grade": "measured"})
    def call_without_tolerance(d): line(d, "2.1")["conformance"] = "conforming"
    def no_evidence(d): line(d, "5.2").update(status="answered", conformance="conforming")
    def unknown_party(d): line(d, "2.1")["signatures"] = [dict(sig, party="passer_by", action="done")]

    # What a person may do. Each is kept, and shown.
    def leave_open(d): d["capture_sign_off"] = dict(sig, left_open=still, reason="Synthetic: the rooms were locked.")
    def accept_declined(d): line(d, "1.2")["signatures"] = [dict(sig, party="checker", action="accepted_with_note", note="Synthetic: accepted on the drawing.")]
    def accept_neutral_note(d): line(d, "1.1")["signatures"] = [dict(sig, party="checker", action="accepted_with_note", note="Height is past the tolerance's ten feet; accepted on the figure.")]
    def no_point(d): line(d, "2.1")["signatures"] = [dict(sig, party="trade", action="done")]
    def below_grade(d): line(d, "1.2").update(status="answered", conformance="conforming", evidence={"grade": "attested", "document": "Synthetic: site walk notes."})
    def no_document(d): line(d, "6.1").update(status="answered", conformance="conforming", evidence={"grade": "attested"})

    refused = [
        ("a sign-off that does not name the lines it left open", sign_off_unnamed),
        ("a sign-off that leaves lines open with no reason", sign_off_no_reason),
        ("plain acceptance where the tolerance made no call", accept_neutral),
        ("a measured answer with no reading", measured_no_reading),
        ("a pass on a measured answer with no tolerance", call_without_tolerance),
        ("answered with no evidence", no_evidence),
        ("a signature from a party the list does not name", unknown_party),
    ]
    allowed = [
        ("leaving with lines open", leave_open),
        ("accepting a declined line, with a note", accept_declined),
        ("accepting where the tolerance made no call, with a note", accept_neutral_note),
        ("signing a line with no point on it", no_point),
        ("a statement on a line that asks for a measurement", below_grade),
        ("a statement with no document", no_document),
    ]
    failed = bool(problems)
    print("the record may not say (synthetic changes to the sample, made only here):")
    for name, change in refused:
        doc = copy.deepcopy(rec)
        change(doc)
        got, _ = check(lst, doc)
        failed |= not got
        print(f"  {'refused' if got else 'ALLOWED'}  {name}: {got[0] if got else ''}")
    print("a person may (kept, and shown):")
    for name, change in allowed:
        doc = copy.deepcopy(rec)
        change(doc)
        got, said = check(lst, doc)
        failed |= bool(got) or not said
        print(f"  {'kept' if not got else 'REFUSED'}  {name}: {(got or said or ['NOT SHOWN'])[0]}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
