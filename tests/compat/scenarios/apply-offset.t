g init
echo ".p/" > .gitignore
mkdir .p
i=1
while [ $i -le 30 ]; do echo "line $i"; i=$((i+1)); done > f
g add f
git add .gitignore
at 1
g commit -m base
g tag base
P=.p
# A patch made for a file with extra lines at the top: the hunks land at an offset.
cat > "$P/off.patch" <<'PATCH'
--- a/f
+++ b/f
@@ -18,7 +18,7 @@
 line 15
 line 16
 line 17
-line 18
+line eighteen
 line 19
 line 20
 line 21
PATCH
g apply -v "$P/off.patch"
git diff
git reset -q --hard base
# Hunks that do not know their counts.
cat > "$P/recount.patch" <<'PATCH'
--- a/f
+++ b/f
@@ -3,99 +3,99 @@
 line 3
-line 4
+line four
 line 5
PATCH
g apply "$P/recount.patch"
g apply --recount "$P/recount.patch"
git diff
git reset -q --hard base
# lint-staged's invocation over a -U0 patch.
cat > "$P/zero.patch" <<'PATCH'
diff --git a/f b/f
--- a/f
+++ b/f
@@ -5 +5,2 @@ line 4
-line 5
+LINE 5
+inserted
@@ -20,2 +21,0 @@ line 19
-line 20
-line 21
@@ -30,0 +30 @@ line 29
+tail
PATCH
g apply -v --whitespace=nowarn --recount --unidiff-zero "$P/zero.patch"
git diff
git reset -q --hard base
g apply --whitespace=nowarn "$P/zero.patch"
git diff --stat
git reset -q --hard base
# Beginning and end of the file.
cat > "$P/ends.patch" <<'PATCH'
--- a/f
+++ b/f
@@ -1,3 +1,4 @@
+first
 line 1
 line 2
 line 3
@@ -28,3 +29,4 @@
 line 28
 line 29
 line 30
+last
PATCH
g apply "$P/ends.patch"
git diff --stat
