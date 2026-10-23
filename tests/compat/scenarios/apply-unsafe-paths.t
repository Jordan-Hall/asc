# fidelity: Q-7
g init
echo ".p/" > .gitignore
mkdir .p
echo keep > f
g add .
git add .gitignore
at 1
g commit -m base
P=.p
# Names that are .git in disguise are refused, as they are by checkout, and nothing is written.
# A good file patch first in the same input must stay unapplied.
n=0
for name in '.git /config' '.git./hooks/x' 'GIT~1/config' 'a/.git:stream' '.git/hooks/pre-commit' 'a/.GIT/x'; do
  n=$((n + 1))
  printf -- '--- a/f\n+++ b/f\n@@ -1 +1 @@\n-keep\n+changed\n--- /dev/null\n+++ b/%s\n@@ -0,0 +1 @@\n+x\n' "$name" > "$P/$n.patch"
  g apply "$P/$n.patch"
  g apply --check "$P/$n.patch"
  g apply --cached "$P/$n.patch"
  cat f
done
# A rename and a copy into an alias.
cat > "$P/rename.patch" <<'PATCH'
diff --git a/f b/.git /f
similarity index 100%
rename from f
rename to .git /f
PATCH
g apply "$P/rename.patch"
cat > "$P/copy.patch" <<'PATCH'
diff --git a/f b/.git./f
similarity index 100%
copy from f
copy to .git./f
PATCH
g apply "$P/copy.patch"
cat f
g status --short
find . -mindepth 1 \( -path ./.git -o -path ./.asc -o -path ./.p \) -prune -o -print | sort
