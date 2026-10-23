g init
echo ".p/" > .gitignore
printf 'a\nb\nc\n' > f
g add .
at 1
g commit -m base
g tag base
printf 'a\nB\nc\n' > f
at 2
g commit -am "Change b to B"
echo x > g
git add g
at 3
g commit -m "Add g: with / odd *chars* and... dots.."
g format-patch base
cat 0001-Change-b-to-B.patch
g format-patch -o out -2
ls out
g format-patch -q -o out2 -1
ls out2
g format-patch -1 -o out3/sub --numbered
cat out3/sub/*.patch
g format-patch --root base
g format-patch
g format-patch --stdout -3 HEAD~5
