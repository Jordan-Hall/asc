g init
echo ".p/" > .gitignore
mkdir .p
printf '1\n2\n3\n4\n5\n' > f
git add .gitignore
g add f
at 1
g commit -m base
g tag base
cat > .p/two.mbox <<'MBOX'
From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001
From: Alice Example <alice@example.com>
Date: Mon, 5 Jan 2026 10:00:00 +0100
Subject: [PATCH 1/2] change three

Body.
---
 f | 2 +-

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
From: Bob <bob@example.com>
Date: Tue, 6 Jan 2026 11:30:00 -0500
Subject: [PATCH 2/2] add file

---
 new.txt | 1 +

diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
MBOX
printf '1\n2\nY\n4\n5\n' > f
at 2
g commit -am divergent
g am .p/two.mbox
for x in .git/rebase-apply/next .git/rebase-apply/last; do :; done
g diff
g am .p/two.mbox
# Resolve without staging.
printf '1\n2\nresolved\n4\n5\n' > f
g am --continue
# Resolve and stage.
g add f
at 3
g am --continue
g log --format='%an | %s'
g status --short
cat f new.txt
# Skip the first, take the second.
git reset -q --hard base
printf '1\n2\nY\n4\n5\n' > f
at 4
git commit -qam divergent2
g am .p/two.mbox
g am --skip
g log --format='%an | %s'
g status --short
# Abort.
git reset -q --hard base
printf '1\n2\nY\n4\n5\n' > f
at 5
git commit -qam divergent3
g am .p/two.mbox
g am --abort
g log --format='%s'
g status --short
cat f
# No session.
g am --continue
g am --skip
g am --abort
# Quit leaves the work as is.
g am .p/two.mbox
g am --quit
g status --short
