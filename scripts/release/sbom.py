#!/usr/bin/env python3
"""CycloneDX 1.5 SBOM of the `asc` executable, from Cargo.lock.

    sbom.py --cargo-lock Cargo.lock --version 1.2.3 --commit <sha> --epoch N > asc-1.2.3.cdx.json

The components are the crates reachable from the `asc` package through the lock file's dependency
lists, for every platform and feature (so the list is a superset of what any one target links).
Hashes are the registry checksums the lock file records. The output is deterministic: sorted, no
clock, and a serial number derived from the version and commit.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
import uuid
from pathlib import Path

ROOT_PACKAGE = "asc"
NAMESPACE = uuid.UUID("5c0f0b6e-3b0e-4c77-9d4a-0a5c0e7b1f21")


class SbomError(Exception):
    pass


def purl(name: str, version: str) -> str:
    return f"pkg:cargo/{name}@{version}"


def parse_ref(ref: str, by_name: dict[str, list[dict]]) -> dict:
    """A lock dependency: `name`, `name version` or `name version (source)`."""
    parts = ref.split(" ", 2)
    candidates = by_name.get(parts[0], [])
    if len(parts) >= 2:
        candidates = [c for c in candidates if c["version"] == parts[1]]
    if len(parts) == 3:
        source = parts[2].strip("()")
        candidates = [c for c in candidates if c.get("source") == source]
    if len(candidates) != 1:
        raise SbomError(f"cannot resolve the dependency {ref!r} in Cargo.lock")
    return candidates[0]


def reachable(lock: dict) -> tuple[list[dict], dict[str, list[str]]]:
    packages = lock.get("package", [])
    by_name: dict[str, list[dict]] = {}
    for p in packages:
        by_name.setdefault(p["name"], []).append(p)
    roots = by_name.get(ROOT_PACKAGE, [])
    if len(roots) != 1:
        raise SbomError(f"Cargo.lock has {len(roots)} packages named {ROOT_PACKAGE!r}, expected 1")
    seen: dict[tuple[str, str], dict] = {}
    edges: dict[str, list[str]] = {}
    stack = [roots[0]]
    while stack:
        pkg = stack.pop()
        key = (pkg["name"], pkg["version"])
        if key in seen:
            continue
        seen[key] = pkg
        deps = []
        for ref in pkg.get("dependencies", []):
            dep = parse_ref(ref, by_name)
            deps.append(purl(dep["name"], dep["version"]))
            stack.append(dep)
        edges[purl(*key)] = sorted(set(deps))
    return [seen[k] for k in sorted(seen)], edges


def render(lock_text: str, version: str, commit: str, epoch: int) -> str:
    import datetime

    lock = tomllib.loads(lock_text)
    pkgs, edges = reachable(lock)
    components = []
    for p in pkgs:
        if p["name"] == ROOT_PACKAGE:
            continue
        c = {
            "type": "library",
            "bom-ref": purl(p["name"], p["version"]),
            "name": p["name"],
            "version": p["version"],
            "purl": purl(p["name"], p["version"]),
            "scope": "required",
        }
        if "checksum" in p:
            c["hashes"] = [{"alg": "SHA-256", "content": p["checksum"]}]
        if "source" not in p:
            c["properties"] = [{"name": "asc:origin", "value": "workspace"}]
        components.append(c)
    # The release version names the root; the lock file's own version only has to resolve.
    root_ref = purl(ROOT_PACKAGE, version)
    lock_root = purl(ROOT_PACKAGE, next(p["version"] for p in pkgs if p["name"] == ROOT_PACKAGE))
    if lock_root != root_ref:
        edges[root_ref] = edges.pop(lock_root)
    doc = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": "urn:uuid:" + str(uuid.uuid5(NAMESPACE, f"{version}/{commit}")),
        "version": 1,
        "metadata": {
            "timestamp": datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            ),
            "tools": {"components": [{"type": "application", "name": "asc-release-sbom"}]},
            "component": {
                "type": "application",
                "bom-ref": root_ref,
                "name": ROOT_PACKAGE,
                "version": version,
                "purl": purl(ROOT_PACKAGE, version),
                "properties": [
                    {"name": "asc:commit", "value": commit},
                    {"name": "asc:lockfile-scope", "value": "all platforms and features"},
                ],
            },
        },
        "components": sorted(components, key=lambda c: c["bom-ref"]),
        "dependencies": [{"ref": r, "dependsOn": d} for r, d in sorted(edges.items())],
    }
    return json.dumps(doc, indent=2, sort_keys=True) + "\n"


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cargo-lock", required=True)
    ap.add_argument("--version", required=True)
    ap.add_argument("--commit", required=True)
    ap.add_argument("--epoch", type=int, required=True)
    args = ap.parse_args(argv)
    try:
        sys.stdout.write(
            render(Path(args.cargo_lock).read_text(encoding="utf-8"), args.version, args.commit, args.epoch)
        )
    except (SbomError, tomllib.TOMLDecodeError, OSError) as e:
        print(f"sbom: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
