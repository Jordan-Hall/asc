# `grep -E` with back-references, and the invalid-reference errors. Git runs these through the C
# library's regcomp: glibc (Linux, the pinned oracle) supports ERE back-references, macOS libc does
# not, so compat.rs runs this scenario everywhere but macOS.
g init
printf 'the the cat\nabab\nAbab here\nno repeat\nxyzzyx\nfoo bar foo\nworld1\n' > a.txt
printf 'ab-ab\nabcabc\ncd cd\nmixed Cd cd\n' > b.txt
g add a.txt b.txt
at 1
g commit -q -m first

g grep -n -E '(ab)\1'
g grep -n -E '(foo).*\1'
g grep -n -E '([a-z])([a-z])\2\1'
g grep -n -w -E '(ab)\1'
g grep -c -E '(cd) \1'
g grep -n -v -E '(.)\1'
g grep -l -E '(ab)-\1'
g grep -n -E -e '(cd) \1' -e 'x(y)z\1'
g grep -n -E '(ab|cd)( )?\1'
g grep -n -E '^(ab)\1$'
g grep -n -i -E '(cd) \1' -- b.txt
g grep -n -E '(a*)*b\1'
g grep -n -E '(z)?b\1'
g grep -E '(a)\2'
g grep -E 'a\'
g grep '\1'
