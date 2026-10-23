# compat: asc-only
# fidelity: Q-7, U-2
# Aliases of .git that real git does not refuse on every platform (HFS ignorable code points),
# aliases of asc's own local state, and aliases that only name the source of a rename, a deletion
# or a mode change (git reports a missing file there): asc refuses them all, up front. `g` always
# succeeds, so the checks are the unchanged file and the final listing.
g init
echo ".p/" > .gitignore
mkdir .p
echo keep > f
g add .
git add .gitignore
at 1
g commit -m base
P=.p
n=0
for name in '.g\342\200\214it/config' '.asc/info/x' '.asc'; do
  n=$((n + 1))
  printf -- '--- a/f\n+++ b/f\n@@ -1 +1 @@\n-keep\n+changed\n--- /dev/null\n+++ b/'"$name"'\n@@ -0,0 +1 @@\n+x\n' > "$P/$n.patch"
  g apply "$P/$n.patch"
  test "$(tr -d '\r' < f)" = keep || exit 1
done
cat > "$P/rename.patch" <<'PATCH'
diff --git a/GIT~1/f b/f2
similarity index 100%
rename from GIT~1/f
rename to f2
PATCH
g apply "$P/rename.patch"
cat > "$P/delete.patch" <<'PATCH'
diff --git a/.git /f b/.git /f
deleted file mode 100644
--- a/.git /f
+++ /dev/null
@@ -1 +0,0 @@
-x
PATCH
g apply "$P/delete.patch"
cat > "$P/mode.patch" <<'PATCH'
diff --git a/GIT~1/f b/GIT~1/f
old mode 100644
new mode 100755
PATCH
g apply "$P/mode.patch"
find . -mindepth 1 \( -path ./.git -o -path ./.asc -o -path ./.p \) -prune -o -print | sort > "$P/tree"
test "$(tr -d '\r' < "$P/tree" | tr '\n' ' ')" = "./.gitignore ./f " || { cat "$P/tree"; exit 1; }
# Each refusal that git does not make names its decision in docs/git-fidelity.md.
note() {
  out=$(git apply "$2" 2>&1 || true)
  case "$out" in *"(git-fidelity $1)"*) ;; *) echo "no $1 note in: $out"; exit 1 ;; esac
}
note Q-7 "$P/1.patch"
note U-2 "$P/rename.patch"
note U-2 "$P/delete.patch"
note U-2 "$P/mode.patch"
