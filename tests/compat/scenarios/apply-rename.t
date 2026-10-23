g init
echo ".p/" > .gitignore
mkdir .p
printf 'a\nb\nc\nd\ne\nf\ng\nh\n' > old.txt
mkdir d
echo keep > d/k
g add .
git add .gitignore
at 1
g commit -m base
g tag base
P=.p
git mv old.txt new.txt
printf 'a\nb\nc\nd\ne\nf\ng\nH\n' > new.txt
git add -A
git diff --cached -M > "$P/r.patch"
git reset -q --hard base
g apply "$P/r.patch"
g status --short
cat new.txt
git reset -q --hard base
rm -f new.txt
g apply --index "$P/r.patch"
g status --short
g diff --cached --stat -M
git reset -q --hard base
# A pure rename and a deletion that empties a directory.
cat > "$P/pure.patch" <<'PATCH'
diff --git a/old.txt b/moved.txt
similarity index 100%
rename from old.txt
rename to moved.txt
diff --git a/d/k b/d/k
deleted file mode 100644
index 6a7c1ff..0000000
--- a/d/k
+++ /dev/null
@@ -1 +0,0 @@
-keep
PATCH
g apply --index "$P/pure.patch"
g status --short
test -d d && echo d-exists
ls
