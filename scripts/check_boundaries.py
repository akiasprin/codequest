#!/usr/bin/env python3
"""Check actual Cargo normal/build dependency edges, including transitive edges."""
import json
import subprocess

metadata = json.loads(subprocess.check_output([
    "cargo", "metadata", "--offline", "--format-version=1"
]))
packages = {p["id"]: p["name"] for p in metadata["packages"]}
nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
for owner, forbidden in {
    "cq-rustc-recorder": {"cq-core", "cq-store", "cq-app"},
    "cq-capture-protocol": {"cq-core", "cq-store", "cq-app", "cq-rustc-recorder"},
    "cq-core": {"cq-store", "cq-app", "cq-rustc-recorder"},
    "cq-store": {"cq-app", "cq-rustc-recorder"},
}.items():
    start = next(p for p, name in packages.items() if name == owner)
    seen, pending = set(), [start]
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        for dep in nodes[current]["deps"]:
            if any(k["kind"] in (None, "build") for k in dep["dep_kinds"]):
                pending.append(dep["pkg"])
    names = sorted(packages[p] for p in seen - {start})
    assert not forbidden.intersection(names), (owner, names)
    assert not any("ladybug" in n.lower() or "ra_ap_" in n for n in names), (owner, names)
    print(json.dumps({"crate": owner, "normal_and_build_transitive": names}))
