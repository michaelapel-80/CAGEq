#!/usr/bin/env python3
"""Merge cargo-about / license-checker / pip-licenses output into one
THIRD_PARTY_LICENSES.txt. Regenerate whenever a dependency changes (Rust,
npm, or the sidecar's requirements.txt) — don't hand-edit the output file.

Run from the repo root, in order:

    # 1. Rust — two separate cargo workspaces, so two separate reports.
    #    about.toml's `accepted` list may need a new SPDX id if a fresh
    #    dependency uses one cargo-about hasn't seen yet; it refuses to run
    #    (rather than silently omit anything) until you add it there.
    cargo install cargo-about --features cli   # once
    cargo about generate --format json -o rust_licenses_root.json
    (cd cageq-app/src-tauri && cargo about generate --format json \\
        --config ../../about.toml -o ../../rust_licenses_tauri.json)

    # 2. npm — production dependencies only; --production doesn't perfectly
    #    exclude devDependencies on a hoisted tree, hence NPM_SHIPPED below
    #    as the authoritative cross-check against package.json.
    (cd cageq-app && npx --yes license-checker --production --json \\
        --out ../npm_licenses.json)

    # 3. Python — the sidecar's own venv, not the system Python.
    cd cageq-sidecar && .venv/Scripts/python -m pip install pip-licenses
    .venv/Scripts/python -m piplicenses --format=json --with-license-file \\
        --no-license-path --with-urls > ../python_licenses.json
    cd ..

    # 4. Merge.
    python scripts/gen_third_party_licenses.py

The four intermediate *_licenses*.json files are gitignored — regenerate
them fresh each time rather than trusting stale ones.
"""
import json
import io

REPO = "."

OWN_RUST_CRATES = {
    "cageq-apo", "cageq-apo-backend", "cageq-backend", "cageq-config-writer",
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

# PyInstaller's own build-time dependencies (Windows PE analysis, packaging
# metadata, etc.) — needed to *produce* the frozen sidecar.exe, not present or
# executing inside it. PyInstaller itself stays (its bootloader is compiled
# into the output — see the note attached to it below).
PY_BUILD_ONLY = {"altgraph", "pefile", "pyinstaller-hooks-contrib", "pywin32-ctypes"}

PYINSTALLER_NOTE = (
    "PyInstaller's bootloader (not its build-time tooling) is compiled into "
    "cageq-sidecar.exe. PyInstaller is GPLv2-or-later WITH a bootloader "
    "exception that explicitly permits this for a program under any license, "
    "commercial or otherwise, without requiring that program itself be GPL."
)


def load(name):
    with io.open(name, encoding="utf-8") as f:
        return json.load(f)


def rust_section():
    root = load("rust_licenses_root.json")
    tauri = load("rust_licenses_tauri.json")

    # key: exact license text -> {"id": spdx-ish name, "crates": {(name,version)}}
    groups = {}
    for report in (root, tauri):
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
                with io.open(license_file, encoding="utf-8", errors="replace") as f:
                    text = f.read().strip()
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


def python_section():
    data = load("python_licenses.json")
    out = []
    out.append("=" * 80)
    out.append("PYTHON — frozen into the DSP sidecar executable (PyInstaller)")
    out.append("=" * 80)
    out.append("")

    groups = {}
    for pkg in data:
        name = pkg["Name"]
        if name in PY_BUILD_ONLY:
            continue
        text = pkg.get("LicenseText") or ""
        key = text if text.strip() else f"__nolicensetext__:{pkg['License']}"
        g = groups.setdefault(key, {"license": pkg["License"], "text": text, "pkgs": []})
        g["pkgs"].append((name, pkg["Version"], pkg.get("URL", "")))

    for key, g in sorted(groups.items(), key=lambda kv: kv[1]["license"]):
        out.append("-" * 80)
        out.append(f"License: {g['license']}")
        out.append("-" * 80)
        out.append("")
        for name, version, url in sorted(g["pkgs"]):
            line = f"  {name} {version}"
            if url and url != "UNKNOWN":
                line += f" — {url}"
            out.append(line)
        out.append("")
        if g["pkgs"][0][0] == "pyinstaller":
            out.append(PYINSTALLER_NOTE)
        elif g["text"].strip():
            out.append(g["text"].strip())
        else:
            out.append(f"(No bundled license file; standard {g['license']} text applies.)")
        out.append("")
    return "\n".join(out)


HEADER = """\
THIRD-PARTY SOFTWARE NOTICES AND INFORMATION
=============================================

CAGEq's own code is licensed under the GNU General Public License v3.0 or
later (see LICENSE). This file lists the third-party software actually
bundled inside the *built* application — compiled into the Rust binaries,
bundled into the frontend's JavaScript build, or frozen into the Python DSP
sidecar — as distinct from tooling that only runs at build time and never
ships (TypeScript, Vite, cargo itself, PyInstaller's own build-time
dependencies, etc.). None of it is GPL-incompatible; every entry below is
under a permissive license (MIT, BSD, Apache-2.0, and similar).

Generated, not hand-written — see scripts/gen_third_party_licenses.py for
the exact commands (cargo-about for Rust, license-checker for npm,
pip-licenses for the Python sidecar) and regenerate with that script rather
than editing this file directly.
"""


def main():
    parts = [HEADER, rust_section(), npm_section(), python_section()]
    with io.open("THIRD_PARTY_LICENSES.txt", "w", encoding="utf-8") as f:
        f.write("\n\n".join(parts))
    print("wrote THIRD_PARTY_LICENSES.txt")


if __name__ == "__main__":
    main()
