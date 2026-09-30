#!/usr/bin/env python3
"""Collect the licence texts of every crate compiled into the release binaries.

    scripts/third-party-licenses.py WORKSPACE:PACKAGE [WORKSPACE:PACKAGE ...]

Prints one text file to stdout: a table of the crates and their licences, then
each distinct licence text once, with the crates it covers. A .deb is a binary
redistribution, and MIT, Apache-2.0 and the BSD licences all ask that their
notices travel with the binary, not only with the source.

Only normal dependencies are followed -- dev- and build-dependencies do not
end up in the binary -- for the platform this machine builds for. Crates with
no registry source are the galdeck workspaces' own, under this repository's
licence, and are left out.
"""
import hashlib
import json
import os
import subprocess
import sys
import textwrap

LICENCE_PREFIXES = ("license", "licence", "copying", "copyright", "notice", "unlicense")


def host_triple():
    out = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    return next(line.split(": ", 1)[1] for line in out.splitlines() if line.startswith("host: "))


def dependencies(workspace, package, target):
    """Every package `package` pulls into its binary, found from cargo's own resolve."""
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target],
            cwd=workspace,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(
        p["id"] for p in meta["packages"] if p["name"] == package and p["source"] is None
    )
    seen, stack = set(), [root]
    while stack:
        pkg = stack.pop()
        if pkg in seen:
            continue
        seen.add(pkg)
        for dep in nodes[pkg]["deps"]:
            # A null kind is a normal dependency.
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    return [packages[pkg] for pkg in seen if packages[pkg]["source"] is not None]


def licence_texts(package):
    root = os.path.dirname(package["manifest_path"])
    names = sorted(
        name
        for name in os.listdir(root)
        if name.lower().startswith(LICENCE_PREFIXES) and os.path.isfile(os.path.join(root, name))
    )
    extra = package.get("license_file")
    if extra and os.path.basename(extra) not in names and os.path.isfile(os.path.join(root, extra)):
        names.append(extra)
    texts = []
    for name in names:
        with open(os.path.join(root, name), encoding="utf-8", errors="replace") as f:
            texts.append(f.read().strip() + "\n")
    return texts


def main(args):
    if not args or any(":" not in arg for arg in args):
        sys.exit(__doc__.strip())
    target = host_triple()
    crates = {}
    for arg in args:
        workspace, package = arg.rsplit(":", 1)
        for p in dependencies(workspace, package, target):
            crates[(p["name"], p["version"])] = p

    by_text = {}
    missing = []
    for key in sorted(crates):
        texts = licence_texts(crates[key])
        if not texts:
            missing.append(key)
        for text in texts:
            digest = hashlib.sha256(text.encode()).hexdigest()
            by_text.setdefault(digest, (text, []))[1].append(key)

    rule = "-" * 78
    out = sys.stdout
    out.write("Third-party licences\n====================\n\n")
    out.write(
        "The binaries in this package are built from the crates below, each under\n"
        "the licence it declares, and from the Rust standard library (MIT OR\n"
        "Apache-2.0). Where a crate offers a choice of licences, it is used here\n"
        "under any one of them; the texts follow as each crate ships them.\n\n"
    )
    width = max(len(f"{n} {v}") for n, v in crates)
    for name, version in sorted(crates):
        licence = crates[(name, version)].get("license") or "see its licence file"
        out.write(f"  {f'{name} {version}':<{width}}  {licence}\n")
    if missing:
        out.write(
            "\nThese crates ship no licence file; the licence they declare above is\n"
            "the standard text of that licence:\n\n"
        )
        for name, version in missing:
            out.write(f"  {name} {version}\n")
    for text, users in sorted(by_text.values(), key=lambda entry: entry[1]):
        out.write(f"\n{rule}\n")
        used_by = "Used by: " + ", ".join(f"{n} {v}" for n, v in users)
        out.write(textwrap.fill(used_by, 78, subsequent_indent="  ", break_on_hyphens=False) + "\n")
        out.write(f"{rule}\n\n{text}")


if __name__ == "__main__":
    main(sys.argv[1:])
