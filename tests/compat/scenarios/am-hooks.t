g init
echo ".p/" > .gitignore
mkdir .p
mkdir -p .hooks
g config core.hooksPath "$(wpwd)/.hooks"
printf '1\n2\n3\n' > f
git add .gitignore
g add f
at 1
g commit -m base
g tag base
cat > .hooks/applypatch-msg <<'HOOK'
#!/bin/sh
echo "applypatch-msg: $(basename "$1") $(head -1 "$1")" >&2
echo "Reviewed-by: hook" >> "$1"
HOOK
cat > .hooks/pre-applypatch <<'HOOK'
#!/bin/sh
echo "pre-applypatch: staged=$(git diff --cached --name-only | tr '\n' ' ')" >&2
HOOK
cat > .hooks/post-applypatch <<'HOOK'
#!/bin/sh
echo "post-applypatch: head=$(git log -1 --format=%s)" >&2
HOOK
chmod +x .hooks/applypatch-msg .hooks/pre-applypatch .hooks/post-applypatch
cat > .p/one.mbox <<'MBOX'
From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001
From: Alice Example <alice@example.com>
Date: Mon, 5 Jan 2026 10:00:00 +0100
Subject: [PATCH] change two

Message body.
---
 f | 2 +-

diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 1
-2
+two
 3
MBOX
g am .p/one.mbox
g log --format=%B -n1
git reset -q --hard base
g am --no-verify .p/one.mbox
g log --format=%B -n1
git reset -q --hard base
# A rejecting pre-applypatch stops the run.
cat > .hooks/pre-applypatch <<'HOOK'
#!/bin/sh
echo "pre-applypatch: refusing" >&2
exit 1
HOOK
g am .p/one.mbox
g log --oneline
g status --short
