#!/usr/bin/env python3
"""Unit tests for assemble.py, sbom.py and verify.py.   python3 -m unittest discover scripts/release/test"""

from __future__ import annotations

import hashlib
import io
import json
import re
import stat
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
RELEASE = HERE.parent
ROOT = RELEASE.parents[1]
sys.path.insert(0, str(RELEASE))

import assemble  # noqa: E402
import sbom  # noqa: E402
import verify  # noqa: E402

COMMIT = "0123456789abcdef0123456789abcdef01234567"
EPOCH = 1_700_000_000


def fake_bins(base: Path, version: str) -> Path:
    for target, (_os, _arch, _fmts, exe) in assemble.TARGETS.items():
        d = base / target
        d.mkdir(parents=True)
        (d / exe).write_bytes(f"binary for {target} {version}\n".encode())
    return base


def run_assemble(out: Path, bins: Path, version: str = "1.2.3", extras: Path | None = None) -> None:
    argv = [
        "--version", version, "--commit", COMMIT, "--epoch", str(EPOCH),
        "--bins", str(bins), "--cargo-lock", str(ROOT / "Cargo.lock"),
        "--license", str(ROOT / "LICENSE-APACHE"), "--license", str(ROOT / "LICENSE-MIT"),
        "--release-url", "https://downloads.example.invalid/asc/" + version,
        "--homepage", "https://example.invalid/asc",
        "--root", str(ROOT), "--out", str(out),
    ]
    if extras is not None:
        argv += ["--extras", str(extras)]
    rc = assemble.main(argv)
    if rc != 0:
        raise AssertionError(f"assemble failed with {rc}")


class AssembleTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        base = Path(cls.tmp.name)
        cls.bins = fake_bins(base / "bins", "1.2.3")
        cls.extras = base / "extras"
        cls.extras.mkdir()
        (cls.extras / "asc-vscode-1.2.3.vsix").write_bytes(b"vsix")
        (cls.extras / "asc-jetbrains-1.2.3.zip").write_bytes(b"jb")
        cls.out = base / "out"
        run_assemble(cls.out, cls.bins, extras=cls.extras)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.tmp.cleanup()

    def manifest(self) -> dict:
        return json.loads((self.out / "manifest.json").read_text(encoding="utf-8"))

    def test_every_target_has_the_promised_archives(self) -> None:
        names = {p.name for p in self.out.iterdir()}
        for target, (_o, _a, fmts, _e) in assemble.TARGETS.items():
            for fmt in fmts:
                self.assertIn(f"asc-1.2.3-{target}.{fmt}", names)
        self.assertNotIn("asc-1.2.3-x86_64-pc-windows-msvc.tar.gz", names)

    def test_archives_are_flat_and_the_executable_is_executable(self) -> None:
        with tarfile.open(self.out / "asc-1.2.3-aarch64-apple-darwin.tar.gz") as t:
            infos = {i.name: i for i in t.getmembers()}
        self.assertEqual(set(infos), {"asc", "LICENSE-APACHE.txt", "LICENSE-MIT.txt"})
        self.assertEqual(infos["asc"].mode, 0o755)
        self.assertEqual(infos["LICENSE-MIT.txt"].mode, 0o644)
        self.assertEqual((infos["asc"].uid, infos["asc"].gid, infos["asc"].mtime), (0, 0, EPOCH))
        with zipfile.ZipFile(self.out / "asc-1.2.3-x86_64-pc-windows-msvc.zip") as z:
            self.assertEqual([i.filename for i in z.infolist()], ["asc.exe", "LICENSE-APACHE.txt", "LICENSE-MIT.txt"])
            self.assertEqual(z.read("asc.exe"), b"binary for x86_64-pc-windows-msvc 1.2.3\n")
        with zipfile.ZipFile(self.out / "asc-1.2.3-x86_64-unknown-linux-gnu.zip") as z:
            mode = z.getinfo("asc").external_attr >> 16
            self.assertTrue(stat.S_ISREG(mode) and mode & 0o111)

    def test_the_manifest_matches_the_files(self) -> None:
        doc = self.manifest()
        self.assertEqual((doc["schema"], doc["name"], doc["version"], doc["commit"]), (1, "asc", "1.2.3", COMMIT))
        self.assertEqual(doc["released_at"], "2023-11-14T22:13:20Z")
        for e in doc["files"]:
            data = (self.out / e["file"]).read_bytes()
            self.assertEqual(e["sha256"], hashlib.sha256(data).hexdigest(), e["file"])
            self.assertEqual(e["size"], len(data), e["file"])
            self.assertEqual(e["version"], "1.2.3")
        row = next(e for e in doc["files"] if e["file"] == "asc-1.2.3-aarch64-unknown-linux-gnu.tar.gz")
        self.assertEqual((row["os"], row["arch"], row["kind"], row["format"]), ("linux", "aarch64", "cli", "tar.gz"))
        kinds = {e["kind"] for e in doc["files"]}
        self.assertEqual(kinds, {"cli", "vscode", "jetbrains", "sbom", "homebrew", "scoop", "winget"})

    def test_sha256sums_covers_everything_else(self) -> None:
        lines = (self.out / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        names = [ln.split("  ", 1)[1] for ln in lines]
        self.assertEqual(names, sorted(names))
        self.assertIn("manifest.json", names)
        self.assertIn("winget-asc.asc.installer.yaml", names)
        self.assertNotIn("SHA256SUMS", names)

    def test_the_output_is_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            again = Path(d) / "out"
            run_assemble(again, self.bins, extras=self.extras)
            self.assertEqual((again / "SHA256SUMS").read_bytes(), (self.out / "SHA256SUMS").read_bytes())

    def test_a_missing_platform_stops_the_release(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            bins = fake_bins(Path(d) / "bins", "1.2.3")
            (bins / "aarch64-apple-darwin" / "asc").unlink()
            with self.assertRaises(AssertionError):
                run_assemble(Path(d) / "out", bins)

    def test_bad_inputs_are_refused(self) -> None:
        for v in ["1.2", "1.2.3-rc1", "01.2.3", "../1.2.3", "1.2.3\n"]:
            with self.assertRaises(assemble.ReleaseError, msg=v):
                assemble.check_version(v)
        for c in ["abc", COMMIT.upper(), COMMIT + "0"]:
            with self.assertRaises(assemble.ReleaseError, msg=c):
                assemble.check_commit(c)

    def test_unexpected_extras_are_refused(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            extras = Path(d)
            (extras / "surprise.exe").write_bytes(b"x")
            with self.assertRaises(assemble.ReleaseError):
                assemble.copy_extras("1.2.3", extras, Path(d))

    def test_rendered_packaging_has_real_hashes_and_no_tokens(self) -> None:
        brew = (self.out / "asc.rb").read_text(encoding="utf-8")
        sha = next(e["sha256"] for e in self.manifest()["files"] if e["file"] == "asc-1.2.3-aarch64-apple-darwin.tar.gz")
        self.assertIn(f'sha256 "{sha}"', brew)
        self.assertIn("https://downloads.example.invalid/asc/1.2.3/asc-1.2.3-aarch64-apple-darwin.tar.gz", brew)
        for p in [self.out / "asc.rb", self.out / "asc.scoop.json", *self.out.glob("winget-*.yaml")]:
            text = p.read_text(encoding="utf-8")
            self.assertIsNone(re.search(r"@[A-Z0-9_]+(:[A-Za-z0-9._-]+)?@", text), p.name)
        scoop = json.loads((self.out / "asc.scoop.json").read_text(encoding="utf-8"))
        self.assertEqual(scoop["version"], "1.2.3")
        self.assertEqual(len(scoop["architecture"]["64bit"]["hash"]), 64)
        installer = (self.out / "winget-asc.asc.installer.yaml").read_text(encoding="utf-8")
        self.assertIn("NestedInstallerType: portable", installer)
        self.assertIn("ReleaseDate: \"2023-11-14\"", installer)

    def test_render_refuses_unknown_tokens_and_files(self) -> None:
        with self.assertRaises(assemble.ReleaseError):
            assemble.render("@NOPE@", {"VERSION": "1"}, {}, "1")
        with self.assertRaises(assemble.ReleaseError):
            assemble.render("@SHA256:x.zip@", {}, {}, "1")

    def test_the_sbom_is_cyclonedx_and_lists_real_dependencies(self) -> None:
        doc = json.loads((self.out / "asc-1.2.3.cdx.json").read_text(encoding="utf-8"))
        self.assertEqual((doc["bomFormat"], doc["specVersion"]), ("CycloneDX", "1.5"))
        self.assertEqual(doc["metadata"]["component"]["version"], "1.2.3")
        refs = {c["bom-ref"] for c in doc["components"]}
        self.assertIn(next(r for r in refs if r.startswith("pkg:cargo/asc-core@")), refs)
        self.assertTrue(any(r.startswith("pkg:cargo/tokio@") for r in refs))
        deps = {d["ref"]: d["dependsOn"] for d in doc["dependencies"]}
        self.assertIn("pkg:cargo/asc@1.2.3", deps)
        for ref, on in deps.items():
            for d in on:
                self.assertIn(d, refs | {"pkg:cargo/asc@1.2.3"}, f"{ref} -> {d}")


class SbomTest(unittest.TestCase):
    LOCK = """
version = 4

[[package]]
name = "asc"
version = "0.1.0"
dependencies = ["a", "b 2.0.0", "b 1.0.0 (registry+https://example.invalid/index)"]

[[package]]
name = "a"
version = "1.0.0"
source = "registry+https://example.invalid/index"
checksum = "{h}"
dependencies = ["b 1.0.0 (registry+https://example.invalid/index)"]

[[package]]
name = "b"
version = "1.0.0"
source = "registry+https://example.invalid/index"
checksum = "{h}"

[[package]]
name = "b"
version = "2.0.0"

[[package]]
name = "unused"
version = "9.9.9"
""".format(h="ab" * 32)

    def test_only_reachable_packages_are_listed(self) -> None:
        doc = json.loads(sbom.render(self.LOCK, "1.0.0", COMMIT, EPOCH))
        refs = [c["bom-ref"] for c in doc["components"]]
        self.assertEqual(refs, ["pkg:cargo/a@1.0.0", "pkg:cargo/b@1.0.0", "pkg:cargo/b@2.0.0"])
        a = doc["components"][0]
        self.assertEqual(a["hashes"], [{"alg": "SHA-256", "content": "ab" * 32}])
        self.assertEqual(doc["components"][2]["properties"], [{"name": "asc:origin", "value": "workspace"}])

    def test_the_serial_number_is_stable_and_depends_on_the_commit(self) -> None:
        one = json.loads(sbom.render(self.LOCK, "1.0.0", COMMIT, EPOCH))["serialNumber"]
        two = json.loads(sbom.render(self.LOCK, "1.0.0", COMMIT, EPOCH + 5))["serialNumber"]
        other = json.loads(sbom.render(self.LOCK, "1.0.0", "f" * 40, EPOCH))["serialNumber"]
        self.assertEqual(one, two)
        self.assertNotEqual(one, other)

    def test_an_ambiguous_dependency_is_an_error(self) -> None:
        lock = self.LOCK.replace('"b 2.0.0"', '"b"')
        with self.assertRaises(sbom.SbomError):
            sbom.render(lock, "1.0.0", COMMIT, EPOCH)

    def test_a_lock_without_the_root_is_an_error(self) -> None:
        with self.assertRaises(sbom.SbomError):
            sbom.render('version = 4\n[[package]]\nname = "x"\nversion = "1.0.0"\n', "1.0.0", COMMIT, EPOCH)


class VerifyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.out = base / "out"
        run_assemble(self.out, fake_bins(base / "bins", "1.2.3"))

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def fails(self, fragment: str) -> None:
        with self.assertRaises(verify.VerifyError) as cm:
            verify.verify(self.out, None, False)
        self.assertIn(fragment, str(cm.exception))

    def test_a_good_release_verifies(self) -> None:
        doc = verify.verify(self.out, None, False)
        self.assertEqual(doc["version"], "1.2.3")

    def test_a_changed_archive_is_caught(self) -> None:
        p = self.out / "asc-1.2.3-x86_64-unknown-linux-gnu.zip"
        p.write_bytes(p.read_bytes() + b"x")
        self.fails("does not match its SHA256SUMS entry")

    def test_a_missing_or_extra_file_is_caught(self) -> None:
        (self.out / "asc-1.2.3-aarch64-apple-darwin.zip").unlink()
        self.fails("differ")

    def test_an_unlisted_file_is_caught(self) -> None:
        (self.out / "surprise.txt").write_text("x", encoding="utf-8")
        self.fails("unlisted")

    def test_a_manifest_that_disagrees_with_the_sums_is_caught(self) -> None:
        path = self.out / "manifest.json"
        doc = json.loads(path.read_text(encoding="utf-8"))
        doc["files"][0]["sha256"] = "0" * 64
        path.write_text(json.dumps(doc), encoding="utf-8")
        # manifest.json changed, so SHA256SUMS no longer matches it either.
        self.fails("manifest.json does not match")
        assemble.write_sums(self.out)
        self.fails("differs from SHA256SUMS")

    def test_a_manifest_missing_a_platform_is_caught(self) -> None:
        path = self.out / "manifest.json"
        doc = json.loads(path.read_text(encoding="utf-8"))
        doc["files"] = [e for e in doc["files"] if "windows" not in e["file"]]
        path.write_text(json.dumps(doc), encoding="utf-8")
        assemble.write_sums(self.out)
        self.fails("promised set")

    def test_an_archive_with_a_stray_member_is_caught(self) -> None:
        path = self.out / "asc-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as t:
            for name in ("asc", "../evil"):
                i = tarfile.TarInfo(name)
                i.size = 1
                i.mode = 0o755
                t.addfile(i, io.BytesIO(b"x"))
        path.write_bytes(buf.getvalue())
        doc = json.loads((self.out / "manifest.json").read_text(encoding="utf-8"))
        for e in doc["files"]:
            if e["file"] == path.name:
                e["sha256"] = hashlib.sha256(buf.getvalue()).hexdigest()
                e["size"] = len(buf.getvalue())
        (self.out / "manifest.json").write_text(json.dumps(doc), encoding="utf-8")
        assemble.write_sums(self.out)
        self.fails("unexpected members")

    def test_a_non_executable_binary_is_caught(self) -> None:
        path = self.out / "asc-1.2.3-aarch64-unknown-linux-gnu.tar.gz"
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as t:
            i = tarfile.TarInfo("asc")
            i.size = 1
            i.mode = 0o644
            t.addfile(i, io.BytesIO(b"x"))
        path.write_bytes(buf.getvalue())
        doc = json.loads((self.out / "manifest.json").read_text(encoding="utf-8"))
        for e in doc["files"]:
            if e["file"] == path.name:
                e["sha256"] = hashlib.sha256(buf.getvalue()).hexdigest()
                e["size"] = len(buf.getvalue())
        (self.out / "manifest.json").write_text(json.dumps(doc), encoding="utf-8")
        assemble.write_sums(self.out)
        self.fails("not executable")

    def test_a_bad_sums_line_is_caught(self) -> None:
        p = self.out / "SHA256SUMS"
        p.write_text(p.read_text(encoding="utf-8") + "zz  ../../etc/passwd\n", encoding="utf-8")
        self.fails("is not '<sha256>  <name>'")
        p.write_text("0" * 64 + "  ..\n", encoding="utf-8")
        self.fails("unsafe name")

    def test_signatures_need_minisign_and_every_file(self) -> None:
        import shutil

        if shutil.which("minisign") is not None:
            self.skipTest("covered end to end by test-release.sh when minisign exists")
        with self.assertRaises(verify.VerifyError) as cm:
            verify.verify(self.out, "RWQ-not-a-real-key", False)
        self.assertIn("minisign is required", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
