g init
echo ".p/" > .gitignore
mkdir .p
printf 'a\nb\nc\n' > f
echo x > g
g add .
git add .gitignore
at 1
g commit -m base
g tag base
P=.p
cat > "$P/bad.patch" <<'PATCH'
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 a
-B
+X
 c
PATCH
g apply "$P/bad.patch"
g apply --check "$P/bad.patch"
g apply --cached "$P/bad.patch"
# One bad file patch leaves the good one unapplied.
cat > "$P/two.patch" <<'PATCH'
--- a/g
+++ b/g
@@ -1 +1 @@
-x
+y
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 a
-B
+X
 c
PATCH
g apply "$P/two.patch"
cat g
g status --short
# Missing and existing files.
cat > "$P/missing.patch" <<'PATCH'
--- a/nope
+++ b/nope
@@ -1 +1 @@
-x
+y
PATCH
g apply "$P/missing.patch"
g apply --cached "$P/missing.patch"
cat > "$P/exists.patch" <<'PATCH'
--- /dev/null
+++ b/g
@@ -0,0 +1 @@
+new
PATCH
g apply "$P/exists.patch"
g apply --cached "$P/exists.patch"
cat > "$P/delete.patch" <<'PATCH'
--- a/f
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
PATCH
g apply "$P/delete.patch"
# Input problems.
printf 'nothing to see\n' > "$P/junk.patch"
g apply "$P/junk.patch"
g apply "$P/does-not-exist.patch"
printf -- '--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n' > "$P/short.patch"
g apply "$P/short.patch"
cat > "$P/unsafe.patch" <<'PATCH'
--- /dev/null
+++ b/../escape
@@ -0,0 +1 @@
+x
PATCH
g apply "$P/unsafe.patch"
g status --short
