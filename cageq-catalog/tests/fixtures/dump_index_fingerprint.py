#!/usr/bin/env python3
"""Ground-truth dump for `cageq_catalog::index` against the live sidecar's own
`build_index`/`list_targets` — both hit the real GitHub API, so this is a genuine
old-vs-new comparison, not a mock.

Run with the sidecar's own venv (from the CAGE repo root, `cageq-catalog` is a sibling
of `cageq-sidecar`):

    cageq-sidecar\\.venv\\Scripts\\python cageq-catalog\\tests\\fixtures\\dump_index_fingerprint.py

Uses a throwaway cache dir (`CAGEQ_CACHE_DIR`) so this always does a real, fresh build
rather than reading whatever's already cached on this machine.
"""
import json
import os
import sys
import tempfile
from pathlib import Path

cache_dir = Path(tempfile.mkdtemp(prefix="cageq-index-dump-"))
os.environ["CAGEQ_CACHE_DIR"] = str(cache_dir)

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "cageq-sidecar" / "python"))
import sidecar_dsp  # noqa: E402

out_dir = Path(__file__).parent

print("building headphone index (this hits the real GitHub API, may take a while)...")
headphones = sidecar_dsp.build_index(refresh=True)
(out_dir / "headphones_fingerprint.json").write_text(json.dumps(headphones))
print(f"  {len(headphones)} headphones")

print("listing targets...")
targets = sidecar_dsp.list_targets(refresh=True)
(out_dir / "targets_index_fingerprint.json").write_text(json.dumps(targets))
print(f"  {len(targets)} targets")
