g init
echo ".p/" > .gitignore
mkdir .p
printf '1\n2\n3\n4\n5\n' > f
git add .gitignore
g add f
at 1
g commit -m base
g tag base
cat > .p/three.mbox <<'MBOX'
From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001
From: Alice Example <alice@example.com>
Date: Mon, 5 Jan 2026 10:00:00 +0100
Subject: [PATCH 1/3] first change

Body of the first change.
Second line of the body.
---
 f | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)

diff --git a/f b/f
index 1111111..2222222 100644
--- a/f
+++ b/f
@@ -1,5 +1,5 @@
 1
 2
-3
+X
 4
 5
-- 
2.0.0

From 2222222222222222222222222222222222222222 Mon Sep 17 00:00:00 2001
From: "Bob, Jr." <bob@example.com>
Date: Tue, 6 Jan 2026 11:30:00 -0500
Subject: Re: [PATCH 2/3] add a new file
 with a folded subject

---
 new.txt | 2 ++
 1 file changed, 2 insertions(+)
 create mode 100644 new.txt

diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
-- 
2.0.0

From 3333333333333333333333333333333333333333 Mon Sep 17 00:00:00 2001
From: Carol <carol@example.com>
Date: Wed, 7 Jan 2026 12:00:00 +0000
Subject: [PATCH 3/3] remove the new file and touch f

---
 f       | 1 +
 new.txt | 2 --
 2 files changed, 1 insertion(+), 2 deletions(-)
 delete mode 100644 new.txt

diff --git a/f b/f
--- a/f
+++ b/f
@@ -4,2 +4,3 @@
 4
 5
+6
diff --git a/new.txt b/new.txt
deleted file mode 100644
index 3333333..0000000
--- a/new.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
MBOX
g am .p/three.mbox
g log --format='%H %an <%ae> %ad | %cn %cd | %s' --date=iso
g log --format=%B -n2
g status --short
cat f
ls
g reflog
git reset -q --hard base
# Quiet, from standard input.
g am -q < .p/three.mbox
g log --oneline
git reset -q --hard base
# Options: strip, committer date, ignore date.
g am --committer-date-is-author-date .p/three.mbox
g log --format='%an %ad | %cn %cd' --date=raw
git reset -q --hard base
# Errors.
git reset -q --hard base
g am .p/missing.mbox
g am < /dev/null
printf 'not a mail\n' > .p/junk
g am .p/junk
echo dirty >> f
git add f
g am .p/three.mbox
g status --short
