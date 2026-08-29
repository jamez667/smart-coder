#!/usr/bin/env python3
"""Vendor SWE-bench-Live instances into evals/swebench/live-subset.json.

SWE-bench-Live is a *different benchmark* from SWE-bench Lite — newer issues, drawn
from recent GitHub activity to limit training contamination, and the one Pi's published
12/25 was measured on. A score on one says nothing about the other.

Like the Lite fetcher, rows are vendored EXACTLY as published: no filtering, no repair.
A benchmark you have edited cannot be compared to anyone else's run of it.

    python scripts/fetch-swebench-live.py --list           # easiest instances first
    python scripts/fetch-swebench-live.py --only <id>      # vendor one

Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import urllib.request

API = (
    "https://datasets-server.huggingface.co/rows"
    "?dataset=SWE-bench-Live%2FSWE-bench-Live&config=default&split=lite"
    "&offset={off}&length=100"
)

# The source subtree handed to the agent, per repo. Only this is copied to the host,
# which is what keeps the test tree out of reach. Add a repo here before vendoring it.
SRC_DIR = {
    "cyclotruc/gitingest": "src/gitingest",
}


def fetch_all() -> list[dict]:
    rows = []
    for off in range(0, 300, 100):
        with urllib.request.urlopen(API.format(off=off), timeout=120) as r:
            rows.extend(x["row"] for x in json.load(r)["rows"])
    return rows


def as_list(v) -> list[str]:
    """FAIL_TO_PASS/PASS_TO_PASS, whichever shape the split uses."""
    return v if isinstance(v, list) else json.loads(v)


def difficulty(row: dict) -> tuple:
    """Patch size, as the dataset itself reports it: files, hunks, lines."""
    d = row.get("difficulty") or {}
    return (d.get("files", 99), d.get("hunks", 99), d.get("lines", 999))


def test_files(node_ids: list[str]) -> list[str]:
    """The distinct files named by a set of node ids.

    Only used to pick which files to copy out for the agent to read; entries that are
    not node ids are skipped here, which touches no scored list.
    """
    return sorted({n.split("::", 1)[0] for n in node_ids if "::" in n})


def convert(row: dict) -> dict:
    repo = row["repo"]
    if repo not in SRC_DIR:
        raise SystemExit(f"no src_dir mapping for {repo} — add one to SRC_DIR")
    # SWE-bench Lite ships these as JSON-encoded strings; Live ships real lists.
    f2p = as_list(row["FAIL_TO_PASS"])
    p2p = as_list(row["PASS_TO_PASS"])
    return {
        "benchmark": "swe-bench-live",
        "instance_id": row["instance_id"],
        "repo": repo,
        "base_commit": row["base_commit"],
        "problem_statement": row["problem_statement"],
        "test_patch": row["test_patch"],
        "fail_to_pass": f2p,
        "pass_to_pass": p2p,
        "src_dir": SRC_DIR[repo],
        "test_files": test_files(f2p + p2p),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="easiest instances, don't write")
    ap.add_argument("--only", action="append", help="instance id to vendor (repeatable)")
    ap.add_argument("--out", default="evals/swebench/live-subset.json")
    args = ap.parse_args()

    rows = fetch_all()

    if args.list:
        rows.sort(key=difficulty)
        print(f"{'instance_id':46} {'files/hunks/lines':18} f2p  p2p  mapped")
        for r in rows[:25]:
            d = r["difficulty"]
            mapped = "yes" if r["repo"] in SRC_DIR else "-"
            print(
                f"{r['instance_id']:46} "
                f"{d['files']}/{d['hunks']}/{d['lines']:<12} "
                f"{len(as_list(r['FAIL_TO_PASS'])):3}  "
                f"{len(as_list(r['PASS_TO_PASS'])):4} {mapped}"
            )
        return 0

    if not args.only:
        raise SystemExit("--only <instance_id> is required (see --list)")

    by_id = {r["instance_id"]: r for r in rows}
    missing = [i for i in args.only if i not in by_id]
    if missing:
        raise SystemExit(f"not in SWE-bench-Live lite: {missing}")

    subset = {
        "source": "SWE-bench-Live/SWE-bench-Live",
        "split": "lite",
        "total_in_split": len(rows),
        "note": (
            "A hand-picked subset of SWE-bench-Live, NOT the whole split, and a "
            "DIFFERENT benchmark from SWE-bench Lite. Scores are not comparable to "
            "either full split."
        ),
        "instances": [convert(by_id[i]) for i in args.only],
    }

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(subset, indent=2) + "\n", encoding="utf-8", newline="\n")
    print(f"wrote {out} — {len(subset['instances'])} of {len(rows)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
