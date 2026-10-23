g init
echo ".p/" > .gitignore
printf 'a\nb\nc\n' > f
echo old > old.txt
g add .
at 1
g commit -m base
g tag base
printf 'a\nB\nc\n' > f
echo new > g
g add .
at 2
g commit -m "Change b to B

Body line one.
Body two.

Signed-off-by: A U Thor <author@example.com>"
echo more >> g
git mv old.txt renamed.txt
at 3
g commit -am "Second change: a long subject line that has to be folded because it passes the width limit

more words"
git add -A
at 4
g commit -m "third"
g format-patch -1 --stdout
g format-patch -2 --stdout
g format-patch --stdout base
g format-patch --stdout base..HEAD~1
g format-patch -N --stdout -2
g format-patch --no-signature --stdout -1
g format-patch --subject-prefix=RFC --start-number=5 --stdout -2
g format-patch --root --stdout -n base
g format-patch -k --stdout -1
g format-patch --stdout HEAD
