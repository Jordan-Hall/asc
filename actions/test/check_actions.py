"""Static contract checks for the ASC Actions (SPEC 12.7). Run by selftest.sh.

Asserts, for every action.yml: it parses, is composite, every run step names a shell, no `run`
body interpolates `${{ }}` (script injection: inputs go through `env`), and the forks keep the
inputs and outputs of upstream. Exits non-zero on the first violated rule, after printing all.
"""

import pathlib
import re
import sys

import yaml

ROOT = pathlib.Path(sys.argv[1])

REQUIRED_INPUTS = {
    "checkout": {"ref", "fetch-depth", "path", "submodules", "lfs", "sparse-checkout"},
    "cache": {"path", "key", "restore-keys"},
    "upload-artifact": {"name", "path"},
    "download-artifact": {"name", "path"},
    "asc/setup": {"version", "url", "sha256"},
    "asc/changed": {"repo", "base", "head"},
    "asc/affected": {"repo", "base", "head"},
    "asc/turbo-cache": {"token", "server"},
}
REQUIRED_OUTPUTS = {
    "cache": {"cache-hit"},
    "asc/affected": {"rust", "matrix", "turbo_filter", "count"},
}
# The hub sets these for every job of an asc repository (SPEC 12.10); an action must not shadow them with the
# forge's URL when its `server` input is left empty.
JOB_ENV_FALLBACK = {
    "cache": "env.ASC_REMOTE",
    "cache/save": "env.ASC_REMOTE",
    "upload-artifact": "env.ASC_REMOTE",
    "download-artifact": "env.ASC_REMOTE",
    "asc/turbo-cache": "env.TURBO_API",
}
errors: list[str] = []


def fail(msg: str) -> None:
    errors.append(msg)


for name, want in REQUIRED_INPUTS.items():
    f = ROOT / name / "action.yml"
    if not f.is_file():
        fail(f"{name}: action.yml missing")
        continue
    doc = yaml.safe_load(f.read_text(encoding="utf-8"))
    if not isinstance(doc, dict):
        fail(f"{name}: not a mapping")
        continue
    have = set((doc.get("inputs") or {}).keys())
    if want - have:
        fail(f"{name}: missing inputs {sorted(want - have)}")
    out = set((doc.get("outputs") or {}).keys())
    if REQUIRED_OUTPUTS.get(name, set()) - out:
        fail(f"{name}: missing outputs {sorted(REQUIRED_OUTPUTS[name] - out)}")
    runs = doc.get("runs") or {}
    if runs.get("using") != "composite":
        fail(f"{name}: must be composite")
    for i, step in enumerate(runs.get("steps") or []):
        if "run" in step:
            if not step.get("shell"):
                fail(f"{name} step {i}: run without shell")
            if "${{" in step["run"]:
                fail(f"{name} step {i}: expression inside run (use env)")
            if "set -euo pipefail" not in step["run"]:
                fail(f"{name} step {i}: run must set -euo pipefail")
        for k, v in (step.get("env") or {}).items():
            if "token" in k.lower() and k not in ("ASC_TOKEN", "IN_TOKEN"):
                fail(f"{name} step {i}: unexpected token env {k}")
    text = f.read_text(encoding="utf-8")
    if "http://" in text.replace("http://127.0.0.1", "").replace("http://localhost", "").replace(
        "http://\\[::1\\]", ""
    ):
        fail(f"{name}: plain http outside loopback")
    for banned in ("--no-verify", "curl -k", "--insecure"):
        if banned in text:
            fail(f"{name}: banned text {banned!r}")
    if re.search(r"\|\s*(sh|bash)", text):
        fail(f"{name}: pipes into a shell")

for name, ref in JOB_ENV_FALLBACK.items():
    doc = yaml.safe_load((ROOT / name / "action.yml").read_text(encoding="utf-8"))
    if ((doc.get("inputs") or {}).get("server") or {}).get("default") != "":
        fail(f"{name}: server must default to empty so the job's environment is used")
    uses = [v for step in doc["runs"]["steps"] for v in (step.get("env") or {}).values() if "inputs.server" in str(v)]
    if not uses or not all(ref in v for v in uses):
        fail(f"{name}: every use of inputs.server must fall back to {ref}")

for rel in ("docs/ci/asc-projects.toml",):
    p = ROOT.parent / rel
    if not p.is_file():
        fail(f"{rel} missing")

if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print(f"ok: {len(REQUIRED_INPUTS)} actions")
