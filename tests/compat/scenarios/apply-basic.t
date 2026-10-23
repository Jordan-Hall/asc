g init
echo ".p/" > .gitignore
mkdir .p
printf 'one\ntwo\nthree\nfour\nfive\n' > f
printf 'x\ny\n' > g.txt
echo keep > k
g add .
git add .gitignore
at 1
g commit -m base
g tag base
P=.p
printf 'one\nTWO\nthree\nfour\nfive\nsix\n' > f
echo new > n
rm k
git add -A
git diff --cached > "$P/a.patch"
git reset -q --hard base
rm -f n
# Work tree only.
g apply --check "$P/a.patch"
g status --short
g apply "$P/a.patch"
g status --short
cat f n
test -e k && echo k-exists
git reset -q --hard base
rm -f n
# Staged tree only.
g apply --cached "$P/a.patch"
g status --short
g diff --cached --stat
cat f
git reset -q --hard base
rm -f n
# Both.
g apply --index "$P/a.patch"
g status --short
g diff --cached --stat
g diff --stat
# Reverse of the applied patch.
g apply -R --index "$P/a.patch"
g status --short
cat f k
# From standard input, verbose.
g apply -v < "$P/a.patch"
g status --short
git reset -q --hard base
rm -f n
g apply --index -v "$P/a.patch" "$P/a.patch"
g status --short
