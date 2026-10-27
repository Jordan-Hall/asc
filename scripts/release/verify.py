#!/usr/bin/env python3
"""Verify an asc release directory: checksums, manifest, archive contents and signatures.

    verify.py <release dir> [--pubkey <minisign public key text>] [--allow-partial]

Checks, each fatal:
  * SHA256SUMS lists exactly the files in the directory (all but itself and *.minisig), once each,
    with the right hash, and every name is one plain path component per level;
  * manifest.json has the documented shape, its version equals the directory's, every entry names an
    existing file with the recorded sha256 and size, and the `cli` entries cover every target and
    archive format the release promises (unless --allow-partial, for tests of single files);
  * every archive is flat, holds the executable and nothing unexpected, and the executable is
    executable in tar.gz and zip;
  * with --pubkey, every file has a `.minisig` that minisign accepts and whose trusted comment names
    `file:<that file's name>` and `version:<the release version>`. minisign is required then.

Exit status 0 means the directory is a complete, consistent, signed release.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import assemble  # noqa: E402

SHA_LINE = re.compile(r"^([0-9a-f]{64})  ([A-Za-z0-9._-]+)$")
KINDS = {"cli", "vscode", "jetbrains", "sbom", "homebrew", "scoop", "winget", "nfpm", "aur"}


class VerifyError(Exception):
    pass


def check_sums(root: Path) -> dict[str, str]:
    sums_path = root / "SHA256SUMS"
    if not sums_path.is_file():
        raise VerifyError("SHA256SUMS is missing")
    listed: dict[str, str] = {}
    for n, line in enumerate(sums_path.read_text(encoding="utf-8").splitlines(), 1):
        m = SHA_LINE.match(line)
        if not m:
            raise VerifyError(f"SHA256SUMS line {n} is not '<sha256>  <name>'")
        digest, name = m.groups()
        if name in (".", "..") or not assemble.PLAIN_NAME_RE.match(name):
            raise VerifyError(f"SHA256SUMS line {n}: unsafe name {name!r}")
        if name in listed:
            raise VerifyError(f"SHA256SUMS lists {name} twice")
        listed[name] = digest
    present = {p.name: p for p in assemble.release_files(root)}
    if set(listed) != set(present):
        missing = sorted(set(listed) - set(present))
        extra = sorted(set(present) - set(listed))
        raise VerifyError(f"SHA256SUMS and the directory differ: missing {missing}, unlisted {extra}")
    for name, digest in listed.items():
        if assemble.sha256_of(present[name]) != digest:
            raise VerifyError(f"{name} does not match its SHA256SUMS entry")
    return listed


def check_manifest(root: Path, sums: dict[str, str], allow_partial: bool) -> dict:
    path = root / "manifest.json"
    if not path.is_file():
        raise VerifyError("manifest.json is missing")
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except ValueError as e:
        raise VerifyError(f"manifest.json is not JSON: {e}") from e
    if not isinstance(doc, dict) or doc.get("schema") != assemble.SCHEMA or doc.get("name") != "asc":
        raise VerifyError("manifest.json has the wrong schema or name")
    version = doc.get("version")
    if not isinstance(version, str) or not assemble.VERSION_RE.match(version):
        raise VerifyError("manifest.json has a bad version")
    if not isinstance(doc.get("commit"), str) or not assemble.COMMIT_RE.match(doc["commit"]):
        raise VerifyError("manifest.json has a bad commit")
    files = doc.get("files")
    if not isinstance(files, list) or not files:
        raise VerifyError("manifest.json lists no files")
    seen = set()
    need = {"version", "kind", "os", "arch", "target", "format", "file", "sha256", "size", "signature"}
    for e in files:
        if not isinstance(e, dict) or set(e) != need:
            raise VerifyError(f"manifest entry has the wrong keys: {e!r}")
        if e["version"] != version or e["kind"] not in KINDS:
            raise VerifyError(f"manifest entry {e['file']!r}: bad version or kind")
        name = e["file"]
        if name in seen:
            raise VerifyError(f"manifest lists {name} twice")
        seen.add(name)
        if name not in sums:
            raise VerifyError(f"manifest names {name}, which SHA256SUMS does not list")
        if e["sha256"] != sums[name]:
            raise VerifyError(f"manifest hash of {name} differs from SHA256SUMS")
        if (root / name).stat().st_size != e["size"]:
            raise VerifyError(f"manifest size of {name} is wrong")
        if e["signature"] != name + ".minisig":
            raise VerifyError(f"manifest signature name of {name} is wrong")
    if "manifest.json" in seen or "SHA256SUMS" in seen:
        raise VerifyError("the manifest must not list itself or SHA256SUMS")
    if not allow_partial:
        want = {
            f"asc-{version}-{t}.{fmt}"
            for t, (_o, _a, fmts, _x) in assemble.TARGETS.items()
            for fmt in fmts
        }
        have = {e["file"] for e in files if e["kind"] == "cli"}
        if have != want:
            raise VerifyError(
                f"the cli entries are not the promised set: missing {sorted(want - have)}, extra {sorted(have - want)}"
            )
        if f"asc-{version}.cdx.json" not in seen:
            raise VerifyError("the SBOM is not in the manifest")
    return doc


def check_archive(root: Path, entry: dict) -> None:
    target = entry["target"]
    if target not in assemble.TARGETS:
        raise VerifyError(f"{entry['file']}: unknown target {target!r}")
    os_name, arch, _fmts, exe = assemble.TARGETS[target]
    if (entry["os"], entry["arch"]) != (os_name, arch):
        raise VerifyError(f"{entry['file']}: os/arch do not match the target")
    path = root / entry["file"]
    allowed = {exe, *assemble.LICENSE_NAMES.values()}
    if entry["format"] == "tar.gz":
        with tarfile.open(path, "r:gz") as tar:
            infos = tar.getmembers()
            names = [i.name for i in infos]
            if any(not i.isreg() for i in infos):
                raise VerifyError(f"{path.name}: a member is not a regular file")
            exe_mode = next((i.mode for i in infos if i.name == exe), None)
    elif entry["format"] == "zip":
        with zipfile.ZipFile(path) as z:
            infos = z.infolist()
            names = [i.filename for i in infos]
            if any(i.is_dir() or stat.S_ISLNK(i.external_attr >> 16) for i in infos):
                raise VerifyError(f"{path.name}: a member is not a regular file")
            exe_mode = next((i.external_attr >> 16 & 0o7777 for i in infos if i.filename == exe), None)
    else:
        raise VerifyError(f"{path.name}: unknown format {entry['format']!r}")
    if len(set(names)) != len(names) or not set(names) <= allowed:
        raise VerifyError(f"{path.name}: unexpected members {sorted(set(names) - allowed)} or duplicates")
    if exe_mode is None:
        raise VerifyError(f"{path.name}: has no {exe}")
    if not exe_mode & 0o100:
        raise VerifyError(f"{path.name}: {exe} is not executable")


def trusted_comment(sig_path: Path) -> str:
    for line in sig_path.read_text(encoding="utf-8").splitlines():
        if line.startswith("trusted comment: "):
            return line[len("trusted comment: ") :]
    raise VerifyError(f"{sig_path.name}: no trusted comment")


def check_signatures(root: Path, version: str, pubkey: str) -> None:
    minisign = shutil.which("minisign")
    if minisign is None:
        raise VerifyError("minisign is required to verify signatures and was not found")
    names = [p.name for p in assemble.release_files(root)] + ["SHA256SUMS"]
    for name in names:
        sig = root / (name + ".minisig")
        if not sig.is_file():
            raise VerifyError(f"{name} has no signature ({sig.name})")
        done = subprocess.run(
            [minisign, "-V", "-q", "-P", pubkey, "-x", str(sig), "-m", str(root / name)],
            capture_output=True,
            text=True,
            stdin=subprocess.DEVNULL,
        )
        if done.returncode != 0:
            raise VerifyError(f"signature check failed for {name}")
        tokens = trusted_comment(sig).replace("\t", " ").split(" ")
        if f"file:{name}" not in tokens or f"version:{version}" not in tokens:
            raise VerifyError(f"the signature of {name} is for another file or version")


def verify(root: Path, pubkey: str | None, allow_partial: bool) -> dict:
    if not root.is_dir():
        raise VerifyError(f"{root} is not a directory")
    sums = check_sums(root)
    doc = check_manifest(root, sums, allow_partial)
    for e in doc["files"]:
        if e["kind"] == "cli":
            check_archive(root, e)
        elif e["kind"] == "sbom":
            sbom = json.loads((root / e["file"]).read_text(encoding="utf-8"))
            if sbom.get("bomFormat") != "CycloneDX":
                raise VerifyError("the SBOM is not CycloneDX")
    if pubkey:
        check_signatures(root, doc["version"], pubkey)
    return doc


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dir")
    ap.add_argument("--pubkey")
    ap.add_argument("--allow-partial", action="store_true")
    args = ap.parse_args(argv)
    try:
        doc = verify(Path(args.dir), args.pubkey, args.allow_partial)
    except (VerifyError, OSError, tarfile.TarError, zipfile.BadZipFile) as e:
        print(f"verify: {e}", file=sys.stderr)
        return 1
    signed = "signed" if args.pubkey else "unsigned check"
    print(f"asc {doc['version']}: {len(doc['files'])} files verified ({signed})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
