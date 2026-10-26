#!/usr/bin/env python3
"""Assemble an asc release directory from built binaries and extension packages.

    assemble.py --version 1.2.3 --commit <sha> --epoch <unix secs> \
        --bins <dir> [--extras <dir>] --cargo-lock Cargo.lock \
        --license LICENSE-APACHE --license LICENSE-MIT \
        --release-url <url of the directory the files will be served from> \
        --homepage <url> --out <dir>

<bins> holds one directory per target triple, each with the stripped executable (`asc`, or
`asc.exe` for Windows): <bins>/x86_64-unknown-linux-gnu/asc. <extras> holds the VS Code and
JetBrains packages already named for the release (asc-vscode-<ver>.vsix, asc-jetbrains-<ver>.zip).

The output directory then holds, for every target, `asc-<ver>-<target>.zip` (what `asc upgrade` and
`asc self-update` install) and, for Unix targets, `asc-<ver>-<target>.tar.gz` (what install.sh
installs); the extension packages; `asc-<ver>.cdx.json` (CycloneDX SBOM); the rendered
Homebrew, winget (winget-asc.asc*.yaml) and Scoop manifests; `manifest.json`; and `SHA256SUMS`. Signing is a separate
step (sign.sh). Everything is deterministic for one (version, commit, epoch, inputs): archive
member order, modes, owners and timestamps are fixed, and gzip carries no name or time.

Nothing here reads the network or the environment except SOURCE_DATE_EPOCH-style input given
as --epoch, and nothing is written outside --out.
"""

from __future__ import annotations

import argparse
import datetime
import gzip
import hashlib
import io
import json
import os
import re
import stat
import sys
import tarfile
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sbom  # noqa: E402  (sibling module)

SCHEMA = 1
VERSION_RE = re.compile(r"^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\Z")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}\Z")
PLAIN_NAME_RE = re.compile(r"^[A-Za-z0-9._-]{1,128}\Z")

# target triple -> (os, arch, archive formats, executable name)
TARGETS: dict[str, tuple[str, str, tuple[str, ...], str]] = {
    "x86_64-unknown-linux-gnu": ("linux", "x86_64", ("tar.gz", "zip"), "asc"),
    "aarch64-unknown-linux-gnu": ("linux", "aarch64", ("tar.gz", "zip"), "asc"),
    "x86_64-apple-darwin": ("macos", "x86_64", ("tar.gz", "zip"), "asc"),
    "aarch64-apple-darwin": ("macos", "aarch64", ("tar.gz", "zip"), "asc"),
    "x86_64-pc-windows-msvc": ("windows", "x86_64", ("zip",), "asc.exe"),
}

# Files each archive carries besides the executable, under these names.
LICENSE_NAMES = {"LICENSE-APACHE": "LICENSE-APACHE.txt", "LICENSE-MIT": "LICENSE-MIT.txt"}

DESCRIPTION = "Git-compatible source control with changesets, stacks and a virtual working tree."


class ReleaseError(Exception):
    """A release input is missing or malformed. The release stops; nothing is repaired."""


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def check_version(version: str) -> str:
    if not VERSION_RE.match(version):
        raise ReleaseError(f"bad version {version!r}: expected MAJOR.MINOR.PATCH")
    return version


def check_commit(commit: str) -> str:
    if not COMMIT_RE.match(commit):
        raise ReleaseError(f"bad commit {commit!r}: expected 40 lowercase hex digits")
    return commit


def zip_time(epoch: int) -> tuple[int, int, int, int, int, int]:
    """ZIP cannot hold a time before 1980."""
    t = datetime.datetime.fromtimestamp(max(epoch, 315532800), datetime.timezone.utc)
    return (t.year, t.month, t.day, t.hour, t.minute, t.second - t.second % 2)


def iso_time(epoch: int) -> str:
    t = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def members(exe_name: str, exe_path: Path, licenses: list[Path]) -> list[tuple[str, bytes, int]]:
    """(name, content, mode) of each archive member, in the fixed order."""
    out = [(exe_name, exe_path.read_bytes(), 0o755)]
    for lic in licenses:
        out.append((LICENSE_NAMES.get(lic.name, lic.name + ".txt"), lic.read_bytes(), 0o644))
    return out


def write_tar_gz(dest: Path, files: list[tuple[str, bytes, int]], epoch: int) -> None:
    raw = io.BytesIO()
    # filename="" and mtime=0 keep the gzip header free of the output name and the clock.
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for name, data, mode in files:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = mode
                info.mtime = epoch
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                tar.addfile(info, io.BytesIO(data))
    dest.write_bytes(raw.getvalue())


def write_zip(dest: Path, files: list[tuple[str, bytes, int]], epoch: int) -> None:
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for name, data, mode in files:
            info = zipfile.ZipInfo(name, date_time=zip_time(epoch))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3  # Unix: the mode below is honoured on extraction
            info.external_attr = (stat.S_IFREG | mode) << 16
            z.writestr(info, data, compresslevel=9)


def build_archives(
    version: str, bins: Path, licenses: list[Path], out: Path, epoch: int
) -> list[dict]:
    """Archives for every target that has a binary. A target without one is an error: a release
    that silently lacks a platform would look complete to the download page."""
    entries = []
    for target, (os_name, arch, formats, exe) in TARGETS.items():
        exe_path = bins / target / exe
        if not exe_path.is_file() or exe_path.is_symlink():
            raise ReleaseError(f"no binary for {target}: {exe_path} is not a regular file")
        if exe_path.stat().st_size == 0:
            raise ReleaseError(f"the binary for {target} is empty")
        files = members(exe, exe_path, licenses)
        for fmt in formats:
            name = f"asc-{version}-{target}.{fmt}"
            dest = out / name
            (write_tar_gz if fmt == "tar.gz" else write_zip)(dest, files, epoch)
            entries.append(
                entry(version, "cli", os_name, arch, fmt, dest, out, target=target)
            )
    return entries


def entry(
    version: str,
    kind: str,
    os_name: str,
    arch: str,
    fmt: str,
    path: Path,
    out: Path,
    target: str | None = None,
) -> dict:
    """A manifest row. `file` is the file name; a release directory is flat."""
    rel = path.name
    e = {
        "version": version,
        "kind": kind,
        "os": os_name,
        "arch": arch,
        "target": target,
        "format": fmt,
        "file": rel,
        "sha256": sha256_of(path),
        "size": path.stat().st_size,
        "signature": rel + ".minisig",
    }
    return e


def copy_extras(version: str, extras: Path | None, out: Path) -> list[dict]:
    """The VS Code and JetBrains packages. Names are checked; contents are not unpacked."""
    if extras is None:
        return []
    found = []
    wants = {
        "vscode": (f"asc-vscode-{version}.vsix", "vsix"),
        "jetbrains": (f"asc-jetbrains-{version}.zip", "zip"),
    }
    for kind, (name, fmt) in wants.items():
        src = extras / name
        if not src.exists():
            continue
        if not src.is_file() or src.is_symlink() or src.stat().st_size == 0:
            raise ReleaseError(f"{src} is not a non-empty regular file")
        dest = out / name
        dest.write_bytes(src.read_bytes())
        found.append(entry(version, kind, "any", "any", fmt, dest, out))
    unknown = sorted(p.name for p in extras.iterdir() if p.name not in {w[0] for w in wants.values()})
    if unknown:
        raise ReleaseError(f"unexpected files in the extras directory: {', '.join(unknown)}")
    return found


# ---- packaging manifests -------------------------------------------------------------------

TOKEN_RE = re.compile(r"@([A-Z0-9_]+)(?::([A-Za-z0-9._-]+))?@")


def render(template: str, values: dict[str, str], hashes: dict[str, str], version: str) -> str:
    """Replaces @NAME@ and @SHA256:<target>.<ext>@. An unknown token or file is an error."""

    def sub(m: re.Match) -> str:
        name, arg = m.group(1), m.group(2)
        if name == "SHA256" and arg:
            key = f"asc-{version}-{arg}"
            if key not in hashes:
                raise ReleaseError(f"template wants the hash of {key}, which is not in the release")
            return hashes[key]
        if arg is None and name in values:
            return values[name]
        raise ReleaseError(f"unknown template token {m.group(0)}")

    return TOKEN_RE.sub(sub, template)


def render_packaging(
    root: Path,
    version: str,
    release_url: str,
    homepage: str,
    epoch: int,
    entries: list[dict],
    out: Path,
) -> list[dict]:
    """Renders packaging/**/*.in into `out`. Returns manifest entries for the rendered files."""
    values = {
        "VERSION": version,
        "RELEASE_URL": release_url.rstrip("/"),
        "HOMEPAGE": homepage.rstrip("/"),
        "DESCRIPTION": DESCRIPTION,
        "DATE": datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime("%Y-%m-%d"),
    }
    hashes = {e["file"]: e["sha256"] for e in entries}
    plan = [
        ("packaging/homebrew/asc.rb.in", "asc.rb", "homebrew"),
        ("packaging/scoop/asc.json.in", "asc.scoop.json", "scoop"),
        ("packaging/winget/portable/asc.asc.yaml.in", "winget-asc.asc.yaml", "winget"),
        ("packaging/winget/portable/asc.asc.installer.yaml.in", "winget-asc.asc.installer.yaml", "winget"),
        ("packaging/winget/portable/asc.asc.locale.en-GB.yaml.in", "winget-asc.asc.locale.en-GB.yaml", "winget"),
    ]
    rendered = []
    for src, dest_name, kind in plan:
        template = (root / src).read_text(encoding="utf-8")
        text = render(template, values, hashes, version)
        dest = out / dest_name
        dest.write_text(text, encoding="utf-8", newline="\n")
        rendered.append(entry(version, kind, "any", "any", dest.suffix.lstrip("."), dest, out))
    return rendered


# ---- manifest and checksums ---------------------------------------------------------------


def write_manifest(
    out: Path, version: str, commit: str, epoch: int, entries: list[dict]
) -> Path:
    doc = {
        "schema": SCHEMA,
        "name": "asc",
        "version": version,
        "commit": commit,
        "released_at": iso_time(epoch),
        "files": sorted(entries, key=lambda e: (e["kind"], e["os"], e["arch"], e["file"])),
    }
    path = out / "manifest.json"
    path.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline="\n")
    return path


def release_files(out: Path) -> list[Path]:
    """Every file that SHA256SUMS covers: all but itself and the signatures."""
    files = []
    for p in sorted(out.iterdir()):
        if p.is_symlink():
            raise ReleaseError(f"{p} is a symbolic link")
        if p.is_dir():
            raise ReleaseError(f"{p} is a directory; a release is one flat directory")
        if p.is_file() and p.name != "SHA256SUMS" and not p.name.endswith(".minisig"):
            files.append(p)
    return files


def write_sums(out: Path) -> Path:
    lines = []
    for p in release_files(out):
        rel = p.relative_to(out).as_posix()
        if not all(PLAIN_NAME_RE.match(part) for part in rel.split("/")):
            raise ReleaseError(f"unsafe file name in the release: {rel!r}")
        lines.append(f"{sha256_of(p)}  {rel}\n")
    path = out / "SHA256SUMS"
    path.write_text("".join(lines), encoding="utf-8", newline="\n")
    return path


def assemble(args: argparse.Namespace) -> None:
    version = check_version(args.version)
    commit = check_commit(args.commit)
    out = Path(args.out)
    if out.exists() and any(out.iterdir()):
        raise ReleaseError(f"{out} is not empty")
    out.mkdir(parents=True, exist_ok=True)
    root = Path(args.root)

    entries = build_archives(version, Path(args.bins), [Path(p) for p in args.license], out, args.epoch)
    entries += copy_extras(version, Path(args.extras) if args.extras else None, out)

    sbom_path = out / f"asc-{version}.cdx.json"
    sbom_path.write_text(
        sbom.render(Path(args.cargo_lock).read_text(encoding="utf-8"), version, commit, args.epoch),
        encoding="utf-8",
        newline="\n",
    )
    entries.append(entry(version, "sbom", "any", "any", "cyclonedx-json", sbom_path, out))

    # Packaging manifests describe the archives only; they are listed, not hashed into themselves.
    cli = [e for e in entries if e["kind"] == "cli"]
    entries += render_packaging(root, version, args.release_url, args.homepage, args.epoch, cli, out)

    write_manifest(out, version, commit, args.epoch, entries)
    write_sums(out)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--version", required=True)
    ap.add_argument("--commit", required=True)
    ap.add_argument("--epoch", type=int, required=True)
    ap.add_argument("--bins", required=True)
    ap.add_argument("--extras")
    ap.add_argument("--cargo-lock", required=True)
    ap.add_argument("--license", action="append", required=True)
    ap.add_argument("--release-url", required=True)
    ap.add_argument("--homepage", required=True)
    ap.add_argument("--root", default=str(Path(__file__).resolve().parents[2]))
    ap.add_argument("--out", required=True)
    args = ap.parse_args(argv)
    for flag in ("release_url", "homepage"):
        if not getattr(args, flag).startswith(("https://", "http://127.0.0.1", "http://localhost")):
            ap.error(f"--{flag.replace('_', '-')} must be an https URL")
    if args.epoch < 0:
        ap.error("--epoch must not be negative")
    try:
        assemble(args)
    except ReleaseError as e:
        print(f"assemble: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
