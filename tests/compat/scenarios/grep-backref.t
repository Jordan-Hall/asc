# `grep` with back-references, in basic and extended expressions: repeated words, case folding,
# whole words, inversion, counts, several patterns with their own group numbers, a backslash
# digit inside a bracket expression (a literal there) and invalid references.
g init
printf 'the the cat\nabab\nAbab here\nno repeat\nxyzzyx\nfoo bar foo\nworld1\n' > a.txt
printf 'ab-ab\nabcabc\ncd cd\nmixed Cd cd\n' > b.txt
g add a.txt b.txt
at 1
g commit -q -m first

g grep -n '\(the\) \1'
g grep -n '\(ab\)\1'
g grep -n -E '(ab)\1'
g grep -n -i '\(ab\)\1'
g grep -n -E '(foo).*\1'
g grep -n -E '([a-z])([a-z])\2\1'
g grep -n -w -E '(ab)\1'
g grep -c -E '(cd) \1'
g grep -n -v -E '(.)\1'
g grep -l -E '(ab)-\1'
g grep -n -e '\(cd\) \1' -e '\(abc\)\1'
g grep -n -E -e '(cd) \1' -e 'x(y)z\1'
g grep -n -E '(ab|cd)( )?\1'
g grep -n -E '^(ab)\1$'
g grep -n 'world[\1]'
g grep -n -i -E '(cd) \1' -- b.txt
g grep -n -E '(a*)*b\1'
g grep -n -E '(z)?b\1'
g grep -E '(a)\2'
g grep '\1'
g grep -E 'a\'
