"""A stand-in for `gh api` with the few GitHub REST calls publish-packages.sh makes, over a state directory.

    python3 fake_gh.py <state-dir> api [-X METHOD] <path> [--jq EXPR] [-f k=v | -F k=v ...]

State: <state>/repos/<owner>/<name>/default (the default branch), .../refs/<branch> (a commit id),
.../files/<branch>/<path> (bytes), .../pulls (one JSON object per line). Every write is logged to <state>/log.
--jq supports the expressions the script uses: .sha, .content, .default_branch, .object.sha and length.
"""

import base64
import hashlib
import json
import os
import sys
from urllib.parse import parse_qs, urlsplit


def fail(code, msg):
    print(json.dumps({"message": msg, "status": str(code)}), file=sys.stderr)
    sys.exit(1)


def main():
    state, args = sys.argv[1], sys.argv[2:]
    if not args or args[0] != "api":
        fail(400, "only api is faked")
    args = args[1:]
    method, path, jq, fields = "GET", None, None, {}
    i = 0
    while i < len(args):
        a = args[i]
        if a == "-X":
            method = args[i + 1]
            i += 2
        elif a == "--jq":
            jq = args[i + 1]
            i += 2
        elif a in ("-f", "-F"):
            k, _, v = args[i + 1].partition("=")
            fields[k] = v
            i += 2
        else:
            path = a
            i += 1
    url = urlsplit(path)
    parts = url.path.split("/")
    query = {k: v[0] for k, v in parse_qs(url.query).items()}
    if parts[0] != "repos" or len(parts) < 3:
        fail(404, "Not Found")
    repo = os.path.join(state, "repos", parts[1], parts[2])
    if not os.path.isdir(repo):
        fail(404, "Not Found")
    rest = parts[3:]

    def log(line):
        with open(os.path.join(state, "log"), "a", encoding="utf-8") as f:
            f.write(line + "\n")

    def default_branch():
        with open(os.path.join(repo, "default"), encoding="utf-8") as f:
            return f.read().strip()

    def ref_file(branch):
        return os.path.join(repo, "refs", branch)

    out = None
    if not rest and method == "GET":
        out = {"default_branch": default_branch()}
    elif rest[:1] == ["contents"]:
        rel = "/".join(rest[1:])
        branch = query.get("ref") or fields.get("branch") or default_branch()
        target = os.path.join(repo, "files", branch, *rel.split("/"))
        if method == "GET":
            if not os.path.isfile(target):
                fail(404, "Not Found")
            data = open(target, "rb").read()
            enc = base64.b64encode(data).decode()
            out = {"sha": hashlib.sha1(data).hexdigest(), "content": "\n".join(enc[j : j + 60] for j in range(0, len(enc), 60))}
        elif method == "PUT":
            if not os.path.isfile(ref_file(branch)):
                fail(404, "Branch not found")
            exists = os.path.isfile(target)
            if exists and fields.get("sha") != hashlib.sha1(open(target, "rb").read()).hexdigest():
                fail(409, "sha does not match")
            if not exists and "sha" in fields:
                fail(422, "sha given for a new file")
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "wb") as f:
                f.write(base64.b64decode(fields["content"]))
            log(f"PUT {parts[1]}/{parts[2]} {branch} {rel}")
            out = {}
    elif rest[:3] == ["git", "ref", "heads"]:
        branch = "/".join(rest[3:])
        if not os.path.isfile(ref_file(branch)):
            fail(404, "Not Found")
        out = {"object": {"sha": open(ref_file(branch), encoding="utf-8").read().strip()}}
    elif rest == ["git", "refs"] and method == "POST":
        branch = fields["ref"].removeprefix("refs/heads/")
        if os.path.isfile(ref_file(branch)):
            fail(422, "Reference already exists")
        os.makedirs(os.path.dirname(ref_file(branch)), exist_ok=True)
        with open(ref_file(branch), "w", encoding="utf-8") as f:
            f.write(fields["sha"])
        log(f"REF {parts[1]}/{parts[2]} {branch} {fields['sha']}")
        out = {}
    elif rest[:3] == ["git", "refs", "heads"] and method == "PATCH":
        branch = "/".join(rest[3:])
        with open(ref_file(branch), "w", encoding="utf-8") as f:
            f.write(fields["sha"])
        log(f"RESET {parts[1]}/{parts[2]} {branch} {fields['sha']}")
        out = {}
    elif rest == ["pulls"]:
        pulls = os.path.join(repo, "pulls")
        rows = [json.loads(line) for line in open(pulls, encoding="utf-8")] if os.path.isfile(pulls) else []
        if method == "GET":
            out = [p for p in rows if p["head"] == query.get("head") and p["state"] == query.get("state", "open")]
        else:
            row = {"head": fields["head"], "base": fields["base"], "title": fields["title"], "state": "open"}
            with open(pulls, "a", encoding="utf-8") as f:
                f.write(json.dumps(row) + "\n")
            log(f"PR {parts[1]}/{parts[2]} {fields['head']} -> {fields['base']}: {fields['title']}")
            out = {}
    else:
        fail(404, "Not Found")

    if jq is None:
        print(json.dumps(out))
    elif jq == "length":
        print(len(out))
    else:
        v = out
        for key in jq.lstrip(".").split("."):
            v = v[key]
        print(v)


if __name__ == "__main__":
    main()
