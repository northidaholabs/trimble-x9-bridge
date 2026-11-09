"""Check a list and its record: both schemas, then the rules a schema cannot say.

    python3 check.py                      # the sample, then the refusals
    python3 check.py LIST.json RECORD.json

The rules are the model's gate. They are the part a bridge would enforce.
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
DEFAULT_EVIDENCE = {"dimensional": "measured", "visual": "seen", "test": "attested", "review": "attested"}


def load(path):
    return json.loads(Path(path).read_text(encoding="utf8"))


def schema_problems(doc, schema_name):
    validator = Draft202012Validator(load(HERE / schema_name), format_checker=FormatChecker())
    return [f"schema: {'/'.join(map(str, e.absolute_path)) or '.'}: {e.message[:110]}" for e in validator.iter_errors(doc)]


def rule_problems(lst, rec):
    out = []
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
        if status == "answered":
            if ev.get("grade") != line["evidence"]:
                out.append(f"{lid}: the list asks for {line['evidence']} evidence, the record gives {ev.get('grade')}")
            if line["evidence"] == "attested" and not ev.get("document"):
                out.append(f"{lid}: an attested line names the document it rests on")
            has_tol = "tolerance" in line
            if line["evidence"] == "measured":
                if has_tol and conf == "not_applicable":
                    out.append(f"{lid}: the line has a tolerance, so the record makes a call or says why it cannot")
                if not has_tol and conf in ("conforming", "non_conforming"):
                    out.append(f"{lid}: no tolerance on the line, so there is nothing to conform to")
        for s in r.get("signatures", []):
            who, action = s["party"], s["action"]
            point = line.get("points", {}).get(who)
            if who not in parties:
                out.append(f"{lid}: {who} is not a party to the list")
            elif point in (None, "S"):
                out.append(f"{lid}: {who} has no inspection point on this line")
            elif action == "released" and point != "H":
                out.append(f"{lid}: only a hold point is released")
            elif action == "witnessed" and point not in ("H", "W"):
                out.append(f"{lid}: {who} reviews this line, and does not witness it")
            if action in CALLS and status != "answered":
                out.append(f"{lid}: {action} on a line that is {status}; a call needs an answer")
            elif action == "accepted" and conf not in ("conforming", "not_applicable"):
                out.append(f"{lid}: plain acceptance of a {conf} answer; it takes a note or a rejection")
    for lid in lines.keys() - seen:
        out.append(f"{lid}: the record has no entry for this line")
    if rec.get("capture_sign_off") and blocking(lst, rec):
        out.append("capture signed off while lines are open: " + ", ".join(blocking(lst, rec)))
    return out


def blocking(lst, rec):
    """Lines that stop the capture sign-off: the mission's existing gate, per line."""
    status = {r["line_id"]: r["status"] for r in rec["lines"]}
    return [l["line_id"] for l in lst["lines"] if l.get("required", True) and status.get(l["line_id"], "open") not in CLOSED]


def owed(lst, rec):
    """Calls still owed before the work is accepted: every hold and review party, on every closed line."""
    signed = {(r["line_id"], s["party"]) for r in rec["lines"] for s in r.get("signatures", [])}
    return [f"{l['line_id']}:{p}" for l in lst["lines"] for p, pt in l.get("points", {}).items()
            if pt in ("H", "R") and (l["line_id"], p) not in signed]


def check(lst, rec):
    return schema_problems(lst, "list.schema.json") + schema_problems(rec, "record.schema.json") or rule_problems(lst, rec)


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
        problems = check(load(argv[1]), load(argv[2]))
        print("\n".join(problems) or "ok")
        return 1 if problems else 0

    lst, rec = load(HERE / "samples/demo-closeout.list.json"), load(HERE / "samples/demo-closeout.record.json")
    problems = check(lst, rec) + sheet_problems(HERE / "samples/demo-closeout.csv", lst)
    by = {}
    for r in rec["lines"]:
        by.setdefault(r["status"] + ("/" + r["conformance"] if "conformance" in r else ""), []).append(r["line_id"])
    print("sample:", "ok" if not problems else problems)
    for k, v in sorted(by.items()):
        print(f"  {k:28} {', '.join(v)}")
    print("  blocks the capture sign-off:", ", ".join(blocking(lst, rec)) or "nothing")
    print("  calls owed before acceptance:", len(owed(lst, rec)))

    sig = {"by": {"name": "Test Signer"}, "at": "2026-01-01T00:00:00Z", "via": "web UI"}

    def line(doc, lid):
        return next(r for r in doc["lines"] if r["line_id"] == lid)

    def accept_declined(d): line(d, "1.2")["signatures"] = [dict(sig, party="checker", action="accepted")]
    def accept_neutral(d): line(d, "1.1")["signatures"] = [dict(sig, party="checker", action="accepted")]
    def sign_off_open(d): d["capture_sign_off"] = dict(sig)
    def wrong_grade(d): line(d, "5.1").update(status="answered", conformance="conforming", evidence={"grade": "measured"})
    def stranger(d): line(d, "2.1")["signatures"] = [dict(sig, party="trade", action="done")]
    def call_without_tolerance(d): line(d, "2.1")["conformance"] = "conforming"
    def no_evidence(d): line(d, "5.2").update(status="answered", conformance="conforming")
    def attest_without_document(d): line(d, "6.1").update(status="answered", conformance="conforming", evidence={"grade": "attested"})

    refusals = [
        ("accepting a declined line", accept_declined),
        ("plain acceptance where the tolerance made no call", accept_neutral),
        ("signing off the capture with lines open", sign_off_open),
        ("a measured answer on a line that asks for a picture", wrong_grade),
        ("a signature from a party with no point on the line", stranger),
        ("a pass on a line with no tolerance", call_without_tolerance),
        ("answered with no evidence", no_evidence),
        ("attested with no document", attest_without_document),
    ]
    failed = bool(problems)
    print("refusals (synthetic changes to the sample, made only here):")
    for name, change in refusals:
        doc = copy.deepcopy(rec)
        change(doc)
        got = check(lst, doc)
        failed |= not got
        print(f"  {'refused' if got else 'ALLOWED'}  {name}: {got[0] if got else ''}")

    doc = copy.deepcopy(rec)
    line(doc, "1.1")["signatures"] = [dict(sig, party="checker", action="accepted_with_note", note="Height is past the tolerance's ten feet; accepted on the figure.")]
    got = check(lst, doc)
    failed |= bool(got)
    print(f"  {'allowed' if not got else 'REFUSED'}  acceptance with a note where the tolerance made no call")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
