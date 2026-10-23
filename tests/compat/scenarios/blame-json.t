# compat: asc-only
fail() { echo "blame-json: $*" >&2; exit 1; }
asc init || fail "asc init"
printf 'one\ntwo\nthree\n' > a.txt
asc add a.txt
asc commit -m first || fail "commit"
printf 'one\nTWO\nthree\nnew\n' > a.txt
asc blame --json a.txt > out.json || fail "blame --json"
[ "$(wc -l < out.json)" = 1 ] || fail "one line expected: $(cat out.json)"
grep -q '"hunks"' out.json || fail "hunks missing: $(cat out.json)"
grep -q '"commits"' out.json || fail "commits missing: $(cat out.json)"
grep -q '"orig_path"' out.json || fail "orig_path missing: $(cat out.json)"
asc blame --json -L 2,3 a.txt > ranged.json || fail "blame --json -L"
grep -q '"len":1' ranged.json || fail "range hunk: $(cat ranged.json)"
asc blame --json nosuch.txt 2> err.txt && fail "missing file must fail"
grep -qx "fatal: no such path 'nosuch.txt' in HEAD" err.txt || fail "message: $(cat err.txt)"
