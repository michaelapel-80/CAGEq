#!/usr/bin/env python3
"""Exhaustive ground-truth dump for `cageq-catalog::csv_parse` — runs the REAL
`autoeq.csv.parse_csv` against every single CSV file actually checked into AutoEq's
`measurements/` and `targets/` directories (not a sample), recording a compact
fingerprint per file rather than the full curve (thousands of files x hundreds of
points each would be a lot of data for no extra confidence: a wrong parse almost
certainly changes point count, endpoints, or the raw-value sum/sum-of-squares).

Run with the sidecar's own venv, given the AutoEq checkout's repo root:

    ..\..\cageq-sidecar\.venv\Scripts\python dump_csv_fingerprints.py <autoeq_repo_dir>
"""
import json
import math
import sys
from pathlib import Path

def fingerprint(repo_dir: Path, rel_dir: str, out_path: Path):
    root = repo_dir / rel_dir
    files = sorted(root.rglob("*.csv"))
    print(f"{rel_dir}: {len(files)} files")
    with open(out_path, "w", encoding="utf-8") as out:
        for i, path in enumerate(files):
            rel = str(path.relative_to(repo_dir)).replace("\\", "/")
            try:
                try:
                    text = path.read_text(encoding="utf-8")
                except UnicodeDecodeError:
                    text = path.read_text(encoding="windows-1252")
                data = autoeq_csv.parse_csv(text.strip())
                freq, raw = data["frequency"], data["raw"]
                n = len(freq)
                record = {
                    "path": rel,
                    "ok": True,
                    "n": n,
                    "f0": freq[0] if n else None,
                    "f_last": freq[-1] if n else None,
                    "raw_sum": math.fsum(raw),
                    "raw_sumsq": math.fsum(v * v for v in raw),
                }
            except Exception as e:
                record = {"path": rel, "ok": False, "error": str(e)}
            out.write(json.dumps(record) + "\n")
            if (i + 1) % 1000 == 0:
                print(f"  {i + 1}/{len(files)}")


def main():
    repo_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[4] / "GitHub" / "AutoEq"
    repo_dir = repo_dir.resolve()
    print(f"AutoEq repo: {repo_dir}")
    # Import the *local checkout's* autoeq.csv (inserted ahead of the venv's pip-
    # installed autoeq==4.1.2 on sys.path) so the fingerprint reflects the exact
    # parse_csv logic that actually processes the measurements/targets files being
    # fingerprinted, not whatever an older pinned package version happened to ship.
    sys.path.insert(0, str(repo_dir))
    global autoeq_csv
    import autoeq.csv as autoeq_csv

    out_dir = Path(__file__).parent
    fingerprint(repo_dir, "measurements", out_dir / "measurements_fingerprint.jsonl")
    fingerprint(repo_dir, "targets", out_dir / "targets_fingerprint.jsonl")


if __name__ == "__main__":
    main()
