#!/usr/bin/env python3
"""Merge cargo-about / license-checker output, plus NLopt's own license files, into one
THIRD_PARTY_LICENSES.txt. Regenerate whenever a dependency changes (Rust or npm) — don't
hand-edit the output file.

Run from the repo root, in order:

    # 1. Rust — one report per shipped binary's crate. about.toml scopes every run to the
    #    Windows target and drops dev-/build-dependencies; its `accepted` list may need a new
    #    SPDX id if a fresh dependency uses one it hasn't seen yet — cargo-about refuses to run
    #    (rather than silently omit anything) until you review it and add it there.
    #    cageq-app/src-tauri is its own workspace, so it gets its own run; the other two are
    #    scoped with -m (without --workspace) so dev-only root members (cageq-sidecar,
    #    cageq-watchdog — test tooling since the Rust port) never leak in.
    cargo install cargo-about --features cli   # once
    cargo about generate --format json -c about.toml -m cageq-apo/Cargo.toml \\
        -o rust_licenses_apo.json
    cargo about generate --format json -c about.toml -m cageq-apo-backend/Cargo.toml \\
        -o rust_licenses_apo_backend.json
    (cd cageq-app/src-tauri && cargo about generate --format json \\
        -c ../../about.toml -o ../../rust_licenses_tauri.json)

    # 2. npm — production dependencies only; --production doesn't perfectly
    #    exclude devDependencies on a hoisted tree, hence NPM_SHIPPED below
    #    as the authoritative cross-check against package.json.
    (cd cageq-app && npx --yes license-checker --production --json \\
        --out ../npm_licenses.json)

    # 3. Merge. Also reads NLopt's own license files straight from the `nlopt` crate's source
    #    (located via `cargo metadata`, so it needs the crate downloaded — any build does that)
    #    and the vendored licenses/LGPL-2.1.txt.
    python scripts/gen_third_party_licenses.py

The *_licenses*.json intermediates are gitignored — regenerate them fresh each time rather
than trusting stale ones.
"""
import io
import json
import os
import subprocess

RUST_REPORTS = ["rust_licenses_apo.json", "rust_licenses_apo_backend.json", "rust_licenses_tauri.json"]

# CAGEq's own GPL-3.0-or-later crates — not third-party, so left out of the listing.
# cageq-peq-solver is deliberately NOT here: it's MIT, a port of AutoEq's fitting code that
# carries AutoEq's own copyright notice, which the MIT terms require reproducing.
OWN_RUST_CRATES = {
    "cageq-apo", "cageq-apo-backend", "cageq-backend", "cageq-catalog", "cageq-config-writer",
    "cageq-sidecar", "cageq-watchdog", "cageq-core", "cageq-monitor", "cageq-app",
}

# Only what's actually bundled into the shipped frontend JS — devDependencies
# (TypeScript, Vite, etc.) never leave the build step. license-checker's
# --production flag doesn't perfectly separate these on a hoisted tree, so this
# is the authoritative allow-list, cross-checked by hand against package.json.
NPM_SHIPPED = {
    "@babel/runtime", "@tauri-apps/api", "@tauri-apps/plugin-opener",
    "html-parse-stringify", "i18next", "react", "react-dom", "react-i18next",
    "scheduler", "use-sync-external-store",
}

# NLopt algorithms whose own license file applies only if NLopt's C++ code is compiled. The
# `nlopt` crate's build.rs turns NLOPT_CXX off (checked below, not assumed), which drops both.
NLOPT_CXX_ONLY_ALGS = {"stogo", "ags"}


def load(name):
    with io.open(name, encoding="utf-8") as f:
        return json.load(f)


def read(path):
    with io.open(path, encoding="utf-8", errors="replace") as f:
        return f.read().strip()


def rust_section():
    # key: exact license text -> {"name": spdx-ish name, "crates": {(name, version)}}
    groups = {}
    for report in map(load, RUST_REPORTS):
        for lic in report["licenses"]:
            text = lic["text"]
            g = groups.setdefault(text, {"name": lic["name"], "crates": set()})
            for u in lic["used_by"]:
                pkg = u["crate"]
                if pkg["name"] in OWN_RUST_CRATES:
                    continue
                g["crates"].add((pkg["name"], pkg["version"]))

    out = []
    out.append("=" * 80)
    out.append("RUST — compiled into cageq-app.exe, CAGEqApo.dll, cageq-apo-setup.exe")
    out.append("=" * 80)
    out.append("")
    # Sort groups by license name for stable, readable output.
    for text, g in sorted(groups.items(), key=lambda kv: (kv[1]["name"], kv[0])):
        if not g["crates"]:
            continue
        out.append("-" * 80)
        out.append(f"License: {g['name']}")
        out.append("-" * 80)
        out.append("")
        crate_list = ", ".join(f"{n} {v}" for n, v in sorted(g["crates"]))
        out.append(f"Used by: {crate_list}")
        out.append("")
        out.append(text.strip())
        out.append("")
    return "\n".join(out)


def npm_section():
    data = load("npm_licenses.json")
    out = []
    out.append("=" * 80)
    out.append("JAVASCRIPT — bundled into the frontend build")
    out.append("=" * 80)
    out.append("")
    # Same exact-text grouping as the Rust section, not just grouping by SPDX id —
    # a same-named license can still carry a different copyright notice per package.
    groups = {}
    for key, info in data.items():
        name = key.rsplit("@", 1)[0]
        if name not in NPM_SHIPPED:
            continue
        version = key.rsplit("@", 1)[1]
        lic = info.get("licenses", "UNKNOWN")
        license_file = info.get("licenseFile")
        text = ""
        if license_file:
            try:
                text = read(license_file)
            except OSError:
                text = ""
        key2 = text if text else f"__notext__:{lic}"
        g = groups.setdefault(key2, {"license": lic, "text": text, "pkgs": []})
        g["pkgs"].append((name, version, info.get("repository", "")))

    for key2, g in sorted(groups.items(), key=lambda kv: kv[1]["license"]):
        out.append("-" * 80)
        out.append(f"License: {g['license']}")
        out.append("-" * 80)
        out.append("")
        for name, version, repo in sorted(g["pkgs"]):
            line = f"  {name} {version}"
            if repo:
                line += f" — {repo}"
            out.append(line)
        out.append("")
        out.append(g["text"] if g["text"] else f"(No bundled license file; standard {g['license']} text applies.)")
        out.append("")
    return "\n".join(out)


def nlopt_source():
    """The NLopt C sources the `nlopt` crate vendors and builds, found via cargo metadata."""
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1"],
        cwd=os.path.join("cageq-app", "src-tauri"),
    ))
    pkgs = [p for p in meta["packages"] if p["name"] == "nlopt"]
    if len(pkgs) != 1:
        raise SystemExit(f"expected exactly one `nlopt` crate in the app's dependency graph, found {len(pkgs)}")
    crate_dir = os.path.dirname(pkgs[0]["manifest_path"])
    src = [d for d in os.listdir(crate_dir) if d.startswith("nlopt-") and os.path.isdir(os.path.join(crate_dir, d))]
    if len(src) != 1:
        raise SystemExit(f"expected one vendored nlopt-* source dir in {crate_dir}, found {src}")
    return pkgs[0]["version"], crate_dir, os.path.join(crate_dir, src[0])


def nlopt_section():
    crate_version, crate_dir, src = nlopt_source()
    nlopt_version = os.path.basename(src).split("-", 1)[1]
    build_rs = read(os.path.join(crate_dir, "build.rs"))
    # What gets compiled is decided by the crate's build.rs, so read it rather than assume it:
    # a future crate version that switches C++ on or Luksan off changes both what's listed here
    # and which license governs the combined library.
    cxx_off = '"NLOPT_CXX", "OFF"' in build_rs
    luksan_off = '"NLOPT_LUKSAN", "OFF"' in build_rs
    if not cxx_off:
        raise SystemExit("nlopt's build.rs no longer turns NLOPT_CXX off — revisit NLOPT_CXX_ONLY_ALGS")
    if luksan_off:
        raise SystemExit("nlopt's build.rs now turns NLOPT_LUKSAN off — NLopt is then MIT, not LGPL; update this section")

    out = []
    out.append("=" * 80)
    out.append("C — compiled into cageq-app.exe (statically linked by the `nlopt` Rust crate)")
    out.append("=" * 80)
    out.append("")
    out.append("-" * 80)
    out.append("License: GNU Lesser General Public License v2.1 or later (combined library);")
    out.append("         MIT and other permissive terms for the individual algorithms")
    out.append("-" * 80)
    out.append("")
    out.append(f"  NLopt {nlopt_version} (via the nlopt crate {crate_version}) — https://github.com/stevengj/nlopt")
    out.append("")
    out.append(
        "Used for the SLSQP parametric-EQ fit. The `nlopt` crate builds NLopt from source with its\n"
        "default algorithm set, which includes the LGPL-licensed Luksan solvers, so — per NLopt's own\n"
        "COPYING below — the compiled library as a whole is governed by the GNU LGPL v2.1 or later.\n"
        "CAGEq's complete source code is available under the GPL v3.0 or later, so the application\n"
        "can be rebuilt against a modified NLopt as the LGPL requires."
    )
    out.append("")
    out.append("NLopt's COPYING:")
    out.append("")
    out.append(read(os.path.join(src, "COPYING")))
    out.append("")
    algs = os.path.join(src, "src", "algs")
    for alg in sorted(os.listdir(algs)):
        if alg in NLOPT_CXX_ONLY_ALGS:
            continue
        for fname in sorted(os.listdir(os.path.join(algs, alg))):
            if fname.upper().startswith(("COPYING", "COPYRIGHT", "LICENSE")):
                out.append(f"NLopt src/algs/{alg}/{fname}:")
                out.append("")
                out.append(read(os.path.join(algs, alg, fname)))
                out.append("")
                if alg == "luksan":
                    # The Luksan license asks that documentation of code using it cite the
                    # copyright, license and availability note above, and say "Used by permission."
                    out.append("The Luksan subroutines above are used by permission.")
                    out.append("")
    out.append("GNU Lesser General Public License v2.1 (full text):")
    out.append("")
    out.append(read(os.path.join("licenses", "LGPL-2.1.txt")))
    out.append("")
    return "\n".join(out)


HEADER = """\
THIRD-PARTY SOFTWARE NOTICES AND INFORMATION
=============================================

CAGEq's own code is licensed under the GNU General Public License v3.0 or
later (see LICENSE). This file lists the third-party software actually
bundled inside the *built* Windows application — compiled into its Rust
binaries (including NLopt's C library, statically linked through the `nlopt`
crate) or bundled into the frontend's JavaScript build — as distinct from
tooling that only runs at build time and never ships (TypeScript, Vite,
cargo itself, build scripts, etc.).

Almost all of it is under a permissive license (MIT, BSD, Apache-2.0, ISC and
similar). The exceptions are weak-copyleft, and all are compatible with the
GPL v3.0 or later: a few Rust crates under the Mozilla Public License 2.0,
and NLopt, which as compiled here is governed by the GNU Lesser General
Public License v2.1 or later (see its section).

Generated, not hand-written — see scripts/gen_third_party_licenses.py for
the exact commands (cargo-about for Rust, license-checker for npm, NLopt's
own license files for its section) and regenerate with that script rather
than editing this file directly.
"""


def main():
    parts = [HEADER, rust_section(), nlopt_section(), npm_section()]
    with io.open("THIRD_PARTY_LICENSES.txt", "w", encoding="utf-8") as f:
        f.write("\n\n".join(parts))
    print("wrote THIRD_PARTY_LICENSES.txt")


if __name__ == "__main__":
    main()
