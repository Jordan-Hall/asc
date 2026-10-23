# compat: asc-only
# A directory with a .ascignore and a differing .gitignore: the shadow warning (SPEC §2.4.2).
fail() { echo "ascignore: $*" >&2; exit 1; }
asc init || fail "asc init"
[ -f .ascignore ] || fail "asc init wrote no .ascignore"
[ "$(head -n 1 .ascignore)" = "# asc: replaces .gitignore in this directory" ] || fail "first line of .ascignore"
printf '*.log\n' > .gitignore
printf '*.tmp\n' >> .ascignore
echo x > a.log
echo x > b.tmp
asc status --porcelain > out.txt 2> err.txt || fail "asc status"
grep -qx '?? a.log' out.txt || fail ".gitignore must not be read: $(cat out.txt)"
grep -q 'b.tmp' out.txt && fail ".ascignore must be read: $(cat out.txt)"
grep -qxF 'warning: asc: .gitignore is not read because .ascignore exists (rename or merge it: asc migrate ignore)' err.txt \
  || fail "shadow warning missing: $(cat err.txt)"
# git init through the shim writes no .ascignore (SPEC §11.4).
mkdir plain && cd plain
git init >/dev/null || fail "git init"
[ ! -e .ascignore ] || fail "git init wrote a .ascignore"
