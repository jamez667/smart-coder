#!/usr/bin/env python3
"""Vendor a SWE-bench Lite subset into evals/swebench/lite-subset.json.

Run deliberately, not at eval time. The subset is COMMITTED so a score is tied to a
commit: a run must be reproducible offline, reviewable in a diff, and unaffected by
the upstream dataset changing under it.

    python scripts/fetch-swebench-subset.py            # the default subset
    python scripts/fetch-swebench-subset.py --list     # what is available, by weight

Stdlib only, so it needs no environment of its own.
"""

from __future__ import annotations

import argparse
import collections
import json
import pathlib
import sys
import urllib.request

API = (
    "https://datasets-server.huggingface.co/rows"
    "?dataset=princeton-nlp%2FSWE-bench_Lite&config=default&split=test"
    "&offset={off}&length=100"
)

# The source subtree handed to the agent, per repo.
#
# Only this directory is copied out of the container, which is what keeps the test
# tree off the host: the agent physically cannot edit a test, rather than being asked
# not to. It also dodges `docker cp` failing on Windows for the two symlinks under
# pylint's `tests/functional/s/symlink/` ("a required privilege is not held").
SRC_DIR = {
    "psf/requests": "requests",
    "pallets/flask": "src/flask",
    "pylint-dev/pylint": "pylint",
    "pytest-dev/pytest": "src/_pytest",
}

# Lightweight pure-Python repos. django (114) and sympy (77) dominate Lite but drag in
# heavy environments; matplotlib and scikit-learn pull compiled numeric stacks. Start
# where the environment is least likely to be what is actually being measured.
DEFAULT_SUBSET = [
    "pylint-dev__pylint-6506",
    "pylint-dev__pylint-5859",
    "pylint-dev__pylint-7993",
    "pylint-dev__pylint-7228",
    "psf__requests-863",
    "psf__requests-3362",
    "psf__requests-1963",
    "pallets__flask-4992",
    "pallets__flask-4045",
    "pallets__flask-5063",
    "pytest-dev__pytest-7220",
    "pytest-dev__pytest-5227",
    "pytest-dev__pytest-8365",
    "pytest-dev__pytest-6116",
    "pytest-dev__pytest-11143",
]


def fetch_all() -> list[dict]:
    rows = []
    for off in range(0, 300, 100):
        with urllib.request.urlopen(API.format(off=off), timeout=120) as r:
            rows.extend(x["row"] for x in json.load(r)["rows"])
    return rows


def clean_ids(node_ids: list[str]) -> list[str]:
    """Drop entries that are not pytest node ids.

    Some upstream rows carry pytest *progress output* in FAIL_TO_PASS/PASS_TO_PASS —
    `pytest-dev__pytest-5227` has `"["`, `"[100%]"` and
    `"[100%]------------------------------"` among its 34 PASS_TO_PASS entries. Scoring
    counts any id it cannot find as `missing`, so three lines of terminal noise make an
    otherwise-fine instance permanently unresolvable.
    """
    return [
        n
        for n in node_ids
        if "::" in n
        and not n.lstrip().startswith("[")
        # Balanced brackets. A parametrised id whose parameter contains a SPACE was
        # split on whitespace upstream, leaving fragments like
        # `test_locate_app[cliapp.factory-` and `create_app2("foo",`. pytest ABORTS on
        # an unknown id -- "no tests ran" -- so one fragment zeroes the whole instance,
        # which is how flask-5063 and pytest-11143 reported all 56 / all 12 tests
        # MISSING. They cannot be reassembled reliably, so drop them.
        and n.count("[") == n.count("]")
    ]


def test_files(node_ids: list[str]) -> list[str]:
    """The distinct files named by a set of pytest node ids."""
    return sorted({n.split("::", 1)[0] for n in clean_ids(node_ids)})


def convert(row: dict) -> dict:
    f2p = clean_ids(json.loads(row["FAIL_TO_PASS"]))
    p2p = clean_ids(json.loads(row["PASS_TO_PASS"]))
    repo = row["repo"]
    if repo not in SRC_DIR:
        raise SystemExit(f"no src_dir mapping for {repo} — add one to SRC_DIR")
    return {
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
    ap.add_argument("--list", action="store_true", help="show candidates, not write")
    ap.add_argument("--out", default="evals/swebench/lite-subset.json")
    args = ap.parse_args()

    rows = fetch_all()

    if args.list:
        by_repo = collections.Counter(r["repo"] for r in rows)
        for repo, n in by_repo.most_common():
            mark = "light" if repo in SRC_DIR else "heavy"
            print(f"{n:4d}  {mark:6}  {repo}")
        return 0

    by_id = {r["instance_id"]: r for r in rows}
    missing = [i for i in DEFAULT_SUBSET if i not in by_id]
    if missing:
        raise SystemExit(f"not in SWE-bench Lite: {missing}")

    subset = {
        "source": "princeton-nlp/SWE-bench_Lite",
        "split": "test",
        "total_in_split": len(rows),
        "note": (
            "A hand-picked subset of light pure-Python instances, NOT all of Lite. "
            "Scores from it are not comparable to published SWE-bench Lite numbers."
        ),
        "instances": [convert(by_id[i]) for i in DEFAULT_SUBSET],
    }

    out = pathlib.Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(subset, indent=2) + "\n", encoding="utf-8", newline="\n")
    print(f"wrote {out} — {len(subset['instances'])} instances of {len(rows)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
